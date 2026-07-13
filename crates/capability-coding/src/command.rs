use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
const INITIAL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FOREGROUND_EXIT_SETTLE_YIELDS: usize = 8;
const MAX_STREAM_READS_PER_TICK: usize = 16;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);

#[cfg(all(test, unix))]
thread_local! {
    static INJECT_GROUP_KILL_WRAPPER_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_PERSISTENT_GROUP_KILL_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE: Cell<bool> = const { Cell::new(false) };
    static COMMAND_GROUP_TERMINATION_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    static LIVE_COMMAND_SUPERVISOR_HANDOFFS: Cell<usize> = const { Cell::new(0) };
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
            (
                "residual_process_group_policy".to_string(),
                json!("kill_requested_before_leader_reap"),
            ),
            (
                "background_process_policy".to_string(),
                json!("kill_requested_at_foreground_exit"),
            ),
            (
                "process_security_policy".to_string(),
                json!(process_security_policy()),
            ),
            (
                "exec_privilege_gain_blocked".to_string(),
                json!(exec_privilege_gain_blocked()),
            ),
            ("detached_processes_tracked".to_string(), json!(false)),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    })
}

#[derive(Debug)]
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
    configure_command_security(&mut process);
    let child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn();
    let mut child = child.map_err(CommandError::Spawn)?;
    let mut stdout = child
        .inner()
        .stdout
        .take()
        .expect("stdout was configured as piped");
    let mut stderr = child
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
        let cleanup = cleanup_process_group(child, false, true);
        return match cleanup {
            Err(cleanup) => Err(cleanup),
            Ok(()) => Err(CommandError::ConfigureOutput(source)),
        };
    }
    let mut stdout_capture = CapturedStream::new(max_output_bytes);
    let mut stderr_capture = CapturedStream::new(max_output_bytes);
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stream_error = None;
    let mut status = None;
    let mut leader_terminal = false;
    let mut drain_started_at = None;
    let mut output_incomplete = false;
    let mut cancelled = false;
    let mut forced_at = None;
    let mut termination_sent = false;
    let mut exit_poll_interval = INITIAL_EXIT_POLL_INTERVAL;

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

            let stdout_progress = read_available_stream(
                &mut stdout,
                &mut stdout_capture,
                &mut stdout_done,
                &mut stream_error,
            );
            let stderr_progress = read_available_stream(
                &mut stderr,
                &mut stderr_capture,
                &mut stderr_done,
                &mut stream_error,
            );
            if stdout_progress || stderr_progress {
                exit_poll_interval = INITIAL_EXIT_POLL_INTERVAL;
            }

            if status.is_none() && !leader_terminal {
                leader_terminal = command_leader_terminal(&child)?;
            }
            if status.is_none() && leader_terminal {
                // Keep the terminal leader unreaped while already-started descendant
                // forks settle, then terminate the still-reserved process group once.
                for _ in 0..FOREGROUND_EXIT_SETTLE_YIELDS {
                    thread::yield_now();
                }
                terminate_signal_compatible_group_after_leader_exit(&mut child)?;
                termination_sent = true;
                status = Some(wait_for_command_group(&mut child)?);
            }

            if status.is_some() && stdout_done && stderr_done {
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
            if !stdout_progress && !stderr_progress {
                wait_for_stream_activity(
                    &stdout,
                    &stderr,
                    stdout_done,
                    stderr_done,
                    exit_poll_interval,
                )?;
                if stdout_done && stderr_done {
                    exit_poll_interval = exit_poll_interval
                        .saturating_mul(2)
                        .min(MAX_EXIT_POLL_INTERVAL);
                }
            }
        }
    })();

    let cleanup_result = cleanup_process_group(
        child,
        status.is_some(),
        control_result.is_err() && status.is_none() && !termination_sent,
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

#[cfg(target_os = "linux")]
fn process_security_policy() -> &'static str {
    "no_new_privs_and_group_termination"
}

