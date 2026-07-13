//! Deterministic reconstruction of run state from canonical Agent Events.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use young_agent_runtime::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalRequest, RunId, RunStatus, TerminalRunStatus,
    TurnId,
};
use young_model_runtime::ModelToolCallId;
use young_tool_runtime::execution::{ToolCall, ToolCallId, ToolOutput, ToolResult};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplayedToolCallIndex {
    requested_event: usize,
    approval_event: Option<usize>,
    approval_resolution_event: Option<usize>,
    result_event: Option<usize>,
}

/// Borrowed view of one tool invocation derived from canonical event indexes.
#[derive(Clone, Copy, Debug)]
pub struct ReplayedToolCall<'a> {
    events: &'a [AgentEvent],
    index: &'a ReplayedToolCallIndex,
}

impl<'a> ReplayedToolCall<'a> {
    pub fn turn_id(&self) -> &TurnId {
        match &self.events[self.index.requested_event] {
            AgentEvent::ToolCallRequested { turn_id, .. } => turn_id,
            _ => unreachable!("requested_event indexes ToolCallRequested"),
        }
    }

    pub fn model_tool_call_id(&self) -> &ModelToolCallId {
        match &self.events[self.index.requested_event] {
            AgentEvent::ToolCallRequested {
                model_tool_call_id, ..
            } => model_tool_call_id,
            _ => unreachable!("requested_event indexes ToolCallRequested"),
        }
    }

    pub fn call(&self) -> &ToolCall {
        match &self.events[self.index.requested_event] {
            AgentEvent::ToolCallRequested { call, .. } => call,
            _ => unreachable!("requested_event indexes ToolCallRequested"),
        }
    }

    pub fn approval(&self) -> Option<&ApprovalRequest> {
        self.index
            .approval_event
            .map(|event_index| match &self.events[event_index] {
                AgentEvent::ApprovalRequested { request, .. } => request,
                _ => unreachable!("approval_event indexes ApprovalRequested"),
            })
    }

    pub fn approval_decision(&self) -> Option<&ApprovalDecision> {
        self.index
            .approval_resolution_event
            .map(|event_index| match &self.events[event_index] {
                AgentEvent::ApprovalResolved { decision, .. } => decision,
                _ => unreachable!("approval_resolution_event indexes ApprovalResolved"),
            })
    }

    pub fn result(&self) -> Option<&ToolResult> {
        self.index
            .result_event
            .map(|event_index| match &self.events[event_index] {
                AgentEvent::ToolResult { result, .. } => result,
                _ => unreachable!("result_event indexes ToolResult"),
            })
    }
}

/// An immutable replay model derived from the ordered canonical timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReplay {
    run_id: RunId,
    status: RunStatus,
    events: Vec<AgentEvent>,
    tool_calls: Vec<ReplayedToolCallIndex>,
    approvals: Vec<usize>,
    errors: Vec<usize>,
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

    pub fn tool_calls(&self) -> impl ExactSizeIterator<Item = ReplayedToolCall<'_>> + '_ {
        self.tool_calls.iter().map(|index| ReplayedToolCall {
            events: &self.events,
            index,
        })
    }

    pub fn approvals(&self) -> impl ExactSizeIterator<Item = &ApprovalRequest> + '_ {
        self.approvals
            .iter()
            .map(|event_index| match &self.events[*event_index] {
                AgentEvent::ApprovalRequested { request, .. } => request,
                _ => unreachable!("approval index references ApprovalRequested"),
            })
    }

    pub fn errors(&self) -> impl ExactSizeIterator<Item = &AgentError> + '_ {
        self.errors
            .iter()
            .map(|event_index| match &self.events[*event_index] {
                AgentEvent::Error { error, .. } => error,
                _ => unreachable!("error index references Error"),
            })
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
    replay_events_with_mode(events, false, ReplayCompatibility::Strict)
}

