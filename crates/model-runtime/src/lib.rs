#![doc = "Provider-neutral model runtime boundary for the Agent Kernel."]

pub mod client;
pub mod stream;

pub use client::{ModelMessage, ModelMessageRole, ModelRequest, ModelToolSpec};
pub use stream::{ModelError, ModelStreamEvent, ModelUsage};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::client::{ModelMessage, ModelMessageRole, ModelRequest, ModelToolSpec};
    use crate::stream::{ModelError, ModelStreamEvent, ModelUsage};

    #[test]
    fn model_request_round_trips_without_provider_impl() {
        let request = ModelRequest {
            model: "qoder-coder".to_string(),
            messages: vec![ModelMessage {
                role: ModelMessageRole::User,
                content: "Read README.md and summarize it.".to_string(),
                name: None,
            }],
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
    }

    #[test]
    fn model_stream_events_round_trip_representative_payloads() {
        let events = vec![
            ModelStreamEvent::Started {
                provider_request_id: Some("qoder-request-001".to_string()),
            },
            ModelStreamEvent::TextDelta {
                delta: "I will inspect the file.".to_string(),
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
}
