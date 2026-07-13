use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use young_agent_runtime::{
    AgentEvent, AgentEventSink, AgentRuntime, AgentRuntimeError, ApprovalDecision, ApprovalRequest,
    RunControl, RunControlFlow, RunId, RunRequest, RunStatus, RunStopToken, TerminalRunStatus,
};
use young_event_store::JsonlEventStore;
use young_model_runtime::{
    FakeModelClient, ModelClient, ModelError, ModelMessage, ModelMessageContent, ModelRequest,
    ModelStreamEvent, ModelToolCallId, ScriptedModelTurn,
};
use young_tool_runtime::{
    FakeToolExecutor, ToolCall, ToolContent, ToolError, ToolExecutor, ToolOutput,
};

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

struct BlockingModelClient {
    entered: Arc<Barrier>,
}

struct BlockingStreamCreationModelClient {
    entered: Arc<Barrier>,
}

impl ModelClient for BlockingStreamCreationModelClient {
    type Stream = std::vec::IntoIter<ModelStreamEvent>;

    fn stream(
        &mut self,
        _request: &ModelRequest,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Self::Stream, ModelError> {
        self.entered.wait();
        while !cancellation.load(Ordering::Acquire) {
            thread::yield_now();
        }
        Ok(Vec::new().into_iter())
    }
}

struct BlockingModelStream {
    entered: Arc<Barrier>,
    cancellation: Arc<AtomicBool>,
    observed_cancellation: bool,
}

impl Iterator for BlockingModelStream {
    type Item = ModelStreamEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.observed_cancellation {
            self.entered.wait();
            while !self.cancellation.load(Ordering::Acquire) {
                thread::yield_now();
            }
            self.observed_cancellation = true;
        }
        None
    }
}

impl ModelClient for BlockingModelClient {
    type Stream = BlockingModelStream;

    fn stream(
        &mut self,
        _request: &ModelRequest,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Self::Stream, ModelError> {
        Ok(BlockingModelStream {
            entered: self.entered.clone(),
            cancellation,
            observed_cancellation: false,
        })
    }
}

struct BlockingToolExecutor {
    entered: Arc<Barrier>,
}

struct BlockingApprovalControl {
    entered: Arc<Barrier>,
}

impl RunControl for BlockingApprovalControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(
        &mut self,
        _request: &ApprovalRequest,
        cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        self.entered.wait();
        while !cancellation.load(Ordering::Acquire) {
            thread::yield_now();
        }
        ApprovalDecision::Deny {
            reason: "approval wait cancelled".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct TestSinkError;

impl fmt::Display for TestSinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "scripted persistence failure")
    }
}

impl Error for TestSinkError {}

#[derive(Clone)]
struct FailOnceOnToolResultSink {
    events: Rc<RefCell<Vec<AgentEvent>>>,
    should_fail: Rc<Cell<bool>>,
}

#[derive(Clone, Default)]
struct FailOnRunFinishedSink {
    events: Rc<RefCell<Vec<AgentEvent>>>,
}

#[derive(Clone, Default)]
struct DurabilitySpySink {
    events: Rc<RefCell<Vec<(AgentEvent, bool)>>>,
}

impl AgentEventSink for DurabilitySpySink {
    type Error = TestSinkError;

    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        self.events.borrow_mut().push((event.clone(), false));
        Ok(())
    }

    fn append_durable(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        self.events.borrow_mut().push((event.clone(), true));
        Ok(())
    }
}

impl FailOnceOnToolResultSink {
    fn new() -> Self {
        Self {
            events: Rc::new(RefCell::new(Vec::new())),
            should_fail: Rc::new(Cell::new(true)),
        }
    }
}

impl AgentEventSink for FailOnceOnToolResultSink {
    type Error = TestSinkError;

    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        if matches!(event, AgentEvent::ToolResult { .. }) && self.should_fail.replace(false) {
            return Err(TestSinkError);
        }
        self.events.borrow_mut().push(event.clone());
        Ok(())
    }

    fn append_durable(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        self.append(event)
    }
}

impl AgentEventSink for FailOnRunFinishedSink {
    type Error = TestSinkError;

    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        self.events.borrow_mut().push(event.clone());
        Ok(())
    }

    fn append_durable(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        if matches!(event, AgentEvent::RunFinished { .. }) {
            return Err(TestSinkError);
        }
        self.events.borrow_mut().push(event.clone());
        Ok(())
    }
}

