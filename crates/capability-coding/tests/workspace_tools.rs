#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use young_capability_coding::{register_builtin_coding_capability, CodingWorkspace};
use young_tool_runtime::{
    ToolCall, ToolCallId, ToolContent, ToolExecutionAuthorization, ToolOutput, ToolRuntime,
};

struct TestWorkspace {
    root: PathBuf,
}

impl TestWorkspace {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-capability-coding-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("test workspace is created");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn git(&self, arguments: &[&str]) {
        run_git(&self.root, arguments);
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_git(path: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .status()
        .expect("git starts");
    assert!(status.success(), "git command failed: {arguments:?}");
}

fn dispatch_approved_patch(
    runtime: &mut ToolRuntime,
    id: &str,
    patch: serde_json::Value,
    cancelled: bool,
) -> ToolOutput {
    let call = ToolCall {
        id: ToolCallId::new(id),
        tool_name: "apply_patch".to_string(),
        arguments: patch,
    };
    runtime
        .dispatch(
            call.clone(),
            ToolExecutionAuthorization::ApprovalGranted {
                call_id: call.id.clone(),
            },
            Arc::new(AtomicBool::new(cancelled)),
        )
        .output
}

#[test]
fn workspace_resolves_the_selected_root_and_records_git_context() {
    let test_workspace = TestWorkspace::new("git-context");
    test_workspace.git(&["init", "--quiet"]);
    let nested = test_workspace.path().join("nested");
    std::fs::create_dir(&nested).expect("nested directory is created");

    let workspace = CodingWorkspace::resolve(&nested).expect("workspace resolves");
    let context = workspace.context();
    let git = context.git_worktree().expect("git worktree is detected");

    assert_eq!(context.root(), nested.canonicalize().unwrap());
    assert_eq!(
        git.worktree_root(),
        test_workspace.path().canonicalize().unwrap()
    );
    assert_eq!(
        git.git_dir(),
        test_workspace.path().join(".git").canonicalize().unwrap()
    );
    assert_eq!(git.common_dir(), git.git_dir());
    assert!(!git.is_linked_worktree());
}

#[test]
fn workspace_distinguishes_a_linked_git_worktree_from_the_common_git_dir() {
    let container = TestWorkspace::new("linked-worktree");
    let main = container.path().join("main");
    let linked = container.path().join("linked");
    std::fs::create_dir(&main).expect("main worktree directory is created");
    run_git(&main, &["init", "--quiet"]);
    std::fs::write(main.join("README.md"), "initial\n").expect("fixture is written");
    run_git(&main, &["add", "README.md"]);
    run_git(
        &main,
        &[
            "-c",
            "user.name=young-agent tests",
            "-c",
            "user.email=tests@example.com",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    let linked_arg = linked.display().to_string();
    run_git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &linked_arg,
            "HEAD",
        ],
    );
    let nested = linked.join("nested");
    std::fs::create_dir(&nested).expect("nested directory is created");

    let workspace = CodingWorkspace::resolve(&nested).expect("workspace resolves");
    let git = workspace
        .context()
        .git_worktree()
        .expect("linked worktree is detected");

    assert_eq!(git.worktree_root(), linked.canonicalize().unwrap());
    assert_eq!(git.common_dir(), main.join(".git").canonicalize().unwrap());
    assert_ne!(git.git_dir(), git.common_dir());
    assert!(git.is_linked_worktree());
}

#[test]
fn read_file_runs_through_the_tool_runtime_and_exposes_workspace_metadata() {
    let test_workspace = TestWorkspace::new("read-file");
    std::fs::write(
        test_workspace.path().join("README.md"),
        "# test workspace\n",
    )
    .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let expected_root = workspace.context().root().display().to_string();
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-read"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content,
        metadata,
        extensions,
    } = result.output
    else {
        panic!("read_file should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Text {
            text: "# test workspace\n".to_string(),
        }]
    );
    assert_eq!(metadata["path"], json!("README.md"));
    assert_eq!(metadata["bytes"], json!(17));
    assert_eq!(metadata["truncated"], json!(false));
    assert_eq!(metadata["workspace"]["root"], json!(expected_root));
    assert!(metadata["workspace"]["git_worktree"].is_null());
    assert!(extensions.is_empty());

    let cancelled = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-read-cancelled"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(true)),
    );
    let ToolOutput::Failure { error, .. } = cancelled.output else {
        panic!("cancelled read must fail before opening the file");
    };
    assert_eq!(error.code, "tool_cancelled");
}

#[test]
fn read_file_metadata_exposes_the_detected_git_worktree_context() {
    let test_workspace = TestWorkspace::new("read-git-metadata");
    test_workspace.git(&["init", "--quiet"]);
    std::fs::write(test_workspace.path().join("README.md"), "workspace\n")
        .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let expected_root = workspace.context().root().display().to_string();
    let expected_git_dir = test_workspace
        .path()
        .join(".git")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-read-git-metadata"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("read_file should succeed");
    };
    assert_eq!(
        metadata["workspace"]["git_worktree"],
        json!({
            "worktree_root": expected_root,
            "git_dir": expected_git_dir,
            "common_dir": expected_git_dir,
            "linked": false,
        })
    );
}

#[test]
fn read_file_rejects_path_traversal_outside_the_selected_workspace() {
    let parent = TestWorkspace::new("read-traversal");
    let selected = parent.path().join("selected");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    std::fs::write(parent.path().join("secret.txt"), "outside\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-traversal"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "../secret.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, extensions } = result.output else {
        panic!("path traversal must fail");
    };
    assert_eq!(error.code, "outside_workspace");
    assert!(!error.retryable);
    assert!(extensions.is_empty());
}

#[test]
fn read_file_rejects_an_oversized_path_before_resolving_it() {
    let test_workspace = TestWorkspace::new("read-oversized-path");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-read-oversized-path"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "x".repeat(8 * 1024 + 1) }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("oversized read path must fail");
    };
    assert_eq!(error.code, "invalid_arguments");
    assert_eq!(error.message, "argument 'path' exceeds 8192 bytes");
    assert!(!error.retryable);
}

