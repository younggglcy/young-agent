//! Deterministic reconstruction of run state from canonical Agent Events.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use young_agent_runtime::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalRequest, RunId, RunStatus, TerminalRunStatus,
};
use young_model_runtime::ModelToolCallId;
use young_tool_runtime::execution::{ToolCall, ToolCallId, ToolResult};

/// The observed lifecycle of one tool invocation during replay.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayedToolCall {
    model_tool_call_id: ModelToolCallId,
    call: ToolCall,
    approval: Option<ApprovalRequest>,
    approval_decision: Option<ApprovalDecision>,
    result: Option<ToolResult>,
}

impl ReplayedToolCall {
    pub fn model_tool_call_id(&self) -> &ModelToolCallId {
        &self.model_tool_call_id
    }

    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    pub fn approval(&self) -> Option<&ApprovalRequest> {
        self.approval.as_ref()
    }

    pub fn approval_decision(&self) -> Option<&ApprovalDecision> {
        self.approval_decision.as_ref()
    }

    pub fn result(&self) -> Option<&ToolResult> {
        self.result.as_ref()
    }
}

/// An immutable replay model derived from the ordered canonical timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReplay {
    run_id: RunId,
    status: RunStatus,
    events: Vec<AgentEvent>,
    tool_calls: Vec<ReplayedToolCall>,
    approvals: Vec<ApprovalRequest>,
    errors: Vec<AgentError>,
}

impl RunReplay {
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn status(&self) -> &RunStatus {
        &self.status
    }

    pub fn events(&self) -> &[AgentEvent] {
        &self.events
    }

    pub fn tool_calls(&self) -> &[ReplayedToolCall] {
        &self.tool_calls
    }

    pub fn approvals(&self) -> &[ApprovalRequest] {
        &self.approvals
    }

    pub fn errors(&self) -> &[AgentError] {
        &self.errors
    }

    /// Returns the single terminal truth carried by `RunFinished`, if present.
    pub fn terminal_status(&self) -> Option<&TerminalRunStatus> {
        match &self.status {
            RunStatus::Finished { terminal_status } => Some(terminal_status),
            RunStatus::Running
            | RunStatus::AwaitingApproval
            | RunStatus::RecoveryRequired { .. } => None,
        }
    }
}

