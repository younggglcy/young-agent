use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use young_agent_runtime::{
    AgentEvent, AgentRuntime, ApprovalDecision, ApprovalRequest, RunControl, RunControlFlow, RunId,
    RunRequest, TerminalRunStatus,
};
use young_event_store::JsonlEventStore;
use young_model_runtime::{
    FakeModelClient, ModelError, ModelMessage, ModelMessageContent, ModelStreamEvent,
    ModelToolCallId, ScriptedModelTurn,
};
use young_tool_runtime::{FakeToolExecutor, ToolContent, ToolError, ToolOutput};

struct TestLog {
    path: PathBuf,
}

impl TestLog {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "young-agent-runtime-{name}-{}-{nonce}.jsonl",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn no_extensions() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::new()
}

fn run_request(run_id: &str) -> RunRequest {
    RunRequest {
        run_id: RunId::new(run_id),
        model: "fake-model".to_string(),
        messages: vec![ModelMessage::user("Read README.md and summarize it.")],
        tools: Vec::new(),
        metadata: no_extensions(),
    }
}

#[test]
fn scripted_run_executes_a_fake_tool_and_persists_the_completed_timeline() {
    let log = TestLog::new("happy-path");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-call-001"),
                name: "read_file".to_string(),
                arguments: json!({ "path": "README.md" }),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: no_extensions(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "The project is an Agent Kernel.".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ]),
    ]);
    let tools = FakeToolExecutor::new([ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "# young-agent".to_string(),
        }],
        metadata: no_extensions(),
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    let outcome = runtime
        .run(run_request("run-001"))
        .expect("scripted run should complete");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Completed {
            final_message: "The project is an Agent Kernel.".to_string(),
        }
    );
    assert_eq!(runtime.model_client().requests().len(), 2);
    assert_eq!(runtime.tool_executor().calls().len(), 1);

    let replay = store.replay().expect("runtime Event Log should replay");
    assert_eq!(replay.terminal_status(), Some(outcome.status()));
    assert_eq!(replay.tool_calls().len(), 1);
    assert!(replay.tool_calls()[0].result().is_some());
}

#[test]
fn model_error_is_persisted_and_finishes_the_run_as_failed() {
    let log = TestLog::new("model-error");
    let store = JsonlEventStore::new(log.path());
    let model_error = ModelError {
        code: "provider_unavailable".to_string(),
        message: "provider returned 503".to_string(),
        retryable: true,
    };
    let model = FakeModelClient::new([ScriptedModelTurn::events([ModelStreamEvent::Failed {
        error: model_error.clone(),
        extensions: no_extensions(),
    }])]);
    let mut runtime = AgentRuntime::new(model, FakeToolExecutor::default(), store.clone());

    let outcome = runtime
        .run(run_request("run-model-error"))
        .expect("model failure should be a recorded terminal outcome");

    let replay = store.replay().expect("failed run should replay");
    assert_eq!(replay.errors().len(), 1);
    assert_eq!(replay.errors()[0].code, model_error.code);
    assert_eq!(replay.errors()[0].message, model_error.message);
    assert_eq!(replay.terminal_status(), Some(outcome.status()));
    assert!(matches!(outcome.status(), TerminalRunStatus::Failed { .. }));
    assert!(replay.events().iter().any(|event| matches!(
        event,
        AgentEvent::ModelOutput {
            event: ModelStreamEvent::Failed { .. },
            ..
        }
    )));
}

#[test]
fn direct_fake_model_error_is_persisted_as_a_failed_run() {
    let log = TestLog::new("direct-model-error");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([ScriptedModelTurn::error(ModelError {
        code: "transport_unavailable".to_string(),
        message: "connection refused".to_string(),
        retryable: true,
    })]);
    let mut runtime = AgentRuntime::new(model, FakeToolExecutor::default(), store.clone());

    let outcome = runtime
        .run(run_request("run-direct-model-error"))
        .expect("direct model error should be a recorded terminal outcome");

    let replay = store.replay().expect("failed run should replay");
    assert_eq!(replay.errors()[0].code, "transport_unavailable");
    assert_eq!(replay.terminal_status(), Some(outcome.status()));
    assert!(matches!(outcome.status(), TerminalRunStatus::Failed { .. }));
}