#[cfg(unix)]
#[test]
fn read_file_rejects_a_symlink_that_escapes_the_selected_workspace() {
    use std::os::unix::fs::symlink;

    let parent = TestWorkspace::new("read-symlink");
    let selected = parent.path().join("selected");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    let secret = parent.path().join("secret.txt");
    std::fs::write(&secret, "outside\n").expect("fixture is written");
    symlink(&secret, selected.join("escape.txt")).expect("escape symlink is created");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-symlink"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "escape.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("symlink escape must fail");
    };
    assert_eq!(error.code, "outside_workspace");
}

#[cfg(unix)]
#[test]
fn file_tools_remain_bound_to_the_open_workspace_when_its_path_is_replaced() {
    use std::os::unix::fs::symlink;

    let container = TestWorkspace::new("workspace-handle");
    let selected = container.path().join("selected");
    let moved = container.path().join("moved");
    let outside = container.path().join("outside");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    std::fs::create_dir(&outside).expect("outside directory is created");
    std::fs::write(selected.join("notes.txt"), "inside\n").expect("inside fixture is written");
    std::fs::write(outside.join("notes.txt"), "outside\n").expect("outside fixture is written");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    std::fs::rename(&selected, &moved).expect("selected workspace path is moved");
    symlink(&outside, &selected).expect("ambient workspace path is replaced");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let read = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-stable-root-read"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "notes.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );
    let ToolOutput::Success { content, .. } = read.output else {
        panic!("read should use the opened workspace handle");
    };
    assert_eq!(
        content,
        vec![ToolContent::Text {
            text: "inside\n".to_string(),
        }]
    );

    let patch_call = ToolCall {
        id: ToolCallId::new("call-stable-root-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-inside\n+updated\n"
        }),
    };
    let patched = runtime.dispatch(
        patch_call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: patch_call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(patched.output, ToolOutput::Success { .. }));
    assert_eq!(
        std::fs::read_to_string(moved.join("notes.txt")).unwrap(),
        "updated\n"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("notes.txt")).unwrap(),
        "outside\n"
    );

    let create_call = ToolCall {
        id: ToolCallId::new("call-stable-root-create"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- /dev/null\n+++ b/created.txt\n@@ -0,0 +1 @@\n+created\n"
        }),
    };
    let created = runtime.dispatch(
        create_call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: create_call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert!(
        matches!(created.output, ToolOutput::Success { .. }),
        "unexpected create result: {:?}",
        created.output
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("created.txt")).unwrap(),
        "created\n"
    );
    assert!(!outside.join("created.txt").exists());

    let command_call = ToolCall {
        id: ToolCallId::new("call-stable-root-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "printf command > command.txt" }),
    };
    let commanded = runtime.dispatch(
        command_call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: command_call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(commanded.output, ToolOutput::Success { .. }));
    assert_eq!(
        std::fs::read_to_string(moved.join("command.txt")).unwrap(),
        "command"
    );
    assert!(
        !outside.join("command.txt").exists(),
        "command cwd must remain the opened workspace"
    );
}

#[test]
fn read_file_truncates_at_a_valid_utf8_boundary() {
    let test_workspace = TestWorkspace::new("read-truncation");
    let mut content = "a".repeat(64 * 1024 - 1);
    content.push('界');
    content.push_str("tail");
    std::fs::write(test_workspace.path().join("large.txt"), &content).expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-large-read"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "large.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("read_file should succeed");
    };
    let [ToolContent::Text { text }] = content.as_slice() else {
        panic!("read_file should return text");
    };
    assert_eq!(text.len(), 48 * 1024 - 2);
    assert_eq!(metadata["bytes"], json!(64 * 1024 + 6));
    assert_eq!(metadata["returned_bytes"], json!(48 * 1024 - 2));
    assert_eq!(metadata["truncated"], json!(true));
    assert_eq!(metadata["truncation_limit_bytes"], json!(48 * 1024));
}

#[test]
fn read_file_bounds_the_complete_serialized_output() {
    let test_workspace = TestWorkspace::new("read-output-budget");
    std::fs::write(
        test_workspace.path().join("control-bytes.txt"),
        vec![0u8; 70_000],
    )
    .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-read-output-budget"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "control-bytes.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let serialized_len = serde_json::to_vec(&result.output)
        .expect("tool output serializes")
        .len();
    assert!(matches!(result.output, ToolOutput::Success { .. }));
    assert!(serialized_len <= 64 * 1024, "serialized output is bounded");
}

#[cfg(unix)]
#[test]
fn file_tools_reject_a_fifo_without_blocking() {
    let test_workspace = TestWorkspace::new("fifo");
    let fifo = test_workspace.path().join("named-pipe");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo starts");
    assert!(status.success(), "mkfifo must create the fixture");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let calls = [
        ToolCall {
            id: ToolCallId::new("call-read-fifo"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "named-pipe" }),
        },
        ToolCall {
            id: ToolCallId::new("call-search-fifo"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "path": "named-pipe", "query": "needle" }),
        },
        ToolCall {
            id: ToolCallId::new("call-patch-fifo"),
            tool_name: "apply_patch".to_string(),
            arguments: json!({
                "patch": "--- a/named-pipe\n+++ b/named-pipe\n@@ -1 +1 @@\n-old\n+new\n"
            }),
        },
    ];

    for call in calls {
        let started = std::time::Instant::now();
        let result = runtime.dispatch(
            call.clone(),
            ToolExecutionAuthorization::ApprovalGranted {
                call_id: call.id.clone(),
            },
            Arc::new(AtomicBool::new(false)),
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{} must not block while opening a FIFO",
            call.tool_name
        );
        assert!(
            matches!(result.output, ToolOutput::Failure { .. }),
            "{} must reject a FIFO",
            call.tool_name
        );
    }
}

