use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(test, unix))]
use std::cell::Cell;

use command_group::{CommandGroup, GroupChild};
use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    failure, finalize_output, truncate_json_string, ToolArguments, MAX_OUTPUT_BYTES,
    MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::CodingWorkspace;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
const MAX_PROCESS_GROUP_MEMBERS: usize = 65_536;

#[cfg(all(test, unix))]
thread_local! {
    static INJECT_GROUP_KILL_WRAPPER_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE: Cell<bool> = const { Cell::new(false) };
    static COMMAND_GROUP_TERMINATION_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
}

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
    let outcome = match run_shell_command(workspace, command, cancellation, MAX_OUTPUT_BYTES) {
        Ok(outcome) => outcome,
        Err(error) => return failure(error.code(), error.to_string(), error.retryable()),
    };
    let cwd = workspace.context().root().display().to_string();
    let (cwd, cwd_truncated) = truncate_json_string(&cwd, 2 * 1024);
    let stream_budget = MAX_TOOL_CONTENT_SERIALIZED_BYTES / 2;
    let (stdout, stdout_serialization_truncated) =
        truncate_json_string(&outcome.stdout, stream_budget);
    let (stderr, stderr_serialization_truncated) =
        truncate_json_string(&outcome.stderr, stream_budget);
    finalize_output(ToolOutput::Success {
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
            ("cwd_truncated".to_string(), json!(cwd_truncated)),
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
            ("process_scope".to_string(), json!("process_group")),
            ("detached_processes_tracked".to_string(), json!(false)),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    })
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
    workspace: &CodingWorkspace,
    command: &str,
    cancellation: &AtomicBool,
    max_output_bytes: usize,
) -> Result<CommandOutcome, CommandError> {
    if cancellation.load(Ordering::Relaxed) {
        return Err(CommandError::Cancelled);
    }
    let mut process = shell_command(command);
    workspace
        .bind_command_working_directory(&mut process)
        .map_err(CommandError::WorkspaceChanged)?;
    ensure_process_tracking_supported()?;
    let child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn();
    let mut child = child.map_err(CommandError::Spawn)?;
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
    #[cfg(all(test, unix))]
    let configure_result =
        if INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE.with(|failure| failure.replace(false)) {
            Err(std::io::Error::other(
                "injected output configuration failure",
            ))
        } else {
            make_nonblocking(&stdout).and_then(|()| make_nonblocking(&stderr))
        };
    #[cfg(not(all(test, unix)))]
    let configure_result = make_nonblocking(&stdout).and_then(|()| make_nonblocking(&stderr));
    if let Err(source) = configure_result {
        let cleanup = cleanup_process_group(&mut child, false, true);
        return match cleanup {
            Ok(()) => Err(CommandError::ConfigureOutput(source)),
            Err(cleanup) => Err(cleanup),
        };
    }
    let (sender, receiver) = mpsc::sync_channel(16);
    let reader_stop = Arc::new(AtomicBool::new(false));
    let stdout_reader = spawn_reader(Stream::Stdout, stdout, sender.clone(), reader_stop.clone());
    let stderr_reader = spawn_reader(Stream::Stderr, stderr, sender, reader_stop.clone());

    let mut stdout_capture = CapturedStream::new(max_output_bytes);
    let mut stderr_capture = CapturedStream::new(max_output_bytes);
    let mut streams_done = 0usize;
    let mut stream_error = None;
    let mut status = None;
    let mut leader_terminal = false;
    let mut process_group_tracker = ProcessGroupTracker::default();
    let mut drain_started_at = None;
    let mut output_incomplete = false;
    let mut cancelled = false;
    let mut forced_at = None;
    let mut termination_sent = false;

    let control_result = (|| -> Result<(), CommandError> {
        loop {
            if cancellation.load(Ordering::Relaxed) && !cancelled {
                cancelled = true;
                if status.is_none() {
                    terminate_command_group(&mut child)?;
                    termination_sent = true;
                    status = Some(wait_for_command_group(&mut child)?);
                }
                forced_at = Some(Instant::now());
            }

            receive_stream_message(
                &receiver,
                &mut stdout_capture,
                &mut stderr_capture,
                &mut streams_done,
                &mut stream_error,
            );

            if status.is_none() && !leader_terminal {
                leader_terminal = command_leader_terminal(&child)?;
            }
            if status.is_none()
                && leader_terminal
                && !process_group_has_members_other_than_leader(&child, &mut process_group_tracker)?
            {
                status = Some(wait_for_command_group(&mut child)?);
            }

            if status.is_some() && streams_done == 2 {
                return Ok(());
            }
            if forced_at.is_some_and(|instant| instant.elapsed() >= OUTPUT_DRAIN_GRACE) {
                return Ok(());
            }
            if forced_at.is_none() && status.is_some() {
                let started = drain_started_at.get_or_insert_with(Instant::now);
                if started.elapsed() >= OUTPUT_DRAIN_GRACE {
                    output_incomplete = true;
                    forced_at = Some(Instant::now());
                }
            } else {
                drain_started_at = None;
            }
        }
    })();

    let cleanup_result = cleanup_command_resources(
        &mut child,
        status.is_some(),
        control_result.is_err() && status.is_none() && !termination_sent,
        OutputReaderResources {
            stop: reader_stop,
            receiver,
            stdout: stdout_reader,
            stderr: stderr_reader,
        },
    );
    match (control_result, cleanup_result) {
        (Err(_), Err(cleanup)) => return Err(cleanup),
        (Err(source), Ok(())) => return Err(source),
        (Ok(()), Err(cleanup)) => return Err(cleanup),
        (Ok(()), Ok(())) => {}
    }
    if let Some(source) = stream_error {
        return Err(CommandError::ReadOutput(source));
    }
    if cancelled {
        return Err(CommandError::TerminationUnverified);
    }
    if output_incomplete {
        return Err(CommandError::OutputIncomplete);
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ensure_process_tracking_supported() -> Result<(), CommandError> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn ensure_process_tracking_supported() -> Result<(), CommandError> {
    Err(CommandError::InspectProcessGroup(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable command process-group tracking is not supported on this platform",
    )))
}

fn terminate_command_group(child: &mut GroupChild) -> Result<(), CommandError> {
    #[cfg(all(test, unix))]
    COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    #[cfg(all(test, unix))]
    let injected_failure = INJECT_GROUP_KILL_WRAPPER_FAILURE.with(Cell::get);
    #[cfg(not(all(test, unix)))]
    let injected_failure = false;
    let wrapper_result = if injected_failure {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected persistent command-group wrapper failure",
        ))
    } else {
        loop {
            match child.kill() {
                Ok(()) => break Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) if group_already_exited(&source) => break Ok(()),
                Err(source) => break Err(source),
            }
        }
    };
    if wrapper_result.is_ok() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        let fallback = kill_process_group_by_id(child.id());
        #[cfg(all(test, unix))]
        INJECT_GROUP_KILL_WRAPPER_FAILURE.with(|failure| failure.set(false));
        fallback.map_err(|fallback| {
            let wrapper = wrapper_result.expect_err("wrapper failure was checked above");
            CommandError::Kill(std::io::Error::new(
                fallback.kind(),
                format!(
                    "command-group wrapper failed ({wrapper}); direct process-group termination also failed ({fallback})"
                ),
            ))
        })
    }
    #[cfg(not(unix))]
    {
        Err(CommandError::Kill(
            wrapper_result.expect_err("wrapper failure was checked above"),
        ))
    }
}

