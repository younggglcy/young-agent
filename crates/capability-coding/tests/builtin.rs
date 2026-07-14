use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use young_capability_coding::{
    coding_manifest, register_builtin_coding_capability, CodingWorkspace,
};
use young_tool_runtime::{
    ToolApprovalPolicy, ToolCall, ToolCallId, ToolExecutionAuthorization, ToolOutput, ToolRuntime,
    ToolSafetyClass,
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
    assert_eq!(
        manifest.tools[3].safety_class,
        ToolSafetyClass::CallDependent
    );

    let definitions = manifest
        .tool_definitions()
        .expect("embedded manifest produces valid definitions");
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
        ToolApprovalPolicy::CallDependent
    );
}

#[test]
fn coding_capability_registers_implementations_for_a_selected_workspace() {
    let mut runtime = ToolRuntime::default();
    let workspace =
        CodingWorkspace::resolve(env!("CARGO_MANIFEST_DIR")).expect("workspace resolves");

    register_builtin_coding_capability(&mut runtime, workspace).expect("capability registers");

    assert_eq!(runtime.len(), 4);
    for name in ["read_file", "search_files", "apply_patch", "run_command"] {
        assert!(runtime.lookup(name).is_some(), "missing definition: {name}");
    }

    let call = ToolCall {
        id: ToolCallId::new("call-read"),
        tool_name: "read_file".to_string(),
        arguments: json!({ "path": "Cargo.toml" }),
    };
    let result = runtime.dispatch(
        call.clone(),
        ToolExecutionAuthorization::NotRequired,
        Arc::new(AtomicBool::new(false)),
    );

    assert_eq!(result.call_id, call.id);
    let ToolOutput::Success { content, .. } = result.output else {
        panic!("read_file should use its real implementation");
    };
    let [young_tool_runtime::ToolContent::Text { text }] = content.as_slice() else {
        panic!("read_file should return one text content item");
    };
    assert!(text.contains("name = \"young-capability-coding\""));
}
