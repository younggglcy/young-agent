#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use common::TestDirectory;
use serde_json::{json, Value};
use young_agent_runtime::{
    AgentRuntime, ApprovalDecision, ApprovalRequest, RunControl, RunControlFlow, RunId, RunRequest,
    TerminalRunStatus,
};
use young_capability_coding::{
    register_builtin_coding_capability, CodingWorkspace, CodingWorkspaceError,
};
use young_event_store::JsonlEventStore;
use young_model_runtime::{
    FakeModelClient, ModelMessage, ModelStreamEvent, ModelToolCallId, ScriptedModelTurn,
};
use young_tool_runtime::{ToolContent, ToolOutput, ToolRuntime};

struct DenyingControl;
struct ApprovingControl;

impl RunControl for DenyingControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(
        &mut self,
        _request: &ApprovalRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        ApprovalDecision::Deny {
            reason: "user denied workspace mutation".to_string(),
        }
    }
}

impl RunControl for ApprovingControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(
        &mut self,
        _request: &ApprovalRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}

fn run_request(run_id: &str) -> RunRequest {
    RunRequest {
        run_id: RunId::new(run_id),
        model: "fake-model".to_string(),
        messages: vec![ModelMessage::user("Create marker.txt")],
        tools: Vec::new(),
        metadata: BTreeMap::new(),
    }
}

fn isolated_git_command() -> Command {
    let inherited_path = std::env::var_os("PATH");
    let mut command = Command::new("git");
    command.env_clear().env("LC_ALL", "C");
    if let Some(path) = inherited_path {
        command.env("PATH", path);
    }
    command
}

fn initialize_git_repository(root: &Path, branch: &str) {
    std::fs::create_dir_all(root).expect("git fixture directory is created");
    let init = isolated_git_command()
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git init starts");
    assert!(init.success(), "git init succeeds");
    let symbolic_ref = isolated_git_command()
        .args(["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])
        .current_dir(root)
        .status()
        .expect("git symbolic-ref starts");
    assert!(symbolic_ref.success(), "git symbolic-ref succeeds");
}

fn run_unapproved_command(root: &Path, run_id: &str, command: &str) -> JsonlEventStore {
    let workspace = CodingWorkspace::resolve(root).expect("workspace resolves");
    run_unapproved_command_with_workspace(root, workspace, run_id, command)
}

fn run_unapproved_command_with_workspace(
    root: &Path,
    workspace: CodingWorkspace,
    run_id: &str,
    command: &str,
) -> JsonlEventStore {
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": command }),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Validation completed.".to_string(),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
    ]);
    let store = JsonlEventStore::new(root.join(format!("{run_id}.jsonl")));
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run(run_request(run_id))
        .expect("automatically allowed command completes");

    store
}

fn run_denied_command(root: &Path, run_id: &str, command: &str) -> JsonlEventStore {
    let workspace = CodingWorkspace::resolve(root).expect("workspace resolves");
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": command }),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Command was denied.".to_string(),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
    ]);
    let store = JsonlEventStore::new(root.join("run.jsonl"));
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run_with_control(run_request(run_id), &mut DenyingControl)
        .expect("denied command should remain a replayable run");

    store
}

fn run_rejected_call(directory: &TestDirectory, run_id: &str, arguments: Value) -> JsonlEventStore {
    let workspace = CodingWorkspace::resolve(directory.path()).expect("workspace resolves");
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments,
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Command was rejected.".to_string(),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
    ]);
    let store = JsonlEventStore::new(directory.path().join("run.jsonl"));
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run(run_request(run_id))
        .expect("rejected command should remain a replayable run");

    store
}

fn assert_replayed_rejection(store: &JsonlEventStore, case: &str, expected_reason: &str) {
    let replay = store.replay().expect("Event Log replays");
    assert!(matches!(
        replay.terminal_status(),
        Some(TerminalRunStatus::Completed { final_message })
            if final_message == "Command was rejected."
    ));
    assert_eq!(replay.approvals().len(), 0, "case: {case}");
    let tool_call = replay.tool_calls().next().expect("tool call replays");
    assert!(tool_call.approval().is_none(), "case: {case}");
    assert!(tool_call.approval_decision().is_none(), "case: {case}");
    let result = tool_call.result().expect("rejected tool result replays");
    let ToolOutput::Failure { error, .. } = &result.output else {
        panic!("rejected call must produce a failure result: {case}");
    };
    assert_eq!(error.code, "tool_rejected", "case: {case}");
    assert!(
        error.message.contains(expected_reason),
        "unexpected rejection for {case}: {}",
        error.message,
    );
}

