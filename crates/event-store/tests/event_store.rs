use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use young_agent_runtime::{
    AgentEvent, AgentEventSink, EventDurability, EventSequence, RunId, RunStatus,
    TerminalRunStatus, TurnId,
};
use young_event_store::{
    replay_events, replay_events_with_compatibility, EventStoreError, JsonlEventStore,
    ReplayCompatibility, ReplayError,
};
use young_tool_runtime::execution::ToolCallId;

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

fn turn_started_event() -> AgentEvent {
    agent_event(json!({
        "type": "turn_started",
        "run_id": "run-001",
        "turn_id": "turn-001"
    }))
}

fn approval_result_events(decision: Value, output: Value) -> Vec<AgentEvent> {
    vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
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
            "decision": decision
        })),
        agent_event(json!({
            "type": "tool_result",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "result": {
                "call_id": "tool-call-001",
                "output": output
            }
        })),
    ]
}

fn run_started(run_id: &str) -> AgentEvent {
    agent_event(json!({ "type": "run_started", "run_id": run_id }))
}

fn turn_started(run_id: &str, turn_id: &str) -> AgentEvent {
    agent_event(json!({
        "type": "turn_started",
        "run_id": run_id,
        "turn_id": turn_id
    }))
}

fn tool_call_requested(run_id: &str, turn_id: &str, call_id: &str, tool_name: &str) -> AgentEvent {
    agent_event(json!({
        "type": "tool_call_requested",
        "run_id": run_id,
        "turn_id": turn_id,
        "model_tool_call_id": format!("model-{call_id}"),
        "call": {
            "id": call_id,
            "tool_name": tool_name,
            "arguments": { "path": "README.md" }
        }
    }))
}

fn approval_requested(
    run_id: &str,
    turn_id: &str,
    approval_id: &str,
    call_id: &str,
    tool_name: &str,
) -> AgentEvent {
    agent_event(json!({
        "type": "approval_requested",
        "run_id": run_id,
        "turn_id": turn_id,
        "request": {
            "id": approval_id,
            "call": {
                "id": call_id,
                "tool_name": tool_name,
                "arguments": { "path": "README.md" }
            },
            "reason": "approval required"
        }
    }))
}

fn approval_resolved(run_id: &str, turn_id: &str, approval_id: &str) -> AgentEvent {
    agent_event(json!({
        "type": "approval_resolved",
        "run_id": run_id,
        "turn_id": turn_id,
        "approval_id": approval_id,
        "decision": { "decision": "approve" }
    }))
}

fn successful_tool_result(run_id: &str, turn_id: &str, call_id: &str) -> AgentEvent {
    agent_event(json!({
        "type": "tool_result",
        "run_id": run_id,
        "turn_id": turn_id,
        "result": {
            "call_id": call_id,
            "output": { "status": "success", "content": [] }
        }
    }))
}

