use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use young_agent_runtime::{
    AgentEvent, AgentEventSink, ApprovalDecision, EventSequence, RunStopToken, TerminalRunStatus,
};
use young_event_store::{EventStoreError, JsonlEventStore};
use young_model_runtime::ModelStreamEvent;
use young_tool_runtime::{ToolContent, ToolOutput};

pub(crate) struct TerminalOutput<W> {
    state: Arc<Mutex<TerminalOutputState<W>>>,
}

struct TerminalOutputState<W> {
    writer: W,
    error: Option<io::Error>,
    reset_before_approval: bool,
}

struct TerminalSanitizer<'a, W> {
    writer: &'a mut W,
    error: Option<io::Error>,
}

impl<W> fmt::Write for TerminalSanitizer<'_, W>
where
    W: Write,
{
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.error.is_some() {
            return Err(fmt::Error);
        }
        if let Err(error) = write_terminal_safe(self.writer, text) {
            self.error = Some(error);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl<W> Clone for TerminalOutput<W> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<W> TerminalOutput<W>
where
    W: Write,
{
    pub(crate) fn new(writer: W, reset_before_approval: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(TerminalOutputState {
                writer,
                error: None,
                reset_before_approval,
            })),
        }
    }

    pub(crate) fn line(&self, arguments: fmt::Arguments<'_>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.error.is_some() {
            return false;
        }
        let (format_result, format_error) = {
            let mut sanitizer = TerminalSanitizer {
                writer: &mut state.writer,
                error: None,
            };
            let result = fmt::write(&mut sanitizer, arguments);
            (result, sanitizer.error)
        };
        let result = match format_error {
            Some(error) => Err(error),
            None if format_result.is_err() => {
                Err(io::Error::other("terminal value could not be formatted"))
            }
            None => state
                .writer
                .write_all(b"\n")
                .and_then(|()| state.writer.flush()),
        };
        if let Err(error) = result {
            state.error = Some(error);
            return false;
        }
        true
    }

    pub(crate) fn prepare_approval_prompt(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.error.is_some() {
            return false;
        }
        if !state.reset_before_approval {
            return true;
        }
        if let Err(error) = state
            .writer
            .write_all(b"\x1b[0m")
            .and_then(|()| state.writer.flush())
        {
            state.error = Some(error);
            return false;
        }
        true
    }

    pub(crate) fn take_error(&self) -> Option<io::Error> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .error
            .take()
    }
}

fn write_terminal_safe(writer: &mut impl Write, text: &str) -> io::Result<()> {
    let mut safe_start = 0;
    for (index, character) in text.char_indices() {
        if is_terminal_control(character) {
            writer.write_all(&text.as_bytes()[safe_start..index])?;
            write!(writer, "\\u{{{:04x}}}", character as u32)?;
            safe_start = index + character.len_utf8();
        }
    }
    writer.write_all(&text.as_bytes()[safe_start..])
}