/// Folds an ordered event timeline into its observable run state.
pub fn replay_events(events: Vec<AgentEvent>) -> Result<RunReplay, ReplayError> {
    let run_id = match events.first() {
        Some(AgentEvent::RunStarted { run_id, .. }) => run_id.clone(),
        Some(_) => return Err(ReplayError::FirstEventIsNotRunStarted),
        None => return Err(ReplayError::EmptyLog),
    };

    let mut status = RunStatus::Running;
    let mut tool_calls = Vec::<ReplayedToolCall>::new();
    let mut tool_call_indexes = HashMap::<ToolCallId, usize>::new();
    let mut approval_indexes = HashMap::<String, usize>::new();
    let mut pending_approvals = HashSet::<ToolCallId>::new();
    let mut approvals = Vec::new();
    let mut errors = Vec::new();
    let mut run_finished = false;

    for (index, event) in events.iter().enumerate().skip(1) {
        let event_number = index + 1;

        if run_finished {
            return Err(ReplayError::EventAfterRunFinished { event_number });
        }

        let found_run_id = event.run_id();
        if found_run_id != &run_id {
            return Err(ReplayError::MismatchedRunId {
                event_number,
                expected: run_id.clone(),
                found: found_run_id.clone(),
            });
        }

        match event {
            AgentEvent::RunStarted { .. } => {
                return Err(ReplayError::DuplicateRunStarted { event_number });
            }
            AgentEvent::ToolCallRequested {
                model_tool_call_id,
                call,
                ..
            } => {
                if tool_call_indexes.contains_key(&call.id) {
                    return Err(ReplayError::DuplicateToolCall {
                        event_number,
                        call_id: call.id.clone(),
                    });
                }

                let tool_call_index = tool_calls.len();
                tool_call_indexes.insert(call.id.clone(), tool_call_index);
                tool_calls.push(ReplayedToolCall {
                    model_tool_call_id: model_tool_call_id.clone(),
                    call: call.clone(),
                    approval: None,
                    approval_decision: None,
                    result: None,
                });
            }
            AgentEvent::ApprovalRequested { request, .. } => {
                let Some(&tool_call_index) = tool_call_indexes.get(&request.call.id) else {
                    return Err(ReplayError::ApprovalForUnknownToolCall {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                };
                let replayed_call = &mut tool_calls[tool_call_index];

                if replayed_call.call != request.call {
                    return Err(ReplayError::ApprovalCallMismatch {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                }
                if replayed_call.result.is_some() {
                    return Err(ReplayError::ApprovalAfterToolResult {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                }
                if replayed_call.approval.is_some() {
                    return Err(ReplayError::DuplicateApproval {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                }
                if approval_indexes.contains_key(&request.id) {
                    return Err(ReplayError::DuplicateApprovalId {
                        event_number,
                        approval_id: request.id.clone(),
                    });
                }

                replayed_call.approval = Some(request.clone());
                approval_indexes.insert(request.id.clone(), tool_call_index);
                approvals.push(request.clone());
                pending_approvals.insert(request.call.id.clone());
                status = RunStatus::AwaitingApproval;
            }
            AgentEvent::ApprovalResolved {
                approval_id,
                decision,
                ..
            } => {
                let Some(&tool_call_index) = approval_indexes.get(approval_id) else {
                    return Err(ReplayError::ResolutionForUnknownApproval {
                        event_number,
                        approval_id: approval_id.clone(),
                    });
                };
                let replayed_call = &mut tool_calls[tool_call_index];
                if replayed_call.approval_decision.is_some() {
                    return Err(ReplayError::DuplicateApprovalResolution {
                        event_number,
                        approval_id: approval_id.clone(),
                    });
                }

                replayed_call.approval_decision = Some(decision.clone());
                pending_approvals.remove(&replayed_call.call.id);
                if pending_approvals.is_empty() {
                    status = RunStatus::Running;
                }
            }
            AgentEvent::ToolResult { result, .. } => {
                let Some(&tool_call_index) = tool_call_indexes.get(&result.call_id) else {
                    return Err(ReplayError::ResultForUnknownToolCall {
                        event_number,
                        call_id: result.call_id.clone(),
                    });
                };
                let replayed_call = &mut tool_calls[tool_call_index];

                if replayed_call.result.is_some() {
                    return Err(ReplayError::DuplicateToolResult {
                        event_number,
                        call_id: result.call_id.clone(),
                    });
                }

                replayed_call.result = Some(result.clone());
                pending_approvals.remove(&result.call_id);
                if pending_approvals.is_empty() {
                    status = RunStatus::Running;
                }
            }
            AgentEvent::Error { error, .. } => errors.push(error.clone()),
            AgentEvent::RunFinished {
                status: terminal_status,
                ..
            } => {
                status = RunStatus::Finished {
                    terminal_status: terminal_status.clone(),
                };
                run_finished = true;
            }
            AgentEvent::TurnStarted { .. } | AgentEvent::ModelOutput { .. } => {}
        }
    }

    if matches!(status, RunStatus::Running) {
        let call_ids = tool_calls
            .iter()
            .filter(|tool_call| tool_call.result.is_none())
            .map(|tool_call| tool_call.call.id.clone())
            .collect::<Vec<_>>();
        if !call_ids.is_empty() {
            status = RunStatus::RecoveryRequired { call_ids };
        }
    }

    Ok(RunReplay {
        run_id,
        status,
        events,
        tool_calls,
        approvals,
        errors,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplayError {
    EmptyLog,
    FirstEventIsNotRunStarted,
    DuplicateRunStarted {
        event_number: usize,
    },
    MismatchedRunId {
        event_number: usize,
        expected: RunId,
        found: RunId,
    },
    EventAfterRunFinished {
        event_number: usize,
    },
    DuplicateToolCall {
        event_number: usize,
        call_id: ToolCallId,
    },
    ApprovalForUnknownToolCall {
        event_number: usize,
        call_id: ToolCallId,
    },
    ResultForUnknownToolCall {
        event_number: usize,
        call_id: ToolCallId,
    },
    ApprovalCallMismatch {
        event_number: usize,
        call_id: ToolCallId,
    },
    ApprovalAfterToolResult {
        event_number: usize,
        call_id: ToolCallId,
    },
    DuplicateApproval {
        event_number: usize,
        call_id: ToolCallId,
    },
    DuplicateApprovalId {
        event_number: usize,
        approval_id: String,
    },
    ResolutionForUnknownApproval {
        event_number: usize,
        approval_id: String,
    },
    DuplicateApprovalResolution {
        event_number: usize,
        approval_id: String,
    },
    DuplicateToolResult {
        event_number: usize,
        call_id: ToolCallId,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyLog => write!(formatter, "cannot replay an empty Event Log"),
            Self::FirstEventIsNotRunStarted => {
                write!(formatter, "Event Log event 1 must be run_started")
            }
            Self::DuplicateRunStarted { event_number } => write!(
                formatter,
                "Event Log event {event_number} starts an already-started run"
            ),
            Self::MismatchedRunId {
                event_number,
                expected,
                found,
            } => write!(
                formatter,
                "Event Log event {event_number} belongs to run '{}' instead of '{}'",
                found.as_str(),
                expected.as_str()
            ),
            Self::EventAfterRunFinished { event_number } => write!(
                formatter,
                "Event Log event {event_number} appears after run_finished"
            ),
            Self::DuplicateToolCall {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats tool call '{}'",
                call_id.as_str()
            ),
            Self::ApprovalForUnknownToolCall {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} has an approval request for unknown tool call '{}'",
                call_id.as_str()
            ),
            Self::ResultForUnknownToolCall {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} has a result for unknown tool call '{}'",
                call_id.as_str()
            ),
            Self::ApprovalCallMismatch {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} changes the approved payload for tool call '{}'",
                call_id.as_str()
            ),
            Self::ApprovalAfterToolResult {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} requests approval for tool call '{}' after it already has a result",
                call_id.as_str()
            ),
            Self::DuplicateApproval {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats approval for tool call '{}'",
                call_id.as_str()
            ),
            Self::DuplicateApprovalId {
                event_number,
                approval_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats approval id '{approval_id}'"
            ),
            Self::ResolutionForUnknownApproval {
                event_number,
                approval_id,
            } => write!(
                formatter,
                "Event Log event {event_number} resolves unknown approval '{approval_id}'"
            ),
            Self::DuplicateApprovalResolution {
                event_number,
                approval_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats resolution for approval '{approval_id}'"
            ),
            Self::DuplicateToolResult {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats the result for tool call '{}'",
                call_id.as_str()
            ),
        }
    }
}

impl Error for ReplayError {}