#[test]
fn replay_errors_expose_actionable_diagnostics() {
    let call_id = || ToolCallId::new("tool-call-001");
    let turn_id = || TurnId::new("turn-001");
    let cases = vec![
        (
            ReplayError::EmptyLog,
            "cannot replay an empty Event Log".to_string(),
        ),
        (
            ReplayError::FirstEventIsNotRunStarted,
            "Event Log event 1 must be run_started".to_string(),
        ),
        (
            ReplayError::DuplicateRunStarted { event_number: 2 },
            "Event Log event 2 starts an already-started run".to_string(),
        ),
        (
            ReplayError::MismatchedRunId {
                event_number: 3,
                expected: RunId::new("run-expected"),
                found: RunId::new("run-found"),
            },
            "Event Log event 3 belongs to run 'run-found' instead of 'run-expected'".to_string(),
        ),
        (
            ReplayError::DuplicateTurnStarted {
                event_number: 4,
                turn_id: turn_id(),
            },
            "Event Log event 4 repeats turn_started for turn 'turn-001'".to_string(),
        ),
        (
            ReplayError::EventForUnknownTurn {
                event_number: 5,
                turn_id: turn_id(),
            },
            "Event Log event 5 belongs to turn 'turn-001' before that turn started".to_string(),
        ),
        (
            ReplayError::EventForInactiveTurn {
                event_number: 6,
                expected: TurnId::new("turn-active"),
                found: TurnId::new("turn-inactive"),
            },
            "Event Log event 6 flows back to inactive turn 'turn-inactive' while turn 'turn-active' is active".to_string(),
        ),
        (
            ReplayError::TurnStartedWithUnresolvedToolCalls {
                event_number: 7,
                turn_id: turn_id(),
                call_ids: vec![call_id(), ToolCallId::new("tool-call-002")],
            },
            "Event Log event 7 starts turn 'turn-001' while 2 tool call(s) remain unresolved".to_string(),
        ),
        (
            ReplayError::MismatchedToolLifecycleTurn {
                event_number: 8,
                call_id: call_id(),
                expected: TurnId::new("turn-origin"),
                found: TurnId::new("turn-other"),
            },
            "Event Log event 8 places tool call 'tool-call-001' on turn 'turn-other' instead of its originating turn 'turn-origin'".to_string(),
        ),
        (
            ReplayError::EventAfterRunFinished { event_number: 9 },
            "Event Log event 9 appears after run_finished".to_string(),
        ),
        (
            ReplayError::DuplicateToolCall {
                event_number: 10,
                call_id: call_id(),
            },
            "Event Log event 10 repeats tool call 'tool-call-001'".to_string(),
        ),
        (
            ReplayError::ApprovalForUnknownToolCall {
                event_number: 11,
                call_id: call_id(),
            },
            "Event Log event 11 has an approval request for unknown tool call 'tool-call-001'"
                .to_string(),
        ),
        (
            ReplayError::ResultForUnknownToolCall {
                event_number: 12,
                call_id: call_id(),
            },
            "Event Log event 12 has a result for unknown tool call 'tool-call-001'".to_string(),
        ),
        (
            ReplayError::ApprovalCallMismatch {
                event_number: 13,
                call_id: call_id(),
            },
            "Event Log event 13 changes the approved payload for tool call 'tool-call-001'"
                .to_string(),
        ),
        (
            ReplayError::ApprovalAfterToolResult {
                event_number: 14,
                call_id: call_id(),
            },
            "Event Log event 14 requests approval for tool call 'tool-call-001' after it already has a result".to_string(),
        ),
        (
            ReplayError::DuplicateApproval {
                event_number: 15,
                call_id: call_id(),
            },
            "Event Log event 15 repeats approval for tool call 'tool-call-001'".to_string(),
        ),
        (
            ReplayError::DuplicateApprovalId {
                event_number: 16,
                approval_id: "approval-001".to_string(),
            },
            "Event Log event 16 repeats approval id 'approval-001'".to_string(),
        ),
        (
            ReplayError::ResolutionForUnknownApproval {
                event_number: 17,
                approval_id: "approval-001".to_string(),
            },
            "Event Log event 17 resolves unknown approval 'approval-001'".to_string(),
        ),
        (
            ReplayError::DuplicateApprovalResolution {
                event_number: 18,
                approval_id: "approval-001".to_string(),
            },
            "Event Log event 18 repeats resolution for approval 'approval-001'".to_string(),
        ),
        (
            ReplayError::ApprovalResolutionAfterToolResult {
                event_number: 19,
                approval_id: "approval-001".to_string(),
            },
            "Event Log event 19 resolves approval 'approval-001' after its tool call already has a result".to_string(),
        ),
        (
            ReplayError::ToolResultBeforeApprovalResolution {
                event_number: 20,
                call_id: call_id(),
            },
            "Event Log event 20 records a result before approval for tool call 'tool-call-001' was resolved".to_string(),
        ),
        (
            ReplayError::InvalidApprovalDenialResult {
                event_number: 21,
                call_id: call_id(),
            },
            "Event Log event 21 records an invalid approval-denial result for tool call 'tool-call-001'"
                .to_string(),
        ),
        (
            ReplayError::MixedApprovalLogFormats { event_number: 22 },
            "Event Log event 22 mixes legacy approvals without resolutions and modern ApprovalResolved events".to_string(),
        ),
        (
            ReplayError::LegacyCompatibilityForSequencedLog { event_number: 23 },
            "Event Log event 23 is sequenced and cannot use pre-ApprovalResolved compatibility"
                .to_string(),
        ),
        (
            ReplayError::TerminalWithUnresolvedToolCalls {
                event_number: 24,
                call_ids: vec![call_id(), ToolCallId::new("tool-call-002")],
            },
            "Event Log event 24 finishes successfully or with failure while 2 tool call(s) remain unresolved".to_string(),
        ),
        (
            ReplayError::DuplicateToolResult {
                event_number: 25,
                call_id: call_id(),
            },
            "Event Log event 25 repeats the result for tool call 'tool-call-001'".to_string(),
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn replay_rejects_duplicate_and_unmatched_lifecycle_events() {
    assert!(matches!(
        replay_events(Vec::new()),
        Err(ReplayError::EmptyLog)
    ));
    assert!(matches!(
        replay_events(vec![turn_started("run-001", "turn-001")]),
        Err(ReplayError::FirstEventIsNotRunStarted)
    ));
    assert!(matches!(
        replay_events(vec![run_started("run-001"), run_started("run-001")]),
        Err(ReplayError::DuplicateRunStarted { event_number: 2 })
    ));
    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            agent_event(json!({
                "type": "run_finished",
                "run_id": "run-001",
                "status": { "status": "completed", "final_message": "done" }
            })),
            agent_event(json!({
                "type": "error",
                "run_id": "run-001",
                "error": {
                    "code": "late",
                    "message": "must not follow completion",
                    "recoverable": false
                }
            })),
        ]),
        Err(ReplayError::EventAfterRunFinished { event_number: 3 })
    ));
    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            turn_started("run-001", "turn-001"),
        ]),
        Err(ReplayError::DuplicateTurnStarted {
            event_number: 3,
            ..
        })
    ));
    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
        ]),
        Err(ReplayError::DuplicateToolCall {
            event_number: 4,
            ..
        })
    ));
    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            approval_requested(
                "run-001",
                "turn-001",
                "approval-001",
                "unknown-call",
                "read_file",
            ),
        ]),
        Err(ReplayError::ApprovalForUnknownToolCall {
            event_number: 3,
            ..
        })
    ));
    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
            approval_requested(
                "run-001",
                "turn-001",
                "approval-001",
                "call-001",
                "search_files",
            ),
        ]),
        Err(ReplayError::ApprovalCallMismatch {
            event_number: 4,
            ..
        })
    ));

    let approved_call = || {
        vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
            approval_requested(
                "run-001",
                "turn-001",
                "approval-001",
                "call-001",
                "read_file",
            ),
        ]
    };
    let mut duplicate_approval = approved_call();
    duplicate_approval.push(approval_requested(
        "run-001",
        "turn-001",
        "approval-002",
        "call-001",
        "read_file",
    ));
    assert!(matches!(
        replay_events(duplicate_approval),
        Err(ReplayError::DuplicateApproval {
            event_number: 5,
            ..
        })
    ));

    let mut duplicate_approval_id = approved_call();
    duplicate_approval_id.push(tool_call_requested(
        "run-001",
        "turn-001",
        "call-002",
        "read_file",
    ));
    duplicate_approval_id.push(approval_requested(
        "run-001",
        "turn-001",
        "approval-001",
        "call-002",
        "read_file",
    ));
    assert!(matches!(
        replay_events(duplicate_approval_id),
        Err(ReplayError::DuplicateApprovalId {
            event_number: 6,
            ..
        })
    ));

    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            approval_resolved("run-001", "turn-001", "unknown-approval"),
        ]),
        Err(ReplayError::ResolutionForUnknownApproval {
            event_number: 3,
            ..
        })
    ));
    let mut duplicate_resolution = approved_call();
    duplicate_resolution.push(approval_resolved("run-001", "turn-001", "approval-001"));
    duplicate_resolution.push(approval_resolved("run-001", "turn-001", "approval-001"));
    assert!(matches!(
        replay_events(duplicate_resolution),
        Err(ReplayError::DuplicateApprovalResolution {
            event_number: 6,
            ..
        })
    ));

    assert!(matches!(
        replay_events(vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            successful_tool_result("run-001", "turn-001", "unknown-call"),
        ]),
        Err(ReplayError::ResultForUnknownToolCall {
            event_number: 3,
            ..
        })
    ));
    let mut duplicate_result = vec![
        run_started("run-001"),
        turn_started("run-001", "turn-001"),
        tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
        successful_tool_result("run-001", "turn-001", "call-001"),
    ];
    duplicate_result.push(successful_tool_result("run-001", "turn-001", "call-001"));
    assert!(matches!(
        replay_events(duplicate_result),
        Err(ReplayError::DuplicateToolResult {
            event_number: 5,
            ..
        })
    ));
}