impl ToolExecutor for BlockingToolExecutor {
    fn execute(&mut self, _call: &ToolCall, cancellation: Arc<AtomicBool>) -> ToolOutput {
        self.entered.wait();
        while !cancellation.load(Ordering::Acquire) {
            thread::yield_now();
        }
        ToolOutput::Failure {
            error: ToolError {
                code: "cancelled".to_string(),
                message: "tool observed cancellation".to_string(),
                retryable: false,
            },
            extensions: no_extensions(),
        }
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
    assert_eq!(runtime.model_client().request_count(), 2);
    assert_eq!(runtime.tool_executor().calls().len(), 1);

    let replay = store.replay().expect("runtime Event Log should replay");
    assert_eq!(replay.terminal_status(), Some(outcome.status()));
    assert_eq!(replay.tool_calls().len(), 1);
    assert!(replay
        .tool_calls()
        .next()
        .expect("tool call should replay")
        .result()
        .is_some());
}

#[test]
fn canonical_state_transitions_use_the_durable_event_sink_boundary() {
    let sink = DurabilitySpySink::default();
    let observed = sink.events.clone();
    let model = FakeModelClient::new([
        ScriptedModelTurn::events([
            ModelStreamEvent::ToolCall {
                id: ModelToolCallId::new("model-call-001"),
                name: "run_command".to_string(),
                arguments: json!({ "command": "touch important-file" }),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
                extensions: no_extensions(),
            },
        ]),
        ScriptedModelTurn::events([ModelStreamEvent::Completed {
            finish_reason: Some("stop".to_string()),
            extensions: no_extensions(),
        }]),
    ]);
    let tools = FakeToolExecutor::requiring_approval(
        "command requires approval",
        [ToolOutput::Success {
            content: Vec::new(),
            metadata: no_extensions(),
            extensions: no_extensions(),
        }],
    );
    let mut runtime = AgentRuntime::new(model, tools, sink);
    let mut control = ApprovingControl::default();

    runtime
        .run_with_control(run_request("run-durable-tool-events"), &mut control)
        .expect("run should complete");

    let events = observed.borrow();
    for expected in [
        "tool_call_requested",
        "approval_requested",
        "approval_resolved",
        "tool_result",
        "run_finished",
    ] {
        assert!(events.iter().any(|(event, durable)| {
            let matches_kind = matches!(
                (expected, event),
                ("tool_call_requested", AgentEvent::ToolCallRequested { .. })
                    | ("approval_requested", AgentEvent::ApprovalRequested { .. })
                    | ("approval_resolved", AgentEvent::ApprovalResolved { .. })
                    | ("tool_result", AgentEvent::ToolResult { .. })
                    | ("run_finished", AgentEvent::RunFinished { .. })
            );
            matches_kind && *durable
        }));
    }
    assert!(events
        .iter()
        .any(|(event, durable)| { matches!(event, AgentEvent::ModelOutput { .. }) && !*durable }));
}

#[test]
fn completed_model_event_ends_the_turn_before_late_events_can_execute_tools() {
    let log = TestLog::new("completed-is-terminal");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([ScriptedModelTurn::events([
        ModelStreamEvent::TextDelta {
            delta: "Done.".to_string(),
            extensions: no_extensions(),
        },
        ModelStreamEvent::Completed {
            finish_reason: Some("stop".to_string()),
            extensions: no_extensions(),
        },
        ModelStreamEvent::ToolCall {
            id: ModelToolCallId::new("late-model-call"),
            name: "run_command".to_string(),
            arguments: json!({ "command": "touch should-not-exist" }),
            extensions: no_extensions(),
        },
    ])]);
    let tools = FakeToolExecutor::new([ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "unexpected".to_string(),
        }],
        metadata: no_extensions(),
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    let outcome = runtime
        .run(run_request("run-completed-terminal"))
        .expect("completed event should end the run");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Completed {
            final_message: "Done.".to_string(),
        }
    );
    assert!(runtime.tool_executor().calls().is_empty());
    assert!(!store
        .read_all()
        .expect("Event Log should read")
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCallRequested { .. })));
}

