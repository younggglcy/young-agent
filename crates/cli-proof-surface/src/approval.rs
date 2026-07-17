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
    use std::io::{self, BufRead, Cursor, Read, Write};
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};

    use serde_json::json;
    use young_agent_runtime::{ApprovalDecision, ApprovalRequest, RunControl};
    use young_tool_runtime::{ToolCall, ToolCallId};

    use crate::terminal::TerminalOutput;

    use super::{
        decode_approval_line, read_approval_input, ApprovalInput, InteractiveApprovalControl,
        MAX_APPROVAL_INPUT_BYTES,
    };

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            id: "approval-001".to_string(),
            call: ToolCall {
                id: ToolCallId::new("tool-001"),
                tool_name: "run_command".to_string(),
                arguments: json!({ "command": "touch approved.txt" }),
            },
            reason: "command mutates the workspace".to_string(),
        }
    }

    fn decide(inputs: Vec<ApprovalInput>, cancelled: bool) -> ApprovalDecision {
        let (sender, receiver) = mpsc::channel();
        for input in inputs {
            sender.send(input).expect("approval input should queue");
        }
        drop(sender);
        let mut control = InteractiveApprovalControl {
            input: receiver,
            output: TerminalOutput::new(Vec::new(), false),
        };
        control.decide_approval(&approval_request(), Arc::new(AtomicBool::new(cancelled)))
    }

    fn assert_denied_contains(decision: ApprovalDecision, expected: &str) {
        match decision {
            ApprovalDecision::Deny { reason } => assert!(
                reason.contains(expected),
                "denial reason '{reason}' did not contain '{expected}'"
            ),
            ApprovalDecision::Approve => panic!("approval should have been denied"),
        }
    }

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

    #[test]
    fn approval_input_distinguishes_lines_eof_and_invalid_utf8() {
        let mut line = Cursor::new(b"yes\nremaining".to_vec());
        assert!(matches!(
            read_approval_input(&mut line),
            ApprovalInput::Line(value) if value == "yes\n"
        ));
        assert_eq!(line.position(), 4);

        let mut final_line = Cursor::new(b"no".to_vec());
        assert!(matches!(
            read_approval_input(&mut final_line),
            ApprovalInput::Line(value) if value == "no"
        ));

        let mut empty = Cursor::new(Vec::<u8>::new());
        assert!(matches!(
            read_approval_input(&mut empty),
            ApprovalInput::Eof
        ));

        assert!(matches!(
            decode_approval_line(vec![0xff]),
            ApprovalInput::Error(error) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    struct BrokenReader;

    impl Read for BrokenReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("input failed"))
        }
    }

    impl BufRead for BrokenReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("input failed"))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    #[test]
    fn approval_input_preserves_reader_errors() {
        assert!(matches!(
            read_approval_input(&mut BrokenReader),
            ApprovalInput::Error(error) if error.to_string() == "input failed"
        ));
    }

    #[test]
    fn approval_control_handles_every_input_terminal_state() {
        assert_eq!(
            decide(vec![ApprovalInput::Line("yes\n".to_string())], false),
            ApprovalDecision::Approve
        );
        assert_denied_contains(
            decide(vec![ApprovalInput::Line("n\n".to_string())], false),
            "user denied",
        );
        assert_denied_contains(
            decide(vec![ApprovalInput::Line("\n".to_string())], false),
            "user denied",
        );
        assert_denied_contains(decide(vec![ApprovalInput::Eof], false), "input closed");
        assert_denied_contains(
            decide(vec![ApprovalInput::TooLong], false),
            "input exceeded",
        );
        assert_denied_contains(
            decide(
                vec![ApprovalInput::Error(io::Error::other("reader failed"))],
                false,
            ),
            "reader failed",
        );
        assert_denied_contains(decide(Vec::new(), false), "input disconnected");
        assert_denied_contains(
            decide(vec![ApprovalInput::Line("yes\n".to_string())], true),
            "wait stopped",
        );

        assert_eq!(
            decide(
                vec![
                    ApprovalInput::Line("maybe\n".to_string()),
                    ApprovalInput::Line("y\n".to_string()),
                ],
                false,
            ),
            ApprovalDecision::Approve
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("terminal unavailable"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("terminal unavailable"))
        }
    }

    #[test]
    fn approval_control_denies_when_the_prompt_cannot_be_written() {
        let (_sender, receiver) = mpsc::channel();
        let mut control = InteractiveApprovalControl {
            input: receiver,
            output: TerminalOutput::new(FailingWriter, true),
        };

        assert_denied_contains(
            control.decide_approval(&approval_request(), Arc::new(AtomicBool::new(false))),
            "terminal output failed",
        );
    }

    struct CorrectionFailingWriter;

    impl Write for CorrectionFailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if buffer
                .windows(b"enter 'y'".len())
                .any(|window| window == b"enter 'y'")
            {
                Err(io::Error::other("correction unavailable"))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn approval_control_denies_when_invalid_input_cannot_be_corrected() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ApprovalInput::Line("maybe\n".to_string()))
            .expect("invalid approval input should queue");
        let mut control = InteractiveApprovalControl {
            input: receiver,
            output: TerminalOutput::new(CorrectionFailingWriter, false),
        };

        assert_denied_contains(
            control.decide_approval(&approval_request(), Arc::new(AtomicBool::new(false))),
            "terminal output failed during approval decision",
        );
    }
}
