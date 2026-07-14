#![cfg(any(target_os = "macos", target_os = "linux"))]

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use common::TestDirectory;
use serde_json::json;
use young_agent_runtime::{
    AgentRuntime, ApprovalDecision, ApprovalRequest, RunControl, RunControlFlow, RunId, RunRequest,
};
use young_capability_coding::{register_builtin_coding_capability, CodingWorkspace};
use young_event_store::JsonlEventStore;
use young_model_runtime::{
    FakeModelClient, ModelMessage, ModelStreamEvent, ModelToolCallId, ScriptedModelTurn,
};
use young_tool_runtime::{ToolOutput, ToolRuntime};

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

#[test]
fn denied_mutating_command_is_not_executed_and_replays_its_decision() {
    let directory = TestDirectory::new("denied");
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
                delta: "Mutation was denied.".to_string(),
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
        .run_with_control(run_request("run-command-denied"), &mut DenyingControl)
        .expect("denied command should remain a replayable run");

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

    let workspace = CodingWorkspace::resolve(directory.path()).expect("workspace resolves");
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace).expect("capability registers");
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-command-001"),
                name: "run_command".to_string(),
                arguments: json!({
                    "command": "printf -v PATH .; git branch --show-current"
                }),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: BTreeMap::new(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Shell mutation was denied.".to_string(),
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
        .run_with_control(
            run_request("run-command-shell-variable-denied"),
            &mut DenyingControl,
        )
        .expect("denied command should remain replayable");

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