#[test]
fn duplicate_model_tool_call_ids_fail_before_any_tool_executes() {
    let log = TestLog::new("duplicate-model-tool-call-id");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([ScriptedModelTurn::events([
        ModelStreamEvent::ToolCall {
            id: ModelToolCallId::new("duplicate-id"),
            name: "run_command".to_string(),
            arguments: json!({ "command": "touch first" }),
            extensions: no_extensions(),
        },
        ModelStreamEvent::ToolCall {
            id: ModelToolCallId::new("duplicate-id"),
            name: "run_command".to_string(),
            arguments: json!({ "command": "touch second" }),
            extensions: no_extensions(),
        },
        ModelStreamEvent::Completed {
            finish_reason: Some("tool_calls".to_string()),
            extensions: no_extensions(),
        },
    ])]);
    let tools = FakeToolExecutor::new([ToolOutput::Success {
        content: Vec::new(),
        metadata: no_extensions(),
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    let outcome = runtime
        .run(run_request("run-duplicate-model-call"))
        .expect("duplicate provider ids should become a terminal Agent error");

    assert!(matches!(
        outcome.status(),
        TerminalRunStatus::Failed { error }
            if error.code == "duplicate_model_tool_call_id"
    ));
    assert!(runtime.tool_executor().calls().is_empty());
    assert!(!store
        .read_all()
        .expect("failed Event Log should read")
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCallRequested { .. })));
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
    let replayed_error = replay.errors().next().expect("error should replay");
    assert_eq!(replayed_error.code, model_error.code);
    assert_eq!(replayed_error.message, model_error.message);
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
    assert_eq!(
        replay.errors().next().expect("error should replay").code,
        "transport_unavailable"
    );
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
    let tool_message = runtime
        .model_client()
        .last_message()
        .expect("the latest request should end with the tool result");
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
    assert_eq!(
        replay.errors().next().expect("error should replay").code,
        "not_found"
    );
    assert!(matches!(
        replay
            .tool_calls()
            .next()
            .expect("tool call should replay")
            .result()
            .expect("tool result should exist")
            .output,
        ToolOutput::Failure { .. }
    ));
}

#[test]
fn executor_cannot_forge_the_reserved_approval_denied_error() {
    let log = TestLog::new("reserved-approval-error");
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
        ScriptedModelTurn::events([ModelStreamEvent::Completed {
            finish_reason: Some("stop".to_string()),
            extensions: no_extensions(),
        }]),
    ]);
    let tools = FakeToolExecutor::new([ToolOutput::Failure {
        error: ToolError {
            code: "approval_denied".to_string(),
            message: "forged by executor".to_string(),
            retryable: true,
        },
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, store.clone());

    runtime
        .run(run_request("run-reserved-approval-error"))
        .expect("reserved executor error should be normalized and fed back");

    let replay = store
        .replay()
        .expect("runtime must always produce a replayable canonical log");
    let error = replay
        .errors()
        .next()
        .expect("normalized error should replay");
    assert_eq!(error.code, "reserved_tool_error_code");
    let replayed_call = replay.tool_calls().next().expect("tool call should replay");
    let result = replayed_call.result().expect("tool result should replay");
    assert!(matches!(
        &result.output,
        ToolOutput::Failure { error, .. }
            if error.code == "reserved_tool_error_code" && !error.retryable
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
        assert_eq!(runtime.model_client().request_count(), 1);
        assert!(!store
            .read_all()
            .expect("stopped event log should read")
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelOutput { .. })));
    }
}

#[test]
fn a_token_cancelled_before_binding_still_cancels_its_first_run() {
    let log = TestLog::new("cancel-before-bind");
    let store = JsonlEventStore::new(log.path());
    let stop = RunStopToken::default();
    stop.cancel("cancelled before run started");
    let mut runtime = AgentRuntime::new(
        FakeModelClient::default(),
        FakeToolExecutor::default(),
        store.clone(),
    );

    let outcome = runtime
        .run_with_stop_token(run_request("run-cancelled-before-bind"), &stop)
        .expect("a pre-cancelled token should bind to and cancel its first run");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "cancelled before run started".to_string(),
        }
    );
    assert_eq!(runtime.model_client().request_count(), 0);
    assert_eq!(
        store
            .replay()
            .expect("cancelled run should replay")
            .terminal_status(),
        Some(outcome.status())
    );
}