#[test]
fn denied_mutating_command_is_not_executed_and_replays_its_decision() {
    let directory = TestDirectory::new("denied");
    let store = run_denied_command(directory.path(), "run-command-denied", "touch marker.txt");

    assert!(!directory.path().join("marker.txt").exists());
    let replay = store.replay().expect("Event Log replays");
    let tool_call = replay.tool_calls().next().expect("tool call replays");
    let approval = tool_call.approval().expect("approval request replays");
    assert!(approval.reason.contains("mutate workspace files"));
    assert_eq!(
        approval.call.arguments,
        json!({ "command": "touch marker.txt" })
    );
    assert!(matches!(
        tool_call.approval_decision(),
        Some(ApprovalDecision::Deny { reason }) if reason == "user denied workspace mutation"
    ));
    let result = tool_call.result().expect("denied tool result replays");
    let ToolOutput::Failure { error, .. } = &result.output else {
        panic!("denied command must produce a failure result");
    };
    assert_eq!(error.code, "approval_denied");
    assert_eq!(error.message, "user denied workspace mutation");
}

#[test]
fn approved_mutating_command_executes_and_replays_its_decision() {
    let directory = TestDirectory::new("approved");
    let workspace = CodingWorkspace::resolve(directory.path()).expect("workspace resolves");
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": "touch marker.txt" }),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Mutation completed.".to_string(),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
    ]);
    let store = JsonlEventStore::new(directory.path().join("run.jsonl"));
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run_with_control(run_request("run-command-approved"), &mut ApprovingControl)
        .expect("approved command should complete");

    assert!(directory.path().join("marker.txt").exists());
    let replay = store.replay().expect("Event Log replays");
    let tool_call = replay.tool_calls().next().expect("tool call replays");
    assert!(matches!(
        tool_call.approval_decision(),
        Some(ApprovalDecision::Approve)
    ));
    let result = tool_call.result().expect("approved tool result replays");
    assert!(matches!(result.output, ToolOutput::Success { .. }));
}

#[test]
fn low_risk_command_executes_without_approval_events() {
    let directory = TestDirectory::new("low-risk");
    let workspace = CodingWorkspace::resolve(directory.path()).expect("workspace resolves");
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": "pwd" }),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Validation completed.".to_string(),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
    ]);
    let store = JsonlEventStore::new(directory.path().join("run.jsonl"));
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run(run_request("run-command-low-risk"))
        .expect("low-risk command should complete without a control handler");

    let replay = store.replay().expect("Event Log replays");
    assert_eq!(replay.approvals().len(), 0);
    let tool_call = replay.tool_calls().next().expect("tool call replays");
    assert!(tool_call.approval().is_none());
    assert!(tool_call.approval_decision().is_none());
    let result = tool_call.result().expect("tool result replays");
    assert!(matches!(result.output, ToolOutput::Success { .. }));
}

#[test]
fn sanitized_inherited_path_cannot_execute_a_workspace_program() {
    const CHILD_ROOT: &str = "YOUNG_AGENT_COMMAND_PATH_CHILD_ROOT";
    const CHILD_MODE: &str = "YOUNG_AGENT_COMMAND_PATH_CHILD_MODE";
    const TEST_NAME: &str = "sanitized_inherited_path_cannot_execute_a_workspace_program";

    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let mode = std::env::var(CHILD_MODE).expect("child mode is provided");
        let error = match CodingWorkspace::resolve(&root) {
            Ok(_) => panic!("workspace selection must fail without a trusted Git executable"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, CodingWorkspaceError::StartGitProbe(_)),
            "unexpected workspace error for {mode}: {error}"
        );
        assert!(!root.join("marker.txt").exists(), "path mode: {mode}");
        assert!(!root.join("git-probe-marker").exists(), "path mode: {mode}");
        return;
    }

    for mode in ["missing", "workspace-only"] {
        let directory = TestDirectory::new(&format!("path-{mode}"));
        let fake_rg = directory.path().join("rg");
        std::fs::write(&fake_rg, "#!/bin/sh\n: > marker.txt\n")
            .expect("fake rg executable is written");
        let fake_git = directory.path().join("git");
        std::fs::write(
            &fake_git,
            "#!/bin/sh\n: > git-probe-marker\necho 'fatal: not a git repository' >&2\nexit 128\n",
        )
        .expect("fake git executable is written");
        for executable in [&fake_rg, &fake_git] {
            let mut permissions = std::fs::metadata(executable)
                .expect("fake executable metadata is available")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(executable, permissions).expect("fake command is executable");
        }

        let mut child = Command::new(std::env::current_exe().expect("test binary path resolves"));
        child
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ROOT, directory.path())
            .env(CHILD_MODE, mode);
        if mode == "workspace-only" {
            child.env("PATH", directory.path());
        } else {
            child.env_remove("PATH");
        }
        let output = child.output().expect("isolated PATH test child runs");

        assert!(
            output.status.success(),
            "path mode {mode} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(!directory.path().join("marker.txt").exists());
        assert!(!directory.path().join("git-probe-marker").exists());
    }
}