#[test]
fn search_files_returns_structured_matches_through_the_tool_runtime() {
    let test_workspace = TestWorkspace::new("search-files");
    let source = test_workspace.path().join("src");
    std::fs::create_dir(&source).expect("source directory is created");
    std::fs::write(
        source.join("lib.rs"),
        "pub fn first() {}\n// agent kernel marker\n",
    )
    .expect("first fixture is written");
    std::fs::write(
        source.join("main.rs"),
        "// agent kernel marker\nfn main() {}\n",
    )
    .expect("second fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "agent kernel", "path": "src" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content,
        metadata,
        extensions,
    } = result.output
    else {
        panic!("search_files should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({
                "matches": [
                    { "path": "src/lib.rs", "line": 2, "text": "// agent kernel marker" },
                    { "path": "src/main.rs", "line": 1, "text": "// agent kernel marker" }
                ]
            }),
        }]
    );
    assert_eq!(metadata["matches"], json!(2));
    assert_eq!(metadata["truncated"], json!(false));
    assert_eq!(metadata["query"], json!("agent kernel"));
    assert!(extensions.is_empty());
}

#[test]
fn search_files_rejects_an_oversized_path_before_scanning() {
    let test_workspace = TestWorkspace::new("search-oversized-path");
    std::fs::write(test_workspace.path().join("match.txt"), "needle\n")
        .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let oversized_path = "./".repeat(4_097);

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-oversized-path"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle", "path": oversized_path }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("oversized search path must fail");
    };
    assert_eq!(error.code, "invalid_arguments");
    assert!(error.message.contains("exceeds 8192 bytes"));
}

#[test]
fn search_files_bounds_path_metadata_before_serialization() {
    let test_workspace = TestWorkspace::new("search-bounded-path-metadata");
    std::fs::write(test_workspace.path().join("match.txt"), "needle\n")
        .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let path = "./".repeat(3_000);

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-bounded-path-metadata"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle", "path": path }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("bounded search path should succeed");
    };
    assert_eq!(metadata["path_bytes"], json!(6_000));
    assert_eq!(metadata["path_truncated"], json!(true));
    assert!(metadata["path"].as_str().expect("path is a string").len() < 6_000);
}

#[test]
fn search_files_marks_results_truncated_at_the_directory_depth_limit() {
    let test_workspace = TestWorkspace::new("search-depth-limit");
    let mut deepest = test_workspace.path().to_path_buf();
    for _ in 0..257 {
        deepest.push("d");
        std::fs::create_dir(&deepest).expect("nested fixture directory is created");
    }
    std::fs::write(deepest.join("match.txt"), "deep needle\n").expect("deep fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-depth-limit"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("bounded deep search should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({ "matches": [] }),
        }]
    );
    assert_eq!(metadata["truncated"], json!(true));
    assert_eq!(metadata["directories_visited"], json!(256));
}

#[test]
fn search_files_marks_a_truncated_matching_line() {
    let test_workspace = TestWorkspace::new("search-truncation");
    let long_line = format!("needle{}\n", "x".repeat(9_000));
    std::fs::write(test_workspace.path().join("large.txt"), long_line).expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-large-search"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("search_files should succeed");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("search_files should return structured JSON");
    };
    assert_eq!(
        value["matches"][0]["text"].as_str().unwrap().len(),
        8 * 1024
    );
    assert_eq!(metadata["truncated"], json!(true));
}

#[test]
fn search_files_does_not_reserve_patch_staging_prefixes_globally() {
    let test_workspace = TestWorkspace::new("search-patch-prefix");
    std::fs::write(
        test_workspace.path().join(".young-agent-patch-notes.md"),
        "visible needle\n",
    )
    .expect("prefixed file is written");
    let directory = test_workspace.path().join(".young-agent-patch-notes");
    std::fs::create_dir(&directory).expect("prefixed directory is created");
    std::fs::write(directory.join("real.rs"), "visible needle\n").expect("nested file is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-patch-prefix"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "visible needle" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { content, .. } = result.output else {
        panic!("prefixed workspace paths should remain searchable");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("search should return structured JSON");
    };
    assert_eq!(value["matches"].as_array().unwrap().len(), 2);
}

#[test]
fn search_files_finds_a_match_beyond_its_bounded_line_preview() {
    let test_workspace = TestWorkspace::new("search-long-line");
    let mut long_line = "x".repeat(2 * 1024 * 1024);
    long_line.push_str("needle\n");
    std::fs::write(test_workspace.path().join("large.txt"), long_line).expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-streaming-search"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("search_files should succeed");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("search_files should return structured JSON");
    };
    assert_eq!(value["matches"][0]["line"], json!(1));
    assert_eq!(
        value["matches"][0]["text"].as_str().unwrap().len(),
        8 * 1024
    );
    assert_eq!(metadata["lines_truncated"], json!(1));
    assert_eq!(metadata["truncated"], json!(true));
}

#[test]
fn search_files_skips_an_entire_file_when_late_bytes_are_not_utf8() {
    let test_workspace = TestWorkspace::new("search-binary-file");
    std::fs::write(
        test_workspace.path().join("a-binary.txt"),
        b"needle before invalid bytes\n\xff\n",
    )
    .expect("binary fixture is written");
    std::fs::write(
        test_workspace.path().join("b-valid.txt"),
        "needle in valid file\n",
    )
    .expect("valid fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-binary-file"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("search_files should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({
                "matches": [
                    { "path": "b-valid.txt", "line": 1, "text": "needle in valid file" }
                ]
            }),
        }]
    );
    assert_eq!(metadata["binary_files_skipped"], json!(1));
}

#[test]
fn search_files_returns_a_deterministic_subset_at_the_match_limit() {
    let test_workspace = TestWorkspace::new("search-deterministic-limit");
    let files = test_workspace.path().join("files");
    std::fs::create_dir(&files).expect("fixture directory is created");
    for index in (0..205).rev() {
        std::fs::write(files.join(format!("{index:03}.txt")), "needle\n")
            .expect("fixture is written");
    }
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-deterministic-limit"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle", "path": "files" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("search_files should succeed");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("search_files should return structured JSON");
    };
    let matches = value["matches"].as_array().expect("matches are an array");
    assert_eq!(matches.len(), 200);
    assert_eq!(matches.first().unwrap()["path"], json!("files/000.txt"));
    assert_eq!(matches.last().unwrap()["path"], json!("files/199.txt"));
    assert_eq!(metadata["truncated"], json!(true));
}