#[cfg(unix)]
fn process_group_id(child: &GroupChild) -> Result<rustix::process::Pid, CommandError> {
    let raw = i32::try_from(child.id()).map_err(|_| {
        CommandError::Kill(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command process-group id does not fit in i32",
        ))
    })?;
    rustix::process::Pid::from_raw(raw).ok_or_else(|| {
        CommandError::Kill(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command process-group id was zero",
        ))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command_leader_terminal(child: &GroupChild) -> Result<bool, CommandError> {
    let pid = process_group_id(child)?;
    loop {
        let options = rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOWAIT;
        match rustix::process::waitid(rustix::process::WaitId::Pid(pid), options) {
            Ok(status) => return Ok(status.is_some()),
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) => return Err(CommandError::Wait(std::io::Error::from(source))),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn command_leader_terminal(_child: &GroupChild) -> Result<bool, CommandError> {
    Err(CommandError::Wait(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "non-reaping command status observation is not supported on this platform",
    )))
}

#[derive(Default)]
struct ProcessGroupTracker {
    #[cfg(target_os = "linux")]
    known_member: Option<u32>,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_group_has_members_other_than_leader(
    child: &GroupChild,
    _tracker: &mut ProcessGroupTracker,
) -> Result<bool, CommandError> {
    use std::ffi::{c_int, c_void};

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_listpgrppids(pgrpid: c_int, buffer: *mut c_void, buffersize: c_int) -> c_int;
    }

    let group_id = i32::try_from(child.id()).map_err(|_| {
        CommandError::InspectProcessGroup(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command process-group id does not fit in i32",
        ))
    })?;
    let mut capacity = 32usize;
    loop {
        let mut pids = vec![0i32; capacity];
        let buffer_bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| {
                CommandError::InspectProcessGroup(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "command process-group member buffer exceeded platform limits",
                ))
            })?;
        // SAFETY: `pids` is writable for exactly `buffer_bytes`, and libproc only
        // writes pid_t values within that buffer. The process-group id is retained
        // by the unreaped leader while this query runs.
        let pids_written = unsafe {
            proc_listpgrppids(group_id, pids.as_mut_ptr().cast::<c_void>(), buffer_bytes)
        };
        if pids_written < 0 {
            return Err(CommandError::InspectProcessGroup(
                std::io::Error::last_os_error(),
            ));
        }
        let pids_written = usize::try_from(pids_written).map_err(|_| {
            CommandError::InspectProcessGroup(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "libproc returned an invalid process-group member count",
            ))
        })?;
        if pids_written >= capacity {
            if capacity >= MAX_PROCESS_GROUP_MEMBERS {
                return Err(CommandError::InspectProcessGroup(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "command process group exceeds the supported member limit",
                )));
            }
            capacity = capacity.checked_mul(2).ok_or_else(|| {
                CommandError::InspectProcessGroup(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "command process-group member buffer capacity overflowed",
                ))
            })?;
            continue;
        }
        return Ok(pids[..pids_written]
            .iter()
            .copied()
            .any(|pid| pid != 0 && pid != group_id));
    }
}