#[test]
fn inherited_git_location_environment_cannot_redirect_allowed_commands() {
    const CHILD_ROOT: &str = "YOUNG_AGENT_GIT_LOCATION_CHILD_ROOT";
    const TEST_NAME: &str = "inherited_git_location_environment_cannot_redirect_allowed_commands";

    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let store = run_unapproved_command(
            &root,
            "run-command-git-location",
            "git branch --show-current; git rev-parse --show-toplevel",
        );

        let replay = store.replay().expect("Event Log replays");
        assert_eq!(replay.approvals().len(), 0);
        let tool_call = replay.tool_calls().next().expect("tool call replays");
        assert!(tool_call.approval().is_none());
        assert!(tool_call.approval_decision().is_none());
        let ToolOutput::Success { content, .. } =
            &tool_call.result().expect("tool result replays").output
        else {
            panic!("allowed Git command should produce a tool success");
        };
        let stdout = content.iter().find_map(|content| match content {
            ToolContent::Json { value } => value.get("stdout").and_then(Value::as_str),
            _ => None,
        });
        let canonical_root = root.canonicalize().expect("workspace path canonicalizes");
        let expected_stdout = format!("workspace-branch\n{}\n", canonical_root.display());
        assert_eq!(stdout, Some(expected_stdout.as_str()));
        return;
    }

    let container = TestDirectory::new("git-location-environment");
    let root = container.path().join("workspace");
    let outside = container.path().join("outside");
    let trace_marker = container.path().join("git-trace-marker");
    let trace2_marker = container.path().join("git-trace2-marker");
    initialize_git_repository(&root, "workspace-branch");
    initialize_git_repository(&outside, "outside-branch");
    let output = Command::new(std::env::current_exe().expect("test binary path resolves"))
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ROOT, &root)
        .env("GIT_DIR", outside.join(".git"))
        .env("GIT_WORK_TREE", &outside)
        .env("GIT_TRACE", &trace_marker)
        .env("GIT_TRACE2_EVENT", &trace2_marker)
        .output()
        .expect("isolated Git environment test child runs");

    assert!(
        output.status.success(),
        "Git environment child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!trace_marker.exists());
    assert!(!trace2_marker.exists());
}

#[test]
fn denied_file_uncompress_cannot_execute_a_workspace_helper_from_path() {
    const CHILD_ROOT: &str = "YOUNG_AGENT_FILE_HELPER_CHILD_ROOT";
    const TEST_NAME: &str = "denied_file_uncompress_cannot_execute_a_workspace_helper_from_path";

    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let store = run_denied_command(&root, "run-command-file-uncompress", "file -z archive.zst");

        assert!(!root.join("file-helper-marker").exists());
        let replay = store.replay().expect("Event Log replays");
        let tool_call = replay.tool_calls().next().expect("tool call replays");
        let approval = tool_call.approval().expect("approval request replays");
        assert!(approval.reason.contains("compressed files"));
        assert!(matches!(
            tool_call.approval_decision(),
            Some(ApprovalDecision::Deny { .. })
        ));
        return;
    }

    let container = TestDirectory::new("file-helper-path");
    let root = container.path().join("workspace");
    let external_bin = container.path().join("external-bin");
    std::fs::create_dir(&root).expect("workspace fixture is created");
    std::fs::create_dir(&external_bin).expect("external bin fixture is created");
    std::fs::write(
        root.join("archive.zst"),
        [0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0],
    )
    .expect("compressed fixture is written");
    let helper = root.join("zstd-helper");
    std::fs::write(&helper, "#!/bin/sh\n: > file-helper-marker\n")
        .expect("decompression helper is written");
    let mut permissions = std::fs::metadata(&helper)
        .expect("decompression helper metadata is available")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&helper, permissions).expect("decompression helper is executable");
    std::os::unix::fs::symlink(&helper, external_bin.join("zstd"))
        .expect("external helper symlink is created");
    let mut path_entries = vec![external_bin.clone()];
    if let Some(path) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&path));
    } else {
        path_entries.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    }
    let inherited_path = std::env::join_paths(path_entries).expect("fixture PATH joins");
    let output = Command::new(std::env::current_exe().expect("test binary path resolves"))
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ROOT, &root)
        .env("PATH", inherited_path)
        .output()
        .expect("isolated file helper test child runs");

    assert!(
        output.status.success(),
        "file helper child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!root.join("file-helper-marker").exists());
}

