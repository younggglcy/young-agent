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
fn append_rejects_an_unterminated_log_without_changing_it() {
    let log = TestLog::new("append-after-unterminated-record");
    let original = "{\"type\":\"run_started\",\"run_id\":\"run-001\"}";
    std::fs::write(log.path(), original).expect("fixture should write");
    let store = JsonlEventStore::new(log.path());
    let next_event = agent_event(json!({
        "type": "run_finished",
        "run_id": "run-001",
        "status": {
            "status": "completed",
            "final_message": "Done"
        }
    }));

    let error = store
        .append(&next_event)
        .expect_err("append should reject an unterminated log");
    let message = error.to_string();
    let contents = std::fs::read_to_string(log.path()).expect("event log should read");

    match error {
        EventStoreError::UnterminatedLog { path } => assert_eq!(
            (
                path,
                message.contains("not terminated by a newline"),
                contents,
            ),
            (log.path().to_path_buf(), true, original.to_string())
        ),
        other => panic!("expected unterminated log error, got {other:?}"),
    }
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
fn replay_rejects_approval_after_the_tool_call_has_a_result() {
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
                "tool_name": "read_file",
                "arguments": { "path": "README.md" }
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
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("approval after a tool result should fail");
    let message = error.to_string();

    match error {
        ReplayError::ApprovalAfterToolResult {
            event_number,
            call_id,
        } => assert_eq!(
            (
                event_number,
                call_id.as_str(),
                message.contains("already has a result"),
            ),
            (4, "tool-call-001", true)
        ),
        other => panic!("expected approval-after-result error, got {other:?}"),
    }
}

#[test]
fn replay_rejects_approval_resolution_after_a_tool_result() {
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
        agent_event(json!({
            "type": "tool_result",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "result": {
                "call_id": "tool-call-001",
                "output": { "status": "success", "content": [] }
            }
        })),
        agent_event(json!({
            "type": "approval_resolved",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "approval_id": "approval-001",
            "decision": { "decision": "deny", "reason": "not allowed" }
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("approval resolution after a tool result should fail");

    assert!(matches!(
        error,
        ReplayError::ApprovalResolutionAfterToolResult {
            event_number: 5,
            ref approval_id,
        } if approval_id == "approval-001"
    ));
}

#[test]
fn replay_rejects_a_tool_result_after_approval_was_denied() {
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
        agent_event(json!({
            "type": "approval_resolved",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "approval_id": "approval-001",
            "decision": { "decision": "deny", "reason": "not allowed" }
        })),
        agent_event(json!({
            "type": "tool_result",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "result": {
                "call_id": "tool-call-001",
                "output": { "status": "success", "content": [] }
            }
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("a denied tool call must not have an execution result");

    assert!(matches!(
        error,
        ReplayError::ToolResultAfterApprovalDenied {
            event_number: 5,
            ref call_id,
        } if call_id.as_str() == "tool-call-001"
    ));
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
fn syntactically_complete_record_without_newline_is_still_truncated() {
    let log = TestLog::new("missing-commit-newline");
    std::fs::write(
        log.path(),
        "{\"type\":\"run_started\",\"run_id\":\"run-001\"}",
    )
    .expect("fixture should write");

    let error = JsonlEventStore::new(log.path())
        .read_all()
        .expect_err("unterminated record should fail");
    let message = error.to_string();

    match error {
        EventStoreError::TruncatedRecord { path, line } => assert_eq!(
            (path, line, message.contains("not terminated by a newline"),),
            (log.path().to_path_buf(), 1, true)
        ),
        other => panic!("expected truncated record error, got {other:?}"),
    }
}

#[test]
fn explicit_tail_repair_discards_only_the_uncommitted_final_record() {
    let log = TestLog::new("repair-truncated-tail");
    let committed_events = [
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
                "arguments": { "command": "touch important-file" }
            }
        })),
    ];
    let committed = committed_events
        .iter()
        .map(|event| format!("{}\n", serde_json::to_string(event).expect("event encodes")))
        .collect::<String>();
    let recovered_result = agent_event(json!({
        "type": "tool_result",
        "run_id": "run-001",
        "turn_id": "turn-001",
        "result": {
            "call_id": "tool-call-001",
            "output": { "status": "success", "content": [] }
        }
    }));
    let result_record = serde_json::to_string(&recovered_result).expect("result encodes");
    let partial = &result_record[..result_record.len() / 2];
    std::fs::write(log.path(), format!("{committed}{partial}")).expect("fixture should write");
    let store = JsonlEventStore::new(log.path());

    let removed = store
        .repair_truncated_tail()
        .expect("exclusive repair should truncate the uncommitted tail");

    assert_eq!(removed, partial.len() as u64);
    assert_eq!(
        std::fs::read_to_string(log.path()).expect("repaired log should read"),
        committed
    );
    assert_eq!(
        store
            .replay_for_recovery()
            .expect("inactive repaired log should expose recovery work")
            .status(),
        &RunStatus::RecoveryRequired {
            call_ids: vec![young_tool_runtime::ToolCallId::new("tool-call-001")],
        }
    );
    store
        .append(&recovered_result)
        .expect("reconciled result should append without executing the tool again");
    store
        .append(&agent_event(json!({
            "type": "run_finished",
            "run_id": "run-001",
            "status": { "status": "completed", "final_message": "done" }
        })))
        .expect("append should resume after explicit repair");
    assert!(matches!(
        store.replay().expect("repaired log should replay").status(),
        RunStatus::Finished { .. }
    ));
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
