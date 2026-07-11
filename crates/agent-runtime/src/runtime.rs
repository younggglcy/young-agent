use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde_json::Value;
use young_model_runtime::{
    ModelClient, ModelError, ModelMessage, ModelMessageContent, ModelRequest, ModelStreamEvent,
    ModelToolCall, ModelToolSpec,
};
use young_tool_runtime::{ToolCall, ToolCallId, ToolContent, ToolExecutor, ToolOutput, ToolResult};

use crate::{AgentError, AgentEvent, ApprovalRequest, RunId, TerminalRunStatus, TurnId};

const MAX_TURNS: usize = 128;

pub trait AgentEventSink {
    type Error;

    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunControlFlow {
    Continue,
    Interrupt { reason: String },
    Cancel { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny { reason: String },
}

pub trait RunControl {
    fn checkpoint(&mut self) -> RunControlFlow;

    fn decide_approval(&mut self, _request: &ApprovalRequest) -> ApprovalDecision {
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
        self.run_with_control(request, &mut control)
    }

    pub fn run_with_control<C>(
        &mut self,
        request: RunRequest,
        control: &mut C,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>>
    where
        C: RunControl,
    {
        let RunRequest {
            run_id,
            model,
            mut messages,
            tools,
            metadata,
        } = request;

        self.emit(AgentEvent::RunStarted {
            run_id: run_id.clone(),
            extensions: BTreeMap::new(),
        })?;

        let mut tool_call_sequence = 0_usize;
        let mut approval_sequence = 0_usize;

        for turn_number in 1..=MAX_TURNS {
            if let Some(status) = stopped_status(control.checkpoint()) {
                return self.finish(&run_id, status);
            }

            let turn_id = TurnId::new(format!("turn-{turn_number:03}"));
            self.emit(AgentEvent::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                extensions: BTreeMap::new(),
            })?;

            let model_request = ModelRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                metadata: metadata.clone(),
            };
            let stream = match self.model_client.stream(model_request) {
                Ok(stream) => stream,
                Err(error) => {
                    return self.finish_model_error(&run_id, Some(turn_id), error);
                }
            };

            let mut text = String::new();
            let mut model_tool_calls = Vec::new();
            let mut completed = false;

            for model_event in stream {
                if let Some(status) = stopped_status(control.checkpoint()) {
                    return self.finish(&run_id, status);
                }

                self.emit(AgentEvent::ModelOutput {
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    event: model_event.clone(),
                    extensions: BTreeMap::new(),
                })?;

                match model_event {
                    ModelStreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
                    ModelStreamEvent::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => model_tool_calls.push(ModelToolCall {
                        id,
                        name,
                        arguments,
                    }),
                    ModelStreamEvent::Completed { .. } => completed = true,
                    ModelStreamEvent::Failed { error, .. } => {
                        return self.finish_model_error(&run_id, Some(turn_id), error);
                    }
                    ModelStreamEvent::Started { .. }
                    | ModelStreamEvent::ToolCallDelta { .. }
                    | ModelStreamEvent::Usage { .. } => {}
                }
            }

            if !completed {
                return self.finish_agent_error(
                    &run_id,
                    Some(turn_id),
                    AgentError {
                        code: "model_stream_incomplete".to_string(),
                        message: "model stream ended without a completed event".to_string(),
                        recoverable: false,
                    },
                );
            }

            if model_tool_calls.is_empty() {
                let status = TerminalRunStatus::Completed {
                    final_message: text,
                };
                return self.finish(&run_id, status);
            }

            messages.push(if text.is_empty() {
                ModelMessage::assistant_tool_calls(model_tool_calls.clone())
            } else {
                ModelMessage::assistant_with_tool_calls(text, model_tool_calls.clone())
            });

            for model_tool_call in model_tool_calls {
                tool_call_sequence += 1;
                let call = ToolCall {
                    id: ToolCallId::new(format!(
                        "{}-tool-{tool_call_sequence:03}",
                        run_id.as_str()
                    )),
                    tool_name: model_tool_call.name.clone(),
                    arguments: model_tool_call.arguments,
                };
                self.emit(AgentEvent::ToolCallRequested {
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    model_tool_call_id: model_tool_call.id.clone(),
                    call: call.clone(),
                    extensions: BTreeMap::new(),
                })?;

                let result = if let Some(reason) = self.tool_executor.approval_reason(&call) {
                    approval_sequence += 1;
                    let approval = ApprovalRequest {
                        id: format!("{}-approval-{approval_sequence:03}", run_id.as_str()),
                        call: call.clone(),
                        reason,
                    };
                    self.emit(AgentEvent::ApprovalRequested {
                        run_id: run_id.clone(),
                        turn_id: turn_id.clone(),
                        request: approval.clone(),
                        extensions: BTreeMap::new(),
                    })?;

                    match control.decide_approval(&approval) {
                        ApprovalDecision::Approve => {
                            if let Some(status) = stopped_status(control.checkpoint()) {
                                return self.finish(&run_id, status);
                            }
                            self.tool_executor.execute(&call)
                        }
                        ApprovalDecision::Deny { reason } => ToolResult {
                            call_id: call.id.clone(),
                            output: ToolOutput::Failure {
                                error: young_tool_runtime::ToolError {
                                    code: "approval_denied".to_string(),
                                    message: reason,
                                    retryable: false,
                                },
                                extensions: BTreeMap::new(),
                            },
                        },
                    }
                } else {
                    if let Some(status) = stopped_status(control.checkpoint()) {
                        return self.finish(&run_id, status);
                    }
                    self.tool_executor.execute(&call)
                };
                self.emit(AgentEvent::ToolResult {
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    result: result.clone(),
                    extensions: BTreeMap::new(),
                })?;

                if let ToolOutput::Failure { error, .. } = &result.output {
                    self.emit(AgentEvent::Error {
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

                messages.push(ModelMessage::Tool {
                    content: tool_result_content(&result),
                    name: model_tool_call.name,
                    tool_call_id: model_tool_call.id,
                });
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
        )
    }

    fn emit(&mut self, event: AgentEvent) -> Result<(), AgentRuntimeError<S::Error>> {
        self.event_sink
            .append(&event)
            .map_err(AgentRuntimeError::EventSink)
    }

    fn finish_model_error(
        &mut self,
        run_id: &RunId,
        turn_id: Option<TurnId>,
        error: ModelError,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        self.finish_agent_error(
            run_id,
            turn_id,
            AgentError {
                code: error.code,
                message: error.message,
                recoverable: false,
            },
        )
    }

    fn finish_agent_error(
        &mut self,
        run_id: &RunId,
        turn_id: Option<TurnId>,
        error: AgentError,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        self.emit(AgentEvent::Error {
            run_id: run_id.clone(),
            turn_id,
            error: error.clone(),
            extensions: BTreeMap::new(),
        })?;
        self.finish(run_id, TerminalRunStatus::Failed { error })
    }

    fn finish(
        &mut self,
        run_id: &RunId,
        status: TerminalRunStatus,
    ) -> Result<RunOutcome, AgentRuntimeError<S::Error>> {
        self.emit(AgentEvent::RunFinished {
            run_id: run_id.clone(),
            status: status.clone(),
            extensions: BTreeMap::new(),
        })?;
        Ok(RunOutcome { status })
    }
}

fn stopped_status(control: RunControlFlow) -> Option<TerminalRunStatus> {
    match control {
        RunControlFlow::Continue => None,
        RunControlFlow::Interrupt { reason } => Some(TerminalRunStatus::Interrupted { reason }),
        RunControlFlow::Cancel { reason } => Some(TerminalRunStatus::Cancelled { reason }),
    }
}

fn tool_result_content(result: &ToolResult) -> Vec<ModelMessageContent> {
    match &result.output {
        ToolOutput::Success { content, .. } => content
            .iter()
            .map(|content| match content {
                ToolContent::Text { text } => ModelMessageContent::text(text.clone()),
                ToolContent::Json { value } => ModelMessageContent::json(value.clone()),
            })
            .collect(),
        ToolOutput::Failure { .. } => vec![ModelMessageContent::json(
            serde_json::to_value(&result.output).expect("ToolOutput is serializable"),
        )],
    }
}

#[derive(Debug)]
pub enum AgentRuntimeError<E> {
    EventSink(E),
}

impl<E: fmt::Display> fmt::Display for AgentRuntimeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventSink(source) => write!(formatter, "failed to append Agent Event: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for AgentRuntimeError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EventSink(source) => Some(source),
        }
    }
}
