#![doc = "Agent Run orchestration boundary for the Agent Kernel."]

pub mod run;
pub mod runtime;
pub mod turn;

pub use run::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalRequest, EventSequence, RunId, RunStatus,
    TerminalRunStatus,
};
pub use runtime::{
    AgentEventSink, AgentRuntime, AgentRuntimeError, EventDurability, RunControl, RunControlFlow,
    RunOutcome, RunRequest, RunStopToken,
};
pub use turn::TurnId;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use young_model_runtime::client::ModelMessage;
    use young_model_runtime::stream::ModelStreamEvent;
    use young_model_runtime::{ModelToolCallId, ModelUsage};
    use young_tool_runtime::execution::{
        ToolCall, ToolCallId, ToolContent, ToolOutput, ToolResult,
    };

    use crate::run::{
        AgentError, AgentEvent, ApprovalDecision, ApprovalRequest, RunId, RunStatus,
        TerminalRunStatus,
    };
    use crate::turn::TurnId;

    #[test]
    fn agent_events_round_trip_across_surface_visible_states() {
        let run_id = RunId::new("run-001");
        let turn_id = TurnId::new("turn-001");
        let call = ToolCall {
            id: ToolCallId::new("call-001"),
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
                extensions: BTreeMap::new(),
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
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ModelOutput {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event: ModelStreamEvent::Usage {
                    usage: ModelUsage {
                        input_tokens: 120,
                        output_tokens: 32,
                    },
                    extensions: BTreeMap::new(),
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ToolCallRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                model_tool_call_id: ModelToolCallId::new("model-call-001"),
                call: call.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ApprovalRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                request: ApprovalRequest {
                    id: "approval-001".to_string(),
                    call: call.clone(),
                    reason: "command mutates the workspace".to_string(),
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ToolResult {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                result,
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::Error {
                run_id: run_id.clone(),
                turn_id: Some(turn_id),
                error: error.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::RunFinished {
                run_id: run_id.clone(),
                status: TerminalRunStatus::Completed {
                    final_message: "Done".to_string(),
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
        ];

        let encoded = serde_json::to_string(&events).expect("agent events serialize");
        let decoded: Vec<AgentEvent> =
            serde_json::from_str(&encoded).expect("agent events deserialize");

        assert_eq!(decoded, events);
    }

    #[test]
    fn agent_events_serialize_representative_turn_wire_payload() {
        let run_id = RunId::new("run-001");
        let turn_id = TurnId::new("turn-001");
        let model_call_id = ModelToolCallId::new("model-call-001");
        let tool_call_id = ToolCallId::new("tool-call-001");
        let tool_call = ToolCall {
            id: tool_call_id.clone(),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };

        let events = vec![
            AgentEvent::RunStarted {
                run_id: run_id.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ModelOutput {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                event: ModelStreamEvent::ToolCall {
                    id: model_call_id.clone(),
                    name: "read_file".to_string(),
                    arguments: json!({ "path": "README.md" }),
                    extensions: BTreeMap::new(),
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ToolCallRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                model_tool_call_id: model_call_id,
                call: tool_call.clone(),
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ApprovalRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                request: ApprovalRequest {
                    id: "approval-001".to_string(),
                    call: tool_call.clone(),
                    reason: "command may read workspace files".to_string(),
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
            AgentEvent::ToolResult {
                run_id,
                turn_id,
                result: ToolResult {
                    call_id: tool_call_id,
                    output: ToolOutput::Success {
                        content: vec![ToolContent::Text {
                            text: "# Agent Kernel".to_string(),
                        }],
                        metadata: BTreeMap::new(),
                        extensions: BTreeMap::from([("display".to_string(), json!("markdown"))]),
                    },
                },
                event_sequence: None,
                extensions: BTreeMap::new(),
            },
        ];

        let encoded = serde_json::to_value(&events).expect("events serialize");

        assert_eq!(
            encoded,
            json!([
                {
                    "type": "run_started",
                    "run_id": "run-001"
                },
                {
                    "type": "turn_started",
                    "run_id": "run-001",
                    "turn_id": "turn-001"
                },
                {
                    "type": "model_output",
                    "run_id": "run-001",
                    "turn_id": "turn-001",
                    "event": {
                        "type": "tool_call",
                        "id": "model-call-001",
                        "name": "read_file",
                        "arguments": { "path": "README.md" }
                    }
                },
                {
                    "type": "tool_call_requested",
                    "run_id": "run-001",
                    "turn_id": "turn-001",
                    "model_tool_call_id": "model-call-001",
                    "call": {
                        "id": "tool-call-001",
                        "tool_name": "read_file",
                        "arguments": { "path": "README.md" }
                    }
                },
                {
                    "type": "approval_requested",
                    "run_id": "run-001",
                    "turn_id": "turn-001",
                    "request": {
                        "id": "approval-001",
                        "call": {
                            "id": "tool-call-001",
                            "tool_name": "read_file",
                            "arguments": { "path": "README.md" }
                        },
                        "reason": "command may read workspace files"
                    }
                },
                {
                    "type": "tool_result",
                    "run_id": "run-001",
                    "turn_id": "turn-001",
                    "result": {
                        "call_id": "tool-call-001",
                        "output": {
                            "status": "success",
                            "content": [
                                {
                                    "type": "text",
                                    "text": "# Agent Kernel"
                                }
                            ],
                            "extensions": {
                                "display": "markdown"
                            }
                        }
                    }
                }
            ])
        );
    }

    #[test]
    fn run_status_variants_round_trip_with_terminal_reasons() {
        let statuses = vec![
            RunStatus::Running,
            RunStatus::AwaitingApproval,
            RunStatus::RecoveryRequired {
                call_ids: vec![ToolCallId::new("tool-call-001")],
            },
            RunStatus::Finished {
                terminal_status: TerminalRunStatus::Completed {
                    final_message: "Done".to_string(),
                },
            },
        ];

        let encoded = serde_json::to_value(&statuses).expect("statuses serialize");
        let decoded: Vec<RunStatus> =
            serde_json::from_value(encoded).expect("statuses deserialize");

        assert_eq!(decoded, statuses);
    }

    #[test]
    fn approval_resolution_serializes_a_replayable_wire_payload() {
        let event = AgentEvent::ApprovalResolved {
            run_id: RunId::new("run-001"),
            turn_id: TurnId::new("turn-001"),
            approval_id: "approval-001".to_string(),
            decision: ApprovalDecision::Deny {
                reason: "user denied the command".to_string(),
            },
            event_sequence: None,
            extensions: BTreeMap::new(),
        };

        assert_eq!(
            serde_json::to_value(event).expect("approval resolution serializes"),
            json!({
                "type": "approval_resolved",
                "run_id": "run-001",
                "turn_id": "turn-001",
                "approval_id": "approval-001",
                "decision": {
                    "decision": "deny",
                    "reason": "user denied the command"
                }
            })
        );
    }

    #[test]
    fn terminal_run_status_variants_round_trip_with_final_reasons() {
        let statuses = vec![
            TerminalRunStatus::Completed {
                final_message: "Done".to_string(),
            },
            TerminalRunStatus::Failed {
                error: AgentError {
                    code: "model_failed".to_string(),
                    message: "model stream ended with an error".to_string(),
                    recoverable: false,
                },
            },
            TerminalRunStatus::Interrupted {
                reason: "user paused the run".to_string(),
            },
            TerminalRunStatus::Cancelled {
                reason: "user cancelled the run".to_string(),
            },
        ];

        let encoded = serde_json::to_value(&statuses).expect("terminal statuses serialize");
        let decoded: Vec<TerminalRunStatus> =
            serde_json::from_value(encoded).expect("terminal statuses deserialize");

        assert_eq!(decoded, statuses);
    }

    #[test]
    fn run_status_payloads_reject_unknown_fields() {
        let run_status_with_unknown_field = json!({
            "status": "finished",
            "terminal_status": {
                "status": "completed",
                "final_message": "Done"
            },
            "future_hint": true
        });
        let terminal_status_with_unknown_field = json!({
            "status": "completed",
            "final_message": "Done",
            "future_hint": true
        });

        assert!(serde_json::from_value::<RunStatus>(run_status_with_unknown_field).is_err());
        assert!(
            serde_json::from_value::<TerminalRunStatus>(terminal_status_with_unknown_field)
                .is_err()
        );
    }

    #[test]
    fn run_finished_serializes_only_terminal_statuses() {
        let event = AgentEvent::RunFinished {
            run_id: RunId::new("run-001"),
            status: TerminalRunStatus::Interrupted {
                reason: "user paused the run".to_string(),
            },
            event_sequence: None,
            extensions: BTreeMap::new(),
        };

        let encoded = serde_json::to_value(&event).expect("event serializes");

        assert_eq!(
            encoded,
            json!({
                "type": "run_finished",
                "run_id": "run-001",
                "status": {
                    "status": "interrupted",
                    "reason": "user paused the run"
                }
            })
        );

        let impossible_finished_event = json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "running"
            }
        });
        let conflicting_terminal_status = json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "completed",
                "final_message": "Done",
                "error": {
                    "code": "model_failed",
                    "message": "model stream ended with an error",
                    "recoverable": false
                }
            }
        });
        let event_with_additive_field = json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "completed",
                "final_message": "Done"
            },
            "extensions": {
                "future_hint": true
            }
        });

        assert!(serde_json::from_value::<AgentEvent>(impossible_finished_event).is_err());
        assert!(serde_json::from_value::<AgentEvent>(conflicting_terminal_status).is_err());
        let decoded: AgentEvent =
            serde_json::from_value(event_with_additive_field).expect("event deserializes");

        match decoded {
            AgentEvent::RunFinished { extensions, .. } => {
                assert_eq!(extensions["future_hint"], json!(true));
            }
            _ => panic!("expected run finished"),
        }
    }

    #[test]
    fn agent_event_extensions_round_trip_without_dropping_additive_fields() {
        let event = AgentEvent::RunStarted {
            run_id: RunId::new("run-001"),
            event_sequence: None,
            extensions: BTreeMap::from([("future_hint".to_string(), json!("preserve"))]),
        };

        let encoded = serde_json::to_value(&event).expect("event serializes");
        let decoded: AgentEvent =
            serde_json::from_value(encoded.clone()).expect("event deserializes");
        let reencoded = serde_json::to_value(&decoded).expect("event serializes");

        assert_eq!(encoded["extensions"]["future_hint"], json!("preserve"));
        assert_eq!(reencoded["extensions"]["future_hint"], json!("preserve"));
    }

    #[test]
    fn tool_invocation_id_is_kernel_owned_while_model_call_id_stays_separate() {
        let call_id = ToolCallId::new("call-001");
        let model_call_id = ModelToolCallId::new("model-call-001");
        let model_tool_call = ModelStreamEvent::ToolCall {
            id: model_call_id,
            name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
            extensions: BTreeMap::new(),
        };
        let tool_call = ToolCall {
            id: call_id.clone(),
            tool_name: "read_file".to_string(),
            arguments: json!({ "path": "README.md" }),
        };
        let tool_result = ToolResult {
            call_id: call_id.clone(),
            output: ToolOutput::Success {
                content: vec![ToolContent::Text {
                    text: "# Agent Kernel".to_string(),
                }],
                metadata: BTreeMap::new(),
                extensions: BTreeMap::new(),
            },
        };
        let tool_message = ModelMessage::tool("# Agent Kernel", "read_file", "model-call-001");

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
            ModelMessage::Tool { tool_call_id, .. } => {
                assert_eq!(tool_call_id.as_str(), "model-call-001");
            }
            _ => panic!("expected tool result message"),
        }
    }
}
