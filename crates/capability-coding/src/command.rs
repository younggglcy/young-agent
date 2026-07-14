use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
#[cfg(all(test, unix))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(unix))]
use command_group::CommandGroup;
use command_group::GroupChild;
use serde_json::json;
#[cfg(all(test, unix))]
use std::cell::Cell;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::command_policy::MAX_COMMAND_BYTES;
use crate::tool_support::{
    failure, finalize_output, truncate_json_string, ToolArguments, MAX_OUTPUT_BYTES,
    MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::CodingWorkspace;

const INITIAL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const MAX_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const TERMINATION_CONFIRM_GRACE: Duration = Duration::from_millis(100);
const FOREGROUND_EXIT_SETTLE_YIELDS: usize = 8;
const TERMINAL_GROUP_SEAL_ROUNDS: usize = 8;
const DESCENDANT_TOKEN_WAIT_SLICE: Duration = Duration::from_millis(5);
const MAX_STREAM_READS_PER_TICK: usize = 16;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
const MAX_COMMAND_SUPERVISION_SLOTS: usize = 64;
const MAX_COMMAND_PROCESSING_PANICS: u8 = 8;
const INITIAL_PROCESSING_PANIC_BACKOFF: Duration = Duration::from_millis(10);
const MAX_PROCESSING_PANIC_BACKOFF: Duration = Duration::from_millis(250);

#[cfg(all(test, unix))]
thread_local! {
    static INJECT_GROUP_KILL_WRAPPER_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_NEXT_FULL_GROUP_KILL_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_PERSISTENT_GROUP_KILL_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS: Cell<bool> = const { Cell::new(false) };
    static INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE: Cell<bool> = const { Cell::new(false) };
    static COMMAND_GROUP_TERMINATION_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    static LIVE_COMMAND_SUPERVISOR_HANDOFFS: Cell<usize> = const { Cell::new(0) };
    static LAST_SUPERVISED_COMMAND_ID: Cell<Option<u64>> = const { Cell::new(None) };
}

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
        Ok(command) if command.len() <= MAX_COMMAND_BYTES => command,
        Ok(_) => {
            return failure(
                "command_too_large",
                format!("command exceeds {MAX_COMMAND_BYTES} bytes"),
                false,
            )
        }
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
                json!("kill_and_tracking_token_close_before_leader_reap"),
            ),
            (
                "background_process_policy".to_string(),
                json!("tracked_descendants_terminated_at_foreground_exit"),
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

struct CommandProcess {
    child: GroupChild,
    #[cfg(unix)]
    descendant_token: Option<DescendantToken>,
    #[cfg(all(test, unix))]
    fail_next_wait: bool,
}

impl CommandProcess {
    #[cfg(unix)]
    fn tracked(child: GroupChild, descendant_token: DescendantToken) -> Self {
        Self {
            child,
            descendant_token: Some(descendant_token),
            #[cfg(all(test, unix))]
            fail_next_wait: false,
        }
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    fn untracked(child: GroupChild) -> Self {
        Self {
            child,
            descendant_token: None,
            fail_next_wait: false,
        }
    }

    #[cfg(not(unix))]
    fn untracked(child: GroupChild) -> Self {
        Self { child }
    }

    fn seal_and_reap_terminal_group(&mut self) -> Result<ExitStatus, CommandError> {
        #[cfg(unix)]
        let descendant_token = self.descendant_token.as_mut();
        #[cfg(not(unix))]
        let descendant_token = None;
        seal_terminal_process_group(&mut self.child, descendant_token)?;
        #[cfg(all(test, unix))]
        if std::mem::replace(&mut self.fail_next_wait, false) {
            return Err(CommandError::Wait(std::io::Error::other(
                "injected supervisor wait failure",
            )));
        }
        wait_for_command_group(&mut self.child)
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.inner().stdout.take()
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.inner().stderr.take()
    }

    fn terminate_group(&mut self) -> Result<(), CommandError> {
        terminate_command_group(&mut self.child)
    }

    fn leader_terminal(&self) -> Result<bool, CommandError> {
        command_leader_terminal(&self.child)
    }

    fn wait_for_leader_terminal(&self, grace: Duration) -> Result<bool, CommandError> {
        wait_for_leader_terminal_bounded(&self.child, grace)
    }

    fn tracked_descendants_are_sealed(&self) -> bool {
        #[cfg(unix)]
        {
            self.descendant_token
                .as_ref()
                .is_some_and(|token| token.closed)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    #[cfg(all(test, unix))]
    fn inject_wait_failure(&mut self) {
        self.fail_next_wait = true;
    }
}

#[cfg(unix)]
struct DescendantToken {
    reader: UnixStream,
    closed: bool,
}

#[cfg(not(unix))]
struct DescendantToken;

#[cfg(not(unix))]
impl DescendantToken {
    fn wait_for_close(&mut self, _timeout: Duration) -> Result<bool, CommandError> {
        Ok(true)
    }
}

#[cfg(unix)]
impl DescendantToken {
    fn prepare(
        command: Command,
    ) -> Result<(young_platform_process::PreparedTrackedCommand, Self), CommandError> {
        let (reader, writer) = UnixStream::pair().map_err(CommandError::ConfigureOutput)?;
        reader
            .set_nonblocking(true)
            .map_err(CommandError::ConfigureOutput)?;
        let command = young_platform_process::PreparedTrackedCommand::new(command, writer.into())
            .map_err(CommandError::ConfigureOutput)?;
        Ok((
            command,
            Self {
                reader,
                closed: false,
            },
        ))
    }

    fn wait_for_close(&mut self, timeout: Duration) -> Result<bool, CommandError> {
        if self.observe_close()? {
            return Ok(true);
        }
        let events = rustix::event::PollFlags::IN
            | rustix::event::PollFlags::HUP
            | rustix::event::PollFlags::ERR;
        let mut descriptor = [rustix::event::PollFd::new(&self.reader, events)];
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.observe_close();
            }
            let timeout = rustix::event::Timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            };
            match rustix::event::poll(&mut descriptor, Some(&timeout)) {
                Ok(_) => return self.observe_close(),
                Err(source) if source == rustix::io::Errno::INTR => continue,
                Err(source) => return Err(CommandError::ReadOutput(std::io::Error::from(source))),
            }
        }
    }

    fn observe_close(&mut self) -> Result<bool, CommandError> {
        if self.closed {
            return Ok(true);
        }
        let mut buffer = [0u8; 64];
        match self.reader.read(&mut buffer) {
            Ok(0) => {
                self.closed = true;
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(source)
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(false)
            }
            Err(source) => Err(CommandError::ReadOutput(source)),
        }
    }
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
    ensure_process_tracking_supported()?;
    let mut process = shell_command(command);
    let supervision_permit = prepare_command_supervision()?;
    workspace
        .bind_command_working_directory(&mut process)
        .map_err(CommandError::WorkspaceChanged)?;
    configure_command_security(&mut process);
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    let (process, descendant_token) = DescendantToken::prepare(process)?;
    #[cfg(unix)]
    let child = process.spawn_group();
    #[cfg(not(unix))]
    let child = process.group_spawn();
    let child = child.map_err(CommandError::Spawn)?;
    #[cfg(unix)]
    let mut child = CommandProcess::tracked(child, descendant_token);
    #[cfg(not(unix))]
    let mut child = CommandProcess::untracked(child);
    let mut stdout = child.take_stdout().expect("stdout was configured as piped");
    let mut stderr = child.take_stderr().expect("stderr was configured as piped");
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
        let cleanup = cleanup_process_group(
            child,
            CommandCleanupState::TerminateAndReap,
            supervision_permit,
        );
        return match cleanup {
            Err(cleanup) => Err(cleanup),
            Ok(_) => Err(CommandError::ConfigureOutput(source)),
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
    let mut exit_poll_interval = INITIAL_EXIT_POLL_INTERVAL;

    let control_result = (|| -> Result<(), CommandError> {
        loop {
            if cancellation.load(Ordering::Relaxed) && !cancelled {
                cancelled = true;
                if status.is_none() {
                    child.terminate_group()?;
                    if child.wait_for_leader_terminal(TERMINATION_CONFIRM_GRACE)? {
                        status = Some(child.seal_and_reap_terminal_group()?);
                    }
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
                exit_poll_interval = next_exit_poll_interval(exit_poll_interval, true);
            }

            if status.is_none() && !leader_terminal {
                leader_terminal = child.leader_terminal()?;
            }
            if status.is_none() && leader_terminal {
                // Keep the terminal leader unreaped while already-started descendant
                // forks settle, then terminate the still-reserved process group once.
                status = Some(child.seal_and_reap_terminal_group()?);
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
                exit_poll_interval = next_exit_poll_interval(exit_poll_interval, false);
            }
        }
    })();

    let cleanup_state = if status.is_some() {
        CommandCleanupState::AlreadyReaped
    } else {
        CommandCleanupState::TerminateAndReap
    };
    let cleanup_result = cleanup_process_group(child, cleanup_state, supervision_permit);
    if control_result.is_err()
        && matches!(&cleanup_result, Ok(CommandCleanupCompletion::Reaped { .. }))
    {
        drain_streams_after_reap(
            (&mut stdout, &mut stdout_capture, &mut stdout_done),
            (&mut stderr, &mut stderr_capture, &mut stderr_done),
            &mut stream_error,
            wait_for_stream_activity,
        )?;
    }
    if let Some(cleanup_status) = reconcile_control_and_cleanup(
        control_result,
        cleanup_result,
        cancelled,
        stdout_done && stderr_done,
    )? {
        status = Some(cleanup_status);
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

fn next_exit_poll_interval(current: Duration, made_progress: bool) -> Duration {
    if made_progress {
        INITIAL_EXIT_POLL_INTERVAL
    } else {
        current.saturating_mul(2).min(MAX_EXIT_POLL_INTERVAL)
    }
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

fn prepare_command_supervision() -> Result<CommandSupervisionPermit, CommandError> {
    prepare_command_supervision_with(command_supervisor())
}

fn prepare_command_supervision_with(
    supervisor: &std::sync::Arc<CommandSupervisor>,
) -> Result<CommandSupervisionPermit, CommandError> {
    supervisor
        .ensure_worker_started()
        .map_err(CommandError::SupervisorUnavailable)?;
    supervisor.ensure_admission_healthy()?;
    supervisor.reserve_slot()
}

fn configure_command_security(command: &mut Command) {
    young_platform_process::block_exec_privilege_gain(command);
}

fn terminate_command_group(child: &mut GroupChild) -> Result<(), CommandError> {
    #[cfg(all(test, unix))]
    COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
    #[cfg(all(test, unix))]
    if INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS.with(Cell::get) {
        return Ok(());
    }
    #[cfg(all(test, unix))]
    let inject_full_failure =
        INJECT_NEXT_FULL_GROUP_KILL_FAILURE.with(|failure| failure.replace(false));
    #[cfg(all(test, unix))]
    let injected_failure = inject_full_failure
        || INJECT_GROUP_KILL_WRAPPER_FAILURE.with(Cell::get)
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
        let direct_failure =
            inject_full_failure || INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(Cell::get);
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
            let kind = if group_signal_permission_denied(&wrapper)
                || group_signal_permission_denied(&fallback)
            {
                std::io::ErrorKind::PermissionDenied
            } else {
                fallback.kind()
            };
            CommandError::Kill(std::io::Error::new(
                kind,
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
        Err(CommandError::Kill(source)) if group_signal_permission_denied(&source) => {
            // The terminal leader is intentionally unreaped and can keep an otherwise
            // empty group present. EPERM means no remaining member is signal-compatible
            // with this process; credential-changing descendants are outside the
            // portable tracking contract exposed in tool metadata.
            Ok(())
        }
        result => result,
    }
}

fn group_signal_permission_denied(source: &std::io::Error) -> bool {
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(unix)]
    {
        source.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn seal_terminal_process_group(
    child: &mut GroupChild,
    mut descendant_token: Option<&mut DescendantToken>,
) -> Result<(), CommandError> {
    for _ in 0..TERMINAL_GROUP_SEAL_ROUNDS {
        for _ in 0..FOREGROUND_EXIT_SETTLE_YIELDS {
            thread::yield_now();
        }
        terminate_signal_compatible_group_after_leader_exit(child)?;
        if let Some(token) = descendant_token.as_deref_mut() {
            if token.wait_for_close(DESCENDANT_TOKEN_WAIT_SLICE)? {
                return Ok(());
            }
        }
    }
    if descendant_token.is_some() {
        return Err(CommandError::TerminationUnverified);
    }
    Ok(())
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

fn wait_for_leader_terminal_bounded(
    child: &GroupChild,
    grace: Duration,
) -> Result<bool, CommandError> {
    let started = Instant::now();
    let mut poll_interval = Duration::from_millis(1);
    loop {
        if command_leader_terminal(child)? {
            return Ok(true);
        }
        if started.elapsed() >= grace {
            return Ok(false);
        }
        thread::sleep(poll_interval);
        poll_interval = poll_interval
            .saturating_mul(2)
            .min(Duration::from_millis(10));
    }
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

#[derive(Clone, Copy)]
enum CommandCleanupState {
    AlreadyReaped,
    TerminateAndReap,
}

#[derive(Debug)]
enum CommandCleanupCompletion {
    AlreadyReaped,
    Reaped {
        status: ExitStatus,
        tracked_descendants_sealed: bool,
    },
}

fn reconcile_control_and_cleanup(
    control_result: Result<(), CommandError>,
    cleanup_result: Result<CommandCleanupCompletion, CommandError>,
    cancellation_observed: bool,
    streams_done: bool,
) -> Result<Option<ExitStatus>, CommandError> {
    match (control_result, cleanup_result) {
        (Err(_), Err(cleanup)) => Err(cleanup),
        (
            Err(CommandError::TerminationUnverified),
            Ok(CommandCleanupCompletion::Reaped {
                status,
                tracked_descendants_sealed: true,
            }),
        ) if streams_done => Ok(Some(status)),
        (
            Err(CommandError::Kill(source)),
            Ok(CommandCleanupCompletion::Reaped {
                status,
                tracked_descendants_sealed: true,
            }),
        ) if cancellation_observed && streams_done && group_signal_permission_denied(&source) => {
            Ok(Some(status))
        }
        (Err(source), Ok(_)) => Err(source),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(CommandCleanupCompletion::Reaped { status, .. })) => Ok(Some(status)),
        (Ok(()), Ok(CommandCleanupCompletion::AlreadyReaped)) => Ok(None),
    }
}

fn cleanup_process_group(
    mut child: CommandProcess,
    state: CommandCleanupState,
    supervision_permit: CommandSupervisionPermit,
) -> Result<CommandCleanupCompletion, CommandError> {
    if matches!(state, CommandCleanupState::AlreadyReaped) {
        return Ok(CommandCleanupCompletion::AlreadyReaped);
    }
    let mut first_error = None;
    if let Err(source) = child.terminate_group() {
        first_error = Some(source);
    }
    match child.wait_for_leader_terminal(TERMINATION_CONFIRM_GRACE) {
        Ok(false) => Err(supervise_live_command(
            child,
            supervision_permit,
            first_error
                .take()
                .unwrap_or(CommandError::TerminationUnverified),
            None,
        )),
        Err(wait) => Err(supervise_live_command(
            child,
            supervision_permit,
            first_error
                .take()
                .unwrap_or(CommandError::TerminationUnverified),
            Some(wait),
        )),
        Ok(true) => {
            let status = match child.seal_and_reap_terminal_group() {
                Ok(status) => status,
                Err(source) => {
                    return Err(supervise_live_command(
                        child,
                        supervision_permit,
                        first_error.take().unwrap_or(source),
                        None,
                    ));
                }
            };
            let tracked_descendants_sealed = child.tracked_descendants_are_sealed();
            if tracked_descendants_sealed
                && first_error.as_ref().is_some_and(|error| {
                    matches!(error, CommandError::Kill(source) if group_signal_permission_denied(source))
                })
            {
                first_error = None;
            }
            match first_error {
                Some(source) => Err(source),
                None => Ok(CommandCleanupCompletion::Reaped {
                    status,
                    tracked_descendants_sealed,
                }),
            }
        }
    }
}

fn supervise_live_command(
    child: CommandProcess,
    supervision_permit: CommandSupervisionPermit,
    termination_error: CommandError,
    wait_error: Option<CommandError>,
) -> CommandError {
    supervise_live_command_with(
        command_supervisor(),
        child,
        supervision_permit,
        termination_error,
        wait_error,
    )
}

fn supervise_live_command_with(
    supervisor: &std::sync::Arc<CommandSupervisor>,
    child: CommandProcess,
    supervision_permit: CommandSupervisionPermit,
    termination_error: CommandError,
    wait_error: Option<CommandError>,
) -> CommandError {
    #[cfg(all(test, unix))]
    {
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(handoffs.get() + 1));
        INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(|failure| failure.set(false));
        INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS.with(|failure| failure.set(false));
    }
    let wait_context = wait_error.map_or_else(String::new, |source| {
        format!("; nonblocking reap inspection also failed ({source})")
    });
    #[cfg(all(test, unix))]
    let command_id = supervisor.enqueue(child, supervision_permit);
    #[cfg(not(all(test, unix)))]
    supervisor.enqueue(child, supervision_permit);
    #[cfg(all(test, unix))]
    LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(Some(command_id)));
    let supervision = match supervisor.ensure_worker_started() {
        Ok(()) => "a live command may remain while the process-wide supervisor retries termination and retains reaping ownership".to_string(),
        Err(source) => format!(
            "the process-wide supervisor worker is unavailable ({source}); live-command ownership remains retained and the next command preflight will retry worker startup"
        ),
    };
    CommandError::TerminationSupervised(format!("{termination_error}{wait_context}; {supervision}"))
}

struct CommandSupervisor {
    registry: std::sync::Mutex<std::collections::BinaryHeap<SupervisedCommand>>,
    wake: std::sync::Condvar,
    worker_state: std::sync::Mutex<SupervisorWorkerState>,
    admission_health: std::sync::Mutex<SupervisorAdmissionHealth>,
    active_slots: AtomicUsize,
    #[cfg(all(test, unix))]
    next_command_id: AtomicU64,
    #[cfg(all(test, unix))]
    completed: std::sync::Mutex<std::collections::HashSet<u64>>,
    #[cfg(all(test, unix))]
    completion_wake: std::sync::Condvar,
    #[cfg(all(test, unix))]
    fail_next_worker_start: AtomicBool,
    #[cfg(all(test, unix))]
    processing_panics_to_inject: AtomicUsize,
    #[cfg(all(test, unix))]
    processing_panic_command_id: AtomicU64,
    #[cfg(all(test, unix))]
    observed_processing_panics: AtomicUsize,
    #[cfg(all(test, unix))]
    fail_next_wait: AtomicBool,
    #[cfg(all(test, unix))]
    wait_attempts: AtomicUsize,
}

impl CommandSupervisor {
    fn new() -> Self {
        Self {
            registry: std::sync::Mutex::new(std::collections::BinaryHeap::new()),
            wake: std::sync::Condvar::new(),
            worker_state: std::sync::Mutex::new(SupervisorWorkerState::Stopped),
            admission_health: std::sync::Mutex::new(SupervisorAdmissionHealth::Healthy),
            active_slots: AtomicUsize::new(0),
            #[cfg(all(test, unix))]
            next_command_id: AtomicU64::new(1),
            #[cfg(all(test, unix))]
            completed: std::sync::Mutex::new(std::collections::HashSet::new()),
            #[cfg(all(test, unix))]
            completion_wake: std::sync::Condvar::new(),
            #[cfg(all(test, unix))]
            fail_next_worker_start: AtomicBool::new(false),
            #[cfg(all(test, unix))]
            processing_panics_to_inject: AtomicUsize::new(0),
            #[cfg(all(test, unix))]
            processing_panic_command_id: AtomicU64::new(0),
            #[cfg(all(test, unix))]
            observed_processing_panics: AtomicUsize::new(0),
            #[cfg(all(test, unix))]
            fail_next_wait: AtomicBool::new(false),
            #[cfg(all(test, unix))]
            wait_attempts: AtomicUsize::new(0),
        }
    }

    #[cfg(all(test, unix))]
    fn enqueue(&self, child: CommandProcess, permit: CommandSupervisionPermit) -> u64 {
        self.enqueue_at(child, permit, Instant::now())
    }

    #[cfg(all(test, unix))]
    fn enqueue_at(
        &self,
        child: CommandProcess,
        permit: CommandSupervisionPermit,
        next_action: Instant,
    ) -> u64 {
        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);
        let mut command = SupervisedCommand::new(id, child, permit);
        command.next_action = next_action;
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(command);
        self.wake.notify_one();
        id
    }

    #[cfg(not(all(test, unix)))]
    fn enqueue(&self, child: CommandProcess, permit: CommandSupervisionPermit) {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(SupervisedCommand::new(child, permit));
        self.wake.notify_one();
    }

    fn ensure_worker_started(self: &std::sync::Arc<Self>) -> std::io::Result<()> {
        #[cfg(all(test, unix))]
        if self.fail_next_worker_start.swap(false, Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "injected worker start failure",
            ));
        }
        let mut state = self
            .worker_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state == SupervisorWorkerState::Running {
            return Ok(());
        }
        *state = SupervisorWorkerState::Running;
        let worker = self.clone();
        if let Err(source) = thread::Builder::new()
            .name("young-command-supervisor".to_string())
            .spawn(move || command_supervisor_worker(worker))
        {
            *state = SupervisorWorkerState::Stopped;
            return Err(source);
        }
        Ok(())
    }

    fn ensure_admission_healthy(&self) -> Result<(), CommandError> {
        let health = self
            .admission_health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *health {
            SupervisorAdmissionHealth::Healthy => Ok(()),
            SupervisorAdmissionHealth::Degraded => Err(CommandError::SupervisorDegraded),
        }
    }

    fn reserve_slot(self: &std::sync::Arc<Self>) -> Result<CommandSupervisionPermit, CommandError> {
        let mut active = self.active_slots.load(Ordering::Acquire);
        loop {
            if active >= MAX_COMMAND_SUPERVISION_SLOTS {
                return Err(CommandError::SupervisorAtCapacity);
            }
            match self.active_slots.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(CommandSupervisionPermit {
                        supervisor: self.clone(),
                    })
                }
                Err(observed) => active = observed,
            }
        }
    }

    fn requeue(&self, command: SupervisedCommand) {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(command);
        self.wake.notify_one();
    }

    fn record_processing_panic(&self, command: &mut SupervisedCommand) {
        command.processing_panics = command.processing_panics.saturating_add(1);
        let exponent = u32::from(command.processing_panics.saturating_sub(1).min(5));
        let delay = INITIAL_PROCESSING_PANIC_BACKOFF
            .saturating_mul(1u32 << exponent)
            .min(MAX_PROCESSING_PANIC_BACKOFF);
        command.next_action = Instant::now() + delay;
        if command.processing_panics >= MAX_COMMAND_PROCESSING_PANICS {
            *self
                .admission_health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SupervisorAdmissionHealth::Degraded;
        }
    }

    #[cfg(all(test, unix))]
    fn inject_processing_panic(&self, command_id: u64) -> bool {
        let target = self.processing_panic_command_id.load(Ordering::Relaxed);
        if target != 0 && target != command_id {
            return false;
        }
        self.processing_panics_to_inject
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    fn wait_for_completion(&self, id: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut completed = self
            .completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !completed.contains(&id) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let waited = self.completion_wake.wait_timeout(completed, remaining);
            let (guard, result) = match waited {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            completed = guard;
            if result.timed_out() && !completed.contains(&id) {
                return false;
            }
        }
        true
    }

    #[cfg(all(test, unix))]
    fn mark_completed(&self, id: u64) {
        self.completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
        self.completion_wake.notify_all();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupervisorWorkerState {
    Stopped,
    Running,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupervisorAdmissionHealth {
    Healthy,
    Degraded,
}

struct CommandSupervisionPermit {
    supervisor: std::sync::Arc<CommandSupervisor>,
}

impl Drop for CommandSupervisionPermit {
    fn drop(&mut self) {
        self.supervisor.active_slots.fetch_sub(1, Ordering::AcqRel);
    }
}

struct SupervisedCommand {
    #[cfg(all(test, unix))]
    id: u64,
    child: CommandProcess,
    _permit: CommandSupervisionPermit,
    processing_panics: u8,
    termination_attempts_remaining: u8,
    retry_delay: Duration,
    next_action: Instant,
}

impl SupervisedCommand {
    #[cfg(all(test, unix))]
    fn new(id: u64, child: CommandProcess, permit: CommandSupervisionPermit) -> Self {
        Self {
            id,
            child,
            _permit: permit,
            processing_panics: 0,
            termination_attempts_remaining: 8,
            retry_delay: Duration::from_millis(10),
            next_action: Instant::now(),
        }
    }

    #[cfg(not(all(test, unix)))]
    fn new(child: CommandProcess, permit: CommandSupervisionPermit) -> Self {
        Self {
            child,
            _permit: permit,
            processing_panics: 0,
            termination_attempts_remaining: 8,
            retry_delay: Duration::from_millis(10),
            next_action: Instant::now(),
        }
    }
}

impl PartialEq for SupervisedCommand {
    fn eq(&self, other: &Self) -> bool {
        self.next_action == other.next_action && self.child.id() == other.child.id()
    }
}

impl Eq for SupervisedCommand {}

impl PartialOrd for SupervisedCommand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SupervisedCommand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .next_action
            .cmp(&self.next_action)
            .then_with(|| other.child.id().cmp(&self.child.id()))
    }
}

struct InFlightCommand {
    supervisor: std::sync::Arc<CommandSupervisor>,
    command: Option<SupervisedCommand>,
}

impl InFlightCommand {
    fn new(supervisor: std::sync::Arc<CommandSupervisor>, command: SupervisedCommand) -> Self {
        Self {
            supervisor,
            command: Some(command),
        }
    }

    fn command_mut(&mut self) -> &mut SupervisedCommand {
        self.command
            .as_mut()
            .expect("in-flight command is present until completion")
    }

    #[cfg(all(test, unix))]
    fn id(&self) -> u64 {
        self.command
            .as_ref()
            .expect("in-flight command is present until completion")
            .id
    }

    fn complete(&mut self) {
        self.command.take();
    }
}

impl Drop for InFlightCommand {
    fn drop(&mut self) {
        if let Some(mut command) = self.command.take() {
            if thread::panicking() {
                self.supervisor.record_processing_panic(&mut command);
            }
            self.supervisor.requeue(command);
        }
    }
}

fn command_supervisor() -> &'static std::sync::Arc<CommandSupervisor> {
    static SUPERVISOR: std::sync::OnceLock<std::sync::Arc<CommandSupervisor>> =
        std::sync::OnceLock::new();
    SUPERVISOR.get_or_init(|| std::sync::Arc::new(CommandSupervisor::new()))
}

struct SupervisorWorkerGuard(std::sync::Arc<CommandSupervisor>);

impl Drop for SupervisorWorkerGuard {
    fn drop(&mut self) {
        let mut state = self
            .0
            .worker_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = SupervisorWorkerState::Stopped;
    }
}

fn command_supervisor_worker(supervisor: std::sync::Arc<CommandSupervisor>) {
    let _guard = SupervisorWorkerGuard(supervisor.clone());
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            command_supervisor_loop(supervisor.clone());
        }));
        if result.is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn command_supervisor_loop(supervisor: std::sync::Arc<CommandSupervisor>) {
    loop {
        let command = take_next_due_command(&supervisor);
        let mut in_flight = InFlightCommand::new(supervisor.clone(), command);
        #[cfg(all(test, unix))]
        if supervisor.inject_processing_panic(in_flight.id()) {
            supervisor
                .observed_processing_panics
                .fetch_add(1, Ordering::Relaxed);
            panic!("injected supervisor processing panic");
        }
        if supervise_command_once(in_flight.command_mut(), Instant::now(), &supervisor) {
            in_flight.command_mut().processing_panics = 0;
            drop(in_flight);
        } else {
            #[cfg(all(test, unix))]
            let id = in_flight.id();
            in_flight.complete();
            #[cfg(all(test, unix))]
            supervisor.mark_completed(id);
        }
    }
}

fn take_next_due_command(supervisor: &CommandSupervisor) -> SupervisedCommand {
    let mut registry = supervisor
        .registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let Some(next) = registry.peek() else {
            registry = supervisor
                .wake
                .wait(registry)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        };
        let wait = next.next_action.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return registry.pop().expect("peeked command remains queued");
        }
        let waited = supervisor.wake.wait_timeout(registry, wait);
        registry = match waited {
            Ok((guard, _)) => guard,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
}

fn supervise_command_once(
    command: &mut SupervisedCommand,
    now: Instant,
    _supervisor: &CommandSupervisor,
) -> bool {
    if matches!(command.child.leader_terminal(), Ok(true)) {
        #[cfg(all(test, unix))]
        if _supervisor.fail_next_wait.swap(false, Ordering::Relaxed) {
            command.child.inject_wait_failure();
        }
        #[cfg(all(test, unix))]
        _supervisor.wait_attempts.fetch_add(1, Ordering::Relaxed);
        if command.child.seal_and_reap_terminal_group().is_ok() {
            return false;
        }
        command.next_action = now + Duration::from_millis(250);
        return true;
    }
    if command.termination_attempts_remaining > 0 {
        let _ = command.child.terminate_group();
        command.termination_attempts_remaining -= 1;
        command.next_action = now + command.retry_delay;
        command.retry_delay = command
            .retry_delay
            .saturating_mul(2)
            .min(Duration::from_millis(250));
    } else {
        command.next_action = now + Duration::from_millis(250);
    }
    true
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

fn drain_streams_after_reap<Out, Err, Wait>(
    stdout: (&mut Out, &mut CapturedStream, &mut bool),
    stderr: (&mut Err, &mut CapturedStream, &mut bool),
    stream_error: &mut Option<std::io::Error>,
    mut wait_for_activity: Wait,
) -> Result<(), CommandError>
where
    Out: Read,
    Err: Read,
    Wait: FnMut(&Out, &Err, bool, bool, Duration) -> Result<(), CommandError>,
{
    let (stdout, stdout_capture, stdout_done) = stdout;
    let (stderr, stderr_capture, stderr_done) = stderr;
    let deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
    loop {
        let stdout_progress =
            read_available_stream(stdout, stdout_capture, stdout_done, stream_error);
        let stderr_progress =
            read_available_stream(stderr, stderr_capture, stderr_done, stream_error);
        if (*stdout_done && *stderr_done) || stream_error.is_some() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        if !stdout_progress && !stderr_progress {
            wait_for_activity(
                stdout,
                stderr,
                *stdout_done,
                *stderr_done,
                remaining.min(MAX_EXIT_POLL_INTERVAL),
            )?;
        }
    }
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
        tv_sec: exit_poll_interval.as_secs() as _,
        tv_nsec: exit_poll_interval.subsec_nanos() as _,
    };
    poll_streams_once(|| rustix::event::poll(descriptors, Some(&timeout)))
}

#[cfg(unix)]
fn poll_streams_once<P>(poll: P) -> Result<(), CommandError>
where
    P: FnOnce() -> Result<usize, rustix::io::Errno>,
{
    match poll() {
        Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
        Err(source) => Err(CommandError::ReadOutput(std::io::Error::from(source))),
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
    thread::sleep(exit_poll_interval);
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
    SupervisorUnavailable(std::io::Error),
    SupervisorDegraded,
    SupervisorAtCapacity,
    WorkspaceChanged(std::io::Error),
    ConfigureOutput(std::io::Error),
    Kill(std::io::Error),
    Wait(std::io::Error),
    ReadOutput(std::io::Error),
    Cancelled,
    OutputIncomplete,
    TerminationUnverified,
    TerminationSupervised(String),
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    UnsupportedProcessTracking,
}

impl CommandError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Spawn(_) => "command_spawn_failed",
            Self::SupervisorUnavailable(_) => "command_supervisor_unavailable",
            Self::SupervisorDegraded => "command_supervisor_degraded",
            Self::SupervisorAtCapacity => "command_supervisor_pressure",
            Self::WorkspaceChanged(_) => "workspace_changed",
            Self::ConfigureOutput(_) | Self::Kill(_) | Self::Wait(_) | Self::ReadOutput(_) => {
                "command_io_error"
            }
            Self::Cancelled => "tool_cancelled",
            Self::OutputIncomplete => "command_output_incomplete",
            Self::TerminationUnverified | Self::TerminationSupervised(_) => {
                "command_termination_unverified"
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            Self::UnsupportedProcessTracking => "command_process_tracking_unsupported",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self,
            Self::SupervisorUnavailable(_) | Self::SupervisorAtCapacity
        ) || matches!(
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
            Self::SupervisorUnavailable(source) => write!(
                formatter,
                "command supervisor is unavailable; no command was started: {source}"
            ),
            Self::SupervisorDegraded => formatter.write_str(
                "command supervisor is degraded after repeated internal failures; no command was started",
            ),
            Self::SupervisorAtCapacity => formatter.write_str(
                "command supervision capacity is exhausted; no command was started",
            ),
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
            Self::TerminationSupervised(message) => formatter.write_str(message),
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            Self::UnsupportedProcessTracking => formatter.write_str(
                "stable command process-group tracking is not supported on this platform",
            ),
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::io::{Cursor, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use command_group::CommandGroup as _;

    use super::{
        cleanup_process_group, command_leader_terminal, command_supervisor,
        drain_streams_after_reap, next_exit_poll_interval, poll_streams_once,
        prepare_command_supervision, prepare_command_supervision_with, run_shell_command,
        shell_command, supervise_live_command, supervise_live_command_with, CapturedStream,
        CommandCleanupState, CommandError, CommandProcess, CommandSupervisor, DescendantToken,
        SupervisorAdmissionHealth, SupervisorWorkerState, COMMAND_GROUP_TERMINATION_ATTEMPTS,
        INJECT_GROUP_KILL_WRAPPER_FAILURE, INJECT_NEXT_FULL_GROUP_KILL_FAILURE,
        INJECT_NEXT_OUTPUT_CONFIGURATION_FAILURE, INJECT_PERSISTENT_GROUP_KILL_FAILURE,
        INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS, LAST_SUPERVISED_COMMAND_ID,
        LIVE_COMMAND_SUPERVISOR_HANDOFFS, MAX_COMMAND_PROCESSING_PANICS,
        MAX_COMMAND_SUPERVISION_SLOTS, MAX_EXIT_POLL_INTERVAL, MAX_STREAM_READS_PER_TICK,
    };
    use crate::workspace::CodingWorkspace;

    #[test]
    fn quiet_command_polling_backs_off_and_output_resets_it() {
        let mut interval = Duration::from_millis(1);
        for _ in 0..16 {
            interval = next_exit_poll_interval(interval, false);
        }
        assert_eq!(interval, MAX_EXIT_POLL_INTERVAL);
        assert_eq!(
            next_exit_poll_interval(interval, true),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn post_cleanup_drain_crosses_the_single_tick_budget_and_observes_eof() {
        let output_bytes = (MAX_STREAM_READS_PER_TICK + 1) * 8 * 1024;
        let mut stdout = Cursor::new(vec![b'x'; output_bytes]);
        let mut stderr = Cursor::new(Vec::<u8>::new());
        let mut stdout_capture = CapturedStream::new(output_bytes);
        let mut stderr_capture = CapturedStream::new(0);
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut stream_error = None;

        drain_streams_after_reap(
            (&mut stdout, &mut stdout_capture, &mut stdout_done),
            (&mut stderr, &mut stderr_capture, &mut stderr_done),
            &mut stream_error,
            |_, _, _, _, _| Ok(()),
        )
        .expect("finite post-cleanup output drains");

        assert!(stdout_done);
        assert!(stderr_done);
        assert!(stream_error.is_none());
        assert_eq!(stdout_capture.total_bytes, output_bytes as u64);
    }

    #[test]
    fn interrupted_stream_poll_returns_to_the_deadline_owner_without_retrying() {
        let mut calls = 0;

        poll_streams_once(|| {
            calls += 1;
            Err(rustix::io::Errno::INTR)
        })
        .expect("an interrupted poll yields to the outer deadline loop");

        assert_eq!(calls, 1);
    }

    #[test]
    fn descendant_token_observation_is_bounded_under_continuous_writes() {
        let (reader, mut writer) = UnixStream::pair().expect("token pair is created");
        reader
            .set_nonblocking(true)
            .expect("token reader is nonblocking");
        writer
            .set_nonblocking(true)
            .expect("token writer is nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let wrote = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        let writer_wrote = wrote.clone();
        let writer_thread = thread::spawn(move || {
            let payload = [b'x'; 4096];
            while !writer_stop.load(Ordering::Relaxed) {
                match writer.write(&payload) {
                    Ok(bytes) if bytes > 0 => writer_wrote.store(true, Ordering::Release),
                    Ok(_) => {}
                    Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now();
                    }
                    Err(source) => panic!("token writer failed: {source}"),
                }
            }
        });
        let write_deadline = Instant::now() + Duration::from_secs(1);
        while !wrote.load(Ordering::Acquire) {
            assert!(
                Instant::now() < write_deadline,
                "writer should make progress"
            );
            thread::yield_now();
        }
        let mut token = DescendantToken {
            reader,
            closed: false,
        };

        let started = Instant::now();
        let closed = token
            .wait_for_close(Duration::from_millis(5))
            .expect("token observation succeeds");

        assert!(!closed);
        assert!(started.elapsed() < Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        writer_thread.join().expect("token writer stops");
    }

    #[test]
    fn open_descendant_token_prevents_leader_reap_until_seal_is_verified() {
        let mut command = shell_command("exit 0");
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        let (reader, writer) = UnixStream::pair().expect("token pair is created");
        reader
            .set_nonblocking(true)
            .expect("token reader is nonblocking");
        let mut process = CommandProcess::tracked(
            child,
            DescendantToken {
                reader,
                closed: false,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process
            .leader_terminal()
            .expect("leader state is observable")
        {
            assert!(Instant::now() < deadline, "leader should become terminal");
            thread::yield_now();
        }

        let error = process
            .seal_and_reap_terminal_group()
            .expect_err("an open token must prevent reap");

        assert!(matches!(error, CommandError::TerminationUnverified));
        assert!(
            process
                .leader_terminal()
                .expect("unreaped leader remains observable"),
            "failed sealing must retain the leader identity"
        );
        drop(writer);
        let status = process
            .seal_and_reap_terminal_group()
            .expect("closed token permits seal and reap");
        assert!(status.success());
    }

    fn take_last_supervised_command_id() -> u64 {
        LAST_SUPERVISED_COMMAND_ID.with(|id| {
            id.replace(None)
                .expect("the command was handed to the supervisor")
        })
    }

    fn wait_for_supervisor_completion(id: u64) {
        assert!(
            command_supervisor().wait_for_completion(id, Duration::from_secs(2)),
            "supervisor did not seal and reap command {id}"
        );
    }

    fn wait_for_terminal_leader(child: &command_group::GroupChild) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match command_leader_terminal(child) {
                Ok(true) => return,
                Ok(false) if Instant::now() < deadline => thread::yield_now(),
                Ok(false) => panic!("command leader did not become terminal"),
                Err(source) => panic!("failed to inspect command leader: {source}"),
            }
        }
    }

    #[test]
    fn supervisor_worker_start_failure_can_be_retried_with_a_real_worker() {
        let supervisor = Arc::new(CommandSupervisor::new());
        supervisor
            .fail_next_worker_start
            .store(true, Ordering::Relaxed);
        let first = match prepare_command_supervision_with(&supervisor) {
            Err(error) => error,
            Ok(_) => panic!("first command preflight must fail closed"),
        };

        assert_eq!(first.code(), "command_supervisor_unavailable");
        assert!(first.retryable());
        let CommandError::SupervisorUnavailable(source) = first else {
            panic!("unexpected preflight error: {first:?}");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            *supervisor
                .worker_state
                .lock()
                .expect("worker state lock remains available"),
            SupervisorWorkerState::Stopped
        );

        let permit = prepare_command_supervision_with(&supervisor)
            .expect("a later preflight starts the real worker");
        let mut process = shell_command("exit 0");
        let child = process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        let id = supervisor.enqueue(CommandProcess::untracked(child), permit);

        assert!(supervisor.wait_for_completion(id, Duration::from_secs(2)));
        assert_eq!(supervisor.active_slots.load(Ordering::Acquire), 0);
    }

    #[test]
    fn supervisor_processing_panic_requeues_the_same_command() {
        let supervisor = Arc::new(CommandSupervisor::new());
        let permit = prepare_command_supervision_with(&supervisor).expect("worker starts");
        supervisor
            .processing_panics_to_inject
            .store(1, Ordering::Relaxed);
        let mut process = shell_command("exit 0");
        let child = process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        let id = supervisor.enqueue(CommandProcess::untracked(child), permit);

        assert!(supervisor.wait_for_completion(id, Duration::from_secs(2)));
        assert_eq!(
            supervisor
                .observed_processing_panics
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(supervisor.active_slots.load(Ordering::Acquire), 0);
    }

    #[test]
    fn repeated_processing_panics_back_off_degrade_and_do_not_starve_other_commands() {
        let supervisor = Arc::new(CommandSupervisor::new());
        let failing_permit = prepare_command_supervision_with(&supervisor).expect("worker starts");
        let healthy_permit = supervisor.reserve_slot().expect("second slot is available");
        supervisor.processing_panics_to_inject.store(
            usize::from(MAX_COMMAND_PROCESSING_PANICS),
            Ordering::Relaxed,
        );
        let mut failing_process = shell_command("exit 0");
        let failing_child = failing_process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("failing test command group starts");
        let mut healthy_process = shell_command("exit 0");
        let healthy_child = healthy_process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("healthy test command group starts");
        let first_deadline = Instant::now() + Duration::from_millis(20);
        let failing_id = supervisor.enqueue_at(
            CommandProcess::untracked(failing_child),
            failing_permit,
            first_deadline,
        );
        let healthy_id = supervisor.enqueue_at(
            CommandProcess::untracked(healthy_child),
            healthy_permit,
            first_deadline + Duration::from_millis(1),
        );
        supervisor
            .processing_panic_command_id
            .store(failing_id, Ordering::Relaxed);

        assert!(supervisor.wait_for_completion(healthy_id, Duration::from_secs(2)));
        assert!(supervisor.wait_for_completion(failing_id, Duration::from_secs(4)));
        assert_eq!(
            supervisor
                .observed_processing_panics
                .load(Ordering::Relaxed),
            usize::from(MAX_COMMAND_PROCESSING_PANICS)
        );
        assert_eq!(
            *supervisor
                .admission_health
                .lock()
                .expect("admission health remains available"),
            SupervisorAdmissionHealth::Degraded
        );
        let error = match prepare_command_supervision_with(&supervisor) {
            Err(error) => error,
            Ok(_) => panic!("degraded supervisor must reject new commands"),
        };
        assert_eq!(error.code(), "command_supervisor_degraded");
        assert!(!error.retryable());
        assert_eq!(supervisor.active_slots.load(Ordering::Acquire), 0);
    }

    #[test]
    fn degraded_admission_restarts_cleanup_worker_and_reports_live_handoff() {
        let supervisor = Arc::new(CommandSupervisor::new());
        *supervisor
            .admission_health
            .lock()
            .expect("admission health remains available") = SupervisorAdmissionHealth::Degraded;
        let permit = supervisor
            .reserve_slot()
            .expect("cleanup ownership is reserved");
        let mut process = shell_command("sleep 0.2");
        let child = process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");

        let error = supervise_live_command_with(
            &supervisor,
            CommandProcess::untracked(child),
            permit,
            CommandError::TerminationUnverified,
            None,
        );

        let CommandError::TerminationSupervised(message) = error else {
            panic!("live ownership must be supervised");
        };
        assert!(message.contains("supervisor retries termination"));
        assert!(!message.contains("worker is unavailable"));
        let id = take_last_supervised_command_id();
        assert!(supervisor.wait_for_completion(id, Duration::from_secs(2)));
        assert_eq!(
            *supervisor
                .worker_state
                .lock()
                .expect("worker state remains available"),
            SupervisorWorkerState::Running
        );
        let admission = match prepare_command_supervision_with(&supervisor) {
            Err(error) => error,
            Ok(_) => panic!("degraded admission rejects new commands after cleanup restart"),
        };
        assert_eq!(admission.code(), "command_supervisor_degraded");
        assert_eq!(supervisor.active_slots.load(Ordering::Acquire), 0);
    }

    #[test]
    fn supervisor_wait_failure_does_not_complete_before_reap() {
        let supervisor = Arc::new(CommandSupervisor::new());
        let permit = prepare_command_supervision_with(&supervisor).expect("worker starts");
        supervisor.fail_next_wait.store(true, Ordering::Relaxed);
        let mut process = shell_command("exit 0");
        let child = process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        let id = supervisor.enqueue(CommandProcess::untracked(child), permit);

        assert!(supervisor.wait_for_completion(id, Duration::from_secs(2)));
        assert!(supervisor.wait_attempts.load(Ordering::Relaxed) >= 2);
        assert_eq!(supervisor.active_slots.load(Ordering::Acquire), 0);
    }

    #[test]
    fn supervisor_capacity_fails_closed_and_recovers() {
        let supervisor = Arc::new(CommandSupervisor::new());
        let permits: Vec<_> = (0..MAX_COMMAND_SUPERVISION_SLOTS)
            .map(|_| supervisor.reserve_slot().expect("slot is available"))
            .collect();

        let error = match supervisor.reserve_slot() {
            Err(error) => error,
            Ok(_) => panic!("capacity must be enforced before command spawn"),
        };
        assert_eq!(error.code(), "command_supervisor_pressure");
        assert!(error.retryable());
        drop(permits);
        assert!(supervisor.reserve_slot().is_ok());
    }

    #[test]
    fn cancellation_permission_denied_reconciles_after_verified_cleanup() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-cancellation-reconcile-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancellation_trigger = cancellation.clone();
        let ready = root.join("cancellation-ready");
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "cancellation fixture did not become ready"
                );
                thread::sleep(Duration::from_millis(5));
            }
            cancellation_trigger.store(true, Ordering::Relaxed);
        });
        INJECT_NEXT_FULL_GROUP_KILL_FAILURE.with(|failure| failure.set(true));
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(0));

        let result = run_shell_command(
            &workspace,
            "sleep 10 >/dev/null 2>&1 & printf ready > cancellation-ready; wait",
            &cancellation,
            1024,
        );

        trigger.join().expect("cancellation trigger finishes");
        assert!(
            matches!(result, Err(CommandError::TerminationUnverified)),
            "verified cleanup must return cancellation semantics: {result:?}"
        );
        assert_eq!(
            LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.get()),
            0,
            "verified cleanup must not hand ownership to the supervisor"
        );

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

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
        let ready = root.join("control-failure-ready");
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "control failure fixture did not become ready"
                );
                thread::sleep(Duration::from_millis(5));
            }
            trigger_flag.store(true, Ordering::Relaxed);
        });
        COMMAND_GROUP_TERMINATION_ATTEMPTS.with(|attempts| attempts.set(0));
        INJECT_GROUP_KILL_WRAPPER_FAILURE.with(|failure| failure.set(true));
        let started = Instant::now();

        let result = run_shell_command(
            &workspace,
            "sleep 10 & printf ready > control-failure-ready; wait",
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
            2,
            "cleanup terminates once, then confirms descendant-token closure before reaping"
        );
        assert!(started.elapsed() < Duration::from_secs(2));

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
        let ready = root.join("supervisor-ready");
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "supervisor fixture did not become ready"
                );
                thread::sleep(Duration::from_millis(5));
            }
            trigger_flag.store(true, Ordering::Relaxed);
        });
        INJECT_PERSISTENT_GROUP_KILL_FAILURE.with(|failure| failure.set(true));
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(0));
        LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(None));

        let result = run_shell_command(
            &workspace,
            "sleep 10 & printf ready > supervisor-ready; wait",
            &cancellation,
            1024,
        );

        trigger.join().expect("cancellation trigger finishes");
        let Err(CommandError::TerminationSupervised(message)) = result else {
            panic!("double termination failure must be reported: {result:?}");
        };
        assert!(message.contains("process-wide supervisor retries termination"));
        assert_eq!(
            LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.get()),
            1
        );
        wait_for_supervisor_completion(take_last_supervised_command_id());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn partial_group_kill_success_hands_a_live_leader_to_the_supervisor() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-partial-termination-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let cancellation = Arc::new(AtomicBool::new(false));
        let trigger_flag = cancellation.clone();
        let ready = root.join("partial-termination-ready");
        let trigger = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "partial termination fixture did not become ready"
                );
                thread::sleep(Duration::from_millis(5));
            }
            trigger_flag.store(true, Ordering::Relaxed);
        });
        INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS.with(|failure| failure.set(true));
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(0));
        LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(None));

        let result = run_shell_command(
            &workspace,
            "sleep 10 & printf ready > partial-termination-ready; wait",
            &cancellation,
            1024,
        );

        trigger.join().expect("cancellation trigger finishes");
        assert!(
            matches!(result, Err(CommandError::TerminationSupervised(_))),
            "partial group termination must be handed off: {result:?}"
        );
        assert_eq!(
            LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.get()),
            1
        );
        wait_for_supervisor_completion(take_last_supervised_command_id());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn terminal_leader_partial_group_kill_success_never_reports_success() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-terminal-partial-termination-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS.with(|failure| failure.set(true));
        LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.set(0));
        LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(None));

        let result = run_shell_command(
            &workspace,
            "sleep 10 >/dev/null 2>&1 & exit 0",
            &AtomicBool::new(false),
            1024,
        );

        INJECT_PERSISTENT_PARTIAL_GROUP_KILL_SUCCESS.with(|failure| failure.set(false));
        assert!(
            matches!(result, Err(CommandError::TerminationSupervised(_))),
            "an unverified terminal group must be handed off: {result:?}"
        );
        assert_eq!(
            LIVE_COMMAND_SUPERVISOR_HANDOFFS.with(|handoffs| handoffs.get()),
            1
        );
        wait_for_supervisor_completion(take_last_supervised_command_id());

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn untracked_terminal_cleanup_preserves_the_initial_kill_error() {
        let mut process = shell_command("exit 0");
        let child = process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        wait_for_terminal_leader(&child);
        INJECT_NEXT_FULL_GROUP_KILL_FAILURE.with(|failure| failure.set(true));

        let permit = prepare_command_supervision().expect("supervision slot is available");
        let result = cleanup_process_group(
            CommandProcess::untracked(child),
            CommandCleanupState::TerminateAndReap,
            permit,
        );

        assert!(
            matches!(result, Err(CommandError::Kill(_))),
            "the injected first termination failure remains visible: {result:?}"
        );
    }

    #[test]
    fn tracked_terminal_cleanup_accepts_permission_denied_after_token_seal() {
        let mut process = shell_command("exit 0");
        process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (process, descendant_token) = DescendantToken::prepare(process).unwrap();
        let child = process.spawn_group().expect("tracked command group starts");
        wait_for_terminal_leader(&child);
        INJECT_NEXT_FULL_GROUP_KILL_FAILURE.with(|failure| failure.set(true));

        let permit = prepare_command_supervision().expect("supervision slot is available");
        let result = cleanup_process_group(
            CommandProcess::tracked(child, descendant_token),
            CommandCleanupState::TerminateAndReap,
            permit,
        );

        assert!(
            result.is_ok(),
            "a sealed tracking token proves terminal cleanup complete: {result:?}"
        );
    }

    #[test]
    fn repeated_early_exit_races_finish_sync_or_at_the_supervisor_barrier() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-repeated-early-exit-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");

        for attempt in 0..30 {
            LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(None));
            let result = run_shell_command(
                &workspace,
                "sleep 10 >/dev/null 2>&1 & exit 0",
                &AtomicBool::new(false),
                1024,
            );
            match result {
                Ok(outcome) => assert!(outcome.status.success()),
                Err(CommandError::TerminationSupervised(message)) => {
                    assert!(message.contains("process-wide supervisor retries termination"));
                    wait_for_supervisor_completion(take_last_supervised_command_id());
                }
                Err(error) => {
                    panic!("attempt {attempt} failed without retained ownership: {error}")
                }
            }
        }

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn supervisor_seals_a_residual_group_when_the_leader_is_already_terminal() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-command-terminal-supervisor-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let mut process = shell_command("sleep 10 >/dev/null 2>&1 & exit 0");
        let child = process
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .group_spawn()
            .expect("test command group starts");
        wait_for_terminal_leader(&child);
        LAST_SUPERVISED_COMMAND_ID.with(|id| id.set(None));

        let permit = prepare_command_supervision().expect("supervision slot is available");
        let result = supervise_live_command(
            CommandProcess::untracked(child),
            permit,
            CommandError::TerminationUnverified,
            None,
        );

        assert!(matches!(result, CommandError::TerminationSupervised(_)));
        wait_for_supervisor_completion(take_last_supervised_command_id());
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

        let result =
            run_shell_command(&workspace, "sleep 10 & wait", &AtomicBool::new(false), 1024);

        assert!(
            matches!(result, Err(CommandError::ConfigureOutput(_))),
            "unexpected result: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
