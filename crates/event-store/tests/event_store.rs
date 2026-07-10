use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use young_agent_runtime::{AgentEvent, RunStatus, TerminalRunStatus};
use young_event_store::{EventStoreError, JsonlEventStore, ReplayError};

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
            "young-event-store-{name}-{}-{nonce}.jsonl",
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

fn agent_event(value: Value) -> AgentEvent {
    serde_json::from_value(value).expect("fixture should be a valid AgentEvent")
}

#[test]
fn appending_events_preserves_their_order_when_read_back() {
    let log = TestLog::new("append-read");
    let store = JsonlEventStore::new(log.path());
    let events = vec![
        agent_event(json!({
            "type": "run_started",
            "run_id": "run-001"
        })),
        agent_event(json!({
            "type": "turn_started",
            "run_id": "run-001",
            "turn_id": "turn-001"
        })),
        agent_event(json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "completed",
                "final_message": "Done"
            }
        })),
    ];

    for event in &events {
        store.append(event).expect("event should append");
    }

    assert_eq!(store.read_all().expect("event log should read"), events);
}

#[test]
fn append_writes_one_canonical_agent_event_per_jsonl_line() {
    let log = TestLog::new("wire-format");
    let store = JsonlEventStore::new(log.path());
    let expected_records = vec![
        json!({
            "type": "run_started",
            "run_id": "run-001"
        }),
        json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "completed",
                "final_message": "Done"
            }
        }),
    ];

    for record in &expected_records {
        store
            .append(&agent_event(record.clone()))
            .expect("event should append");
    }

    let contents = std::fs::read_to_string(log.path()).expect("event log should read");
    let actual_records = contents
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be valid JSON"))
        .collect::<Vec<_>>();

    assert_eq!(
        (contents.ends_with('\n'), actual_records),
        (true, expected_records)
    );
}

#[test]
fn replay_reconstructs_the_run_state_from_the_ordered_event_log() {
    let log = TestLog::new("replay");
    let store = JsonlEventStore::new(log.path());
    let events = vec![
        agent_event(json!({
            "type": "run_started",
            "run_id": "run-001"
        })),
        agent_event(json!({
            "type": "turn_started",
            "run_id": "run-001",
            "turn_id": "turn-001"
        })),
        agent_event(json!({
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "model_tool_call_id": "model-call-001",
            "call": {
                "id": "tool-call-001",
                "tool_name": "read_file",
                "arguments": { "path": "README.md" }
            }
        })),
        agent_event(json!({
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
                "reason": "workspace read requires approval"
            }
        })),
        agent_event(json!({
            "type": "tool_result",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "result": {
                "call_id": "tool-call-001",
                "output": {
                    "status": "success",
                    "content": [{ "type": "text", "text": "# young-agent" }]
                }
            }
        })),
        agent_event(json!({
            "type": "error",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "error": {
                "code": "model_warning",
                "message": "a recoverable stream warning occurred",
                "recoverable": true
            }
        })),
        agent_event(json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": {
                "status": "completed",
                "final_message": "Done"
            }
        })),
    ];

    for event in &events {
        store.append(event).expect("event should append");
    }

    let replay = store.replay().expect("event log should replay");
    let expected_terminal_status = TerminalRunStatus::Completed {
        final_message: "Done".to_string(),
    };
    let expected_status = RunStatus::Finished {
        terminal_status: expected_terminal_status.clone(),
    };

    assert_eq!(
        (
            replay.run_id().as_str(),
            replay.status(),
            replay.events(),
            replay.tool_calls().len(),
            replay.tool_calls()[0].model_tool_call_id().as_str(),
            replay.tool_calls()[0].call().id.as_str(),
            replay.tool_calls()[0]
                .result()
                .map(|result| result.call_id.as_str()),
            replay.approvals()[0].id.as_str(),
            replay.errors()[0].code.as_str(),
            replay.terminal_status(),
        ),
        (
            "run-001",
            &expected_status,
            events.as_slice(),
            1,
            "model-call-001",
            "tool-call-001",
            Some("tool-call-001"),
            "approval-001",
            "model_warning",
            Some(&expected_terminal_status),
        )
    );
}