#[test]
fn replay_keeps_tool_lifecycle_events_on_their_originating_turn() {
    let completed_call = || {
        vec![
            run_started("run-001"),
            turn_started("run-001", "turn-001"),
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
            successful_tool_result("run-001", "turn-001", "call-001"),
            turn_started("run-001", "turn-002"),
        ]
    };

    let mut mismatched_approval = completed_call();
    mismatched_approval.push(approval_requested(
        "run-001",
        "turn-002",
        "approval-late",
        "call-001",
        "read_file",
    ));
    assert!(matches!(
        replay_events(mismatched_approval),
        Err(ReplayError::MismatchedToolLifecycleTurn {
            event_number: 6,
            ..
        })
    ));

    let mut resolved_call = vec![
        run_started("run-001"),
        turn_started("run-001", "turn-001"),
        tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
        approval_requested(
            "run-001",
            "turn-001",
            "approval-001",
            "call-001",
            "read_file",
        ),
        approval_resolved("run-001", "turn-001", "approval-001"),
        successful_tool_result("run-001", "turn-001", "call-001"),
        turn_started("run-001", "turn-002"),
    ];
    resolved_call.push(approval_resolved("run-001", "turn-002", "approval-001"));
    assert!(matches!(
        replay_events(resolved_call),
        Err(ReplayError::MismatchedToolLifecycleTurn {
            event_number: 8,
            ..
        })
    ));

    let mut mismatched_result = completed_call();
    mismatched_result.push(successful_tool_result("run-001", "turn-002", "call-001"));
    assert!(matches!(
        replay_events(mismatched_result),
        Err(ReplayError::MismatchedToolLifecycleTurn {
            event_number: 6,
            ..
        })
    ));

    let mut mixed_legacy = vec![
        run_started("run-001"),
        turn_started("run-001", "turn-001"),
        tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
        approval_requested(
            "run-001",
            "turn-001",
            "approval-001",
            "call-001",
            "read_file",
        ),
        approval_resolved("run-001", "turn-001", "approval-001"),
        successful_tool_result("run-001", "turn-001", "call-001"),
        tool_call_requested("run-001", "turn-001", "call-002", "read_file"),
        approval_requested(
            "run-001",
            "turn-001",
            "approval-002",
            "call-002",
            "read_file",
        ),
    ];
    mixed_legacy.push(successful_tool_result("run-001", "turn-001", "call-002"));
    assert!(matches!(
        replay_events_with_compatibility(
            mixed_legacy,
            ReplayCompatibility::LegacyApprovalWithoutResolution,
        ),
        Err(ReplayError::ToolResultBeforeApprovalResolution {
            event_number: 9,
            ..
        })
    ));
}

