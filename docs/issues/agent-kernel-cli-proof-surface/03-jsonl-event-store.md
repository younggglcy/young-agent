## Context

The canonical Event Log is the only persisted session format in phase one. It should be simple, append-only, and replayable.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement an append-only JSONL Event Store.
- Persist Agent Events using the core contract types.
- Support reading a complete event log back into ordered events.
- Support replaying events into a readable run timeline or replay model.
- Handle malformed, truncated, or unsupported event records with clear errors.

## Acceptance Criteria

- A run can append multiple Agent Events to a JSONL log.
- The same log can be read back in order.
- Replay can reconstruct key run state such as status, tool calls, approvals, errors, and final result.
- Corrupted event records fail with actionable error messages.
- Event Store tests do not require a real model provider.

## Test Notes

- Add append/read/replay tests.
- Add a malformed JSONL test.
- Add an unsupported event version or schema test if versioning is introduced.

## Out of Scope

- External session import/export.
- Compression.
- Remote event storage.
- UI rendering of replay output.