#[test]
fn search_files_bounds_the_complete_serialized_output() {
    let test_workspace = TestWorkspace::new("search-output-budget");
    let query = "\0".repeat(8 * 1024);
    std::fs::write(
        test_workspace.path().join("control-bytes.txt"),
        format!("{query}\n"),
    )
    .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-output-budget"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": query }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let serialized_len = serde_json::to_vec(&result.output)
        .expect("tool output serializes")
        .len();
    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("search_files should succeed");
    };
    assert!(serialized_len <= 64 * 1024, "serialized output is bounded");
    assert_eq!(metadata["query_truncated"], json!(true));
    assert_eq!(metadata["query_bytes"], json!(8 * 1024));
}

#[cfg(unix)]
#[test]
fn search_files_rejects_an_explicit_symlink_escape() {
    use std::os::unix::fs::symlink;

    let parent = TestWorkspace::new("search-symlink");
    let selected = parent.path().join("selected");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    std::fs::create_dir(&outside).expect("outside directory is created");
    std::fs::write(outside.join("secret.txt"), "needle\n").expect("fixture is written");
    symlink(&outside, selected.join("escape")).expect("escape symlink is created");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-symlink"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "needle", "path": "escape" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("search symlink escape must fail");
    };
    assert_eq!(error.code, "outside_workspace");
}

#[test]
fn builtin_tools_reject_malformed_argument_envelopes_consistently() {
    let test_workspace = TestWorkspace::new("invalid-arguments");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let cases = [
        ("read_file", json!("README.md")),
        ("read_file", json!({ "path": "README.md", "extra": true })),
        ("read_file", json!({})),
        ("read_file", json!({ "path": 7 })),
        ("search_files", json!({ "query": "needle", "path": "" })),
        ("search_files", json!({ "query": "needle", "path": false })),
        ("search_files", json!({ "query": "" })),
        ("apply_patch", json!({ "patch": null })),
    ];

    for (index, (tool_name, arguments)) in cases.into_iter().enumerate() {
        let call = ToolCall {
            id: ToolCallId::new(format!("call-invalid-arguments-{index}")),
            tool_name: tool_name.to_string(),
            arguments,
        };
        let authorization = if tool_name == "apply_patch" {
            ToolExecutionAuthorization::ApprovalGranted {
                call_id: call.id.clone(),
            }
        } else {
            ToolExecutionAuthorization::NotRequired
        };
        let result = runtime.dispatch(call, authorization, Arc::new(AtomicBool::new(false)));

        let ToolOutput::Failure { error, extensions } = result.output else {
            panic!("{tool_name} malformed arguments must fail");
        };
        assert_eq!(error.code, "invalid_arguments");
        assert!(!error.retryable);
        assert!(extensions.is_empty());
    }
}

#[test]
fn apply_patch_rejects_malformed_and_conflicting_unified_diffs() {
    let test_workspace = TestWorkspace::new("patch-validation-matrix");
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "first\nsecond\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let cases = [
        ("empty", "", "invalid_arguments"),
        (
            "no-file-header",
            "diff --git a/notes.txt b/notes.txt\n",
            "invalid_patch",
        ),
        ("missing-new-header", "--- a/notes.txt\n", "invalid_patch"),
        (
            "empty-path",
            "--- \n+++ b/notes.txt\n@@ -1 +1 @@\n-first\n+changed\n",
            "invalid_patch",
        ),
        (
            "rename",
            "--- a/notes.txt\n+++ b/renamed.txt\n@@ -1 +1 @@\n-first\n+changed\n",
            "invalid_patch",
        ),
        (
            "both-dev-null",
            "--- /dev/null\n+++ /dev/null\n@@ -0,0 +0,0 @@\n",
            "invalid_patch",
        ),
        (
            "missing-hunk",
            "--- a/notes.txt\n+++ b/notes.txt\n",
            "invalid_patch",
        ),
        (
            "invalid-hunk",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ broken @@\n",
            "invalid_patch",
        ),
        (
            "missing-new-range",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 @@\n",
            "invalid_patch",
        ),
        (
            "extra-range",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 +2 @@\n",
            "invalid_patch",
        ),
        (
            "invalid-range-start",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -x +1 @@\n",
            "invalid_patch",
        ),
        (
            "invalid-range-count",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1,x +1 @@\n",
            "invalid_patch",
        ),
        (
            "early-hunk-end",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n first\n",
            "invalid_patch",
        ),
        (
            "invalid-hunk-line",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n?first\n",
            "invalid_patch",
        ),
        (
            "declared-count-overflow",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1,0 @@\n first\n",
            "invalid_patch",
        ),
        (
            "orphan-newline-marker",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n\\ No newline at end of file\n",
            "invalid_patch",
        ),
        (
            "new-file-already-exists",
            "--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+changed\n",
            "patch_conflict",
        ),
        (
            "missing-source",
            "--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "patch_conflict",
        ),
        (
            "hunk-outside-file",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -9 +9 @@\n-missing\n+changed\n",
            "patch_conflict",
        ),
        (
            "context-mismatch",
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n missing\n",
            "patch_conflict",
        ),
        (
            "out-of-order-hunks",
            concat!(
                "--- a/notes.txt\n",
                "+++ b/notes.txt\n",
                "@@ -2 +2 @@\n",
                " second\n",
                "@@ -1 +1 @@\n",
                " first\n"
            ),
            "patch_conflict",
        ),
    ];

    for (name, patch, expected_code) in cases {
        let output = dispatch_approved_patch(
            &mut runtime,
            &format!("call-patch-validation-{name}"),
            json!({ "patch": patch }),
            false,
        );
        let ToolOutput::Failure { error, .. } = output else {
            panic!("{name} must fail without publishing a change");
        };
        assert_eq!(error.code, expected_code, "case {name}");
        assert_eq!(
            std::fs::read_to_string(&notes).expect("fixture remains readable"),
            "first\nsecond\n",
            "case {name} must not mutate the target"
        );
    }
}

