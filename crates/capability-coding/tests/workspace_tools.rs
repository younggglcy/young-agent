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
fn search_files_observes_cancellation_while_scanning_a_large_line() {
    let test_workspace = TestWorkspace::new("search-cancellation");
    std::fs::write(
        test_workspace.path().join("large.txt"),
        vec![b'x'; 32 * 1024 * 1024],
    )
    .expect("fixture is written");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        cancellation_trigger.store(true, Ordering::Relaxed);
    });

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-cancelled-search"),
            tool_name: "search_files".to_string(),
            arguments: json!({ "query": "absent" }),
        },
        ToolExecutionAuthorization::NotRequired,
        cancellation,
    );
    trigger.join().expect("cancellation trigger finishes");

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("cancelled search must fail");
    };
    assert_eq!(error.code, "tool_cancelled");
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
    assert_eq!(metadata["detached_processes_tracked"], json!(false));
    assert!(extensions.is_empty());
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
fn run_command_requires_approval_until_command_policy_is_implemented() {
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
    assert_eq!(
        error.message,
        "command execution requires approval until a command safety policy is configured"
    );
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
        arguments: json!({ "command": "while :; do :; done" }),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
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
        "command process group was terminated, but detached descendants could not be verified"
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
            "command": "(sleep 0.2; printf leaked > descendant.txt) & wait"
        }),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_trigger = cancellation.clone();
    let trigger = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
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
    thread::sleep(Duration::from_millis(250));
    assert!(
        !test_workspace.path().join("descendant.txt").exists(),
        "cancelled descendants must not mutate the workspace later"
    );
}

#[test]
fn run_command_waits_for_an_approved_background_process_with_open_pipes() {
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

    assert!(started.elapsed() >= Duration::from_millis(75));
    assert!(started.elapsed() < Duration::from_secs(2));
    let ToolOutput::Success { metadata, .. } = result.output else {
        panic!("approved background command should run to completion");
    };
    assert_eq!(metadata["output_incomplete"], json!(false));
}

#[test]
fn run_command_reports_success_only_after_background_work_finishes() {
    let test_workspace = TestWorkspace::new("command-background-completion");
    let workspace = CodingWorkspace::resolve(test_workspace.path()).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("call-background-completion"),
        tool_name: "run_command".to_string(),
        arguments: json!({
            "command": "(sleep 0.1; printf complete > completed-background.txt) &"
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
    assert!(matches!(result.output, ToolOutput::Success { .. }));
    assert_eq!(
        std::fs::read_to_string(test_workspace.path().join("completed-background.txt")).unwrap(),
        "complete"
    );
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
            "command": "python3 -c 'import subprocess,time; subprocess.Popen([\"python3\", \"-c\", \"import time; open(\\\"escaped-ready\\\", \\\"w\\\").close(); time.sleep(0.6); open(\\\"escaped-after-cancel\\\", \\\"w\\\").write(\\\"survived\\\")\"], start_new_session=True, close_fds=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); time.sleep(10)'"
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
    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        std::fs::read_to_string(test_workspace.path().join("escaped-after-cancel")).unwrap(),
        "survived"
    );
}
