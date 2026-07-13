use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use young_model_runtime::{
    ModelClient, ModelError, ModelMessage, ModelMessageContent, ModelRequest, ModelStreamEvent,
    ModelToolCall, ModelToolSpec,
};
use young_tool_runtime::{ToolCall, ToolCallId, ToolContent, ToolExecutor, ToolOutput, ToolResult};

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalRequest, RunId, TerminalRunStatus, TurnId,
};

const MAX_TURNS: usize = 128;

pub trait AgentEventSink {
    type Error;

    /// Attempts to append one event. An error may be ambiguous for durable
    /// sinks (for example, a flush can fail after bytes were written), so
    /// callers must inspect the Canonical Event Log before retrying.
    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error>;

    /// Appends an event and makes its commit marker durable before returning.
    /// In-memory sinks may implement this identically to [`Self::append`], but
    /// persistent sinks must not return until the commit marker is stable.
    fn append_durable(&mut self, event: &AgentEvent) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunControlFlow {
    Continue,
    Interrupt { reason: String },
    Cancel { reason: String },
}

/// Synchronous control seam for the deterministic proof runtime.
/// Interactive surfaces should eventually provide async approval and stop
/// handling rather than treating this trait as stable.
pub trait RunControl {
    /// Synchronous checkpoint evaluated between runtime steps. Use
    /// `RunStopToken` when another thread must stop provider or tool work that
    /// is currently pending.
    fn checkpoint(&mut self) -> RunControlFlow;

