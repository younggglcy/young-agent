#![doc = "Tool definition, policy, and execution boundary for the Agent Kernel."]

pub mod execution;
pub mod registry;

pub use execution::{ToolCall, ToolCallId, ToolContent, ToolError, ToolOutput, ToolResult};
pub use registry::{CapabilityRef, McpCompatibility, ToolApprovalPolicy, ToolDefinition};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::execution::{ToolCall, ToolCallId, ToolContent, ToolError, ToolOutput, ToolResult};
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
    fn tool_definition_serializes_representative_wire_payload() {
        let definition = ToolDefinition {
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
            mcp: Some(McpCompatibility {
                server: "builtin-coding".to_string(),
                tool_name: "run_command".to_string(),
                protocol_version: "reserved".to_string(),
            }),
        };

        let encoded = serde_json::to_value(&definition).expect("definition serializes");

        assert_eq!(
            encoded,
            json!({
                "name": "run_command",
                "description": "Run a command in the workspace.",
                "input_schema": { "type": "object" },
                "capability": {
                    "id": "coding",
                    "version": "0.1.0"
                },
                "approval_policy": {
                    "policy": "requires_approval",
                    "reason": "command may mutate the workspace"
                },
                "mcp": {
                    "server": "builtin-coding",
                    "tool_name": "run_command",
                    "protocol_version": "reserved"
                }
            })
        );
    }

    #[test]
    fn tool_call_result_and_error_round_trip() {
        let call = ToolCall {
            id: ToolCallId::new("call-001"),
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
                extensions: BTreeMap::new(),
            },
        };
        let failure = ToolResult {
            call_id: ToolCallId::new("call-002"),
            output: ToolOutput::Failure {
                error: ToolError {
                    code: "outside_workspace".to_string(),
                    message: "path escapes the workspace boundary".to_string(),
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            },
        };

        let encoded =
            serde_json::to_value((&call, &success, &failure)).expect("payloads serialize");
        let decoded: (ToolCall, ToolResult, ToolResult) =
            serde_json::from_value(encoded).expect("payloads deserialize");

        assert_eq!(decoded, (call, success, failure));
    }

    #[test]
    fn tool_result_serializes_representative_wire_payload() {
        let result = ToolResult {
            call_id: ToolCallId::new("call-001"),
            output: ToolOutput::Success {
                content: vec![
                    ToolContent::Text {
                        text: "# Agent Kernel".to_string(),
                    },
                    ToolContent::Json {
                        value: json!({ "ok": true }),
                    },
                ],
                metadata: BTreeMap::from([("bytes".to_string(), json!(14))]),
                extensions: BTreeMap::new(),
            },
        };

        let encoded = serde_json::to_value(&result).expect("result serializes");

        assert_eq!(
            encoded,
            json!({
                "call_id": "call-001",
                "output": {
                    "status": "success",
                    "content": [
                        {
                            "type": "text",
                            "text": "# Agent Kernel"
                        },
                        {
                            "type": "json",
                            "value": { "ok": true }
                        }
                    ],
                    "metadata": {
                        "bytes": 14
                    }
                }
            })
        );
    }

    #[test]
    fn tool_contracts_reject_unknown_wire_fields() {
        let call_with_unknown_field = json!({
            "id": "call-001",
            "tool_name": "read_file",
            "arguments": { "path": "README.md" },
            "future_hint": true
        });
        let result_with_unknown_field = json!({
            "call_id": "call-001",
            "output": {
                "status": "success",
                "content": [
                    {
                        "type": "text",
                        "text": "# Agent Kernel",
                        "mime_type": "text/markdown"
                    }
                ]
            }
        });
        let definition_with_unknown_field = json!({
            "name": "read_file",
            "description": "Read a UTF-8 file inside the workspace boundary.",
            "input_schema": { "type": "object" },
            "capability": {
                "id": "coding",
                "version": "0.1.0"
            },
            "approval_policy": {
                "policy": "always_allow"
            },
            "x-provider": true
        });

        assert!(serde_json::from_value::<ToolCall>(call_with_unknown_field).is_err());
        assert!(serde_json::from_value::<ToolResult>(result_with_unknown_field).is_err());
        assert!(serde_json::from_value::<ToolDefinition>(definition_with_unknown_field).is_err());
    }

    #[test]
    fn tool_output_envelopes_preserve_additive_fields() {
        let output_with_additive_field = json!({
            "status": "success",
            "content": [
                {
                    "type": "text",
                    "text": "# Agent Kernel"
                }
            ],
            "metadata": {
                "bytes": 14
            },
            "producer_hint": "display_as_markdown"
        });

        let decoded: ToolOutput =
            serde_json::from_value(output_with_additive_field).expect("output deserializes");
        let reencoded = serde_json::to_value(&decoded).expect("output serializes");

        assert_eq!(
            decoded,
            ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "# Agent Kernel".to_string(),
                }],
                metadata: BTreeMap::from([("bytes".to_string(), json!(14))]),
                extensions: BTreeMap::from([(
                    "producer_hint".to_string(),
                    json!("display_as_markdown")
                )]),
            }
        );
        assert_eq!(reencoded["producer_hint"], json!("display_as_markdown"));
    }

    #[test]
    fn empty_tool_metadata_is_omitted_from_success_output() {
        let output = ToolOutput::Success {
            content: vec![ToolContent::Json {
                value: json!({ "ok": true }),
            }],
            metadata: BTreeMap::new(),
            extensions: BTreeMap::new(),
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