/// Folds an inactive run's timeline and marks tool calls without results as
/// recovery work. Callers must first ensure no live runtime can still append to
/// the log; use [`replay_events`] for concurrent, read-only observation.
pub fn replay_events_for_recovery(events: Vec<AgentEvent>) -> Result<RunReplay, ReplayError> {
    replay_events_with_mode(events, true, ReplayCompatibility::Strict)
}

/// Replays a timeline with an explicit compatibility policy. Strict replay is
/// the default; the legacy mode exists only for pre-`ApprovalResolved` logs.
pub fn replay_events_with_compatibility(
    events: Vec<AgentEvent>,
    compatibility: ReplayCompatibility,
) -> Result<RunReplay, ReplayError> {
    replay_events_with_mode(events, false, compatibility)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplayCompatibility {
    #[default]
    Strict,
    LegacyApprovalWithoutResolution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApprovalLogFormat {
    #[default]
    Undetermined,
    Legacy,
    Resolved,
}

fn replay_events_with_mode(
    events: Vec<AgentEvent>,
    detect_recovery: bool,
    compatibility: ReplayCompatibility,
) -> Result<RunReplay, ReplayError> {
    let run_id = match events.first() {
        Some(AgentEvent::RunStarted { run_id, .. }) => run_id.clone(),
        Some(_) => return Err(ReplayError::FirstEventIsNotRunStarted),
        None => return Err(ReplayError::EmptyLog),
    };
    if compatibility == ReplayCompatibility::LegacyApprovalWithoutResolution {
        if let Some((index, _)) = events
            .iter()
            .enumerate()
            .find(|(_, event)| event.event_sequence().is_some())
        {
            return Err(ReplayError::LegacyCompatibilityForSequencedLog {
                event_number: index + 1,
            });
        }
    }

    let mut status = RunStatus::Running;
    let mut tool_calls = Vec::<ReplayedToolCallIndex>::new();
    let mut tool_call_indexes = HashMap::<ToolCallId, usize>::new();
    let mut approval_indexes = HashMap::<String, usize>::new();
    let mut pending_approvals = HashSet::<usize>::new();
    let mut approvals = Vec::new();
    let mut errors = Vec::new();
    let mut run_finished = false;
    let mut approval_log_format = ApprovalLogFormat::Undetermined;
    let mut started_turns = HashSet::<TurnId>::new();
    let mut active_turn = None::<TurnId>;

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
            AgentEvent::TurnStarted { turn_id, .. } => {
                if !started_turns.insert(turn_id.clone()) {
                    return Err(ReplayError::DuplicateTurnStarted {
                        event_number,
                        turn_id: turn_id.clone(),
                    });
                }
                let call_ids = tool_calls
                    .iter()
                    .filter(|tool_call| tool_call.result_event.is_none())
                    .map(|tool_call| {
                        ReplayedToolCall {
                            events: &events,
                            index: tool_call,
                        }
                        .call()
                        .id
                        .clone()
                    })
                    .collect::<Vec<_>>();
                if !call_ids.is_empty() {
                    return Err(ReplayError::TurnStartedWithUnresolvedToolCalls {
                        event_number,
                        turn_id: turn_id.clone(),
                        call_ids,
                    });
                }
                active_turn = Some(turn_id.clone());
            }
            AgentEvent::ModelOutput { turn_id, .. }
            | AgentEvent::ToolCallRequested { turn_id, .. }
            | AgentEvent::ApprovalRequested { turn_id, .. }
            | AgentEvent::ApprovalResolved { turn_id, .. }
            | AgentEvent::ToolResult { turn_id, .. }
            | AgentEvent::Error {
                turn_id: Some(turn_id),
                ..
            } => {
                if !started_turns.contains(turn_id) {
                    return Err(ReplayError::EventForUnknownTurn {
                        event_number,
                        turn_id: turn_id.clone(),
                    });
                }
                if active_turn.as_ref() != Some(turn_id) {
                    return Err(ReplayError::EventForInactiveTurn {
                        event_number,
                        expected: active_turn
                            .clone()
                            .expect("a known turn implies an active turn"),
                        found: turn_id.clone(),
                    });
                }
            }
            AgentEvent::RunStarted { .. }
            | AgentEvent::Error { turn_id: None, .. }
            | AgentEvent::RunFinished { .. } => {}
        }

        match event {
            AgentEvent::RunStarted { .. } => {
                return Err(ReplayError::DuplicateRunStarted { event_number });
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                if tool_call_indexes.contains_key(&call.id) {
                    return Err(ReplayError::DuplicateToolCall {
                        event_number,
                        call_id: call.id.clone(),
                    });
                }

                let tool_call_index = tool_calls.len();
                tool_call_indexes.insert(call.id.clone(), tool_call_index);
                tool_calls.push(ReplayedToolCallIndex {
                    requested_event: index,
                    approval_event: None,
                    approval_resolution_event: None,
                    result_event: None,
                });
            }
            AgentEvent::ApprovalRequested {
                turn_id, request, ..
            } => {
                let Some(&tool_call_index) = tool_call_indexes.get(&request.call.id) else {
                    return Err(ReplayError::ApprovalForUnknownToolCall {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                };
                let replayed_call = ReplayedToolCall {
                    events: &events,
                    index: &tool_calls[tool_call_index],
                };

                if replayed_call.turn_id() != turn_id {
                    return Err(ReplayError::MismatchedToolLifecycleTurn {
                        event_number,
                        call_id: request.call.id.clone(),
                        expected: replayed_call.turn_id().clone(),
                        found: turn_id.clone(),
                    });
                }

                if replayed_call.call() != &request.call {
                    return Err(ReplayError::ApprovalCallMismatch {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                }
                if replayed_call.result().is_some() {
                    return Err(ReplayError::ApprovalAfterToolResult {
                        event_number,
                        call_id: request.call.id.clone(),
                    });
                }
                if replayed_call.approval().is_some() {
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

                tool_calls[tool_call_index].approval_event = Some(index);
                approval_indexes.insert(request.id.clone(), tool_call_index);
                approvals.push(index);
                pending_approvals.insert(tool_call_index);
                status = RunStatus::AwaitingApproval;
            }
            AgentEvent::ApprovalResolved {
                turn_id,
                approval_id,
                ..
            } => {
                let Some(&tool_call_index) = approval_indexes.get(approval_id) else {
                    return Err(ReplayError::ResolutionForUnknownApproval {
                        event_number,
                        approval_id: approval_id.clone(),
                    });
                };
                let replayed_call = ReplayedToolCall {
                    events: &events,
                    index: &tool_calls[tool_call_index],
                };
                if replayed_call.turn_id() != turn_id {
                    return Err(ReplayError::MismatchedToolLifecycleTurn {
                        event_number,
                        call_id: replayed_call.call().id.clone(),
                        expected: replayed_call.turn_id().clone(),
                        found: turn_id.clone(),
                    });
                }
                if replayed_call.approval_decision().is_some() {
                    return Err(ReplayError::DuplicateApprovalResolution {
                        event_number,
                        approval_id: approval_id.clone(),
                    });
                }
                if replayed_call.result().is_some() {
                    return Err(ReplayError::ApprovalResolutionAfterToolResult {
                        event_number,
                        approval_id: approval_id.clone(),
                    });
                }
                if compatibility == ReplayCompatibility::LegacyApprovalWithoutResolution
                    && approval_log_format == ApprovalLogFormat::Legacy
                {
                    return Err(ReplayError::MixedApprovalLogFormats { event_number });
                }
                approval_log_format = ApprovalLogFormat::Resolved;

                tool_calls[tool_call_index].approval_resolution_event = Some(index);
                pending_approvals.remove(&tool_call_index);
                if pending_approvals.is_empty() {
                    status = RunStatus::Running;
                }
            }
            AgentEvent::ToolResult {
                turn_id, result, ..
            } => {
                let Some(&tool_call_index) = tool_call_indexes.get(&result.call_id) else {
                    return Err(ReplayError::ResultForUnknownToolCall {
                        event_number,
                        call_id: result.call_id.clone(),
                    });
                };
                let replayed_call = ReplayedToolCall {
                    events: &events,
                    index: &tool_calls[tool_call_index],
                };

                if replayed_call.turn_id() != turn_id {
                    return Err(ReplayError::MismatchedToolLifecycleTurn {
                        event_number,
                        call_id: result.call_id.clone(),
                        expected: replayed_call.turn_id().clone(),
                        found: turn_id.clone(),
                    });
                }

                if replayed_call.result().is_some() {
                    return Err(ReplayError::DuplicateToolResult {
                        event_number,
                        call_id: result.call_id.clone(),
                    });
                }
                match replayed_call.approval_decision() {
                    None if replayed_call.approval().is_some()
                        && compatibility == ReplayCompatibility::Strict =>
                    {
                        return Err(ReplayError::ToolResultBeforeApprovalResolution {
                            event_number,
                            call_id: result.call_id.clone(),
                        });
                    }
                    None if replayed_call.approval().is_some() => {
                        if approval_log_format == ApprovalLogFormat::Resolved {
                            return Err(ReplayError::ToolResultBeforeApprovalResolution {
                                event_number,
                                call_id: result.call_id.clone(),
                            });
                        }
                        if matches!(
                            &result.output,
                            ToolOutput::Failure { error, extensions }
                                if error.code == "approval_denied"
                                    && (error.retryable || !extensions.is_empty())
                        ) {
                            return Err(ReplayError::InvalidApprovalDenialResult {
                                event_number,
                                call_id: result.call_id.clone(),
                            });
                        }
                        approval_log_format = ApprovalLogFormat::Legacy;
                    }
                    Some(ApprovalDecision::Deny { reason }) => {
                        let is_canonical = matches!(
                            &result.output,
                            ToolOutput::Failure { error, extensions }
                                if error.code == "approval_denied"
                                    && error.message == *reason
                                    && !error.retryable
                                    && extensions.is_empty()
                        );
                        if !is_canonical {
                            return Err(ReplayError::InvalidApprovalDenialResult {
                                event_number,
                                call_id: result.call_id.clone(),
                            });
                        }
                    }
                    Some(ApprovalDecision::Approve) | None => {
                        if matches!(
                            &result.output,
                            ToolOutput::Failure { error, .. }
                                if error.code == "approval_denied"
                        ) {
                            return Err(ReplayError::InvalidApprovalDenialResult {
                                event_number,
                                call_id: result.call_id.clone(),
                            });
                        }
                    }
                }

                tool_calls[tool_call_index].result_event = Some(index);
                pending_approvals.remove(&tool_call_index);
                if pending_approvals.is_empty() {
                    status = RunStatus::Running;
                }
            }
            AgentEvent::Error { .. } => errors.push(index),
            AgentEvent::RunFinished {
                status: terminal_status,
                ..
            } => {
                if matches!(
                    terminal_status,
                    TerminalRunStatus::Completed { .. } | TerminalRunStatus::Failed { .. }
                ) {
                    let call_ids = tool_calls
                        .iter()
                        .filter(|tool_call| tool_call.result_event.is_none())
                        .map(|tool_call| {
                            ReplayedToolCall {
                                events: &events,
                                index: tool_call,
                            }
                            .call()
                            .id
                            .clone()
                        })
                        .collect::<Vec<_>>();
                    if !call_ids.is_empty() {
                        return Err(ReplayError::TerminalWithUnresolvedToolCalls {
                            event_number,
                            call_ids,
                        });
                    }
                }
                status = RunStatus::Finished {
                    terminal_status: terminal_status.clone(),
                };
                run_finished = true;
            }
            AgentEvent::TurnStarted { .. } | AgentEvent::ModelOutput { .. } => {}
        }
    }

    if detect_recovery && matches!(status, RunStatus::Running) {
        let call_ids = tool_calls
            .iter()
            .filter(|tool_call| tool_call.result_event.is_none())
            .map(|tool_call| {
                ReplayedToolCall {
                    events: &events,
                    index: tool_call,
                }
                .call()
                .id
                .clone()
            })
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
    DuplicateTurnStarted {
        event_number: usize,
        turn_id: TurnId,
    },
    EventForUnknownTurn {
        event_number: usize,
        turn_id: TurnId,
    },
    EventForInactiveTurn {
        event_number: usize,
        expected: TurnId,
        found: TurnId,
    },
    TurnStartedWithUnresolvedToolCalls {
        event_number: usize,
        turn_id: TurnId,
        call_ids: Vec<ToolCallId>,
    },
    MismatchedToolLifecycleTurn {
        event_number: usize,
        call_id: ToolCallId,
        expected: TurnId,
        found: TurnId,
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
    ApprovalResolutionAfterToolResult {
        event_number: usize,
        approval_id: String,
    },
    ToolResultBeforeApprovalResolution {
        event_number: usize,
        call_id: ToolCallId,
    },
    InvalidApprovalDenialResult {
        event_number: usize,
        call_id: ToolCallId,
    },
    MixedApprovalLogFormats {
        event_number: usize,
    },
    LegacyCompatibilityForSequencedLog {
        event_number: usize,
    },
    TerminalWithUnresolvedToolCalls {
        event_number: usize,
        call_ids: Vec<ToolCallId>,
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
            Self::DuplicateTurnStarted {
                event_number,
                turn_id,
            } => write!(
                formatter,
                "Event Log event {event_number} repeats turn_started for turn '{}'",
                turn_id.as_str()
            ),
            Self::EventForUnknownTurn {
                event_number,
                turn_id,
            } => write!(
                formatter,
                "Event Log event {event_number} belongs to turn '{}' before that turn started",
                turn_id.as_str()
            ),
            Self::EventForInactiveTurn {
                event_number,
                expected,
                found,
            } => write!(
                formatter,
                "Event Log event {event_number} flows back to inactive turn '{}' while turn '{}' is active",
                found.as_str(),
                expected.as_str()
            ),
            Self::TurnStartedWithUnresolvedToolCalls {
                event_number,
                turn_id,
                call_ids,
            } => write!(
                formatter,
                "Event Log event {event_number} starts turn '{}' while {} tool call(s) remain unresolved",
                turn_id.as_str(),
                call_ids.len()
            ),
            Self::MismatchedToolLifecycleTurn {
                event_number,
                call_id,
                expected,
                found,
            } => write!(
                formatter,
                "Event Log event {event_number} places tool call '{}' on turn '{}' instead of its originating turn '{}'",
                call_id.as_str(),
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
            Self::ApprovalResolutionAfterToolResult {
                event_number,
                approval_id,
            } => write!(
                formatter,
                "Event Log event {event_number} resolves approval '{approval_id}' after its tool call already has a result"
            ),
            Self::ToolResultBeforeApprovalResolution {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} records a result before approval for tool call '{}' was resolved",
                call_id.as_str()
            ),
            Self::InvalidApprovalDenialResult {
                event_number,
                call_id,
            } => write!(
                formatter,
                "Event Log event {event_number} records an invalid approval-denial result for tool call '{}'",
                call_id.as_str()
            ),
            Self::MixedApprovalLogFormats { event_number } => write!(
                formatter,
                "Event Log event {event_number} mixes legacy approvals without resolutions and modern ApprovalResolved events"
            ),
            Self::LegacyCompatibilityForSequencedLog { event_number } => write!(
                formatter,
                "Event Log event {event_number} is sequenced and cannot use pre-ApprovalResolved compatibility"
            ),
            Self::TerminalWithUnresolvedToolCalls {
                event_number,
                call_ids,
            } => write!(
                formatter,
                "Event Log event {event_number} finishes successfully or with failure while {} tool call(s) remain unresolved",
                call_ids.len()
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
