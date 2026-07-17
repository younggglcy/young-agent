use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, ExitStatus};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "young-agent-cli-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct CliCommand {
    command: Command,
}

impl CliCommand {
    fn new(prompt: &str, workspace: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_young-agent"));
        command
            .args(["--fake", "--prompt", prompt, "--workspace"])
            .arg(workspace);
        Self { command }
    }

    fn event_log(mut self, path: &Path) -> Self {
        self.command.arg("--event-log").arg(path);
        self
    }

    fn fake_script(mut self, path: &Path) -> Self {
        self.command.arg("--fake-script").arg(path);
        self
    }

    fn state_directory(mut self, path: &Path) -> Self {
        self.command.env("YOUNG_AGENT_STATE_DIR", path);
        self
    }

    fn on_signal(mut self, action: &str) -> Self {
        self.command.arg("--on-signal").arg(action);
        self
    }

    fn output(mut self) -> Output {
        self.command.output().expect("CLI should start")
    }

    fn spawn_with_pipes(mut self) -> Child {
        self.command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("CLI should start")
    }

    #[cfg(unix)]
    fn spawn_for_signal_test(mut self) -> Child {
        self.command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("CLI should start")
    }
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8")
}

#[cfg(unix)]
struct ChildGuard {
    child: Option<Child>,
    tool_process_group_file: PathBuf,
}

#[cfg(unix)]
impl ChildGuard {
    fn new(child: Child, tool_process_group_file: PathBuf) -> Self {
        Self {
            child: Some(child),
            tool_process_group_file,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child should still be running")
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child_mut()
                .try_wait()
                .expect("CLI status should be readable")
            {
                self.child = None;
                return status;
            }
            assert!(Instant::now() < deadline, "CLI did not stop in time");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    cleanup_process_group(&self.tool_process_group_file);
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
            cleanup_process_group(&self.tool_process_group_file);
        }
    }
}

#[cfg(unix)]
fn cleanup_process_group(path: &Path) {
    let Ok(process_group) = fs::read_to_string(path) else {
        return;
    };
    let Ok(process_group) = process_group.trim().parse::<u32>() else {
        return;
    };
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{process_group}")])
        .status();
}

#[test]
fn fake_provider_run_streams_events_and_reports_the_event_log() {
    let directory = TestDirectory::new("fake-run");
    let event_log = directory.path().join("run.jsonl");

    let output = CliCommand::new("Summarize the workspace", directory.path())
        .event_log(&event_log)
        .output();

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("[event-log]"), "stdout: {stdout}");
    assert!(
        stdout.contains(event_log.to_string_lossy().as_ref()),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("[model] Fake provider response for: Summarize the workspace"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("[status] completed"), "stdout: {stdout}");

    let log = fs::read_to_string(&event_log).expect("Event Log should exist");
    assert!(log.contains("\"type\":\"run_started\""), "log: {log}");
    assert!(log.contains("\"type\":\"model_output\""), "log: {log}");
    assert!(log.contains("\"type\":\"run_finished\""), "log: {log}");
}