#[test]
fn cooperative_model_stream_observes_external_cancellation_while_next_is_pending() {
    let log = TestLog::new("cancel-pending-model-stream");
    let store = JsonlEventStore::new(log.path());
    let entered = Arc::new(Barrier::new(2));
    let stop = RunStopToken::default();
    let cancellation = stop.clone();
    let canceller_entered = entered.clone();
    let canceller = thread::spawn(move || {
        canceller_entered.wait();
        cancellation.cancel("user cancelled pending model output");
    });
    let mut runtime = AgentRuntime::new(
        BlockingModelClient { entered },
        FakeToolExecutor::default(),
        store.clone(),
    );

    let outcome = runtime
        .run_with_stop_token(run_request("run-cancel-pending-model"), &stop)
        .expect("cooperative cancellation should finish the run");
    canceller.join().expect("canceller should finish");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "user cancelled pending model output".to_string(),
        }
    );
    assert_eq!(
        store
            .replay()
            .expect("cancelled run should replay")
            .terminal_status(),
        Some(outcome.status())
    );
}

#[test]
fn cooperative_model_client_observes_external_cancellation_while_stream_starts() {
    let log = TestLog::new("cancel-pending-stream-start");
    let store = JsonlEventStore::new(log.path());
    let entered = Arc::new(Barrier::new(2));
    let stop = RunStopToken::default();
    let cancellation = stop.clone();
    let canceller_entered = entered.clone();
    let canceller = thread::spawn(move || {
        canceller_entered.wait();
        cancellation.cancel("user cancelled provider startup");
    });
    let mut runtime = AgentRuntime::new(
        BlockingStreamCreationModelClient { entered },
        FakeToolExecutor::default(),
        store.clone(),
    );

    let outcome = runtime
        .run_with_stop_token(run_request("run-cancel-stream-start"), &stop)
        .expect("cooperative provider startup should observe cancellation");
    canceller.join().expect("canceller should finish");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "user cancelled provider startup".to_string(),
        }
    );
    assert_eq!(
        store
            .replay()
            .expect("cancelled run should replay")
            .terminal_status(),
        Some(outcome.status())
    );
}

#[test]
fn cooperative_tool_observes_external_cancellation_while_execution_is_pending() {
    let log = TestLog::new("cancel-pending-tool");
    let store = JsonlEventStore::new(log.path());
    let entered = Arc::new(Barrier::new(2));
    let stop = RunStopToken::default();
    let cancellation = stop.clone();
    let canceller_entered = entered.clone();
    let canceller = thread::spawn(move || {
        canceller_entered.wait();
        cancellation.cancel("user cancelled pending tool execution");
    });
    let model = FakeModelClient::new([ScriptedModelTurn::events([
        ModelStreamEvent::ToolCall {
            id: ModelToolCallId::new("model-call-001"),
            name: "run_command".to_string(),
            arguments: json!({ "command": "long-running-command" }),
            extensions: no_extensions(),
        },
        ModelStreamEvent::Completed {
            finish_reason: Some("tool_calls".to_string()),
            extensions: no_extensions(),
        },
    ])]);
    let mut runtime = AgentRuntime::new(model, BlockingToolExecutor { entered }, store.clone());

    let outcome = runtime
        .run_with_stop_token(run_request("run-cancel-pending-tool"), &stop)
        .expect("cooperative cancellation should finish the run");
    canceller.join().expect("canceller should finish");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "user cancelled pending tool execution".to_string(),
        }
    );
    let events = store.read_all().expect("cancelled Event Log should read");
    let result_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::ToolResult { .. }))
        .expect("cooperative tool result should be persisted");
    let finished_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::RunFinished { .. }))
        .expect("cancelled run should finish");
    assert!(result_index < finished_index);
}

#[test]
fn cooperative_approval_wait_observes_external_cancellation() {
    let log = TestLog::new("cancel-pending-approval");
    let store = JsonlEventStore::new(log.path());
    let entered = Arc::new(Barrier::new(2));
    let stop = RunStopToken::default();
    let cancellation = stop.clone();
    let canceller_entered = entered.clone();
    let canceller = thread::spawn(move || {
        canceller_entered.wait();
        cancellation.cancel("user cancelled pending approval");
    });
    let model = FakeModelClient::new([ScriptedModelTurn::events([
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
    ])]);
    let tools = FakeToolExecutor::requiring_approval(
        "command requires approval",
        [ToolOutput::Success {
            content: Vec::new(),
            metadata: no_extensions(),
            extensions: no_extensions(),
        }],
    );
    let mut runtime = AgentRuntime::new(model, tools, store.clone());
    let mut control = BlockingApprovalControl { entered };

    let outcome = runtime
        .run_with_control_and_stop(
            run_request("run-cancel-pending-approval"),
            &mut control,
            &stop,
        )
        .expect("cooperative approval wait should observe cancellation");
    canceller.join().expect("canceller should finish");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "user cancelled pending approval".to_string(),
        }
    );
    assert!(runtime.tool_executor().calls().is_empty());
    let events = store.read_all().expect("cancelled Event Log should read");
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalRequested { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ApprovalResolved { .. })));
}