#[cfg(target_os = "macos")]
fn process_security_policy() -> &'static str {
    "group_termination_without_credential_lock"
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_security_policy() -> &'static str {
    "unsupported"
}

fn exec_privilege_gain_blocked() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn ensure_process_tracking_supported() -> Result<(), CommandError> {
    Err(CommandError::UnsupportedProcessTracking)
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn configure_command_security(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: rustix issues the single `prctl(PR_SET_NO_NEW_PRIVS)` syscall and
    // does not allocate or touch shared process state in the post-fork child.
    unsafe {
        command.pre_exec(|| rustix::thread::set_no_new_privs(true).map_err(std::io::Error::from));
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_command_security(_command: &mut Command) {}

fn terminate_command_group(child: &mut GroupChild) -> Result<(), CommandError> {
    #[cfg(all(test, unix))]
    COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    #[cfg(all(test, unix))]
    let injected_failure = INJECT_GROUP_KILL_WRAPPER_FAILURE.with(Cell::get)
        || INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(Cell::get);
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
        #[cfg(all(test, unix))]
        let direct_failure = INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(Cell::get);
        #[cfg(not(all(test, unix)))]
        let direct_failure = false;
        let fallback = if direct_failure {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected direct process-group termination failure",
            ))
        } else {
            kill_process_group_by_id(child.id())
        };
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

fn terminate_signal_compatible_group_after_leader_exit(
    child: &mut GroupChild,
) -> Result<(), CommandError> {
    match terminate_command_group(child) {
        Err(CommandError::Kill(source))
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            // The terminal leader is intentionally unreaped and can keep an otherwise
            // empty group present. EPERM means no remaining member is signal-compatible
            // with this process; credential-changing descendants are outside the
            // portable tracking contract exposed in tool metadata.
            Ok(())
        }
        result => result,
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

#[cfg(unix)]
fn wait_for_command_group(child: &mut GroupChild) -> Result<ExitStatus, CommandError> {
    let pid = process_group_id(child)?;
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()) {
            Ok(Some((_pid, status))) => return Ok(ExitStatus::from_raw(status.as_raw())),
            Ok(None) => continue,
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) => return Err(CommandError::Wait(std::io::Error::from(source))),
        }
    }
}

#[cfg(not(unix))]
fn wait_for_command_group(child: &mut GroupChild) -> Result<ExitStatus, CommandError> {
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(CommandError::Wait(source)),
        }
    }
}

#[cfg(unix)]
fn try_wait_for_command_group(child: &mut GroupChild) -> Result<Option<ExitStatus>, CommandError> {
    let pid = process_group_id(child)?;
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
            Ok(Some((_pid, status))) => return Ok(Some(ExitStatus::from_raw(status.as_raw()))),
            Ok(None) => return Ok(None),
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) => return Err(CommandError::Wait(std::io::Error::from(source))),
        }
    }
}

#[cfg(not(unix))]
fn try_wait_for_command_group(child: &mut GroupChild) -> Result<Option<ExitStatus>, CommandError> {
    loop {
        match child.try_wait() {
            Ok(status) => return Ok(status),
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(CommandError::Wait(source)),
        }
    }
}