#[cfg(target_os = "linux")]
fn process_group_has_members_other_than_leader(
    child: &GroupChild,
    tracker: &mut ProcessGroupTracker,
) -> Result<bool, CommandError> {
    let group_id = child.id();
    if let Some(pid) = tracker.known_member.take() {
        if linux_process_group(pid)? == Some(group_id) {
            tracker.known_member = Some(pid);
            return Ok(true);
        }
    }
    let entries = std::fs::read_dir("/proc").map_err(CommandError::InspectProcessGroup)?;
    for entry in entries {
        let entry = entry.map_err(CommandError::InspectProcessGroup)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == group_id {
            continue;
        }
        if linux_process_group(pid)? == Some(group_id) {
            tracker.known_member = Some(pid);
            return Ok(true);
        }
    }
    // `/proc` enumeration is not an atomic snapshot. The unreaped leader still
    // owns the process-group id here, so one final group signal safely closes a
    // fork/exit transition that the directory walk could otherwise miss. A
    // terminal leader ignores the signal; any missed residual member cannot
    // mutate the workspace after a successful return.
    kill_process_group_by_id(group_id).map_err(CommandError::Kill)?;
    Ok(false)
}

#[cfg(target_os = "linux")]
fn linux_process_group(pid: u32) -> Result<Option<u32>, CommandError> {
    let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) if source.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(source) => return Err(CommandError::InspectProcessGroup(source)),
    };
    let delimiter = stat
        .windows(2)
        .rposition(|window| window == b") ")
        .ok_or_else(|| {
            CommandError::InspectProcessGroup(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("process {pid} returned malformed /proc stat data"),
            ))
        })?;
    let group = stat[delimiter + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .nth(2)
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse::<u32>().ok())
        .ok_or_else(|| {
            CommandError::InspectProcessGroup(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("process {pid} returned an invalid process-group id"),
            ))
        })?;
    Ok(Some(group))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_group_has_members_other_than_leader(
    _child: &GroupChild,
    _tracker: &mut ProcessGroupTracker,
) -> Result<bool, CommandError> {
    Err(CommandError::InspectProcessGroup(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable command process-group membership inspection is not supported on this platform",
    )))
}

