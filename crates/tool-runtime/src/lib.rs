#![doc = "Tool definition, policy, and execution boundary for the Agent Kernel."]

pub mod execution;
pub mod registry;

pub use execution::{ToolCall, ToolContent, ToolError, ToolOutput, ToolResult};
pub use registry::{CapabilityRef, McpCompatibility, ToolApprovalPolicy, ToolDefinition};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::execution::{ToolCall, ToolContent, ToolError, ToolOutput, ToolResult};
    use crate::registry::{CapabilityRef, McpCompatibility, ToolApprovalPolicy, ToolDefinition};

    #[test]
    fn tool_definition_round_trips_with_mcp_compatibility_metadata() {
        let definition = ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a UTF-8 file inside the workspace boundary.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
                }
            })),
            capability: CapabilityRef {
                id: "coding".to_string(),
                version: "0.1.0".to_string(),
            },
            approval_policy: ToolApprovalPolicy::AlwaysAllow,
            mcp: Some(McpCompatibility {
                server: "builtin-coding".to_string(),
                tool_name: "read_file".to_string(),
                protocol_version: "reserved".to_string(),
            }),
        };

        let encoded = serde_json::to_string_pretty(&definition).expect("definition serializes");
        let decoded: ToolDefinition =
            serde_json::from_str(&encoded).expect("definition deserializes");

        assert_eq!(decoded, definition);
    }

    #[test]
    fn tool_call_result_and_error_round_trip() {
        let call = ToolCall {
            id: "call-001".to_string(),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };
        let success = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "# Agent Kernel".to_string(),
                }],
                metadata: BTreeMap::from([("bytes".to_string(), json!(14))]),
            },
        };
        let failure = ToolResult {
            call_id: "call-002".to_string(),
            output: ToolOutput::Failure {
                error: ToolError {
                    code: "outside_workspace".to_string(),
                    message: "path escapes the workspace boundary".to_string(),
                    retryable: false,
                },
            },
        };

        let encoded =
            serde_json::to_value((&call, &success, &failure)).expect("payloads serialize");
        let decoded: (ToolCall, ToolResult, ToolResult) =
            serde_json::from_value(encoded).expect("payloads deserialize");

        assert_eq!(decoded, (call, success, failure));
    }

    #[test]
    fn empty_tool_metadata_is_omitted_from_success_output() {
        let output = ToolOutput::Success {
            content: vec![ToolContent::Json {
                value: json!({ "ok": true }),
            }],
            metadata: BTreeMap::new(),
        };

        let encoded = serde_json::to_value(&output).expect("output serializes");

        assert!(encoded.get("metadata").is_none());
    }

    #[test]
    fn tool_definition_optional_fields_and_policy_variants_round_trip() {
        let requires_approval = ToolDefinition {
            name: "run_command".to_string(),
            description: "Run a command in the workspace.".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            capability: CapabilityRef {
                id: "coding".to_string(),
                version: "0.1.0".to_string(),
            },
            approval_policy: ToolApprovalPolicy::RequiresApproval {
                reason: "command may mutate the workspace".to_string(),
            },
            mcp: None,
        };
        let always_reject = ToolDefinition {
            name: "delete_workspace".to_string(),
            description: "Rejected destructive operation.".to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: None,
            capability: CapabilityRef {
                id: "coding".to_string(),
                version: "0.1.0".to_string(),
            },
            approval_policy: ToolApprovalPolicy::AlwaysReject {
                reason: "outside the first-phase safety boundary".to_string(),
            },
            mcp: None,
        };

        let encoded = serde_json::to_value((&requires_approval, &always_reject))
            .expect("definitions serialize");
        assert!(encoded[0].get("output_schema").is_none());
        assert!(encoded[0].get("mcp").is_none());

        let decoded: (ToolDefinition, ToolDefinition) =
            serde_json::from_value(encoded).expect("definitions deserialize");
        assert_eq!(decoded, (requires_approval, always_reject));
    }
}