fn cleanup_process_group(
    mut child: GroupChild,
    process_exited: bool,
    terminate_group: bool,
) -> Result<(), CommandError> {
    let mut first_error = None;
    if terminate_group {
        if let Err(source) = terminate_command_group(&mut child) {
            first_error = Some(source);
        }
    }
    if !process_exited {
        let wait_result = if terminate_group && first_error.is_some() {
            match try_wait_for_command_group(&mut child) {
                Ok(Some(_)) => Ok(()),
                Ok(None) => {
                    return Err(supervise_live_command(
                        child,
                        first_error
                            .take()
                            .expect("termination failure was recorded"),
                        None,
                    ));
                }
                Err(wait) => {
                    return Err(supervise_live_command(
                        child,
                        first_error
                            .take()
                            .expect("termination failure was recorded"),
                        Some(wait),
                    ));
                }
            }
        } else {
            wait_for_command_group(&mut child).map(|_| ())
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

fn supervise_live_command(
    child: GroupChild,
    termination_error: CommandError,
    wait_error: Option<CommandError>,
) -> CommandError {
    #[cfg(all(test, unix))]
    {
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(handoffs.get() + 1));
        INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(|failure| failure.set(false));
    }
    let kind = match &termination_error {
        CommandError::Kill(source) => source.kind(),
        _ => std::io::ErrorKind::Other,
    };
    let wait_context = wait_error.map_or_else(String::new, |source| {
        format!("; nonblocking reap inspection also failed ({source})")
    });
    let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    let worker_owner = owner.clone();
    let spawn = thread::Builder::new()
        .name("young-command-reaper".to_string())
        .spawn(move || {
            let mut child = worker_owner
                .lock()
                .expect("command supervisor ownership lock was poisoned")
                .take()
                .expect("command supervisor takes ownership once");
            supervise_and_reap_command(&mut child);
        });
    if let Err(source) = spawn {
        let mut child = owner
            .lock()
            .expect("command supervisor ownership lock was poisoned")
            .take()
            .expect("failed supervisor spawn retains command ownership");
        let _ = wait_for_command_group(&mut child);
        return CommandError::Kill(std::io::Error::new(
            kind,
            format!(
                "{termination_error}{wait_context}; background supervisor could not start ({source}), so cleanup waited for the command leader"
            ),
        ));
    }
    CommandError::Kill(std::io::Error::new(
        kind,
        format!(
            "{termination_error}{wait_context}; a live command may remain while a background supervisor retries termination and retains reaping ownership"
        ),
    ))
}

fn supervise_and_reap_command(child: &mut GroupChild) {
    let mut retry_delay = Duration::from_millis(10);
    for _ in 0..8 {
        if matches!(try_wait_for_command_group(child), Ok(Some(_))) {
            return;
        }
        if terminate_command_group(child).is_ok() {
            let _ = wait_for_command_group(child);
            return;
        }
        thread::sleep(retry_delay);
        retry_delay = retry_delay
            .saturating_mul(2)
            .min(Duration::from_millis(250));
    }
    let _ = wait_for_command_group(child);
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

fn read_available_stream<R: Read>(
    reader: &mut R,
    capture: &mut CapturedStream,
    done: &mut bool,
    stream_error: &mut Option<std::io::Error>,
) -> bool {
    if *done {
        return false;
    }
    let mut progressed = false;
    let mut buffer = [0u8; 8 * 1024];
    for _ in 0..MAX_STREAM_READS_PER_TICK {
        match reader.read(&mut buffer) {
            Ok(0) => {
                *done = true;
                return true;
            }
            Ok(bytes_read) => {
                capture.push(&buffer[..bytes_read]);
                progressed = true;
            }
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => return progressed,
            Err(source) => {
                *done = true;
                progressed = true;
                if stream_error.is_none() {
                    *stream_error = Some(source);
                }
                return progressed;
            }
        }
    }
    progressed
}

#[cfg(unix)]
fn wait_for_stream_activity<Out, Err>(
    stdout: &Out,
    stderr: &Err,
    stdout_done: bool,
    stderr_done: bool,
    exit_poll_interval: Duration,
) -> Result<(), CommandError>
where
    Out: std::os::fd::AsFd,
    Err: std::os::fd::AsFd,
{
    let events = rustix::event::PollFlags::IN
        | rustix::event::PollFlags::HUP
        | rustix::event::PollFlags::ERR;
    if stdout_done && stderr_done {
        thread::sleep(exit_poll_interval);
        return Ok(());
    }
    let mut stdout_descriptor = [rustix::event::PollFd::new(stdout, events)];
    let mut stderr_descriptor = [rustix::event::PollFd::new(stderr, events)];
    let mut both_descriptors = [
        rustix::event::PollFd::new(stdout, events),
        rustix::event::PollFd::new(stderr, events),
    ];
    let descriptors = match (stdout_done, stderr_done) {
        (false, false) => &mut both_descriptors[..],
        (false, true) => &mut stdout_descriptor[..],
        (true, false) => &mut stderr_descriptor[..],
        (true, true) => unreachable!("completed streams returned above"),
    };
    let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: POLL_INTERVAL.as_nanos() as _,
    };
    loop {
        match rustix::event::poll(descriptors, Some(&timeout)) {
            Ok(_) => return Ok(()),
            Err(source) if source == rustix::io::Errno::INTR => continue,
            Err(source) => return Err(CommandError::ReadOutput(std::io::Error::from(source))),
        }
    }
}

#[cfg(not(unix))]
fn wait_for_stream_activity<Out, Err>(
    _stdout: &Out,
    _stderr: &Err,
    _stdout_done: bool,
    _stderr_done: bool,
    exit_poll_interval: Duration,
) -> Result<(), CommandError> {
    if _stdout_done && _stderr_done {
        thread::sleep(exit_poll_interval);
    } else {
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
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
    Kill(std::io::Error),
    Wait(std::io::Error),
    ReadOutput(std::io::Error),
    Cancelled,
    OutputIncomplete,
    TerminationUnverified,
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    UnsupportedProcessTracking,
}

impl CommandError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "command_spawn_failed",
            Self::WorkspaceChanged(_) => "workspace_changed",
            Self::ConfigureOutput(_) | Self::Kill(_) | Self::Wait(_) | Self::ReadOutput(_) => {
                "command_io_error"
            }
            Self::Cancelled => "tool_cancelled",
            Self::OutputIncomplete => "command_output_incomplete",
            Self::TerminationUnverified => "command_termination_unverified",
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            Self::UnsupportedProcessTracking => "command_process_tracking_unsupported",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ConfigureOutput(source)
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
            Self::Kill(source) => write!(formatter, "failed to terminate command group: {source}"),
            Self::Wait(source) => write!(formatter, "failed to wait for command: {source}"),
            Self::ReadOutput(source) => {
                write!(formatter, "failed to capture command output: {source}")
            }
            Self::Cancelled => formatter.write_str("run_command was cancelled"),
            Self::OutputIncomplete => formatter
                .write_str("command output remained open after the command process group exited"),
            Self::TerminationUnverified => formatter.write_str(
                "termination was requested for signal-compatible process-group members; detached or credential-changing descendants were not verified",
            ),
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            Self::UnsupportedProcessTracking => formatter.write_str(
                "stable command process-group tracking is not supported on this platform",
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
        INJECT_PERSISTENT_GROUP_KILL_FAILURE, LIVE_COMMAND_SUPERVISOR_HANDOFFS,
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
        assert!(
            matches!(result, Err(CommandError::TerminationUnverified)),
            "unexpected result: {result:?}"
        );
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
    fn double_termination_failure_hands_the_live_child_to_a_supervisor() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-supervisor-{}-{nonce}",
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
        INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(|failure| failure.set(true));
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(0));

        let result = run_shell_command(
            &workspace,
            "(sleep 0.2; printf leaked > delayed.txt) & wait",
            &cancellation,
            1024,
        );

        trigger.join().expect("cancellation trigger finishes");
        let Err(CommandError::Kill(source)) = result else {
            panic!("double termination failure must be reported: {result:?}");
        };
        assert!(source
            .to_string()
            .contains("background supervisor retries termination"));
        assert_eq!(
            LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.get()),
            1
        );
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

        assert!(
            matches!(result, Err(CommandError::ConfigureOutput(_))),
            "unexpected result: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(250));
        assert!(!root.join("delayed.txt").exists());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