#[test]
fn store_identity_durable_io_and_replay_entrypoints_are_observable() {
    let log = TestLog::new("durable-public-api");
    let store = JsonlEventStore::new(log.path());
    let cloned = store.clone();
    let other = JsonlEventStore::new(log.path().with_extension("other.jsonl"));

    assert_eq!(store.path(), log.path());
    assert_eq!(store, cloned);
    assert_ne!(store, other);
    assert!(format!("{store:?}").contains("JsonlEventStore"));

    let started = agent_event(json!({ "type": "run_started", "run_id": "run-001" }));
    let finished = agent_event(json!({
        "type": "run_finished",
        "run_id": "run-001",
        "status": { "status": "completed", "final_message": "done" }
    }));
    store
        .append_durable(&started)
        .expect("first durable append succeeds");
    store
        .append_durable(&finished)
        .expect("second durable append reuses the validated handle");
    assert_eq!(
        store
            .repair_truncated_tail()
            .expect("committed log is intact"),
        0
    );
    assert_eq!(
        store
            .replay_with_compatibility(ReplayCompatibility::Strict)
            .expect("strict replay succeeds")
            .terminal_status(),
        Some(&TerminalRunStatus::Completed {
            final_message: "done".to_string(),
        })
    );
    assert_eq!(
        store
            .replay_for_recovery()
            .expect("recovery replay succeeds")
            .events()
            .len(),
        2
    );

    let empty = TestLog::new("empty-repair");
    std::fs::write(empty.path(), []).expect("empty log is created");
    assert_eq!(
        JsonlEventStore::new(empty.path())
            .repair_truncated_tail()
            .expect("empty log needs no repair"),
        0
    );
}

#[test]
fn missing_and_empty_logs_fail_closed_at_each_public_entrypoint() {
    let missing = TestLog::new("missing-entrypoints");
    let missing_store = JsonlEventStore::new(missing.path());
    assert!(matches!(
        missing_store.read_all(),
        Err(EventStoreError::OpenForRead { .. })
    ));
    assert!(matches!(
        missing_store.repair_truncated_tail(),
        Err(EventStoreError::OpenForRepair { .. })
    ));

    let empty = TestLog::new("empty-entrypoints");
    std::fs::write(empty.path(), []).expect("empty log is created");
    let empty_store = JsonlEventStore::new(empty.path());
    for error in [
        empty_store.replay().expect_err("empty replay must fail"),
        empty_store
            .replay_with_compatibility(ReplayCompatibility::Strict)
            .expect_err("empty compatibility replay must fail"),
        empty_store
            .replay_for_recovery()
            .expect_err("empty recovery replay must fail"),
    ] {
        assert!(matches!(
            error,
            EventStoreError::Replay {
                source: ReplayError::EmptyLog,
                ..
            }
        ));
    }
}