    /// Waits for an approval decision. Implementations that block on a human
    /// or external policy service must observe `cancellation` and return
    /// promptly once it is set.
    fn decide_approval(
        &mut self,
        _request: &ApprovalRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        ApprovalDecision::Deny {
            reason: "no approval handler accepted the tool call".to_string(),
        }
    }
}

impl<F> RunControl for F
where
    F: FnMut() -> RunControlFlow,
{
    fn checkpoint(&mut self) -> RunControlFlow {
        self()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunStopToken {
    cancellation: Arc<AtomicBool>,
    terminal_status: Arc<Mutex<Option<TerminalRunStatus>>>,
}

impl RunStopToken {
    pub fn interrupt(&self, reason: impl Into<String>) {
        self.request_stop(TerminalRunStatus::Interrupted {
            reason: reason.into(),
        });
    }

    pub fn cancel(&self, reason: impl Into<String>) {
        self.request_stop(TerminalRunStatus::Cancelled {
            reason: reason.into(),
        });
    }

    pub fn is_requested(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }

    /// Returns the first terminal status chosen for this run, including normal
    /// completion and failure as well as interruption and cancellation.
    pub fn terminal_status(&self) -> Option<TerminalRunStatus> {
        self.terminal_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn request_stop(&self, status: TerminalRunStatus) {
        self.resolve_terminal(status);
    }

    fn resolve_terminal(&self, proposed: TerminalRunStatus) -> TerminalRunStatus {
        let mut terminal_status = self
            .terminal_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if terminal_status.is_none() {
            let is_stop = matches!(
                proposed,
                TerminalRunStatus::Interrupted { .. } | TerminalRunStatus::Cancelled { .. }
            );
            *terminal_status = Some(proposed);
            if is_stop {
                self.cancellation.store(true, Ordering::Release);
            }
        }
        terminal_status
            .clone()
            .expect("terminal status was initialized")
    }

    fn status(&self) -> Option<TerminalRunStatus> {
        if !self.is_requested() {
            return None;
        }
        self.terminal_status()
    }

    fn cancellation_flag(&self) -> Arc<AtomicBool> {
        self.cancellation.clone()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunRequest {
    pub run_id: RunId,
    pub model: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSpec>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    status: TerminalRunStatus,
}

impl RunOutcome {
    pub fn status(&self) -> &TerminalRunStatus {
        &self.status
    }
}

struct CollectedModelTurn {
    text: String,
    tool_calls: Vec<ModelToolCall>,
}

enum ModelTurnProgress {
    Collected(CollectedModelTurn),
    Finished(RunOutcome),
}

enum ToolCallProgress {
    Message(ModelMessage),
    Finished(RunOutcome),
}

#[derive(Default)]
struct RunSequences {
    tool_call: usize,
    approval: usize,
}

pub struct AgentRuntime<M, T, S> {
    model_client: M,
    tool_executor: T,
    event_sink: S,
}

impl<M, T, S> AgentRuntime<M, T, S> {
    pub fn new(model_client: M, tool_executor: T, event_sink: S) -> Self {
        Self {
            model_client,
            tool_executor,
            event_sink,
        }
    }

    pub fn model_client(&self) -> &M {
        &self.model_client
    }

    pub fn tool_executor(&self) -> &T {
        &self.tool_executor
    }
}

impl<M, T, S> AgentRuntime<M, T, S>
where
    M: ModelClient,
    T: ToolExecutor,
    S: AgentEventSink,
{
    pub fn run(&mut self, request: RunRequest) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        let mut control = || RunControlFlow::Continue;
        let stop = RunStopToken::default();
        self.run_with_control_and_stop(request, &mut control, &stop)
    }

    pub fn run_with_control<C>(
        &mut self,
        request: RunRequest,
        control: &mut C,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>>
    where
        C: RunControl,
    {
        let stop = RunStopToken::default();
        self.run_with_control_and_stop(request, control, &stop)
    }

    pub fn run_with_stop_token(
        &mut self,
        request: RunRequest,
        stop: &RunStopToken,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        let mut control = || RunControlFlow::Continue;
        self.run_with_control_and_stop(request, &mut control, stop)
    }

    pub fn run_with_control_and_stop<C>(
        &mut self,
        request: RunRequest,
        control: &mut C,
        stop: &RunStopToken,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>>
    where
        C: RunControl,
    {
        let RunRequest {
            run_id,
            model,
            messages,
            tools,
            metadata,
        } = request;

        let mut model_request = ModelRequest {
            model,
            messages,
            tools,
            metadata,
        };

        self.emit(&AgentEvent::RunStarted {
            run_id: run_id.clone(),
            extensions: BTreeMap::new(),
        })?;

        let mut sequences = RunSequences::default();

        for turn_number in 1..=MAX_TURNS {
            if let Some(status) = stopped_status(control.checkpoint(), stop) {
                return self.finish(&run_id, status, stop);
            }

            let turn_id = TurnId::new(format!("turn-{turn_number:03}"));
            self.emit(&AgentEvent::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                extensions: BTreeMap::new(),
            })?;

            let collected =
                match self.collect_model_turn(&run_id, &turn_id, &model_request, control, stop)? {
                    ModelTurnProgress::Collected(collected) => collected,
                    ModelTurnProgress::Finished(outcome) => return Ok(outcome),
                };

            if collected.tool_calls.is_empty() {
                let status = TerminalRunStatus::Completed {
                    final_message: collected.text,
                };
                return self.finish(&run_id, status, stop);
            }

            model_request.messages.push(if collected.text.is_empty() {
                ModelMessage::assistant_tool_calls(collected.tool_calls.clone())
            } else {
                ModelMessage::assistant_with_tool_calls(
                    collected.text,
                    collected.tool_calls.clone(),
                )
            });

            for model_tool_call in collected.tool_calls {
                match self.drive_tool_call(
                    &run_id,
                    &turn_id,
                    model_tool_call,
                    &mut sequences,
                    control,
                    stop,
                )? {
                    ToolCallProgress::Message(message) => model_request.messages.push(message),
                    ToolCallProgress::Finished(outcome) => return Ok(outcome),
                }
            }
        }

        self.finish_agent_error(
            &run_id,
            None,
            AgentError {
                code: "turn_limit_reached".to_string(),
                message: format!("Agent Run exceeded the {MAX_TURNS}-turn safety limit"),
                recoverable: false,
            },
            stop,
        )
    }

    fn collect_model_turn<C>(
        &mut self,
        run_id: &RunId,
        turn_id: &TurnId,
        request: &ModelRequest,
        control: &mut C,
        stop: &RunStopToken,
    ) -> Result<ModelTurnProgress, AgentRuntimeError<S::Error>>
    where
        C: RunControl,
    {
        let stream = match self.model_client.stream(request, stop.cancellation_flag()) {
            Ok(stream) => stream,
            Err(error) => {
                let outcome = if let Some(status) = stop.status() {
                    self.finish(run_id, status, stop)?
                } else {
                    self.finish_model_error(run_id, Some(turn_id.clone()), error, stop)?
                };
                return Ok(ModelTurnProgress::Finished(outcome));
            }
        };

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut model_tool_call_ids = HashSet::new();
        let mut completed = false;

        for model_event in stream {
            if let Some(status) = stopped_status(control.checkpoint(), stop) {
                return self
                    .finish(run_id, status, stop)
                    .map(ModelTurnProgress::Finished);
            }

            let model_output_event = AgentEvent::ModelOutput {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event: model_event,
                extensions: BTreeMap::new(),
            };
            self.emit(&model_output_event)?;
            let AgentEvent::ModelOutput {
                event: model_event, ..
            } = model_output_event
            else {
                unreachable!("model_output_event is constructed as AgentEvent::ModelOutput")
            };

            match model_event {
                ModelStreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
                ModelStreamEvent::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    if !model_tool_call_ids.insert(id.clone()) {
                        return self
                            .finish_agent_error(
                                run_id,
                                Some(turn_id.clone()),
                                AgentError {
                                    code: "duplicate_model_tool_call_id".to_string(),
                                    message: format!(
                                        "model emitted duplicate tool call id '{}' in one turn",
                                        id.as_str()
                                    ),
                                    recoverable: false,
                                },
                                stop,
                            )
                            .map(ModelTurnProgress::Finished);
                    }
                    tool_calls.push(ModelToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                ModelStreamEvent::Completed { .. } => {
                    completed = true;
                    break;
                }
                ModelStreamEvent::Failed { error, .. } => {
                    return self
                        .finish_model_error(run_id, Some(turn_id.clone()), error, stop)
                        .map(ModelTurnProgress::Finished);
                }
                ModelStreamEvent::Started { .. }
                | ModelStreamEvent::ToolCallDelta { .. }
                | ModelStreamEvent::Usage { .. } => {}
            }
        }

        if let Some(status) = stop.status() {
            return self
                .finish(run_id, status, stop)
                .map(ModelTurnProgress::Finished);
        }
        if !completed {
            return self
                .finish_agent_error(
                    run_id,
                    Some(turn_id.clone()),
                    AgentError {
                        code: "model_stream_incomplete".to_string(),
                        message: "model stream ended without a completed event".to_string(),
                        recoverable: false,
                    },
                    stop,
                )
                .map(ModelTurnProgress::Finished);
        }

        Ok(ModelTurnProgress::Collected(CollectedModelTurn {
            text,
            tool_calls,
        }))
    }

    fn drive_tool_call<C>(
        &mut self,
        run_id: &RunId,
        turn_id: &TurnId,
        model_tool_call: ModelToolCall,
        sequences: &mut RunSequences,
        control: &mut C,
        stop: &RunStopToken,
    ) -> Result<ToolCallProgress, AgentRuntimeError<S::Error>>
    where
        C: RunControl,
    {
        sequences.tool_call += 1;
        let call = ToolCall {
            id: ToolCallId::new(format!(
                "{}-tool-{:03}",
                run_id.as_str(),
                sequences.tool_call
            )),
            tool_name: model_tool_call.name.clone(),
            arguments: model_tool_call.arguments,
        };
        let requested_event = AgentEvent::ToolCallRequested {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            model_tool_call_id: model_tool_call.id.clone(),
            call,
            extensions: BTreeMap::new(),
        };
        self.emit_durable(&requested_event)?;
        let AgentEvent::ToolCallRequested { call, .. } = requested_event else {
            unreachable!("requested_event is constructed as AgentEvent::ToolCallRequested")
        };

        let output = if let Some(reason) = self.tool_executor.approval_reason(&call) {
            sequences.approval += 1;
            let approval = ApprovalRequest {
                id: format!("{}-approval-{:03}", run_id.as_str(), sequences.approval),
                call: call.clone(),
                reason,
            };
            let approval_event = AgentEvent::ApprovalRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                request: approval,
                extensions: BTreeMap::new(),
            };
            self.emit(&approval_event)?;
            let AgentEvent::ApprovalRequested {
                request: approval, ..
            } = approval_event
            else {
                unreachable!("approval_event is constructed as AgentEvent::ApprovalRequested")
            };

            let decision = control.decide_approval(&approval, stop.cancellation_flag());
            if let Some(status) = stop.status() {
                return self
                    .finish(run_id, status, stop)
                    .map(ToolCallProgress::Finished);
            }
            self.emit(&AgentEvent::ApprovalResolved {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                approval_id: approval.id,
                decision: decision.clone(),
                extensions: BTreeMap::new(),
            })?;

            match decision {
                ApprovalDecision::Approve => {
                    if let Some(status) = stopped_status(control.checkpoint(), stop) {
                        return self
                            .finish(run_id, status, stop)
                            .map(ToolCallProgress::Finished);
                    }
                    self.tool_executor.execute(&call, stop.cancellation_flag())
                }
                ApprovalDecision::Deny { reason } => ToolOutput::Failure {
                    error: young_tool_runtime::ToolError {
                        code: "approval_denied".to_string(),
                        message: reason,
                        retryable: false,
                    },
                    extensions: BTreeMap::new(),
                },
            }
        } else {
            if let Some(status) = stopped_status(control.checkpoint(), stop) {
                return self
                    .finish(run_id, status, stop)
                    .map(ToolCallProgress::Finished);
            }
            self.tool_executor.execute(&call, stop.cancellation_flag())
        };

        let result = ToolResult {
            call_id: call.id.clone(),
            output,
        };
        let result_event = AgentEvent::ToolResult {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            result,
            extensions: BTreeMap::new(),
        };
        self.emit_tool_result(&result_event)?;
        let AgentEvent::ToolResult { result, .. } = result_event else {
            unreachable!("result_event is constructed as AgentEvent::ToolResult")
        };

        if let Some(status) = stop.status() {
            return self
                .finish(run_id, status, stop)
                .map(ToolCallProgress::Finished);
        }
        if let ToolOutput::Failure { error, .. } = &result.output {
            self.emit(&AgentEvent::Error {
                run_id: run_id.clone(),
                turn_id: Some(turn_id.clone()),
                error: AgentError {
                    code: error.code.clone(),
                    message: error.message.clone(),
                    recoverable: true,
                },
                extensions: BTreeMap::new(),
            })?;
        }

        Ok(ToolCallProgress::Message(ModelMessage::Tool {
            content: tool_result_content(result.output),
            name: model_tool_call.name,
            tool_call_id: model_tool_call.id,
        }))
    }

    fn emit(&mut self, event: &AgentEvent) -> Result<(), AgentRuntimeError<S::Error>> {
        self.event_sink
            .append(event)
            .map_err(AgentRuntimeError::EventSink)
    }

    fn emit_durable(&mut self, event: &AgentEvent) -> Result<(), AgentRuntimeError<S::Error>> {
        self.event_sink
            .append_durable(event)
            .map_err(AgentRuntimeError::EventSink)
    }

    fn emit_tool_result(&mut self, event: &AgentEvent) -> Result<(), AgentRuntimeError<S::Error>> {
        self.event_sink.append_durable(event).map_err(|source| {
            AgentRuntimeError::ToolResultPersistenceIndeterminate {
                event: Box::new(event.clone()),
                source,
            }
        })
    }

    fn finish_model_error(
        &mut self,
        run_id: &RunId,
        turn_id: Option<TurnId>,
        error: ModelError,
        stop: &RunStopToken,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        self.finish_agent_error(
            run_id,
            turn_id,
            AgentError {
                code: error.code,
                message: error.message,
                recoverable: false,
            },
            stop,
        )
    }

    fn finish_agent_error(
        &mut self,
        run_id: &RunId,
        turn_id: Option<TurnId>,
        error: AgentError,
        stop: &RunStopToken,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        self.emit(&AgentEvent::Error {
            run_id: run_id.clone(),
            turn_id,
            error: error.clone(),
            extensions: BTreeMap::new(),
        })?;
        self.finish(run_id, TerminalRunStatus::Failed { error }, stop)
    }

    fn finish(
        &mut self,
        run_id: &RunId,
        proposed_status: TerminalRunStatus,
        stop: &RunStopToken,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        let status = stop.resolve_terminal(proposed_status);
        self.emit(&AgentEvent::RunFinished {
            run_id: run_id.clone(),
            status: status.clone(),
            extensions: BTreeMap::new(),
        })?;
        Ok(RunOutcome { status })
    }
}

fn stopped_status(control: RunControlFlow, stop: &RunStopToken) -> Option<TerminalRunStatus> {
    match control {
        RunControlFlow::Continue => {}
        RunControlFlow::Interrupt { reason } => stop.interrupt(reason),
        RunControlFlow::Cancel { reason } => stop.cancel(reason),
    }
    stop.status()
}

fn tool_result_content(output: ToolOutput) -> Vec<ModelMessageContent> {
    match output {
        ToolOutput::Success { content, .. } => content
            .into_iter()
            .map(|content| match content {
                ToolContent::Text { text } => ModelMessageContent::text(text),
                ToolContent::Json { value } => ModelMessageContent::json(value),
            })
            .collect(),
        failure @ ToolOutput::Failure { .. } => vec![ModelMessageContent::json(
            serde_json::to_value(failure).expect("ToolOutput is serializable"),
        )],
    }
}

#[derive(Debug)]
pub enum AgentRuntimeError<E> {
    EventSink(E),
    /// The tool returned, but its result could not be recorded. The caller
    /// must reconcile or retry persistence of `event`; it must not execute the
    /// tool call again because its external side effects are indeterminate.
    ToolResultPersistenceIndeterminate {
        event: Box<AgentEvent>,
        source: E,
    },
}

impl<E: fmt::Display> fmt::Display for AgentRuntimeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventSink(source) => write!(formatter, "failed to append Agent Event: {source}"),
            Self::ToolResultPersistenceIndeterminate { event, source } => write!(
                formatter,
                "tool execution completed but persistence of its result Agent Event is indeterminate; inspect and reconcile the Event Log without re-executing the tool: {event:?}: {source}"
            ),
        }
    }
}

impl<E: Error + 'static> Error for AgentRuntimeError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EventSink(source) | Self::ToolResultPersistenceIndeterminate { source, .. } => {
                Some(source)
            }
        }
    }
}