#[test]
fn the_first_stop_request_is_shared_across_control_and_stop_token() {
    let first_log = TestLog::new("first-stop-control");
    let first_store = JsonlEventStore::new(first_log.path());
    let stop = RunStopToken::default();
    let mut control = || RunControlFlow::Interrupt {
        reason: "control interrupted first".to_string(),
    };
    let mut first_runtime = AgentRuntime::new(
        FakeModelClient::default(),
        FakeToolExecutor::default(),
        first_store,
    );

    let first_outcome = first_runtime
        .run_with_control_and_stop(run_request("run-first-stop"), &mut control, &stop)
        .expect("control interruption should finish the run");
    assert_eq!(
        first_outcome.status(),
        &TerminalRunStatus::Interrupted {
            reason: "control interrupted first".to_string(),
        }
    );
    assert!(stop.is_requested());

    stop.cancel("later cancellation must not replace the first stop");
    assert_eq!(stop.terminal_status(), Some(first_outcome.status().clone()));
}

#[test]
fn completed_terminal_status_wins_over_a_later_cancellation() {
    let log = TestLog::new("completed-before-late-cancel");
    let stop = RunStopToken::default();
    let mut runtime = AgentRuntime::new(
        FakeModelClient::new([ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "done".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ])]),
        FakeToolExecutor::default(),
        JsonlEventStore::new(log.path()),
    );

    let outcome = runtime
        .run_with_stop_token(run_request("run-completed-first"), &stop)
        .expect("run should complete");
    stop.cancel("late cancellation");

    assert_eq!(stop.terminal_status(), Some(outcome.status().clone()));
    assert!(!stop.is_requested());

    let second_log = TestLog::new("completed-token-reuse");
    let mut second_runtime = AgentRuntime::new(
        FakeModelClient::default(),
        FakeToolExecutor::default(),
        JsonlEventStore::new(second_log.path()),
    );
    let error = second_runtime
        .run_with_stop_token(run_request("run-reusing-completed-token"), &stop)
        .expect_err("a RunStopToken is bound to exactly one Agent Run");
    assert!(matches!(
        error,
        AgentRuntimeError::StopTokenAlreadyBound { ref run_id }
            if run_id.as_str() == "run-completed-first"
    ));
}

#[test]
fn terminal_persistence_failure_returns_the_event_for_reconciliation() {
    let sink = FailOnRunFinishedSink::default();
    let observed = sink.events.clone();
    let stop = RunStopToken::default();
    let mut runtime = AgentRuntime::new(
        FakeModelClient::new([ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: "done".to_string(),
                extensions: no_extensions(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: no_extensions(),
            },
        ])]),
        FakeToolExecutor::default(),
        sink,
    );

    let error = runtime
        .run_with_stop_token(run_request("run-terminal-persistence-failure"), &stop)
        .expect_err("terminal durability failure must require reconciliation");

    let terminal_event = match error {
        AgentRuntimeError::TerminalPersistenceIndeterminate { event, .. } => *event,
        other => panic!("expected terminal persistence recovery error, got {other:?}"),
    };
    assert!(matches!(
        &terminal_event,
        AgentEvent::RunFinished {
            status: TerminalRunStatus::Completed { final_message },
            ..
        } if final_message == "done"
    ));
    assert_eq!(
        stop.terminal_status(),
        Some(TerminalRunStatus::Completed {
            final_message: "done".to_string(),
        })
    );
    assert!(!observed
        .borrow()
        .iter()
        .any(|event| matches!(event, AgentEvent::RunFinished { .. })));
}