#[test]
fn apply_patch_enforces_cancellation_and_target_resource_limits() {
    let test_workspace = TestWorkspace::new("patch-resource-limits");
    std::fs::write(test_workspace.path().join("invalid.txt"), [0xff, 0xfe])
        .expect("invalid UTF-8 fixture is written");
    let oversized = std::fs::File::create(test_workspace.path().join("oversized.txt"))
        .expect("oversized fixture is created");
    oversized
        .set_len(32 * 1024 * 1024 + 1)
        .expect("oversized fixture is extended");
    std::fs::write(
        test_workspace.path().join("too-many-lines.txt"),
        "\n".repeat(1_000_001),
    )
    .expect("line-heavy fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let cancelled = dispatch_approved_patch(
        &mut runtime,
        "call-patch-cancelled",
        json!({
            "patch": "--- a/invalid.txt\n+++ b/invalid.txt\n@@ -1 +1 @@\n-old\n+new\n"
        }),
        true,
    );
    let ToolOutput::Failure { error, .. } = cancelled else {
        panic!("pre-cancelled patch must fail");
    };
    assert_eq!(error.code, "tool_cancelled");

    let line_limit = dispatch_approved_patch(
        &mut runtime,
        "call-patch-line-limit",
        json!({ "patch": "\n".repeat(200_001) }),
        false,
    );
    let ToolOutput::Failure { error, .. } = line_limit else {
        panic!("line-heavy patch must fail");
    };
    assert_eq!(error.code, "patch_too_large");

    for (name, expected_code) in [
        ("invalid.txt", "workspace_io_error"),
        ("oversized.txt", "patch_too_large"),
        ("too-many-lines.txt", "patch_too_large"),
    ] {
        let output = dispatch_approved_patch(
            &mut runtime,
            &format!("call-patch-target-{name}"),
            json!({
                "patch": format!(
                    "--- a/{name}\n+++ b/{name}\n@@ -1 +1 @@\n-old\n+new\n"
                )
            }),
            false,
        );
        let ToolOutput::Failure { error, .. } = output else {
            panic!("{name} must be rejected");
        };
        assert_eq!(error.code, expected_code, "target {name}");
    }
}

#[test]
fn apply_patch_accepts_standard_diff_metadata_without_publishing_it() {
    let test_workspace = TestWorkspace::new("patch-diff-metadata");
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let output = dispatch_approved_patch(
        &mut runtime,
        "call-patch-diff-metadata",
        json!({
            "patch": concat!(
                "diff --git a/notes.txt b/notes.txt\n",
                "index 1111111..2222222 100644\n",
                "old mode 100644\n",
                "new mode 100644\n",
                "--- a/notes.txt\n",
                "+++ b/notes.txt\n",
                "@@ -1 +1 @@\n",
                "-old\n",
                "+new\n"
            )
        }),
        false,
    );

    assert!(matches!(output, ToolOutput::Success { .. }));
    assert_eq!(std::fs::read_to_string(notes).unwrap(), "new\n");
}

#[test]
fn apply_patch_updates_a_workspace_file_after_approval() {
    let test_workspace = TestWorkspace::new("apply-patch");
    test_workspace.git(&["init", "--quiet"]);
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "first\nold value\nlast\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": concat!(
                "--- a/notes.txt\n",
                "+++ b/notes.txt\n",
                "@@ -1,3 +1,3 @@\n",
                " first\n",
                "-old value\n",
                "+new value\n",
                " last\n"
            )
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content,
        metadata,
        extensions,
    } = result.output
    else {
        panic!("apply_patch should succeed");
    };
    assert_eq!(
        std::fs::read_to_string(notes).expect("patched file is readable"),
        "first\nnew value\nlast\n"
    );
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("apply_patch should return structured JSON");
    };
    assert_eq!(value["changed_files"], json!(["notes.txt"]));
    let recovery_files = value["recovery_files"]
        .as_array()
        .expect("recovery files are an array");
    assert_eq!(recovery_files.len(), 1);
    let recovery = recovery_files[0]
        .as_str()
        .expect("recovery path is a string");
    assert!(recovery.starts_with(".young-agent-recovery/.young-agent-patch-displaced-"));
    assert_eq!(
        std::fs::read_to_string(test_workspace.path().join(recovery)).unwrap(),
        "first\nold value\nlast\n"
    );
    assert_eq!(metadata["files_changed"], json!(1));
    assert_eq!(metadata["recovery_files"], json!(1));
    assert_eq!(
        metadata["recovery_policy"],
        json!("ignored_by_search_and_git_until_caller_removes")
    );
    assert!(extensions.is_empty());

    let search = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-search-after-patch"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "old value" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );
    let ToolOutput::Success { content, .. } = search.output else {
        panic!("search after patch should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({ "matches": [] }),
        }]
    );

    let ignored = Command::new("git")
        .arg("-C")
        .arg(test_workspace.path())
        .args(["check-ignore", "--quiet", recovery])
        .status()
        .expect("git check-ignore starts");
    assert!(ignored.success(), "recovery file must be ignored by Git");
    let ignore_rule = Command::new("git")
        .arg("-C")
        .arg(test_workspace.path())
        .args([
            "check-ignore",
            "--quiet",
            ".young-agent-recovery/.gitignore",
        ])
        .status()
        .expect("git check-ignore starts");
    assert!(
        ignore_rule.success(),
        "the recovery namespace ignore rule must not be staged by git add"
    );
}