#[test]
fn denied_shell_variable_mutation_cannot_redirect_a_later_command() {
    let directory = TestDirectory::new("shell-variable-denied");
    let fake_git = directory.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\n: > marker.txt\n")
        .expect("fake git executable is written");
    let mut permissions = std::fs::metadata(&fake_git)
        .expect("fake git metadata is available")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_git, permissions).expect("fake git is executable");

    let store = run_denied_command(
        directory.path(),
        "run-command-shell-variable-denied",
        "printf -v PATH .; git branch --show-current",
    );

    assert!(!directory.path().join("marker.txt").exists());
    let replay = store.replay().expect("Event Log replays");
    let tool_call = replay.tool_calls().next().expect("tool call replays");
    let approval = tool_call.approval().expect("approval request replays");
    assert!(approval.reason.contains("shell variable"));
    assert!(matches!(
        tool_call.approval_decision(),
        Some(ApprovalDecision::Deny { .. })
    ));
}

#[test]
fn denied_git_index_commands_cannot_execute_a_configured_fsmonitor_helper() {
    for (case, command) in [("ls-files", "git ls-files"), ("grep", "git grep needle")] {
        let directory = TestDirectory::new(&format!("git-fsmonitor-{case}"));
        let init = isolated_git_command()
            .args(["init", "--quiet"])
            .current_dir(directory.path())
            .status()
            .expect("git init starts");
        assert!(init.success(), "git init succeeds");
        let helper = directory.path().join("fsmonitor-helper");
        std::fs::write(&helper, "#!/bin/sh\n: > fsmonitor-marker\n")
            .expect("fsmonitor helper is written");
        let mut permissions = std::fs::metadata(&helper)
            .expect("fsmonitor helper metadata is available")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("fsmonitor helper is executable");
        let config = isolated_git_command()
            .args(["config", "core.fsmonitor"])
            .arg(&helper)
            .current_dir(directory.path())
            .status()
            .expect("git config starts");
        assert!(config.success(), "git config succeeds");

        let store = run_denied_command(
            directory.path(),
            &format!("run-command-git-fsmonitor-{case}"),
            command,
        );

        assert!(!directory.path().join("fsmonitor-marker").exists());
        let replay = store.replay().expect("Event Log replays");
        let tool_call = replay.tool_calls().next().expect("tool call replays");
        let approval = tool_call.approval().expect("approval request replays");
        assert!(approval.reason.contains("fsmonitor"));
        assert_eq!(approval.call.arguments, json!({ "command": command }));
        assert!(matches!(
            tool_call.approval_decision(),
            Some(ApprovalDecision::Deny { .. })
        ));
    }
}

#[test]
fn rejected_command_is_not_executed_and_replays_without_approval_events() {
    let directory = TestDirectory::new("rejected");
    let fake_sudo = directory.path().join("sudo");
    std::fs::write(&fake_sudo, "#!/bin/sh\n: > marker.txt\n")
        .expect("fake sudo executable is written");
    let mut permissions = std::fs::metadata(&fake_sudo)
        .expect("fake sudo metadata is available")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_sudo, permissions).expect("fake sudo is executable");

    let store = run_rejected_call(
        &directory,
        "run-command-rejected",
        json!({ "command": "./sudo" }),
    );

    assert!(!directory.path().join("marker.txt").exists());
    assert_replayed_rejection(&store, "privilege-elevation", "privilege elevation");
}

#[test]
fn malformed_run_command_arguments_replay_as_rejections_without_approval_events() {
    for (case, arguments, expected_reason) in [
        (
            "arguments-not-object",
            json!("pwd"),
            "arguments must be an object",
        ),
        (
            "command-missing",
            json!({}),
            "requires a string 'command' argument",
        ),
        (
            "command-not-string",
            json!({ "command": 7 }),
            "requires a string 'command' argument",
        ),
        (
            "unknown-argument",
            json!({ "command": "pwd", "unexpected": true }),
            "does not accept unknown arguments",
        ),
    ] {
        let directory = TestDirectory::new(case);
        let store = run_rejected_call(&directory, case, arguments);
        assert_replayed_rejection(&store, case, expected_reason);
    }
}

#[test]
fn oversized_run_command_replays_as_rejection_without_approval_events() {
    let directory = TestDirectory::new("oversized-command");
    let store = run_rejected_call(
        &directory,
        "oversized-command",
        json!({ "command": "x".repeat(64 * 1024 + 1) }),
    );

    assert_replayed_rejection(&store, "oversized-command", "65536 bytes policy limit");
}

#[test]
fn malformed_shell_syntax_replays_as_rejection_without_approval_events() {
    for (case, command) in [
        ("leading-semicolon", "; pwd"),
        ("consecutive-semicolons", "pwd;;pwd"),
    ] {
        let directory = TestDirectory::new(case);
        let store = run_rejected_call(&directory, case, json!({ "command": command }));
        assert_replayed_rejection(&store, case, "malformed shell syntax");
    }
}
