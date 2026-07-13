use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use young_tool_runtime::{
    CapabilityRef, FakeToolExecutor, ToolApprovalPolicy, ToolCall, ToolCallId, ToolContent,
    ToolDefinition, ToolExecutionAuthorization, ToolExecutor, ToolOutput, ToolRuntime,
};

fn read_file_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_string(),
        description: "Read one workspace file.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }),
        output_schema: None,
        capability: CapabilityRef {
            id: "coding".to_string(),
            version: "0.1.0".to_string(),
        },
        approval_policy: ToolApprovalPolicy::AlwaysAllow,
        mcp: None,
    }
}

#[test]
fn registered_tool_can_be_looked_up_and_dispatched() {
    let output = ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "# young-agent".to_string(),
        }],
        metadata: BTreeMap::new(),
        extensions: BTreeMap::new(),
    };
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            read_file_definition(),
            FakeToolExecutor::new([output.clone()]),
        )
        .expect("tool registers");

    assert_eq!(runtime.lookup("read_file"), Some(&read_file_definition()));

    let call = ToolCall {
        id: ToolCallId::new("call-001"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };
    let result = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(result.call_id, call.id);
    assert_eq!(result.output, output);
}

#[test]
fn unknown_tool_returns_a_correlated_clear_failure() {
    let mut runtime = ToolRuntime::default();
    let call = ToolCall {
        id: ToolCallId::new("call-unknown"),
        tool_name: "missing_tool".to_string(),
        arguments: json!({}),
    };

    let result = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(result.call_id, call.id);
    let ToolOutput::Failure { error, extensions } = result.output else {
        panic!("unknown tool must fail");
    };
    assert_eq!(error.code, "unknown_tool");
    assert_eq!(error.message, "tool 'missing_tool' is not registered");
    assert!(!error.retryable);
    assert!(extensions.is_empty());
}

#[test]
fn duplicate_registration_fails_without_replacing_the_original_tool() {
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            read_file_definition(),
            FakeToolExecutor::new([ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "original".to_string(),
                }],
                metadata: BTreeMap::new(),
                extensions: BTreeMap::new(),
            }]),
        )
        .expect("first registration succeeds");

    let error = runtime
        .register(
            read_file_definition(),
            FakeToolExecutor::new([ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "replacement".to_string(),
                }],
                metadata: BTreeMap::new(),
                extensions: BTreeMap::new(),
            }]),
        )
        .expect_err("duplicate registration fails");

    assert_eq!(error.to_string(), "tool 'read_file' is already registered");
    assert_eq!(runtime.len(), 1);
}

#[test]
fn manifest_policy_drives_the_agent_runtime_approval_seam() {
    let mut definition = read_file_definition();
    definition.name = "apply_patch".to_string();
    definition.approval_policy = ToolApprovalPolicy::RequiresApproval {
        reason: "patching mutates workspace files".to_string(),
    };
    let mut runtime = ToolRuntime::default();
    runtime
        .register(definition, FakeToolExecutor::default())
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-approval"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({ "patch": "*** Begin Patch" }),
    };

    assert_eq!(
        runtime.approval_reason(&call).as_deref(),
        Some("patching mutates workspace files")
    );
}

#[test]
fn call_dependent_policy_delegates_to_the_registered_executor() {
    let mut definition = read_file_definition();
    definition.approval_policy = ToolApprovalPolicy::CallDependent;
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            definition,
            FakeToolExecutor::requiring_approval(
                "the concrete call crosses a dynamic safety boundary",
                [],
            ),
        )
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-dynamic-approval"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };

    assert_eq!(
        runtime.approval_reason(&call).as_deref(),
        Some("the concrete call crosses a dynamic safety boundary")
    );
}

#[test]
fn dispatch_cannot_bypass_a_required_approval() {
    let mut definition = read_file_definition();
    definition.approval_policy = ToolApprovalPolicy::RequiresApproval {
        reason: "reading this path requires approval".to_string(),
    };
    let expected = ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "secret".to_string(),
        }],
        metadata: BTreeMap::new(),
        extensions: BTreeMap::new(),
    };
    let mut runtime = ToolRuntime::default();
    runtime
        .register(definition, FakeToolExecutor::new([expected.clone()]))
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-protected"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "secrets.txt" }),
    };

    let denied = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );
    let ToolOutput::Failure { error, .. } = denied.output else {
        panic!("dispatch without approval must fail");
    };
    assert_eq!(error.code, "approval_required");
    assert_eq!(error.message, "reading this path requires approval");

    let mismatched = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: ToolCallId::new("another-call"),
        },
        Arc::new(AtomicBool::new(false)),
    );
    let ToolOutput::Failure { error, .. } = mismatched.output else {
        panic!("approval for another call must not authorize this dispatch");
    };
    assert_eq!(error.code, "approval_required");

    let approved = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(approved.output, expected);
}

#[test]
fn registered_tool_failure_is_propagated_without_losing_details() {
    let expected = ToolOutput::Failure {
        error: young_tool_runtime::ToolError {
            code: "outside_workspace".to_string(),
            message: "path escapes the workspace boundary".to_string(),
            retryable: false,
        },
        extensions: BTreeMap::from([("audit_id".to_string(), json!("audit-001"))]),
    };
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            read_file_definition(),
            FakeToolExecutor::new([expected.clone()]),
        )
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-failure"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "../outside" }),
    };

    let result = runtime.dispatch(
        &call,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(result.call_id, call.id);
    assert_eq!(result.output, expected);
}