#[test]
fn apply_patch_creates_a_new_file_without_recovery_artifacts() {
    let test_workspace = TestWorkspace::new("apply-patch-create");
    test_workspace.git(&["init", "--quiet"]);
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-patch-create"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": concat!(
                "--- /dev/null\n",
                "+++ b/new.txt\n",
                "@@ -0,0 +1,2 @@\n",
                "+first line\n",
                "+second line\n"
            )
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content,
        metadata,
        extensions,
    } = result.output
    else {
        panic!("new-file patch should succeed");
    };
    assert_eq!(
        std::fs::read_to_string(test_workspace.path().join("new.txt"))
            .expect("created file is readable"),
        "first line\nsecond line\n"
    );
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("apply_patch should return structured JSON");
    };
    assert_eq!(value["changed_files"], json!(["new.txt"]));
    assert_eq!(value["recovery_files"], json!([]));
    assert_eq!(metadata["files_changed"], json!(1));
    assert_eq!(metadata["recovery_files"], json!(0));
    assert!(extensions.is_empty());
}

#[cfg(unix)]
#[test]
fn apply_patch_preserves_private_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let test_workspace = TestWorkspace::new("patch-permissions");
    let notes = test_workspace.path().join("private.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o600))
        .expect("fixture permissions are set");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-private-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/private.txt\n+++ b/private.txt\n@@ -1 +1 @@\n-old\n+new\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    assert!(matches!(result.output, ToolOutput::Success { .. }));
    assert_eq!(std::fs::metadata(notes).unwrap().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn apply_patch_preserves_setuid_mode_after_writing_staging_content() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let test_workspace = TestWorkspace::new("patch-setuid-mode");
    let executable = test_workspace.path().join("tool.sh");
    std::fs::write(&executable, "old\n").expect("fixture is written");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o4750))
        .expect("fixture mode is set");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-setuid-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/tool.sh\n+++ b/tool.sh\n@@ -1 +1 @@\n-old\n+new\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    assert!(matches!(result.output, ToolOutput::Success { .. }));
    assert_eq!(
        std::fs::metadata(executable).unwrap().mode() & 0o7777,
        0o4750
    );
}

#[cfg(unix)]
#[test]
fn apply_patch_refuses_to_break_hard_link_identity() {
    let test_workspace = TestWorkspace::new("patch-hard-link");
    let notes = test_workspace.path().join("shared.txt");
    let alias = test_workspace.path().join("alias.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    std::fs::hard_link(&notes, &alias).expect("hard-link fixture is created");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-hard-link-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/shared.txt\n+++ b/shared.txt\n@@ -1 +1 @@\n-old\n+new\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("hard-linked patch target must be rejected");
    };
    assert_eq!(error.code, "unsupported_file_metadata");
    assert_eq!(std::fs::read_to_string(notes).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(alias).unwrap(), "old\n");
}

#[cfg(target_os = "macos")]
#[test]
fn apply_patch_refuses_to_drop_an_extended_acl() {
    let test_workspace = TestWorkspace::new("patch-extended-acl");
    let notes = test_workspace.path().join("protected.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    let user = std::env::var("USER").expect("test user is available");
    exacl::setfacl(
        &[&notes],
        &[exacl::AclEntry::deny_user(&user, exacl::Perm::WRITE, None)],
        None,
    )
    .expect("extended ACL fixture is set");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-extended-acl-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/protected.txt\n+++ b/protected.txt\n@@ -1 +1 @@\n-old\n+new\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("ACL-bearing patch target must be rejected");
    };
    assert_eq!(error.code, "unsupported_file_metadata");
    assert_eq!(std::fs::read_to_string(notes).unwrap(), "old\n");
}

#[test]
fn apply_patch_cannot_mutate_a_file_without_approval() {
    let test_workspace = TestWorkspace::new("patch-approval");
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-unapproved-patch"),
            tool_name: "apply_patch".to_string(),
            arguments: json!({
                "patch": "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+new\n"
            }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("unapproved patch must fail");
    };
    assert_eq!(error.code, "approval_required");
    assert_eq!(
        std::fs::read_to_string(notes).expect("file remains readable"),
        "old\n"
    );
}

#[test]
fn apply_patch_rejects_a_target_outside_the_selected_workspace() {
    let parent = TestWorkspace::new("patch-traversal");
    let selected = parent.path().join("selected");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    let secret = parent.path().join("secret.txt");
    std::fs::write(&secret, "old\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-outside-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/../secret.txt\n+++ b/../secret.txt\n@@ -1 +1 @@\n-old\n+stolen\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("outside patch must fail");
    };
    assert_eq!(error.code, "outside_workspace");
    assert_eq!(
        std::fs::read_to_string(secret).expect("outside file remains readable"),
        "old\n"
    );
}

#[test]
fn apply_patch_rejects_multi_file_edits_without_mutating_any_file() {
    let test_workspace = TestWorkspace::new("patch-create-delete");
    let old = test_workspace.path().join("old.txt");
    std::fs::write(&old, "obsolete\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-create-delete"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": concat!(
                "--- /dev/null\n",
                "+++ b/new.txt\n",
                "@@ -0,0 +1,2 @@\n",
                "+created\n",
                "+file\n",
                "--- a/old.txt\n",
                "+++ /dev/null\n",
                "@@ -1 +0,0 @@\n",
                "-obsolete\n"
            )
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("multi-file patch must fail");
    };
    assert_eq!(error.code, "invalid_patch");
    assert!(!test_workspace.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(old).expect("old file remains readable"),
        "obsolete\n"
    );
}

#[test]
fn apply_patch_rejects_delete_file_edits_without_mutating_the_file() {
    let test_workspace = TestWorkspace::new("patch-delete");
    let obsolete = test_workspace.path().join("obsolete.txt");
    std::fs::write(&obsolete, "obsolete\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-delete-file"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/obsolete.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-obsolete\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("delete-file patch must be rejected");
    };
    assert_eq!(error.code, "invalid_patch");
    assert_eq!(std::fs::read_to_string(obsolete).unwrap(), "obsolete\n");
}

#[test]
fn apply_patch_rejects_oversized_input_before_parsing_it() {
    let test_workspace = TestWorkspace::new("patch-input-limit");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-oversized-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({ "patch": "x".repeat(4 * 1024 * 1024 + 1) }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("oversized patch must fail");
    };
    assert_eq!(error.code, "patch_too_large");
}