#[test]
fn append_and_read_paths_reject_external_corruption() {
    let missing_parent = TestLog::new("append-missing-parent");
    let nested_path = missing_parent
        .path()
        .with_extension("missing")
        .join("events.jsonl");
    let error = JsonlEventStore::new(&nested_path)
        .append(&run_started("run-001"))
        .expect_err("append must not invent a missing parent directory");
    assert!(matches!(
        error,
        EventStoreError::OpenForAppend { path, .. } if path == nested_path
    ));

    let corrupted_tail = TestLog::new("cached-invalid-tail");
    let store = JsonlEventStore::new(corrupted_tail.path());
    store
        .append(&run_started("run-001"))
        .expect("initial event should append");
    std::fs::write(corrupted_tail.path(), b"{not-json}\n")
        .expect("external corruption fixture should write");
    let error = store
        .append(&turn_started("run-001", "turn-001"))
        .expect_err("cached append state must re-decode the final committed record");
    assert!(matches!(
        error,
        EventStoreError::DecodeRecord { line: 1, .. }
    ));

    for (name, sequences, expected_line, expected, found) in [
        ("first-sequence", vec![2], 1, Some(1), Some(2)),
        ("later-gap", vec![1, 3], 2, Some(2), Some(3)),
    ] {
        let log = TestLog::new(name);
        let records = sequences
            .into_iter()
            .enumerate()
            .map(|(index, sequence)| {
                let event = if index == 0 {
                    run_started("run-001")
                } else {
                    turn_started("run-001", "turn-001")
                };
                serde_json::to_string(&event.with_event_sequence(EventSequence::new(sequence)))
                    .expect("fixture should serialize")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(log.path(), records).expect("sequence fixture should write");

        let error = JsonlEventStore::new(log.path())
            .read_all()
            .expect_err("invalid physical sequence must fail closed");
        assert!(matches!(
            error,
            EventStoreError::InvalidEventSequence {
                line,
                expected: actual_expected,
                found: actual_found,
                ..
            } if line == expected_line
                && actual_expected.map(EventSequence::as_u64) == expected
                && actual_found.map(EventSequence::as_u64) == found
        ));
    }
}

#[test]
fn reconciliation_rejects_legacy_history_and_sequence_gaps() {
    let legacy = TestLog::new("reconcile-legacy");
    let legacy_store = JsonlEventStore::new(legacy.path());
    let started = agent_event(json!({ "type": "run_started", "run_id": "run-001" }));
    legacy_store
        .append(&started)
        .expect("legacy event should append");
    assert!(matches!(
        legacy_store.reconcile(
            EventSequence::new(2),
            &turn_started_event(),
            EventDurability::Buffered,
        ),
        Err(EventStoreError::ReconciliationConflict { persisted, .. }) if persisted.len() == 1
    ));

    let missing = TestLog::new("reconcile-gap");
    let missing_store = JsonlEventStore::new(missing.path());
    assert!(matches!(
        missing_store.reconcile(
            EventSequence::new(2),
            &started,
            EventDurability::Buffered,
        ),
        Err(EventStoreError::ReconciliationConflict { persisted, .. }) if persisted.is_empty()
    ));
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
fn sequenced_event_logs_reject_gaps_duplicates_and_mixed_legacy_records() {
    for (name, first_sequence, second_sequence, expected, found) in [
        ("sequence-gap", Some(1), Some(3), Some(2), Some(3)),
        ("sequence-duplicate", Some(1), Some(1), Some(2), Some(1)),
        ("sequence-mixed", None, Some(2), None, Some(2)),
    ] {
        let log = TestLog::new(name);
        let mut store = JsonlEventStore::new(log.path());
        let first = agent_event(json!({ "type": "run_started", "run_id": "run-001" }));
        let second = turn_started_event();
        if let Some(sequence) = first_sequence {
            <JsonlEventStore as AgentEventSink>::append(
                &mut store,
                EventSequence::new(sequence),
                &first,
            )
            .expect("first physical record should append");
        } else {
            store.append(&first).expect("legacy fixture should append");
        }
        let error = <JsonlEventStore as AgentEventSink>::append(
            &mut store,
            EventSequence::new(second_sequence.expect("second record is sequenced")),
            &second,
        )
        .expect_err("invalid sequence must be rejected before writing");

        assert!(matches!(
            error,
            EventStoreError::InvalidEventSequence {
                line: 2,
                expected: actual_expected,
                found: actual_found,
                ..
            } if actual_expected.map(EventSequence::as_u64) == expected
                && actual_found.map(EventSequence::as_u64) == found
        ));
        let expected_first = first_sequence
            .map(|sequence| {
                first
                    .clone()
                    .with_event_sequence(EventSequence::new(sequence))
            })
            .unwrap_or_else(|| first.clone());
        assert_eq!(
            store.read_all().expect("unchanged log should remain valid"),
            [expected_first]
        );
        assert_eq!(
            std::fs::read_to_string(log.path())
                .expect("unchanged log bytes should read")
                .lines()
                .count(),
            1
        );
    }
}

#[test]
fn independent_store_instances_atomically_reject_the_same_sequence() {
    let log = TestLog::new("concurrent-sequence");
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for message in ["first", "second"] {
        let path = log.path().to_path_buf();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            let mut store = JsonlEventStore::new(path);
            let event = agent_event(json!({
                "type": "run_started",
                "run_id": message
            }));
            barrier.wait();
            <JsonlEventStore as AgentEventSink>::append(&mut store, EventSequence::new(1), &event)
        }));
    }
    barrier.wait();

    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("append worker should not panic"))
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        JsonlEventStore::new(log.path())
            .read_all()
            .expect("winning event should remain canonical")
            .len(),
        1
    );
}