#[test]
fn tool_error_is_emitted_and_fed_back_to_the_next_model_turn() {
    let log = TestLog::new("tool-error");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-call-001"),
                name: "read_file".to_string(),
                arguments: json!({ "path": "missing.md" }),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: no_extensions(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "The file could not be read.".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ]),
    ]);
    let tools = FakeToolExecutor::new([ToolOutput::Failure {
        error: ToolError {
            code: "not_found".to_string(),
            message: "missing.md does not exist".to_string(),
            retryable: false,
        },
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    let outcome = runtime
        .run(run_request("run-tool-error"))
        .expect("recoverable tool failure should return to the model loop");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Completed {
            final_message: "The file could not be read.".to_string(),
        }
    );
    let second_request = &runtime.model_client().requests()[1];
    let tool_message = second_request
        .messages
        .last()
        .expect("second turn should include the tool result");
    match tool_message {
        ModelMessage::Tool { content, .. } => assert!(matches!(
            content.as_slice(),
            [ModelMessageContent::Json { value }]
                if value["status"] == json!("failure")
                    && value["error"]["code"] == json!("not_found")
        )),
        other => panic!("expected tool message, got {other:?}"),
    }

    let replay = store.replay().expect("tool-error run should replay");
    assert_eq!(replay.errors()[0].code, "not_found");
    assert!(matches!(
        replay.tool_calls()[0]
            .result()
            .expect("tool result should exist")
            .output,
        ToolOutput::Failure { .. }
    ));
}

#[test]
fn interruption_and_cancellation_produce_distinct_terminal_states() {
    for (run_id, control, expected_status) in [
        (
            "run-interrupted",
            RunControlFlow::Interrupt {
                reason: "user paused the run".to_string(),
            },
            TerminalRunStatus::Interrupted {
                reason: "user paused the run".to_string(),
            },
        ),
        (
            "run-cancelled",
            RunControlFlow::Cancel {
                reason: "user cancelled the run".to_string(),
            },
            TerminalRunStatus::Cancelled {
                reason: "user cancelled the run".to_string(),
            },
        ),
    ] {
        let log = TestLog::new(run_id);
        let store = JsonlEventStore::new(log.path());
        let mut runtime = AgentRuntime::new(
            FakeModelClient::new([ScriptedModelTurn::events([
                ModelStreamEvent::TextDelta {
                    delta: "partial response".to_string(),
                    extensions: no_extensions(),
                },
                ModelStreamEvent::Completed {
                    finish_reason: Some("stop".to_string()),
                    extensions: no_extensions(),
                },
            ])]),
            FakeToolExecutor::default(),
            store.clone(),
        );
        let mut checkpoints = 0;
        let mut control = || {
            checkpoints += 1;
            if checkpoints == 1 {
                RunControlFlow::Continue
            } else {
                control.clone()
            }
        };

        let outcome = runtime
            .run_with_control(run_request(run_id), &mut control)
            .expect("external stop should be a recorded terminal outcome");

        assert_eq!(outcome.status(), &expected_status);
        assert_eq!(
            store
                .replay()
                .expect("stopped run should replay")
                .terminal_status(),
            Some(&expected_status)
        );
        assert_eq!(runtime.model_client().requests().len(), 1);
        assert!(!store
            .read_all()
            .expect("stopped event log should read")
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelOutput { .. })));
    }
}

#[derive(Default)]
struct ApprovingControl {
    requests: Vec<ApprovalRequest>,
}

impl RunControl for ApprovingControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
        self.requests.push(request.clone());
        ApprovalDecision::Approve
    }
}

#[test]
fn approval_is_emitted_before_an_approved_fake_tool_executes() {
    let log = TestLog::new("approval");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-call-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": "cargo test" }),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: no_extensions(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "Tests passed.".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ]),
    ]);
    let tools = FakeToolExecutor::requiring_approval(
        "command may mutate the workspace",
        [ToolOutput::Success {
            content: vec![ToolContent::Text {
                text: "43 tests passed".to_string(),
            }],
            metadata: no_extensions(),
            extensions: no_extensions(),
        }],
    );
    let mut runtime = AgentRuntime::new(model, tools, store.clone());
    let mut control = ApprovingControl::default();

    let outcome = runtime
        .run_with_control(run_request("run-approved"), &mut control)
        .expect("approved tool run should complete");

    assert!(matches!(
        outcome.status(),
        TerminalRunStatus::Completed { .. }
    ));
    assert_eq!(control.requests.len(), 1);
    assert_eq!(runtime.tool_executor().calls().len(), 1);
    let events = store.read_all().expect("approval Event Log should read");
    let approval_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ApprovalRequested { .. }))
        .expect("approval request should be persisted");
    let result_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolResult { .. }))
        .expect("tool result should be persisted");
    assert!(approval_index < result_index);
}

#[test]
fn unhandled_approval_is_denied_without_executing_the_tool() {
    let log = TestLog::new("approval-denied");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-call-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": "rm -rf target" }),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: no_extensions(),
            },
        ]),
        ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "The command was not run.".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ]),
    ]);
    let tools = FakeToolExecutor::requiring_approval(
        "destructive command",
        [ToolOutput::Success {
            content: vec![ToolContent::Text {
                text: "should never be returned".to_string(),
            }],
            metadata: no_extensions(),
            extensions: no_extensions(),
        }],
    );
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    let outcome = runtime
        .run(run_request("run-denied"))
        .expect("denial should be fed back to the model");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Completed {
            final_message: "The command was not run.".to_string(),
        }
    );
    assert!(runtime.tool_executor().calls().is_empty());
    let replay = store.replay().expect("denied run should replay");
    assert_eq!(replay.approvals().len(), 1);
    assert_eq!(replay.errors()[0].code, "approval_denied");
    assert!(matches!(
        replay.tool_calls()[0]
            .result()
            .expect("denial should be represented as a tool result")
            .output,
        ToolOutput::Failure { .. }
    ));
}