#[test]
fn apply_patch_handles_files_without_a_trailing_newline() {
    let test_workspace = TestWorkspace::new("patch-no-newline");
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "old").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-no-newline"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": concat!(
                "--- a/notes.txt\n",
                "+++ b/notes.txt\n",
                "@@ -1 +1 @@\n",
                "-old\n",
                "\\ No newline at end of file\n",
                "+new\n",
                "\\ No newline at end of file\n"
            )
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { .. } = result.output else {
        panic!("no-newline patch should succeed");
    };
    assert_eq!(
        std::fs::read_to_string(notes).expect("patched file is readable"),
        "new"
    );
}

#[test]
fn apply_patch_rejects_unaccounted_content_instead_of_silently_ignoring_it() {
    let test_workspace = TestWorkspace::new("patch-extra-content");
    let notes = test_workspace.path().join("notes.txt");
    std::fs::write(&notes, "old\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-extra-content"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": concat!(
                "--- a/notes.txt\n",
                "+++ b/notes.txt\n",
                "@@ -1 +1 @@\n",
                "-old\n",
                "+new\n",
                "+not-declared-in-hunk\n"
            )
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("malformed patch must fail");
    };
    assert_eq!(error.code, "invalid_patch");
    assert_eq!(
        std::fs::read_to_string(notes).expect("file remains readable"),
        "old\n"
    );
}

#[cfg(unix)]
#[test]
fn apply_patch_rejects_a_symlink_target_outside_the_selected_workspace() {
    use std::os::unix::fs::symlink;

    let parent = TestWorkspace::new("patch-symlink");
    let selected = parent.path().join("selected");
    std::fs::create_dir(&selected).expect("selected workspace is created");
    let secret = parent.path().join("secret.txt");
    std::fs::write(&secret, "old\n").expect("fixture is written");
    symlink(&secret, selected.join("notes.txt")).expect("escape symlink is created");
    let workspace = CodingWorkspace::resolve(&selected).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-symlink-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+stolen\n"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("symlink patch must fail");
    };
    assert_eq!(error.code, "outside_workspace");
    assert_eq!(
        std::fs::read_to_string(secret).expect("outside file remains readable"),
        "old\n"
    );
}

#[test]
fn run_command_executes_from_the_workspace_and_returns_structured_output() {
    let test_workspace = TestWorkspace::new("run-command");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let expected_root = workspace.context().root().display().to_string();
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "printf 'stdout:'; pwd; printf 'stderr line\\n' >&2"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content,
        metadata,
        extensions,
    } = result.output
    else {
        panic!("run_command should succeed");
    };
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({
                "success": true,
                "exit_code": 0,
                "stdout": format!("stdout:{expected_root}\n"),
                "stderr": "stderr line\n"
            }),
        }]
    );
    assert_eq!(metadata["cwd"], json!(expected_root));
    assert_eq!(metadata["stdout_truncated"], json!(false));
    assert_eq!(metadata["stderr_truncated"], json!(false));
    assert_eq!(metadata["process_scope"], json!("process_group"));
    assert_eq!(
        metadata["residual_process_group_policy"],
        json!("kill_and_tracking_token_close_before_leader_reap")
    );
    assert_eq!(
        metadata["background_process_policy"],
        json!("tracked_descendants_terminated_at_foreground_exit")
    );
    assert!(metadata["process_security_policy"].is_string());
    assert!(metadata["exec_privilege_gain_blocked"].is_boolean());
    assert_eq!(metadata["detached_processes_tracked"], json!(false));
    assert!(extensions.is_empty());
}

#[test]
fn run_command_executes_low_risk_validation_without_approval() {
    let test_workspace = TestWorkspace::new("run-command-low-risk");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let expected_root = workspace.context().root().display().to_string();
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-low-risk-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "pwd" }),
    };

    let result = runtime.dispatch(
        call,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { content, .. } = result.output else {
        panic!("low-risk command should execute without approval");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("run_command should return structured JSON");
    };
    assert_eq!(value["stdout"], json!(format!("{expected_root}\n")));
}

#[test]
fn run_command_captures_output_after_a_quiet_period() {
    let test_workspace = TestWorkspace::new("run-command-quiet-period");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-command-quiet-period"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "sleep 0.12; printf delayed" }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { content, .. } = result.output else {
        panic!("quiet command should succeed");
    };
    let ToolContent::Json { value } = &content[0] else {
        panic!("quiet command should return structured output");
    };
    assert_eq!(value["stdout"], json!("delayed"));
}

#[test]
fn run_command_truncates_large_output_without_losing_the_total_size() {
    let test_workspace = TestWorkspace::new("command-truncation");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-large-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "awk 'BEGIN { for (i = 0; i < 70000; i++) printf \"x\" }'"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success {
        content, metadata, ..
    } = result.output
    else {
        panic!("run_command should succeed");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("run_command should return structured JSON");
    };
    assert_eq!(value["stdout"].as_str().unwrap().len(), 24 * 1024 - 2);
    assert_eq!(metadata["stdout_bytes"], json!(70_000));
    assert_eq!(metadata["stdout_truncated"], json!(true));
    assert_eq!(metadata["output_incomplete"], json!(false));
}

#[test]
fn run_command_bounds_json_escaped_output() {
    let test_workspace = TestWorkspace::new("command-json-budget");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-json-budget-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "dd if=/dev/zero bs=70000 count=1 2>/dev/null"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let serialized_len = serde_json::to_vec(&result.output)
        .expect("tool output serializes")
        .len();
    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("run_command should succeed");
    };
    assert_eq!(metadata["stdout_bytes"], json!(70_000));
    assert_eq!(metadata["stdout_truncated"], json!(true));
    assert!(serialized_len <= 64 * 1024, "serialized output is bounded");
}

