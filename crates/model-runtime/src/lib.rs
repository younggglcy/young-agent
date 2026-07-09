#![doc = "Provider-neutral model runtime boundary for the Agent Kernel."]

pub mod client;
pub mod stream;

pub use client::{ModelMessage, ModelMessageRole, ModelRequest, ModelToolCall, ModelToolSpec};
pub use stream::{ModelError, ModelStreamEvent, ModelUsage};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::client::{
        ModelMessage, ModelMessageRole, ModelRequest, ModelToolCall, ModelToolSpec,
    };
    use crate::stream::{ModelError, ModelStreamEvent, ModelUsage};

    #[test]
    fn model_request_round_trips_without_provider_impl() {
        let request = ModelRequest {
            model: "qoder-coder".to_string(),
            messages: vec![ModelMessage::user("Read README.md and summarize it.")],
            tools: vec![ModelToolSpec {
                name: "read_file".to_string(),
                description: "Read a UTF-8 file in the workspace.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
            }],
            metadata: BTreeMap::from([("trace_id".to_string(), json!("run-001"))]),
        };

        let encoded = serde_json::to_string(&request).expect("request serializes");
        let decoded: ModelRequest = serde_json::from_str(&encoded).expect("request deserializes");

        assert_eq!(decoded, request);
        assert_eq!(decoded.messages[0].role(), ModelMessageRole::User);
    }

    #[test]
    fn model_request_preserves_tool_result_correlation() {
        let request = ModelRequest {
            model: "qoder-coder".to_string(),
            messages: vec![
                ModelMessage::assistant_with_tool_calls(
                    "I need to read README.md.",
                    vec![ModelToolCall {
                        id: "call-001".to_string(),
                        name: "read_file".to_string(),
                        arguments: json!({ "path": "README.md" }),
                    }],
                ),
                ModelMessage::tool("# Agent Kernel", "read_file", "call-001"),
            ],
            tools: Vec::new(),
            metadata: BTreeMap::new(),
        };

        let encoded = serde_json::to_value(&request).expect("request serializes");
        assert_eq!(
            encoded["messages"][0]["tool_calls"][0]["id"],
            json!("call-001")
        );
        assert_eq!(
            encoded["messages"][0]["tool_calls"][0]["name"],
            json!("read_file")
        );
        assert_eq!(
            encoded["messages"][0]["tool_calls"][0]["arguments"],
            json!({ "path": "README.md" })
        );
        assert_eq!(encoded["messages"][1]["tool_call_id"], json!("call-001"));

        let decoded: ModelRequest = serde_json::from_value(encoded).expect("request deserializes");
        assert_eq!(decoded, request);
    }

    #[test]
    fn model_request_serializes_representative_wire_payload() {
        let request = ModelRequest {
            model: "qoder-coder".to_string(),
            messages: vec![
                ModelMessage::system("You are a coding agent."),
                ModelMessage::user("Read README.md."),
                ModelMessage::assistant_with_tool_calls(
                    "I will read the file.",
                    vec![ModelToolCall {
                        id: "call-001".to_string(),
                        name: "read_file".to_string(),
                        arguments: json!({ "path": "README.md" }),
                    }],
                ),
                ModelMessage::tool("# Agent Kernel", "read_file", "call-001"),
            ],
            tools: vec![ModelToolSpec {
                name: "read_file".to_string(),
                description: "Read a UTF-8 file in the workspace.".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            metadata: BTreeMap::from([("trace_id".to_string(), json!("run-001"))]),
        };

        let encoded = serde_json::to_value(&request).expect("request serializes");

        assert_eq!(
            encoded,
            json!({
                "model": "qoder-coder",
                "messages": [
                    {
                        "role": "system",
                        "content": "You are a coding agent."
                    },
                    {
                        "role": "user",
                        "content": "Read README.md."
                    },
                    {
                        "role": "assistant",
                        "content": "I will read the file.",
                        "tool_calls": [
                            {
                                "id": "call-001",
                                "name": "read_file",
                                "arguments": { "path": "README.md" }
                            }
                        ]
                    },
                    {
                        "role": "tool",
                        "content": "# Agent Kernel",
                        "name": "read_file",
                        "tool_call_id": "call-001"
                    }
                ],
                "tools": [
                    {
                        "name": "read_file",
                        "description": "Read a UTF-8 file in the workspace.",
                        "input_schema": { "type": "object" }
                    }
                ],
                "metadata": {
                    "trace_id": "run-001"
                }
            })
        );
    }

    #[test]
    fn model_message_role_controls_allowed_wire_fields() {
        let user_message =
            serde_json::to_value(ModelMessage::user("hello")).expect("message serializes");
        assert_eq!(user_message["role"], json!("user"));
        assert!(user_message.get("name").is_none());
        assert!(user_message.get("tool_call_id").is_none());

        let tool_message = serde_json::to_value(ModelMessage::tool(
            "# Agent Kernel",
            "read_file",
            "call-001",
        ))
        .expect("message serializes");
        assert_eq!(tool_message["role"], json!("tool"));
        assert_eq!(tool_message["name"], json!("read_file"));
        assert_eq!(tool_message["tool_call_id"], json!("call-001"));

        let user_with_tool_fields = json!({
            "role": "user",
            "content": "hello",
            "name": "read_file",
            "tool_call_id": "call-001"
        });
        let missing_tool_call_id = json!({
            "role": "tool",
            "content": "# Agent Kernel",
            "name": "read_file"
        });
        let assistant_with_unknown_tool_call_field = json!({
            "role": "assistant",
            "content": "I will read the file.",
            "tool_calls": [
                {
                    "id": "call-001",
                    "name": "read_file",
                    "arguments": { "path": "README.md" },
                    "provider_only": true
                }
            ]
        });

        assert!(serde_json::from_value::<ModelMessage>(user_with_tool_fields).is_err());
        assert!(serde_json::from_value::<ModelMessage>(missing_tool_call_id).is_err());
        assert!(
            serde_json::from_value::<ModelMessage>(assistant_with_unknown_tool_call_field).is_err()
        );
    }

    #[test]
    fn model_stream_events_round_trip_representative_payloads() {
        let events = vec![
            ModelStreamEvent::Started {
                provider_request_id: Some("qoder-request-001".to_string()),
            },
            ModelStreamEvent::Started {
                provider_request_id: None,
            },
            ModelStreamEvent::TextDelta {
                delta: "I will inspect the file.".to_string(),
            },
            ModelStreamEvent::ToolCallDelta {
                id: "call-001".to_string(),
                name: None,
                arguments_delta: "{\"path\"".to_string(),
            },
            ModelStreamEvent::ToolCall {
                id: "call-001".to_string(),
                name: "read_file".to_string(),
                arguments: json!({ "path": "README.md" }),
            },
            ModelStreamEvent::Usage {
                usage: ModelUsage {
                    input_tokens: 120,
                    output_tokens: 32,
                },
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
            },
            ModelStreamEvent::Completed {
                finish_reason: None,
            },
            ModelStreamEvent::Failed {
                error: ModelError {
                    code: "provider_unavailable".to_string(),
                    message: "provider returned 503".to_string(),
                    retryable: true,
                },
            },
        ];

        let encoded = serde_json::to_value(&events).expect("stream events serialize");
        let decoded: Vec<ModelStreamEvent> =
            serde_json::from_value(encoded).expect("stream events deserialize");

        assert_eq!(decoded, events);
    }

    #[test]
    fn model_stream_event_serializes_representative_wire_payload() {
        let event = ModelStreamEvent::ToolCall {
            id: "call-001".to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };

        let encoded = serde_json::to_value(&event).expect("stream event serializes");

        assert_eq!(
            encoded,
            json!({
                "type": "tool_call",
                "id": "call-001",
                "name": "read_file",
                "arguments": {
                    "path": "README.md"
                }
            })
        );
    }

    #[test]
    fn model_contracts_reject_unknown_wire_fields() {
        let request_with_unknown_field = json!({
            "model": "qoder-coder",
            "messages": [],
            "provider_only": true
        });
        let tool_spec_with_unknown_field = json!({
            "name": "read_file",
            "description": "Read a UTF-8 file in the workspace.",
            "input_schema": { "type": "object" },
            "provider_only": true
        });
        let stream_event_with_unknown_field = json!({
            "type": "tool_call",
            "id": "call-001",
            "name": "read_file",
            "arguments": { "path": "README.md" },
            "provider_only": true
        });

        assert!(serde_json::from_value::<ModelRequest>(request_with_unknown_field).is_err());
        assert!(serde_json::from_value::<ModelToolSpec>(tool_spec_with_unknown_field).is_err());
        assert!(
            serde_json::from_value::<ModelStreamEvent>(stream_event_with_unknown_field).is_err()
        );
    }
}
