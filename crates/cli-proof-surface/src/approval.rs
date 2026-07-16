use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use young_agent_runtime::{ApprovalDecision, ApprovalRequest, RunControl, RunControlFlow};

use crate::terminal::TerminalOutput;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_APPROVAL_INPUT_BYTES: usize = 4 * 1024;

enum ApprovalInput {
    Line(String),
    TooLong,
    Eof,
    Error(io::Error),
}

pub(crate) struct InteractiveApprovalControl<W> {
    input: Receiver<ApprovalInput>,
    output: TerminalOutput<W>,
}

impl<W> InteractiveApprovalControl<W>
where
    W: Write,
{
    pub(crate) fn from_stdin(output: TerminalOutput<W>) -> Self {
        let (sender, input) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let stdin = io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let input = read_approval_input(&mut stdin);
                let finished = matches!(
                    input,
                    ApprovalInput::TooLong | ApprovalInput::Eof | ApprovalInput::Error(_)
                );
                if sender.send(input).is_err() || finished {
                    break;
                }
            }
        });
        Self { input, output }
    }

    fn deny(reason: impl Into<String>) -> ApprovalDecision {
        ApprovalDecision::Deny {
            reason: reason.into(),
        }
    }
}

impl<W> RunControl for InteractiveApprovalControl<W>
where
    W: Write,
{
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(
        &mut self,
        request: &ApprovalRequest,
        cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        if !self.output.prepare_approval_prompt()
            || !self.output.line(format_args!(
                "[approval-prompt] {} {}",
                request.id, request.call.tool_name
            ))
            || !self
                .output
                .line(format_args!("  arguments: {}", request.call.arguments))
            || !self
                .output
                .line(format_args!("  reason: {}", request.reason))
            || !self.output.line(format_args!("Approve? [y/N]"))
        {
            return Self::deny("terminal output failed before approval decision");
        }

        loop {
            if cancellation.load(Ordering::Acquire) {
                return Self::deny("approval wait stopped before a decision");
            }
            match self.input.recv_timeout(INPUT_POLL_INTERVAL) {
                Ok(ApprovalInput::Line(line)) => match line.trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => return ApprovalDecision::Approve,
                    "" | "n" | "no" => return Self::deny("user denied the tool call"),
                    _ => {
                        if !self.output.line(format_args!(
                            "[approval-prompt] enter 'y' to approve or 'n' to deny"
                        )) {
                            return Self::deny("terminal output failed during approval decision");
                        }
                    }
                },
                Ok(ApprovalInput::Eof) => {
                    return Self::deny("approval input closed without a decision");
                }
                Ok(ApprovalInput::TooLong) => {
                    return Self::deny(format!(
                        "approval input exceeded {MAX_APPROVAL_INPUT_BYTES} bytes"
                    ));
                }
                Ok(ApprovalInput::Error(error)) => {
                    return Self::deny(format!("approval input failed: {error}"));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Self::deny("approval input disconnected without a decision");
                }
            }
        }
    }
}

fn read_approval_input(reader: &mut impl BufRead) -> ApprovalInput {
    let mut line = Vec::with_capacity(16);
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) => return ApprovalInput::Error(error),
        };
        if buffer.is_empty() {
            return if line.is_empty() {
                ApprovalInput::Eof
            } else {
                decode_approval_line(line)
            };
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let chunk_length = newline.map_or(buffer.len(), |index| index + 1);
        let remaining = MAX_APPROVAL_INPUT_BYTES.saturating_sub(line.len());
        let copied = chunk_length.min(remaining);
        line.extend_from_slice(&buffer[..copied]);
        if copied < chunk_length {
            reader.consume(copied);
            return ApprovalInput::TooLong;
        }
        reader.consume(chunk_length);

        if newline.is_some() {
            return decode_approval_line(line);
        }
    }
}

fn decode_approval_line(line: Vec<u8>) -> ApprovalInput {
    match String::from_utf8(line) {
        Ok(line) => ApprovalInput::Line(line),
        Err(error) => ApprovalInput::Error(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("approval input is not UTF-8: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{read_approval_input, ApprovalInput, MAX_APPROVAL_INPUT_BYTES};

    #[test]
    fn oversized_approval_line_returns_immediately_without_draining_the_source() {
        let mut source = vec![b'x'; MAX_APPROVAL_INPUT_BYTES + 1];
        source.resize(MAX_APPROVAL_INPUT_BYTES * 5, b'x');
        let mut reader = Cursor::new(source);

        assert!(matches!(
            read_approval_input(&mut reader),
            ApprovalInput::TooLong
        ));
        assert_eq!(reader.position(), MAX_APPROVAL_INPUT_BYTES as u64);
    }
}