#[test]
fn default_event_log_uses_state_storage_outside_the_workspace() {
    let directory = TestDirectory::new("default-log");
    let workspace = directory.path().join("workspace");
    let state_directory = directory.path().join("state");
    fs::create_dir(&workspace).expect("workspace should be created");

    let output = CliCommand::new("Inspect", &workspace)
        .state_directory(&state_directory)
        .output();

    assert!(output.status.success());
    let event_log = stdout(&output)
        .lines()
        .find_map(|line| line.strip_prefix("[event-log] "))
        .map(PathBuf::from)
        .expect("CLI should report the Event Log path");
    assert!(event_log.starts_with(state_directory.join("runs")));
    assert!(!event_log.starts_with(&workspace));
    assert!(event_log.is_file());
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&state_directory)
                .expect("state directory metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state_directory.join("runs"))
                .expect("runs directory metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&event_log)
                .expect("Event Log metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(not(windows))]
#[test]
fn default_event_log_honors_xdg_and_home_state_fallbacks() {
    for (name, variable, state_root, expected_root) in [
        (
            "xdg-state",
            "XDG_STATE_HOME",
            "xdg-state",
            PathBuf::from("xdg-state/young-agent"),
        ),
        (
            "home-state",
            "HOME",
            "home",
            PathBuf::from("home/.local/state/young-agent"),
        ),
    ] {
        let directory = TestDirectory::new(name);
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace should be created");
        let state_root = directory.path().join(state_root);

        let mut command = CliCommand::new("Inspect", &workspace);
        command
            .command
            .env_remove("YOUNG_AGENT_STATE_DIR")
            .env_remove("XDG_STATE_HOME")
            .env_remove("HOME")
            .env(variable, &state_root);
        let output = command.output();

        assert!(output.status.success(), "stderr: {}", stderr(&output));
        let event_log = stdout(&output)
            .lines()
            .find_map(|line| line.strip_prefix("[event-log] "))
            .map(PathBuf::from)
            .expect("CLI should report an Event Log");
        assert!(
            event_log.starts_with(directory.path().join(expected_root)),
            "unexpected Event Log path: {event_log:?}"
        );
        assert!(event_log.exists(), "Event Log should exist");
    }
}

#[cfg(unix)]
#[test]
fn default_state_directory_rejects_a_symlink() {
    let directory = TestDirectory::new("state-symlink");
    let workspace = directory.path().join("workspace");
    let state_target = directory.path().join("state-target");
    let state_link = directory.path().join("state-link");
    fs::create_dir(&workspace).expect("workspace should be created");
    fs::create_dir(&state_target).expect("state target should be created");
    std::os::unix::fs::symlink(&state_target, &state_link).expect("state link should be created");

    let output = CliCommand::new("Inspect", &workspace)
        .state_directory(&state_link)
        .output();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("refusing untrusted state directory"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(!state_target.join("runs").exists());
}

#[test]
fn explicit_event_log_path_must_be_new() {
    let directory = TestDirectory::new("existing-log");
    let event_log = directory.path().join("run.jsonl");
    fs::write(&event_log, "existing log\n").expect("existing log should be written");

    let output = CliCommand::new("Inspect", directory.path())
        .event_log(&event_log)
        .output();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("failed to reserve new Event Log"),
        "stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&event_log).expect("existing log should remain readable"),
        "existing log\n"
    );
}

#[test]
fn approval_prompt_grants_or_denies_the_exact_fake_provider_tool_call() {
    for (name, answer, expected_decision, should_create_file) in [
        ("approve", "y\n", "approved", true),
        ("deny", "n\n", "denied", false),
    ] {
        let directory = TestDirectory::new(name);
        let event_log = directory.path().join("run.jsonl");
        let fake_script = directory.path().join("fake-script.json");
        fs::write(
            &fake_script,
            r#"{
                "turns": [
                    [
                        {
                            "type": "tool_call",
                            "id": "model-command-001",
                            "name": "run_command",
                            "arguments": { "command": "touch approved.txt" }
                        },
                        { "type": "completed", "finish_reason": "tool_calls" }
                    ],
                    [
                        { "type": "text_delta", "delta": "Tool decision recorded." },
                        { "type": "completed", "finish_reason": "stop" }
                    ]
                ]
            }"#,
        )
        .expect("fake script should be written");

        let mut child = CliCommand::new("Create approved.txt", directory.path())
            .event_log(&event_log)
            .fake_script(&fake_script)
            .spawn_with_pipes();
        child
            .stdin
            .take()
            .expect("stdin should be piped")
            .write_all(answer.as_bytes())
            .ok();
        let output = child.wait_with_output().expect("CLI should finish");

        assert!(output.status.success(), "stderr: {}", stderr(&output));
        let stdout = stdout(&output);
        assert!(stdout.contains("[approval] requested"), "stdout: {stdout}");
        assert!(stdout.contains(expected_decision), "stdout: {stdout}");
        assert!(stdout.contains("[tool-result]"), "stdout: {stdout}");
        assert_eq!(
            directory.path().join("approved.txt").exists(),
            should_create_file,
            "stdout: {stdout}"
        );

        let log = fs::read_to_string(&event_log).expect("Event Log should exist");
        assert!(
            log.contains("\"type\":\"approval_requested\""),
            "log: {log}"
        );
        assert!(log.contains("\"type\":\"approval_resolved\""), "log: {log}");
    }
}

#[test]
fn untrusted_model_control_sequences_cannot_change_the_approval_display() {
    let directory = TestDirectory::new("terminal-controls");
    let event_log = directory.path().join("run.jsonl");
    let fake_script = directory.path().join("fake-script.json");
    fs::write(
        &fake_script,
        r#"{
            "turns": [
                [
                    { "type": "text_delta", "delta": "\u001b[8mconcealed" },
                    {
                        "type": "tool_call",
                        "id": "model-command-001",
                        "name": "run_command",
                        "arguments": { "command": "touch must-not-exist.txt" }
                    },
                    { "type": "completed", "finish_reason": "tool_calls" }
                ],
                [
                    { "type": "text_delta", "delta": "Denied." },
                    { "type": "completed", "finish_reason": "stop" }
                ]
            ]
        }"#,
    )
    .expect("fake script should be written");

    let mut child = CliCommand::new("Do not conceal", directory.path())
        .event_log(&event_log)
        .fake_script(&fake_script)
        .spawn_with_pipes();
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"n\n")
        .expect("denial should be sent");
    let output = child.wait_with_output().expect("CLI should finish");

    assert!(output.status.success());
    let stdout = stdout(&output);
    assert!(!stdout.contains('\u{001b}'), "stdout: {stdout:?}");
    assert!(
        stdout.contains(r"\u{001b}[8mconcealed"),
        "stdout: {stdout:?}"
    );
    assert!(stdout.contains("[approval-prompt]"), "stdout: {stdout:?}");
    assert!(!directory.path().join("must-not-exist.txt").exists());
}

