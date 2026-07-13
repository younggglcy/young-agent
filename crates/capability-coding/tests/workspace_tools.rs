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
    assert_eq!(text.len(), 64 * 1024 - 1);
    assert_eq!(metadata["bytes"], json!(64 * 1024 + 6));
    assert_eq!(metadata["returned_bytes"], json!(64 * 1024 - 1));
    assert_eq!(metadata["truncated"], json!(true));
    assert_eq!(metadata["truncation_limit_bytes"], json!(64 * 1024));
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
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({ "changed_files": ["notes.txt"] }),
        }]
    );
    assert_eq!(metadata["files_changed"], json!(1));
    assert!(extensions.is_empty());
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
fn apply_patch_can_create_and_delete_files_in_one_validated_patch() {
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

    let ToolOutput::Success { content, .. } = result.output else {
        panic!("create/delete patch should succeed");
    };
    assert_eq!(
        std::fs::read_to_string(test_workspace.path().join("new.txt"))
            .expect("new file is readable"),
        "created\nfile\n"
    );
    assert!(!old.exists());
    assert_eq!(
        content,
        vec![ToolContent::Json {
            value: json!({ "changed_files": ["new.txt", "old.txt"] }),
        }]
    );
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
    assert_eq!(value["stdout"].as_str().unwrap().len(), 64 * 1024);
    assert_eq!(metadata["stdout_bytes"], json!(70_000));
    assert_eq!(metadata["stdout_truncated"], json!(true));
    assert_eq!(metadata["output_incomplete"], json!(false));
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
    assert_eq!(error.code, "tool_cancelled");
}
