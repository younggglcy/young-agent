use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use command_group::CommandGroup;
use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    failure, truncate_json_string, ToolArguments, MAX_OUTPUT_BYTES,
    MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::CodingWorkspace;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);

pub(crate) const APPROVAL_REASON: &str =
    "command execution requires approval until a command safety policy is configured";

pub(crate) fn execute(
    workspace: &CodingWorkspace,
    call: &ToolCall,
    cancellation: &AtomicBool,
) -> ToolOutput {
    let arguments = match ToolArguments::parse(&call.arguments, &["command"]) {
        Ok(arguments) => arguments,
        Err(output) => return output,
    };
    let command = match arguments.required_string("command") {
        Ok(command) => command,
        Err(output) => return output,
    };
    let outcome = match run_shell_command(
        workspace.context().root(),
        command,
        cancellation,
        MAX_OUTPUT_BYTES,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return failure(error.code(), error.to_string(), error.retryable()),
    };
    let cwd = workspace.context().root().display().to_string();
    let stream_budget = MAX_TOOL_CONTENT_SERIALIZED_BYTES / 2;
    let (stdout, stdout_serialization_truncated) =
        truncate_json_string(&outcome.stdout, stream_budget);
    let (stderr, stderr_serialization_truncated) =
        truncate_json_string(&outcome.stderr, stream_budget);
    ToolOutput::Success {
        content: vec![ToolContent::Json {
            value: json!({
                "success": outcome.status.success(),
                "exit_code": outcome.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            }),
        }],
        metadata: BTreeMap::from([
            ("cwd".to_string(), json!(cwd)),
            ("stdout_bytes".to_string(), json!(outcome.stdout_bytes)),
            ("stderr_bytes".to_string(), json!(outcome.stderr_bytes)),
            (
                "stdout_truncated".to_string(),
                json!(outcome.stdout_truncated || stdout_serialization_truncated),
            ),
            (
                "stderr_truncated".to_string(),
                json!(outcome.stderr_truncated || stderr_serialization_truncated),
            ),
            (
                "output_incomplete".to_string(),
                json!(outcome.output_incomplete),
            ),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    }
}

pub(crate) struct CommandOutcome {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    pub(crate) output_incomplete: bool,
}

fn run_shell_command(
    workspace_root: &Path,
    command: &str,
    cancellation: &AtomicBool,
    max_output_bytes: usize,
) -> Result<CommandOutcome, CommandError> {
    if cancellation.load(Ordering::Relaxed) {
        return Err(CommandError::Cancelled);
    }

    let mut process = shell_command(command);
    let mut child = process
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
        .map_err(CommandError::Spawn)?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .expect("stdout was configured as piped");
    let stderr = child
        .inner()
        .stderr
        .take()
        .expect("stderr was configured as piped");
    let (sender, receiver) = mpsc::sync_channel(16);
    let stdout_reader = spawn_reader(Stream::Stdout, stdout, sender.clone());
    let stderr_reader = spawn_reader(Stream::Stderr, stderr, sender);

    let mut stdout_capture = CapturedStream::new(max_output_bytes);
    let mut stderr_capture = CapturedStream::new(max_output_bytes);
    let mut streams_done = 0usize;
    let mut stream_error = None;
    let mut status = None;
    let mut exited_at = None;
    let mut output_incomplete = false;
    let mut cancelled = false;

    loop {
        if cancellation.load(Ordering::Relaxed) && status.is_none() {
            cancelled = true;
            child.kill().map_err(CommandError::Kill)?;
            status = Some(child.wait().map_err(CommandError::Wait)?);
            exited_at = Some(Instant::now());
        }

        receive_stream_message(
            &receiver,
            &mut stdout_capture,
            &mut stderr_capture,
            &mut streams_done,
            &mut stream_error,
        );

        if status.is_none() {
            status = child.try_wait().map_err(CommandError::Wait)?;
            if status.is_some() {
                exited_at = Some(Instant::now());
            }
        }

        if status.is_some() && streams_done == 2 {
            break;
        }
        if exited_at.is_some_and(|instant| instant.elapsed() >= OUTPUT_DRAIN_GRACE) {
            output_incomplete = true;
            break;
        }
    }

    stdout_reader
        .join()
        .map_err(|_| CommandError::ReaderPanicked)?;
    stderr_reader
        .join()
        .map_err(|_| CommandError::ReaderPanicked)?;
    if let Some(source) = stream_error {
        return Err(CommandError::ReadOutput(source));
    }
    if cancelled {
        return Err(CommandError::Cancelled);
    }
    let status = status.expect("loop only exits after the child exits");
    Ok(CommandOutcome {
        status,
        stdout: stdout_capture.text(),
        stderr: stderr_capture.text(),
        stdout_bytes: stdout_capture.total_bytes,
        stderr_bytes: stderr_capture.total_bytes,
        stdout_truncated: stdout_capture.truncated || output_incomplete,
        stderr_truncated: stderr_capture.truncated || output_incomplete,
        output_incomplete,
    })
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("/bin/sh");
    process.args(["-c", command]);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd.exe");
    process.args(["/D", "/S", "/C", command]);
    process
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

enum StreamMessage {
    Chunk(Stream, Vec<u8>),
    Done(Stream),
    Failed(Stream, std::io::Error),
}

fn spawn_reader<R>(
    stream: Stream,
    mut reader: R,
    sender: SyncSender<StreamMessage>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = vec![0u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(StreamMessage::Done(stream));
                    return;
                }
                Ok(bytes_read) => {
                    if sender
                        .send(StreamMessage::Chunk(stream, buffer[..bytes_read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => {}
                Err(source) => {
                    let _ = sender.send(StreamMessage::Failed(stream, source));
                    return;
                }
            }
        }
    })
}

fn receive_stream_message(
    receiver: &Receiver<StreamMessage>,
    stdout: &mut CapturedStream,
    stderr: &mut CapturedStream,
    streams_done: &mut usize,
    stream_error: &mut Option<std::io::Error>,
) {
    match receiver.recv_timeout(POLL_INTERVAL) {
        Ok(StreamMessage::Chunk(Stream::Stdout, bytes)) => stdout.push(&bytes),
        Ok(StreamMessage::Chunk(Stream::Stderr, bytes)) => stderr.push(&bytes),
        Ok(StreamMessage::Done(stream)) => {
            let _ = stream;
            *streams_done += 1;
        }
        Ok(StreamMessage::Failed(stream, source)) => {
            let _ = stream;
            *streams_done += 1;
            if stream_error.is_none() {
                *stream_error = Some(source);
            }
        }
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
    }
}

struct CapturedStream {
    retained: Vec<u8>,
    total_bytes: u64,
    max_bytes: usize,
    truncated: bool,
}

impl CapturedStream {
    fn new(max_bytes: usize) -> Self {
        Self {
            retained: Vec::with_capacity(max_bytes),
            total_bytes: 0,
            max_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
        let remaining = self.max_bytes.saturating_sub(self.retained.len());
        let retained = remaining.min(bytes.len());
        self.retained.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained < bytes.len();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.retained).into_owned()
    }
}

#[derive(Debug)]
pub(crate) enum CommandError {
    Spawn(std::io::Error),
    Kill(std::io::Error),
    Wait(std::io::Error),
    ReadOutput(std::io::Error),
    ReaderPanicked,
    Cancelled,
}

impl CommandError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "command_spawn_failed",
            Self::Kill(_) | Self::Wait(_) | Self::ReadOutput(_) | Self::ReaderPanicked => {
                "command_io_error"
            }
            Self::Cancelled => "tool_cancelled",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Kill(source) | Self::Wait(source) | Self::ReadOutput(source)
                if source.kind() == std::io::ErrorKind::Interrupted
        )
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(source) => write!(formatter, "failed to start command: {source}"),
            Self::Kill(source) => write!(formatter, "failed to terminate command group: {source}"),
            Self::Wait(source) => write!(formatter, "failed to wait for command: {source}"),
            Self::ReadOutput(source) => {
                write!(formatter, "failed to capture command output: {source}")
            }
            Self::ReaderPanicked => formatter.write_str("command output reader panicked"),
            Self::Cancelled => formatter.write_str("run_command was cancelled"),
        }
    }
}