#[test]
fn reader_waits_for_a_locked_partial_record_to_commit() {
    let log = TestLog::new("reader-waits-for-record-commit");
    let mut store = JsonlEventStore::new(log.path());
    let first = agent_event(json!({ "type": "run_started", "run_id": "run-001" }));
    <JsonlEventStore as AgentEventSink>::append(&mut store, EventSequence::new(1), &first)
        .expect("first event should append");
    let second = turn_started_event().with_event_sequence(EventSequence::new(2));
    let mut record = serde_json::to_vec(&second).expect("second event should encode");
    record.push(b'\n');
    let split = record.len() / 2;

    let (partial_tx, partial_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let writer_path = log.path().to_path_buf();
    let writer = thread::spawn(move || {
        let mut file = OpenOptions::new()
            .append(true)
            .open(writer_path)
            .expect("writer should open Event Log");
        file.lock().expect("writer should lock Event Log");
        file.write_all(&record[..split])
            .and_then(|()| file.flush())
            .expect("partial record should reach the file");
        partial_tx
            .send(())
            .expect("writer should signal partial write");
        continue_rx
            .recv()
            .expect("writer should await reader check");
        file.write_all(&record[split..])
            .and_then(|()| file.flush())
            .expect("record should finish");
        file.unlock().expect("writer should unlock Event Log");
    });
    partial_rx.recv().expect("partial record should be visible");

    let (reader_started_tx, reader_started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let reader_path = log.path().to_path_buf();
    let reader = thread::spawn(move || {
        reader_started_tx
            .send(())
            .expect("reader should signal start");
        result_tx
            .send(JsonlEventStore::new(reader_path).read_all())
            .expect("reader should send result");
    });
    reader_started_rx.recv().expect("reader should start");
    assert!(
        result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "reader must block instead of observing the partial record"
    );

    continue_tx.send(()).expect("writer should finish record");
    writer.join().expect("writer should finish cleanly");
    let events = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reader should finish after commit")
        .expect("reader should observe a canonical snapshot");
    reader.join().expect("reader should finish cleanly");
    assert_eq!(
        events,
        [first.with_event_sequence(EventSequence::new(1)), second]
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
            "type": "approval_resolved",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "approval_id": "approval-001",
            "decision": { "decision": "approve" }
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
    let replayed_tool_call = replay.tool_calls().next().expect("tool call should replay");
    let replayed_approval = replay.approvals().next().expect("approval should replay");
    let replayed_error = replay.errors().next().expect("error should replay");

    assert_eq!(
        (
            replay.run_id().as_str(),
            replay.status(),
            replay.events(),
            replay.tool_calls().len(),
            replayed_tool_call.model_tool_call_id().as_str(),
            replayed_tool_call.call().id.as_str(),
            replayed_tool_call
                .result()
                .map(|result| result.call_id.as_str()),
            replayed_approval.id.as_str(),
            replayed_error.code.as_str(),
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
        turn_started_event(),
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
    let replayed_tool_call = replay.tool_calls().next().expect("tool call should replay");

    assert_eq!(
        (
            replay.status(),
            replay.terminal_status(),
            replayed_tool_call
                .approval()
                .map(|request| request.id.as_str()),
            replayed_tool_call.result(),
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
fn strict_replay_requires_approval_resolution_before_a_tool_result() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
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
    ];

    let error = young_event_store::replay_events(events.clone())
        .expect_err("strict replay must reject an unresolved approval result");
    assert!(matches!(
        error,
        ReplayError::ToolResultBeforeApprovalResolution {
            event_number: 5,
            ref call_id,
        } if call_id.as_str() == "tool-call-001"
    ));

    let legacy = young_event_store::replay_events_with_compatibility(
        events,
        ReplayCompatibility::LegacyApprovalWithoutResolution,
    )
    .expect("legacy compatibility must be explicit");
    assert_eq!(legacy.status(), &RunStatus::Running);
}

#[test]
fn sequenced_logs_cannot_opt_into_legacy_approval_compatibility() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
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
    ]
    .into_iter()
    .enumerate()
    .map(|(index, event)| event.with_event_sequence(EventSequence::new(index as u64 + 1)))
    .collect();

    let error = young_event_store::replay_events_with_compatibility(
        events,
        ReplayCompatibility::LegacyApprovalWithoutResolution,
    )
    .expect_err("sequenced logs must use modern approval semantics");

    assert!(matches!(
        error,
        ReplayError::LegacyCompatibilityForSequencedLog { event_number: 1 }
    ));
}

#[test]
fn legacy_replay_rejects_mixed_approval_event_formats() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
        agent_event(json!({
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "model_tool_call_id": "model-call-001",
            "call": { "id": "tool-call-001", "tool_name": "one", "arguments": {} }
        })),
        agent_event(json!({
            "type": "approval_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "request": {
                "id": "approval-001",
                "call": { "id": "tool-call-001", "tool_name": "one", "arguments": {} },
                "reason": "approval one"
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
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "model_tool_call_id": "model-call-002",
            "call": { "id": "tool-call-002", "tool_name": "two", "arguments": {} }
        })),
        agent_event(json!({
            "type": "approval_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "request": {
                "id": "approval-002",
                "call": { "id": "tool-call-002", "tool_name": "two", "arguments": {} },
                "reason": "approval two"
            }
        })),
        agent_event(json!({
            "type": "approval_resolved",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "approval_id": "approval-002",
            "decision": { "decision": "approve" }
        })),
    ];

    let error = young_event_store::replay_events_with_compatibility(
        events,
        ReplayCompatibility::LegacyApprovalWithoutResolution,
    )
    .expect_err("one Agent Run cannot mix legacy and resolved approval formats");

    assert!(matches!(
        error,
        ReplayError::MixedApprovalLogFormats { event_number: 8 }
    ));
}

#[test]
fn legacy_replay_still_validates_the_reserved_denial_shape() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
        agent_event(json!({
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "model_tool_call_id": "model-call-001",
            "call": { "id": "tool-call-001", "tool_name": "one", "arguments": {} }
        })),
        agent_event(json!({
            "type": "approval_requested",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "request": {
                "id": "approval-001",
                "call": { "id": "tool-call-001", "tool_name": "one", "arguments": {} },
                "reason": "approval one"
            }
        })),
        agent_event(json!({
            "type": "tool_result",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "result": {
                "call_id": "tool-call-001",
                "output": {
                    "status": "failure",
                    "error": {
                        "code": "approval_denied",
                        "message": "legacy denial",
                        "retryable": true
                    }
                }
            }
        })),
    ];

    let error = young_event_store::replay_events_with_compatibility(
        events,
        ReplayCompatibility::LegacyApprovalWithoutResolution,
    )
    .expect_err("legacy compatibility must not accept a retryable denial");

    assert!(matches!(
        error,
        ReplayError::InvalidApprovalDenialResult {
            event_number: 5,
            ref call_id,
        } if call_id.as_str() == "tool-call-001"
    ));
}