#[cfg(unix)]
fn kill_process_group_by_id(id: u32) -> std::io::Result<()> {
    let raw = i32::try_from(id).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command process-group id does not fit in i32",
        )
    })?;
    let pid = rustix::process::Pid::from_raw(raw).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command process-group id was zero",
        )
    })?;
    loop {
        match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
            Ok(()) => return Ok(()),
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) if source == rustix::io::Errno::SRCH => return Ok(()),
            Err(source) => return Err(std::io::Error::from(source)),
        }
    }
}

fn wait_for_command_group(child: &mut GroupChild) -> Result<ExitStatus, CommandError> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(CommandError::Wait(source)),
        }
    }
}

fn try_wait_for_command_group(child: &mut GroupChild) -> Result<Option<ExitStatus>, CommandError> {
    loop {
        match child.try_wait() {
            Ok(status) => return Ok(status),
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(CommandError::Wait(source)),
        }
    }
}

fn cleanup_command_resources(
    child: &mut GroupChild,
    process_exited: bool,
    terminate_group: bool,
    output: OutputReaderResources,
) -> Result<(), CommandError> {
    let mut first_error = cleanup_process_group(child, process_exited, terminate_group).err();

    output.stop.store(true, Ordering::Relaxed);
    drop(output.receiver);
    if output.stdout.join().is_err() && first_error.is_none() {
        first_error = Some(CommandError::ReaderPanicked);
    }
    if output.stderr.join().is_err() && first_error.is_none() {
        first_error = Some(CommandError::ReaderPanicked);
    }
    match first_error {
        Some(source) => Err(source),
        None => Ok(()),
    }
}

struct OutputReaderResources {
    stop: Arc<AtomicBool>,
    receiver: Receiver<StreamMessage>,
    stdout: thread::JoinHandle<()>,
    stderr: thread::JoinHandle<()>,
}