fn is_terminal_control(character: char) -> bool {
    matches!(
        character,
        '\u{0000}'..='\u{001f}'
            | '\u{007f}'..='\u{009f}'
            | '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

pub(crate) struct StreamingEventStore<W> {
    store: JsonlEventStore,
    output: TerminalOutput<W>,
    stop: RunStopToken,
}

impl<W> StreamingEventStore<W>
where
    W: Write,
{
    pub(crate) fn new(
        store: JsonlEventStore,
        output: TerminalOutput<W>,
        stop: RunStopToken,
    ) -> Self {
        Self {
            store,
            output,
            stop,
        }
    }

    fn render(&self, event: &AgentEvent) {
        if !render_event(&self.output, event) {
            self.stop
                .cancel("terminal output failed while streaming Agent Events");
        }
    }
}

impl<W> AgentEventSink for StreamingEventStore<W>
where
    W: Write,
{
    type Error = EventStoreError;

    fn append(&mut self, sequence: EventSequence, event: &AgentEvent) -> Result<(), Self::Error> {
        <JsonlEventStore as AgentEventSink>::append(&mut self.store, sequence, event)?;
        self.render(event);
        Ok(())
    }

    fn append_durable(
        &mut self,
        sequence: EventSequence,
        event: &AgentEvent,
    ) -> Result<(), Self::Error> {
        <JsonlEventStore as AgentEventSink>::append_durable(&mut self.store, sequence, event)?;
        self.render(event);
        Ok(())
    }
}

fn render_event<W>(output: &TerminalOutput<W>, event: &AgentEvent) -> bool
where
    W: Write,
{
    match event {
        AgentEvent::RunStarted { run_id, .. } => {
            output.line(format_args!("[run] started {}", run_id.as_str()))
        }
        AgentEvent::TurnStarted { turn_id, .. } => {
            output.line(format_args!("[turn] started {}", turn_id.as_str()))
        }
        AgentEvent::ModelOutput { event, .. } => render_model_event(output, event),
        AgentEvent::ToolCallRequested { call, .. } => output.line(format_args!(
            "[tool-call] {} {} {}",
            call.id.as_str(),
            call.tool_name,
            call.arguments
        )),
        AgentEvent::ApprovalRequested { request, .. } => output.line(format_args!(
            "[approval] requested {} for {}: {}",
            request.id, request.call.tool_name, request.reason
        )),
        AgentEvent::ApprovalResolved {
            approval_id,
            decision,
            ..
        } => match decision {
            ApprovalDecision::Approve => {
                output.line(format_args!("[approval] {approval_id} approved"))
            }
            ApprovalDecision::Deny { reason } => {
                output.line(format_args!("[approval] {approval_id} denied: {reason}"))
            }
        },
        AgentEvent::ToolResult { result, .. } => match &result.output {
            ToolOutput::Success { content, .. } => {
                let mut rendered = output.line(format_args!(
                    "[tool-result] {} success",
                    result.call_id.as_str()
                ));
                for item in content {
                    rendered &= match item {
                        ToolContent::Text { text } => {
                            output.line(format_args!("[tool-output] {text}"))
                        }
                        ToolContent::Json { value } => {
                            output.line(format_args!("[tool-output] {value}"))
                        }
                    };
                }
                rendered
            }
            ToolOutput::Failure { error, .. } => output.line(format_args!(
                "[tool-result] {} failed {}: {}",
                result.call_id.as_str(),
                error.code,
                error.message
            )),
        },
        AgentEvent::Error { error, .. } => {
            output.line(format_args!("[error] {}: {}", error.code, error.message))
        }
        AgentEvent::RunFinished { status, .. } => render_terminal_status(output, status),
    }
}

fn render_model_event<W>(output: &TerminalOutput<W>, event: &ModelStreamEvent) -> bool
where
    W: Write,
{
    match event {
        ModelStreamEvent::Started { request_id, .. } => output.line(format_args!(
            "[model] started{}",
            request_id
                .as_ref()
                .map(|id| format!(" {}", id.as_str()))
                .unwrap_or_default()
        )),
        ModelStreamEvent::TextDelta { delta, .. } => output.line(format_args!("[model] {delta}")),
        ModelStreamEvent::ToolCallDelta {
            id,
            name,
            arguments_delta,
            ..
        } => output.line(format_args!(
            "[model-tool-delta] {} {} {}",
            id.as_str(),
            name.as_deref().unwrap_or("<pending>"),
            arguments_delta
        )),
        ModelStreamEvent::ToolCall {
            id,
            name,
            arguments,
            ..
        } => output.line(format_args!(
            "[model-tool-call] {} {name} {arguments}",
            id.as_str()
        )),
        ModelStreamEvent::Usage { usage, .. } => output.line(format_args!(
            "[usage] input={} output={}",
            usage.input_tokens, usage.output_tokens
        )),
        ModelStreamEvent::Completed { finish_reason, .. } => output.line(format_args!(
            "[model] completed{}",
            finish_reason
                .as_ref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        )),
        ModelStreamEvent::Failed { error, .. } => output.line(format_args!(
            "[model-error] {}: {}",
            error.code, error.message
        )),
    }
}

fn render_terminal_status<W>(output: &TerminalOutput<W>, status: &TerminalRunStatus) -> bool
where
    W: Write,
{
    match status {
        TerminalRunStatus::Completed { final_message } => {
            output.line(format_args!("[status] completed: {final_message}"))
        }
        TerminalRunStatus::Failed { error } => output.line(format_args!(
            "[status] failed {}: {}",
            error.code, error.message
        )),
        TerminalRunStatus::Interrupted { reason } => {
            output.line(format_args!("[status] interrupted: {reason}"))
        }
        TerminalRunStatus::Cancelled { reason } => {
            output.line(format_args!("[status] cancelled: {reason}"))
        }
    }
}