#[test]
fn fake_provider_failure_is_visible_and_returns_a_failed_status() {
    let directory = TestDirectory::new("failed-run");
    let event_log = directory.path().join("run.jsonl");
    let fake_script = directory.path().join("fake-script.json");
    fs::write(
        &fake_script,
        r#"{
            "turns": [[{
                "type": "failed",
                "error": {
                    "code": "fake_failure",
                    "message": "scripted provider failure",
                    "retryable": false
                }
            }]]
        }"#,
    )
    .expect("fake script should be written");

    let output = CliCommand::new("Fail", directory.path())
        .event_log(&event_log)
        .fake_script(&fake_script)
        .output();

    assert_eq!(output.status.code(), Some(2));
    let stdout = stdout(&output);
    assert!(
        stdout.contains("[model-error] fake_failure: scripted provider failure"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("[error] fake_failure: scripted provider failure"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("[status] failed fake_failure: scripted provider failure"),
        "stdout: {stdout}"
    );
}

#[test]
fn fake_provider_script_rejects_resource_limits_before_starting_a_run() {
    let directory = TestDirectory::new("oversized-script");
    let event_log = directory.path().join("run.jsonl");
    let fake_script = directory.path().join("fake-script.json");
    let turns = (0..129).map(|_| "[]").collect::<Vec<_>>().join(",");
    fs::write(&fake_script, format!(r#"{{"turns":[{turns}]}}"#))
        .expect("fake script should be written");

    let output = CliCommand::new("Too many turns", directory.path())
        .event_log(&event_log)
        .fake_script(&fake_script)
        .output();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("exceeds 128 turns"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !event_log.exists(),
        "invalid script must not reserve a run log"
    );
}

#[cfg(unix)]
#[test]
fn process_signal_produces_distinct_interrupted_or_cancelled_terminal_status() {
    for (name, signal_action, expected_code, expected_status) in [
        ("interrupt", "interrupt", 130, "interrupted"),
        ("cancel", "cancel", 125, "cancelled"),
    ] {
        let directory = TestDirectory::new(name);
        let event_log = directory.path().join("run.jsonl");
        let fake_script = directory.path().join("fake-script.json");
        fs::write(
            &fake_script,
            r#"{
                "turns": [
                    [
                        {
                            "type": "tool_call",
                            "id": "model-command-001",
                            "name": "run_command",
                            "arguments": { "command": "echo $$ > tool-pgid && touch tool-started && sleep 30" }
                        },
                        { "type": "completed", "finish_reason": "tool_calls" }
                    ],
                    [
                        { "type": "text_delta", "delta": "Sleep completed." },
                        { "type": "completed", "finish_reason": "stop" }
                    ]
                ]
            }"#,
        )
        .expect("fake script should be written");

        let child = CliCommand::new("Sleep", directory.path())
            .event_log(&event_log)
            .fake_script(&fake_script)
            .on_signal(signal_action)
            .spawn_for_signal_test();
        let mut child = ChildGuard::new(child, directory.path().join("tool-pgid"));
        child
            .child_mut()
            .stdin
            .take()
            .expect("stdin should be piped")
            .write_all(b"y\n")
            .expect("approval should be sent");

        wait_for_path(&directory.path().join("tool-started"));
        let signal_status = Command::new("kill")
            .args(["-INT", &child.child_mut().id().to_string()])
            .status()
            .expect("SIGINT should be sent");
        assert!(signal_status.success());

        let status = child.wait_for_exit(Duration::from_secs(10));
        assert_eq!(status.code(), Some(expected_code));

        let log = fs::read_to_string(&event_log).expect("Event Log should exist");
        assert!(
            log.contains(&format!("\"status\":\"{expected_status}\"")),
            "log: {log}"
        );
    }
}

#[cfg(unix)]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if path.exists() {
            return;
        }
        assert!(Instant::now() < deadline, "path never appeared: {path:?}");
        thread::sleep(Duration::from_millis(10));
    }
}
