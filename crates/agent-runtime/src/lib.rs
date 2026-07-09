#![doc = "Agent Run orchestration boundary for the Agent Kernel."]

pub mod run;
pub mod turn;

pub use run::{AgentError, AgentEvent, ApprovalRequest, RunId, RunStatus};
pub use turn::TurnId;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use young_model_runtime::client::ModelMessage;
    use young_model_runtime::stream::{ModelStreamEvent, ModelUsage};
    use young_tool_runtime::execution::{ToolCall, ToolContent, ToolOutput, ToolResult};

    use crate::run::{AgentError, AgentEvent, ApprovalRequest, RunId, RunStatus};
    use crate::turn::TurnId;

    #[test]
    fn agent_events_round_trip_across_surface_visible_states() {
        let run_id = RunId::new("run-001");
        let turn_id = TurnId::new("turn-001");
        let call = ToolCall {
            id: "call-001".to_string(),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "# Agent Kernel".to_string(),
                }],
                metadata: BTreeMap::from([("bytes".to_string(), json!(14))]),
            },
        };
        let error = AgentError {
            code: "model_failed".to_string(),
            message: "model stream ended with an error".to_string(),
            recoverable: true,
        };

        let events = vec![
            AgentEvent::RunStarted {
                run_id: run_id.clone(),
            },
            AgentEvent::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
            },
            AgentEvent::ModelOutput {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event: ModelStreamEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: 120,
                        output_tokens: 32,
                    },
                },
            },
            AgentEvent::ToolCallRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                call: call.clone(),
            },
            AgentEvent::ApprovalRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                request: ApprovalRequest {
                    id: "approval-001".to_string(),
                    call: call.clone(),
                    reason: "command mutates the workspace".to_string(),
                },
            },
            AgentEvent::ToolResult {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                result,
            },
            AgentEvent::Error {
                run_id: run_id.clone(),
                turn_id: Some(turn_id),
                error: error.clone(),
            },
            AgentEvent::RunFinished {
                run_id: run_id.clone(),
                status: RunStatus::Completed {
                    final_message: "Done".to_string(),
                },
            },
        ];

        let encoded = serde_json::to_string(&events).expect("agent events serialize");
        let decoded: Vec<AgentEvent> =
            serde_json::from_str(&encoded).expect("agent events deserialize");

        assert_eq!(decoded, events);
    }

    #[test]
    fn run_status_variants_round_trip_with_terminal_reasons() {
        let statuses = vec![
            RunStatus::Running,
            RunStatus::AwaitingApproval,
            RunStatus::Completed {
                final_message: "Done".to_string(),
            },
            RunStatus::Failed {
                error: AgentError {
                    code: "model_failed".to_string(),
                    message: "model stream ended with an error".to_string(),
                    recoverable: false,
                },
            },
            RunStatus::Interrupted {
                reason: "user paused the run".to_string(),
            },
            RunStatus::Cancelled {
                reason: "user cancelled the run".to_string(),
            },
        ];

        let encoded = serde_json::to_value(&statuses).expect("statuses serialize");
        let decoded: Vec<RunStatus> =
            serde_json::from_value(encoded).expect("statuses deserialize");

        assert_eq!(decoded, statuses);
    }

    #[test]
    fn tool_invocation_id_is_shared_across_runtime_contracts() {
        let call_id = "call-001";
        let model_tool_call = ModelStreamEvent::ToolCall {
            id: call_id.to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };
        let tool_call = ToolCall {
            id: call_id.to_string(),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };
        let tool_result = ToolResult {
            call_id: call_id.to_string(),
            output: ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "# Agent Kernel".to_string(),
                }],
                metadata: BTreeMap::new(),
            },
        };
        let tool_message = ModelMessage::tool("# Agent Kernel", "read_file", call_id);

        let encoded =
            serde_json::to_value((&model_tool_call, &tool_call, &tool_result, &tool_message))
                .expect("runtime payloads serialize");
        let decoded: (ModelStreamEvent, ToolCall, ToolResult, ModelMessage) =
            serde_json::from_value(encoded).expect("runtime payloads deserialize");

        assert_eq!(
            decoded,
            (model_tool_call, tool_call, tool_result, tool_message)
        );
        assert_eq!(decoded.1.id, decoded.2.call_id);
        match decoded.3 {
            ModelMessage::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, decoded.2.call_id),
            _ => panic!("expected tool result message"),
        }
    }
}
