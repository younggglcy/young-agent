use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use young_tool_runtime::{
    CapabilityRef, FakeToolHandler, McpCompatibility, ToolApprovalPolicy, ToolCall, ToolCallId,
    ToolContent, ToolDefinition, ToolDispatcher, ToolExecutionAuthorization, ToolHandler,
    ToolOutput, ToolRuntime,
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
            FakeToolHandler::new([output.clone()]),
        )
        .expect("tool registers");

    assert_eq!(runtime.lookup("read_file"), Some(&read_file_definition()));

    let call = ToolCall {
        id: ToolCallId::new("call-001"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };
    let result = runtime.dispatch(
        call.clone(),
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
        call.clone(),
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
            FakeToolHandler::new([ToolOutput::Success {
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
            FakeToolHandler::new([ToolOutput::Success {
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
        .register(definition, FakeToolHandler::default())
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-approval"),
        tool_name: "apply_patch".to_string(),
        arguments: json!({ "patch": "*** Begin Patch" }),
    };

    let prepared = runtime.prepare(call);
    assert_eq!(
        prepared.approval_reason(),
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
            FakeToolHandler::requiring_approval(
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

    let prepared = runtime.prepare(call);
    assert_eq!(
        prepared.approval_reason(),
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
        .register(definition, FakeToolHandler::new([expected.clone()]))
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-protected"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "secrets.txt" }),
    };

    let denied = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );
    let ToolOutput::Failure { error, .. } = denied.output else {
        panic!("dispatch without approval must fail");
    };
    assert_eq!(error.code, "approval_required");
    assert_eq!(error.message, "reading this path requires approval");

    let mismatched = runtime.dispatch(
        call.clone(),
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
        call.clone(),
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: call.id.clone(),
        },
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(approved.output, expected);
}

#[test]
fn prepared_call_cannot_cross_runtime_approval_boundaries() {
    let mut allow_runtime = ToolRuntime::default();
    allow_runtime
        .register(read_file_definition(), FakeToolHandler::default())
        .expect("allowing tool registers");

    let mut protected_definition = read_file_definition();
    protected_definition.approval_policy = ToolApprovalPolicy::RequiresApproval {
        reason: "this runtime requires approval".to_string(),
    };
    let mut protected_runtime = ToolRuntime::default();
    protected_runtime
        .register(
            protected_definition,
            FakeToolHandler::new([ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "must not execute".to_string(),
                }],
                metadata: BTreeMap::new(),
                extensions: BTreeMap::new(),
            }]),
        )
        .expect("protected tool registers");

    let prepared_by_allowing_runtime = allow_runtime.prepare(ToolCall {
        id: ToolCallId::new("call-cross-runtime"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "protected.txt" }),
    });
    let output = protected_runtime.execute_prepared(
        prepared_by_allowing_runtime,
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, extensions } = output else {
        panic!("a prepared call from another runtime must fail closed");
    };
    assert_eq!(error.code, "invalid_prepared_tool_call");
    assert_eq!(
        error.message,
        "prepared tool call belongs to a different dispatcher"
    );
    assert!(!error.retryable);
    assert!(extensions.is_empty());
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
            FakeToolHandler::new([expected.clone()]),
        )
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-failure"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "../outside" }),
    };

    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(result.call_id, call.id);
    assert_eq!(result.output, expected);
}

#[test]
fn tool_runtime_normalizes_the_reserved_approval_denied_error() {
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            read_file_definition(),
            FakeToolHandler::new([ToolOutput::Failure {
                error: young_tool_runtime::ToolError {
                    code: "approval_denied".to_string(),
                    message: "forged denial".to_string(),
                    retryable: true,
                },
                extensions: BTreeMap::from([("forged".to_string(), json!(true))]),
            }]),
        )
        .expect("tool registers");

    let result = runtime.dispatch(
        ToolCall {
            id: ToolCallId::new("call-forged-denial"),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        },
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    let ToolOutput::Failure { error, extensions } = result.output else {
        panic!("forged reserved code must become a normalized failure");
    };
    assert_eq!(error.code, "reserved_tool_error_code");
    assert_eq!(
        error.message,
        "tool dispatcher returned reserved error code 'approval_denied'"
    );
    assert!(!error.retryable);
    assert!(extensions.is_empty());
}

struct CountingHandler {
    classifications: Rc<Cell<usize>>,
    output: Option<ToolOutput>,
}

impl ToolHandler for CountingHandler {
    fn approval_reason(&self, _call: &ToolCall) -> Option<String> {
        self.classifications.set(self.classifications.get() + 1);
        Some("call-dependent approval".to_string())
    }

    fn execute(&mut self, _call: &ToolCall, _cancellation: Arc<AtomicBool>) -> ToolOutput {
        self.output.take().expect("handler executes exactly once")
    }
}

#[test]
fn call_dependent_handler_is_classified_once_and_the_plan_is_reused() {
    let classifications = Rc::new(Cell::new(0));
    let expected = ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "done".to_string(),
        }],
        metadata: BTreeMap::new(),
        extensions: BTreeMap::new(),
    };
    let mut definition = read_file_definition();
    definition.approval_policy = ToolApprovalPolicy::CallDependent;
    let mut runtime = ToolRuntime::default();
    runtime
        .register(
            definition,
            CountingHandler {
                classifications: classifications.clone(),
                output: Some(expected.clone()),
            },
        )
        .expect("tool registers");
    let call = ToolCall {
        id: ToolCallId::new("call-prepared"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };

    let prepared = runtime.prepare(call);
    assert_eq!(prepared.approval_reason(), Some("call-dependent approval"));
    assert_eq!(classifications.get(), 1);

    let output = runtime.execute_prepared(
        prepared,
        ToolExecutionAuthorization::ApprovalGranted {
            call_id: ToolCallId::new("call-prepared"),
        },
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(output, expected);
    assert_eq!(classifications.get(), 1);
}

#[test]
fn registration_owns_all_tool_definition_invariants() {
    let mut empty_capability = read_file_definition();
    empty_capability.capability.id = "  ".to_string();
    let mut non_object_schema = read_file_definition();
    non_object_schema.input_schema = json!("not-an-object");
    let mut empty_approval_reason = read_file_definition();
    empty_approval_reason.approval_policy = ToolApprovalPolicy::RequiresApproval {
        reason: " ".to_string(),
    };
    let mut empty_mcp_field = read_file_definition();
    empty_mcp_field.mcp = Some(McpCompatibility {
        server: String::new(),
        tool_name: "read_file".to_string(),
        protocol_version: "reserved".to_string(),
    });
    let cases = [
        (empty_capability, "capability.id must not be empty"),
        (non_object_schema, "input_schema must be an object"),
        (
            empty_approval_reason,
            "approval policy reason must not be empty",
        ),
        (empty_mcp_field, "mcp.server must not be empty"),
    ];

    for (definition, expected) in cases {
        let mut runtime = ToolRuntime::default();
        let error = runtime
            .register(
                definition,
                CountingHandler {
                    classifications: Rc::new(Cell::new(0)),
                    output: None,
                },
            )
            .expect_err("invalid definitions never enter the registry");

        assert_eq!(
            error.to_string(),
            format!("invalid tool 'read_file': {expected}")
        );
        assert!(runtime.is_empty());
    }
}