#[test]
fn run_command_rejects_oversized_input_before_spawning_it() {
    let test_workspace = TestWorkspace::new("command-input-limit");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let command = format!(
        "printf spawned > oversized-command-ran #{}",
        "x".repeat(64 * 1024)
    );
    let call = ToolCall {
        id: ToolCallId::new("call-oversized-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": command }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("oversized command must fail");
    };
    assert_eq!(error.code, "tool_rejected");
    assert!(error.message.contains("65536 bytes"));
    assert!(!error.retryable);
    assert!(!test_workspace.path().join("oversized-command-ran").exists());
}

#[test]
fn run_command_requires_approval_for_workspace_mutation() {
    let test_workspace = TestWorkspace::new("command-approval");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-unapproved-command"),
            tool_name: "run_command".to_string(),
            arguments: json!({ "command": "touch marker.txt" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("unapproved command must fail");
    };
    assert_eq!(error.code, "approval_required");
    assert_eq!(error.message, "command may mutate workspace files");
    assert!(!test_workspace.path().join("marker.txt").exists());
}

#[test]
fn run_command_observes_cooperative_cancellation() {
    let test_workspace = TestWorkspace::new("command-cancellation");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-cancelled-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "printf ready > command-ready; while :; do :; done"
        }),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let ready = test_workspace.path().join("command-ready");
    let trigger = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "command did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
        cancellation_trigger.store(true, Ordering::Relaxed);
    });

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        cancellation,
    );
    trigger.join().expect("cancellation trigger finishes");

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("cancelled command must fail");
    };
    assert_eq!(error.code, "command_termination_unverified");
    assert_eq!(
        error.message,
        "termination was requested for signal-compatible process-group members; detached or credential-changing descendants were not verified"
    );
}

#[test]
fn run_command_cancellation_terminates_descendant_processes() {
    let test_workspace = TestWorkspace::new("command-descendant-cancellation");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-cancelled-command-tree"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "sleep 10 & printf ready > descendant-ready; wait"
        }),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let ready = test_workspace.path().join("descendant-ready");
    let trigger = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "descendant command did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
        cancellation_trigger.store(true, Ordering::Relaxed);
    });

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        cancellation,
    );
    trigger.join().expect("cancellation trigger finishes");

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("cancelled command must fail");
    };
    assert_eq!(error.code, "command_termination_unverified");
    assert_eq!(
        error.message,
        "termination was requested for signal-compatible process-group members; detached or credential-changing descendants were not verified"
    );
}

#[test]
fn run_command_terminates_a_background_process_when_the_foreground_shell_exits() {
    let test_workspace = TestWorkspace::new("command-background-pipe");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-background-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "sleep 0.1 &" }),
    };
    let started = std::time::Instant::now();

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    assert!(started.elapsed() < Duration::from_secs(2));
    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("foreground shell status should be reported after background termination");
    };
    assert_eq!(metadata["output_incomplete"], json!(false));
}

#[test]
fn run_command_seals_or_supervises_a_background_process_with_closed_pipes() {
    let test_workspace = TestWorkspace::new("command-background-closed-pipes");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-background-closed-pipes"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "sleep 10 >/dev/null 2>&1 & exit 0"
        }),
    };
    let started = std::time::Instant::now();

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    assert!(started.elapsed() < Duration::from_secs(2));
    match result.output {
        ToolOutput::Success { .. } => {}
        ToolOutput::Failure { error, .. } => {
            assert_eq!(error.code, "command_termination_unverified");
            assert!(error
                .message
                .contains("process-wide supervisor retries termination"));
        }
    }
}

#[test]
fn run_command_does_not_inject_shell_variables_or_source_text() {
    let test_workspace = TestWorkspace::new("command-shell-source-fidelity");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-shell-source-fidelity"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "readonly __young_agent_foreground_status=0; printf x \\"
        }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { content, .. } = result.output else {
        panic!(
            "unmodified shell source should succeed: {:?}",
            result.output
        );
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("command should return structured JSON");
    };
    assert_eq!(value["success"], json!(true));
    assert_eq!(value["exit_code"], json!(0));
    assert_eq!(value["stdout"], json!("x"));
}

#[test]
fn run_command_preserves_signal_termination_status() {
    let test_workspace = TestWorkspace::new("command-signal-status");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-signal-status"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "kill -KILL $$" }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Success { content, .. } = result.output else {
        panic!("signal termination should remain a command outcome");
    };
    let [ToolContent::Json { value }] = content.as_slice() else {
        panic!("command should return structured JSON");
    };
    assert_eq!(value["success"], json!(false));
    assert_eq!(value["exit_code"], serde_json::Value::Null);
}

#[cfg(unix)]
#[test]
fn run_command_fails_closed_when_a_close_fds_descendant_survives_cancellation() {
    let test_workspace = TestWorkspace::new("command-escaped-cancellation");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-escaped-command-cancellation"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "python3 -c 'import subprocess,time; subprocess.Popen([\"python3\", \"-c\", \"import os,time; open(\\\"escaped-ready\\\", \\\"w\\\").close(); exec(\\\"while not os.path.exists(\\\\\\\"escaped-release\\\\\\\"): time.sleep(0.01)\\\"); open(\\\"escaped-after-cancel.tmp\\\", \\\"w\\\").write(\\\"survived\\\"); os.replace(\\\"escaped-after-cancel.tmp\\\", \\\"escaped-after-cancel\\\")\"], start_new_session=True, close_fds=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); time.sleep(10)'"
        }),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let ready = test_workspace.path().join("escaped-ready");
    let trigger = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "escaped descendant did not become ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
        cancellation_trigger.store(true, Ordering::Relaxed);
    });

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        cancellation,
    );
    trigger.join().expect("cancellation trigger finishes");

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("incompletely terminated command must fail");
    };
    assert_eq!(error.code, "command_termination_unverified");
    std::fs::write(test_workspace.path().join("escaped-release"), "release")
        .expect("escaped descendant is released");
    let survived = test_workspace.path().join("escaped-after-cancel");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !survived.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "escaped descendant did not report survival"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(std::fs::read_to_string(survived).unwrap(), "survived");
}
