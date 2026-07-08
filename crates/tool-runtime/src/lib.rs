#![doc = "Tool definition, policy, and execution boundary for the Agent Kernel."]

pub mod execution;
pub mod registry;

pub use execution::{ToolCall, ToolContent, ToolError, ToolOutput, ToolResult};
pub use registry::{CapabilityRef, McpCompatibility, ToolApprovalPolicy, ToolDefinition};

#[cfg(test)]
mod tests {
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
                metadata: json!({ "bytes": 14 }),
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
}
