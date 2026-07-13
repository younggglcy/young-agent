use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use young_capability_coding::{coding_manifest, register_builtin_coding_capability};
use young_tool_runtime::{
    ToolApprovalPolicy, ToolCall, ToolCallId, ToolOutput, ToolRuntime, ToolSafetyClass,
};

#[test]
fn coding_capability_loads_its_embedded_builtin_manifest() {
    let manifest = coding_manifest().expect("embedded manifest is valid");

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.capability.id, "coding");
    assert_eq!(manifest.capability.version, "0.1.0");
    assert_eq!(manifest.capability.name, "Coding Capability");
    assert_eq!(
        manifest
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read_file", "search_files", "apply_patch", "run_command"]
    );
    assert_eq!(manifest.tools[0].safety_class, ToolSafetyClass::AlwaysAllow);
    assert_eq!(
        manifest.tools[2].safety_class,
        ToolSafetyClass::RequiresApproval
    );

    let definitions = manifest.tool_definitions();
    assert_eq!(
        definitions[0].approval_policy,
        ToolApprovalPolicy::AlwaysAllow
    );
    assert_eq!(
        definitions[0].mcp.as_ref().unwrap().server,
        "builtin-coding"
    );
    assert_eq!(
        definitions[3].approval_policy,
        ToolApprovalPolicy::RequiresApproval {
            reason: "commands may mutate the workspace or start processes".to_string(),
        }
    );
}

#[test]
fn coding_capability_registers_initial_tools_with_explicit_stubs() {
    let mut runtime = ToolRuntime::default();

    register_builtin_coding_capability(&mut runtime).expect("capability registers");

    assert_eq!(runtime.len(), 4);
    for name in ["read_file", "search_files", "apply_patch", "run_command"] {
        assert!(runtime.lookup(name).is_some(), "missing definition: {name}");
    }

    let call = ToolCall {
        id: ToolCallId::new("call-read"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "README.md" }),
    };
    let result = runtime.dispatch(&call, Arc::new(AtomicBool::new(false)));

    assert_eq!(result.call_id, call.id);
    let ToolOutput::Failure { error, .. } = result.output else {
        panic!("issue #7 owns the real coding tool implementations");
    };
    assert_eq!(error.code, "tool_not_implemented");
    assert_eq!(
        error.message,
        "coding tool 'read_file' is declared but not implemented"
    );
    assert!(!error.retryable);
}
