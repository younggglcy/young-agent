#![cfg(not(any(target_os = "macos", target_os = "linux")))]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use young_capability_coding::{register_builtin_coding_capability, CodingWorkspace};
use young_tool_runtime::{
    ToolCall, ToolCallId, ToolExecutionAuthorization, ToolOutput, ToolRuntime,
};

struct TestWorkspace(PathBuf);

impl TestWorkspace {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-capability-coding-unsupported-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("test workspace is created");
        Self(root)
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn existing_file_patch_fails_closed_when_metadata_validation_is_unsupported() {
    let root = TestWorkspace::new();
    std::fs::write(root.0.join("notes.txt"), "old\n").expect("fixture is written");
    let workspace = CodingWorkspace::resolve(&root.0).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("unsupported-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+new\n"
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
        panic!("unsupported replacement metadata must fail closed");
    };
    assert_eq!(error.code, "unsupported_file_metadata");
    assert_eq!(
        std::fs::read_to_string(root.0.join("notes.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn new_file_patch_fails_before_creating_staging_on_unsupported_platforms() {
    let root = TestWorkspace::new();
    let workspace = CodingWorkspace::resolve(&root.0).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("unsupported-new-file-patch"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "patch": "--- /dev/null\n+++ b/created.txt\n@@ -0,0 +1 @@\n+created\n"
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
        panic!("unsupported new-file patch must fail closed");
    };
    assert_eq!(error.code, "unsupported_file_metadata");
    assert!(!root.0.join("created.txt").exists());
    assert!(std::fs::read_dir(&root.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".young-agent-patch-")
    }));
}

#[cfg(not(unix))]
#[test]
fn command_fails_closed_without_handle_bound_working_directories() {
    let root = TestWorkspace::new();
    let workspace = CodingWorkspace::resolve(&root.0).expect("workspace resolves");
    let mut runtime = ToolRuntime::default();
    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");
    let call = ToolCall {
        id: ToolCallId::new("unsupported-command"),
        tool_name: "run_command".to_string(),
        arguments: json!({ "command": "echo unsafe" }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("unsupported command cwd binding must fail closed");
    };
    assert_eq!(error.code, "workspace_changed");
}