#[test]
fn replay_reconstructs_a_run_waiting_for_tool_approval() {
    let events = vec![
        agent_event(json!({
            "type": "run_started",
            "run_id": "run-001"
        })),
        agent_event(json!({
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "model_tool_call_id": "model-call-001",
            "call": {
                "id": "tool-call-001",
                "tool_name": "run_command",
                "arguments": { "command": "cargo test" }
            }
        })),
        agent_event(json!({
            "type": "approval_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "request": {
                "id": "approval-001",
                "call": {
                    "id": "tool-call-001",
                    "tool_name": "run_command",
                    "arguments": { "command": "cargo test" }
                },
                "reason": "command requires approval"
            }
        })),
    ];

    let replay = young_event_store::replay_events(events).expect("events should replay");

    assert_eq!(
        (
            replay.status(),
            replay.terminal_status(),
            replay.tool_calls()[0]
                .approval()
                .map(|request| request.id.as_str()),
            replay.tool_calls()[0].result(),
        ),
        (
            &RunStatus::AwaitingApproval,
            None,
            Some("approval-001"),
            None,
        )
    );
}

#[test]
fn replay_rejects_an_event_from_a_different_run() {
    let log = TestLog::new("mixed-runs");
    let store = JsonlEventStore::new(log.path());
    for event in [
        agent_event(json!({
            "type": "run_started",
            "run_id": "run-001"
        })),
        agent_event(json!({
            "type": "turn_started",
            "run_id": "run-002",
            "turn_id": "turn-001"
        })),
    ] {
        store.append(&event).expect("event should append");
    }

    let error = store.replay().expect_err("mixed runs should fail");

    match error {
        EventStoreError::Replay {
            path,
            source:
                ReplayError::MismatchedRunId {
                    event_number,
                    expected,
                    found,
                },
        } => assert_eq!(
            (
                path,
                event_number,
                expected.as_str().to_string(),
                found.as_str().to_string(),
            ),
            (
                log.path().to_path_buf(),
                2,
                "run-001".to_string(),
                "run-002".to_string(),
            )
        ),
        other => panic!("expected mismatched run id, got {other:?}"),
    }
}

#[test]
fn malformed_record_reports_its_path_line_and_syntax_error() {
    let log = TestLog::new("malformed");
    std::fs::write(
        log.path(),
        concat!(
            "{\"type\":\"run_started\",\"run_id\":\"run-001\"}\n",
            "{not-json}\n",
        ),
    )
    .expect("fixture should write");

    let error = JsonlEventStore::new(log.path())
        .read_all()
        .expect_err("malformed record should fail");
    let message = error.to_string();

    match error {
        EventStoreError::DecodeRecord { path, line, source } => assert_eq!(
            (
                path,
                line,
                source.is_syntax(),
                message.contains(&log.path().display().to_string()),
                message.contains("line 2"),
            ),
            (log.path().to_path_buf(), 2, true, true, true)
        ),
        other => panic!("expected decode error, got {other:?}"),
    }
}

#[test]
fn truncated_final_record_reports_its_path_line_and_eof_error() {
    let log = TestLog::new("truncated");
    std::fs::write(
        log.path(),
        concat!(
            "{\"type\":\"run_started\",\"run_id\":\"run-001\"}\n",
            "{\"type\":\"run_finished\",\"run_id\":\"run-001\"",
        ),
    )
    .expect("fixture should write");

    let error = JsonlEventStore::new(log.path())
        .read_all()
        .expect_err("truncated record should fail");

    match error {
        EventStoreError::DecodeRecord { path, line, source } => assert_eq!(
            (path, line, source.is_eof()),
            (log.path().to_path_buf(), 2, true)
        ),
        other => panic!("expected decode error, got {other:?}"),
    }
}

#[test]
fn unsupported_event_type_reports_its_path_line_and_schema_error() {
    let log = TestLog::new("unsupported");
    std::fs::write(
        log.path(),
        concat!(
            "{\"type\":\"run_started\",\"run_id\":\"run-001\"}\n",
            "{\"type\":\"future_event\",\"run_id\":\"run-001\"}\n",
        ),
    )
    .expect("fixture should write");

    let error = JsonlEventStore::new(log.path())
        .read_all()
        .expect_err("unsupported event should fail");
    let message = error.to_string();

    match error {
        EventStoreError::DecodeRecord { path, line, source } => assert_eq!(
            (
                path,
                line,
                source.is_data(),
                message.contains("future_event"),
            ),
            (log.path().to_path_buf(), 2, true, true)
        ),
        other => panic!("expected decode error, got {other:?}"),
    }
}