#[test]
fn tool_result_persistence_failure_surfaces_recovery_event_without_reexecuting_tool() {
    let sink = FailOnceOnToolResultSink::new();
    let observed_events = sink.events.clone();
    let model = FakeModelClient::new([ScriptedModelTurn::events([
        ModelStreamEvent::ToolCall {
            id: ModelToolCallId::new("model-call-001"),
            name: "run_command".to_string(),
            arguments: json!({ "command": "touch important-file" }),
            extensions: no_extensions(),
        },
        ModelStreamEvent::Completed {
            finish_reason: Some("tool_calls".to_string()),
            extensions: no_extensions(),
        },
    ])]);
    let tools = FakeToolExecutor::new([ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: "created important-file".to_string(),
        }],
        metadata: no_extensions(),
        extensions: no_extensions(),
    }]);
    let mut runtime = AgentRuntime::new(model, tools, sink);

    let error = runtime
        .run(run_request("run-tool-result-persistence-failure"))
        .expect_err("ToolResult persistence failure must stop the run");

    let recovery_event = match error {
        AgentRuntimeError::ToolResultPersistenceIndeterminate { event, .. } => *event,
        other => panic!("expected ToolResult recovery error, got {other:?}"),
    };
    assert!(matches!(
        &recovery_event,
        AgentEvent::ToolResult { result, .. }
            if result.call_id.as_str() == "run-tool-result-persistence-failure-tool-001"
    ));
    assert_eq!(runtime.tool_executor().calls().len(), 1);

    let replay = young_event_store::replay_events(observed_events.borrow().clone())
        .expect("pre-execution intent should remain replayable");
    assert_eq!(replay.status(), &RunStatus::Running);

    let recovery = young_event_store::replay_events_for_recovery(observed_events.borrow().clone())
        .expect("an inactive incomplete run should expose recovery work");
    assert_eq!(
        recovery.status(),
        &RunStatus::RecoveryRequired {
            call_ids: vec![runtime.tool_executor().calls()[0].id.clone()],
        }
    );
}

#[derive(Default)]
struct ApprovingControl {
    requests: Vec<ApprovalRequest>,
}

#[derive(Default)]
struct ApproveThenCancelControl {
    approved: bool,
}

impl RunControl for ApproveThenCancelControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        if self.approved {
            RunControlFlow::Cancel {
                reason: "cancelled after approval".to_string(),
            }
        } else {
            RunControlFlow::Continue
        }
    }

    fn decide_approval(
        &mut self,
        _request: &ApprovalRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
        self.approved = true;
        ApprovalDecision::Approve
    }
}

impl RunControl for ApprovingControl {
    fn checkpoint(&mut self) -> RunControlFlow {
        RunControlFlow::Continue
    }

    fn decide_approval(
        &mut self,
        request: &ApprovalRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> ApprovalDecision {
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
fn approval_decision_is_persisted_before_a_post_approval_cancellation() {
    let log = TestLog::new("approval-then-cancel");
    let store = JsonlEventStore::new(log.path());
    let model = FakeModelClient::new([ScriptedModelTurn::events([
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
    ])]);
    let tools = FakeToolExecutor::requiring_approval(
        "command requires approval",
        [ToolOutput::Success {
            content: vec![],
            metadata: no_extensions(),
            extensions: no_extensions(),
        }],
    );
    let mut runtime = AgentRuntime::new(model, tools, store.clone());
    let mut control = ApproveThenCancelControl::default();

    let outcome = runtime
        .run_with_control(run_request("run-approval-cancel"), &mut control)
        .expect("post-approval cancellation should be terminal");

    assert_eq!(
        outcome.status(),
        &TerminalRunStatus::Cancelled {
            reason: "cancelled after approval".to_string(),
        }
    );
    assert!(runtime.tool_executor().calls().is_empty());
    let events = store.read_all().expect("approval Event Log should read");
    let resolved_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ApprovalResolved {
                    decision: ApprovalDecision::Approve,
                    ..
                }
            )
        })
        .expect("approval decision should be persisted");
    let finished_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::RunFinished { .. }))
        .expect("cancelled run should finish");
    assert!(resolved_index < finished_index);
    assert_eq!(
        store
            .replay()
            .expect("approval decision should replay")
            .tool_calls()
            .next()
            .expect("tool call should replay")
            .approval_decision(),
        Some(&ApprovalDecision::Approve)
    );
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
    assert_eq!(
        replay.errors().next().expect("error should replay").code,
        "approval_denied"
    );
    assert!(matches!(
        replay
            .tool_calls()
            .next()
            .expect("tool call should replay")
            .result()
            .expect("denial should be represented as a tool result")
            .output,
        ToolOutput::Failure { .. }
    ));
}