fn cleanup_process_group(
    child: &mut GroupChild,
    process_exited: bool,
    terminate_group: bool,
) -> Result<(), CommandError> {
    let mut first_error = None;
    if terminate_group {
        if let Err(source) = terminate_command_group(child) {
            first_error = Some(source);
        }
    }
    if !process_exited {
        let wait_result = if terminate_group && first_error.is_some() {
            try_wait_for_command_group(child).map(|_| ())
        } else {
            wait_for_command_group(child).map(|_| ())
        };
        if let Err(source) = wait_result {
            if first_error.is_none() {
                first_error = Some(source);
            }
        }
    }
    match first_error {
        Some(source) => Err(source),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn group_already_exited(source: &std::io::Error) -> bool {
    matches!(
        source.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    ) || source.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

#[cfg(not(unix))]
fn group_already_exited(source: &std::io::Error) -> bool {
    matches!(
        source.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    )
}

#[cfg(unix)]
fn make_nonblocking<F: std::os::fd::AsFd>(file: &F) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(file, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)
}

#[cfg(not(unix))]
fn make_nonblocking<F>(_file: &F) -> std::io::Result<()> {
    Ok(())
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
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = vec![0u8; 8 * 1024];
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
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
                Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(POLL_INTERVAL);
                }
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
    WorkspaceChanged(std::io::Error),
    ConfigureOutput(std::io::Error),
    InspectProcessGroup(std::io::Error),
    Kill(std::io::Error),
    Wait(std::io::Error),
    ReadOutput(std::io::Error),
    ReaderPanicked,
    Cancelled,
    OutputIncomplete,
    TerminationUnverified,
}

impl CommandError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "command_spawn_failed",
            Self::WorkspaceChanged(_) => "workspace_changed",
            Self::ConfigureOutput(_)
            | Self::InspectProcessGroup(_)
            | Self::Kill(_)
            | Self::Wait(_)
            | Self::ReadOutput(_)
            | Self::ReaderPanicked => "command_io_error",
            Self::Cancelled => "tool_cancelled",
            Self::OutputIncomplete => "command_output_incomplete",
            Self::TerminationUnverified => "command_termination_unverified",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ConfigureOutput(source)
            | Self::InspectProcessGroup(source)
            | Self::Kill(source)
            | Self::Wait(source)
            | Self::ReadOutput(source)
                if source.kind() == std::io::ErrorKind::Interrupted
        )
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(source) => write!(formatter, "failed to start command: {source}"),
            Self::WorkspaceChanged(source) => {
                write!(
                    formatter,
                    "selected workspace is no longer available: {source}"
                )
            }
            Self::ConfigureOutput(source) => {
                write!(
                    formatter,
                    "failed to configure command output capture: {source}"
                )
            }
            Self::InspectProcessGroup(source) => {
                write!(formatter, "failed to inspect command process group: {source}")
            }
            Self::Kill(source) => write!(formatter, "failed to terminate command group: {source}"),
            Self::Wait(source) => write!(formatter, "failed to wait for command: {source}"),
            Self::ReadOutput(source) => {
                write!(formatter, "failed to capture command output: {source}")
            }
            Self::ReaderPanicked => formatter.write_str("command reader panicked"),
            Self::Cancelled => formatter.write_str("run_command was cancelled"),
            Self::OutputIncomplete => formatter
                .write_str("command output remained open after the command process group exited"),
            Self::TerminationUnverified => formatter.write_str(
                "command process group was terminated, but detached descendants could not be verified",
            ),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        run_shell_command, CommandError, COMMAND_GROUP_TERMINATION_ATTEMPTS,
        INJECT_GROUP_KILL_WRAPPER_FAILURE, INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE,
    };
    use crate::workspace::CodingWorkspace;

    #[test]
    fn process_control_failure_still_runs_the_cleanup_epilogue() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-cleanup-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let cancellation = Arc::new(AtomicBool::new(false));
        let trigger_flag = cancellation.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            trigger_flag.store(true, Ordering::Relaxed);
        });
        COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.set(0));
        INJECT_GROUP_KILL_WRAPPER_FAILURE.with(|failure| failure.set(true));
        let started = Instant::now();

        let result = run_shell_command(
            &workspace,
            "(sleep 0.2; printf leaked > delayed.txt) & wait",
            &cancellation,
            1024,
        );

        trigger.join().expect("cancellation trigger finishes");
        assert!(matches!(result, Err(CommandError::TerminationUnverified)));
        assert_eq!(
            COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.get()),
            1,
            "a reaped group must never be signalled again through its stale process id"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(250));
        assert!(!root.join("delayed.txt").exists());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn output_configuration_failure_still_reaps_the_spawned_group() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-config-cleanup-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE.with(|failure| failure.set(true));
        let started = Instant::now();

        let result = run_shell_command(
            &workspace,
            "(sleep 0.2; printf leaked > delayed.txt) & wait",
            &AtomicBool::new(false),
            1024,
        );

        assert!(matches!(result, Err(CommandError::ConfigureOutput(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(250));
        assert!(!root.join("delayed.txt").exists());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