#[test]
fn replay_rejects_successful_completion_with_an_unresolved_tool_call() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
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
            "type": "run_finished",
            "run_id": "run-001",
            "status": { "status": "completed", "final_message": "done" }
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("completed runs cannot abandon unresolved tool calls");
    assert!(matches!(
        error,
        ReplayError::TerminalWithUnresolvedToolCalls {
            event_number: 4,
            ref call_ids,
        } if call_ids == &[young_tool_runtime::ToolCallId::new("tool-call-001")]
    ));
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
fn replay_rejects_tool_events_for_a_turn_that_never_started() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        agent_event(json!({
            "type": "tool_call_requested",
            "run_id": "run-001",
            "turn_id": "turn-missing",
            "model_tool_call_id": "model-call-001",
            "call": {
                "id": "tool-call-001",
                "tool_name": "read_file",
                "arguments": { "path": "README.md" }
            }
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("turn-scoped events require a preceding TurnStarted");

    assert!(matches!(
        error,
        ReplayError::EventForUnknownTurn {
            event_number: 2,
            ref turn_id,
        } if turn_id.as_str() == "turn-missing"
    ));
}

#[test]
fn replay_rejects_starting_a_new_turn_with_an_unresolved_tool_call() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
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
            "type": "turn_started",
            "run_id": "run-001",
            "turn_id": "turn-002"
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("a new turn cannot bypass unresolved side effects");

    assert!(matches!(
        error,
        ReplayError::TurnStartedWithUnresolvedToolCalls {
            event_number: 4,
            ref turn_id,
            ref call_ids,
        } if turn_id.as_str() == "turn-002"
            && call_ids == &[young_tool_runtime::ToolCallId::new("tool-call-001")]
    ));
}

#[test]
fn replay_rejects_events_that_flow_back_to_an_inactive_turn() {
    let events = vec![
        agent_event(json!({ "type": "run_started", "run_id": "run-001" })),
        turn_started_event(),
        agent_event(json!({
            "type": "turn_started",
            "run_id": "run-001",
            "turn_id": "turn-002"
        })),
        agent_event(json!({
            "type": "model_output",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "event": { "type": "text_delta", "delta": "late" }
        })),
    ];

    let error = young_event_store::replay_events(events)
        .expect_err("events cannot flow back after the next turn starts");

    assert!(matches!(
        error,
        ReplayError::EventForInactiveTurn {
            event_number: 4,
            ref expected,
            ref found,
        } if expected.as_str() == "turn-002" && found.as_str() == "turn-001"
    ));
}

#[test]
fn replay_rejects_approval_after_the_tool_call_has_a_result() {
    let events = vec![
        agent_event(json!({
            "type": "run_started",
            "run_id": "run-001"
        })),
        turn_started_event(),
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
            (5, "tool-call-001", true)
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
        turn_started_event(),
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

    let error = young_event_store::replay_events_with_compatibility(
        events,
        ReplayCompatibility::LegacyApprovalWithoutResolution,
    )
    .expect_err("approval resolution after a tool result should fail");

    assert!(matches!(
        error,
        ReplayError::ApprovalResolutionAfterToolResult {
            event_number: 6,
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
        turn_started_event(),
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
        ReplayError::InvalidApprovalDenialResult {
            event_number: 6,
            ref call_id,
        } if call_id.as_str() == "tool-call-001"
    ));
}

#[test]
fn replay_requires_the_exact_canonical_result_for_a_denied_approval() {
    let events = approval_result_events(
        json!({ "decision": "deny", "reason": "not allowed" }),
        json!({
            "status": "failure",
            "error": {
                "code": "approval_denied",
                "message": "different reason",
                "retryable": true
            }
        }),
    );

    let error = young_event_store::replay_events(events)
        .expect_err("a forged denial result must fail replay");

    assert!(matches!(
        error,
        ReplayError::InvalidApprovalDenialResult {
            event_number: 6,
            ref call_id,
        } if call_id.as_str() == "tool-call-001"
    ));
}

#[test]
fn replay_rejects_the_reserved_denial_result_after_approval() {
    let events = approval_result_events(
        json!({ "decision": "approve" }),
        json!({
            "status": "failure",
            "error": {
                "code": "approval_denied",
                "message": "not allowed",
                "retryable": false
            }
        }),
    );

    let error = young_event_store::replay_events(events)
        .expect_err("approval_denied is reserved for an actual denial decision");

    assert!(matches!(
        error,
        ReplayError::InvalidApprovalDenialResult {
            event_number: 6,
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
        turn_started_event(),
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
fn durable_event_reconciliation_is_idempotent_before_and_after_commit() {
    for (name, already_committed) in [
        ("reconcile-before-commit", false),
        ("reconcile-after-commit", true),
    ] {
        let log = TestLog::new(name);
        let mut store = JsonlEventStore::new(log.path());
        let sequence = EventSequence::new(1);
        let event = agent_event(json!({
            "type": "approval_resolved",
            "run_id": "run-001",
            "turn_id": "turn-001",
            "approval_id": "approval-001",
            "decision": {
                "decision": "deny",
                "reason": "policy denied this exact invocation at decision time"
            }
        }));
        if already_committed {
            <JsonlEventStore as AgentEventSink>::append_durable(&mut store, sequence, &event)
                .expect("fixture event should commit durably");
        }

        store
            .reconcile(sequence, &event, EventDurability::Durable)
            .expect("first reconciliation should establish one durable event");
        store
            .reconcile(sequence, &event, EventDurability::Durable)
            .expect("repeated reconciliation should be idempotent");

        assert_eq!(
            store.read_all().expect("reconciled log should read"),
            [event.with_event_sequence(sequence)]
        );
    }
}

#[test]
fn durable_event_reconciliation_rejects_a_later_conflicting_identity() {
    let log = TestLog::new("reconcile-conflict");
    let mut store = JsonlEventStore::new(log.path());
    let committed = agent_event(json!({
        "type": "approval_resolved",
        "run_id": "run-001",
        "turn_id": "turn-001",
        "approval_id": "approval-001",
        "decision": { "decision": "approve" }
    }));
    let conflicting = agent_event(json!({
        "type": "approval_resolved",
        "run_id": "run-001",
        "turn_id": "turn-001",
        "approval_id": "approval-001",
        "decision": { "decision": "deny", "reason": "late policy result" }
    }));
    <JsonlEventStore as AgentEventSink>::append_durable(
        &mut store,
        EventSequence::new(1),
        &committed,
    )
    .expect("fixture event should commit");
    <JsonlEventStore as AgentEventSink>::append_durable(
        &mut store,
        EventSequence::new(2),
        &conflicting,
    )
    .expect("fixture event should commit");

    let error = store
        .reconcile(EventSequence::new(1), &committed, EventDurability::Durable)
        .expect_err("a later conflicting durable identity must remain visible");

    assert!(matches!(
        error,
        EventStoreError::ReconciliationConflict { .. }
    ));
    assert_eq!(
        store.read_all().expect("conflicting log should read"),
        [
            committed.with_event_sequence(EventSequence::new(1)),
            conflicting.with_event_sequence(EventSequence::new(2)),
        ]
    );
}

#[test]
fn durable_event_reconciliation_rejects_a_later_exact_duplicate() {
    let log = TestLog::new("reconcile-duplicate");
    let mut store = JsonlEventStore::new(log.path());
    let event = agent_event(json!({
        "type": "approval_resolved",
        "run_id": "run-001",
        "turn_id": "turn-001",
        "approval_id": "approval-001",
        "decision": { "decision": "approve" }
    }));
    for sequence in [EventSequence::new(1), EventSequence::new(2)] {
        <JsonlEventStore as AgentEventSink>::append_durable(&mut store, sequence, &event)
            .expect("duplicate fixture should append physically");
    }

    let error = store
        .reconcile(EventSequence::new(1), &event, EventDurability::Durable)
        .expect_err("duplicate durable identity must remain visible");

    assert!(matches!(
        error,
        EventStoreError::ReconciliationConflict { .. }
    ));
    assert_eq!(
        store.read_all().expect("duplicate log should read"),
        [
            event.clone().with_event_sequence(EventSequence::new(1)),
            event.with_event_sequence(EventSequence::new(2)),
        ]
    );
}

#[test]
fn durable_reconciliation_detects_every_kernel_owned_event_identity() {
    let cases = [
        (
            "tool-call",
            tool_call_requested("run-001", "turn-001", "call-001", "read_file"),
        ),
        (
            "approval-request",
            approval_requested(
                "run-001",
                "turn-001",
                "approval-001",
                "call-001",
                "read_file",
            ),
        ),
        (
            "tool-result",
            successful_tool_result("run-001", "turn-001", "call-001"),
        ),
        (
            "run-finished",
            agent_event(json!({
                "type": "run_finished",
                "run_id": "run-001",
                "status": { "status": "completed", "final_message": "done" }
            })),
        ),
    ];

    for (name, event) in cases {
        let log = TestLog::new(&format!("reconcile-identity-{name}"));
        let mut store = JsonlEventStore::new(log.path());
        <JsonlEventStore as AgentEventSink>::append_durable(
            &mut store,
            EventSequence::new(1),
            &event,
        )
        .expect("fixture event should commit");

        let error = store
            .reconcile(EventSequence::new(2), &event, EventDurability::Durable)
            .expect_err("a durable identity cannot move to another sequence");

        assert!(matches!(
            error,
            EventStoreError::ReconciliationConflict {
                sequence,
                persisted,
                ..
            } if sequence == EventSequence::new(2) && persisted.len() == 1
        ));
    }

    let log = TestLog::new("reconcile-unrelated-events");
    let mut store = JsonlEventStore::new(log.path());
    <JsonlEventStore as AgentEventSink>::append_durable(
        &mut store,
        EventSequence::new(1),
        &run_started("run-001"),
    )
    .expect("run start should commit");
    store
        .reconcile(
            EventSequence::new(2),
            &turn_started("run-001", "turn-001"),
            EventDurability::Durable,
        )
        .expect("unrelated consecutive events should reconcile normally");
    assert_eq!(store.read_all().unwrap().len(), 2);
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
