## Context

The Agent Kernel needs stable contracts before behavior is implemented. These contracts should let model providers, tool runtime, agent runtime, event store, and surfaces evolve without tight coupling.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Define provider-neutral model request and model stream event types.
- Define tool definition, tool call, tool result, and tool error types.
- Define Agent Event types used by surfaces and Event Log persistence.
- Define run identity, turn identity, and basic status/error types.
- Add serialization and deserialization support for persisted contracts.

## Acceptance Criteria

- Contracts compile without requiring a concrete provider.
- Contracts compile without requiring concrete coding tools.
- Agent Events can represent model output, tool calls, tool results, approval requests, errors, final status, interruption, and cancellation.
- Tool definitions include metadata needed for future MCP boundary compatibility without implementing MCP runtime.
- Serialization tests cover representative event and tool payloads.

## Test Notes

- Add contract round-trip tests.
- Prefer explicit fixture values over snapshot tests unless snapshots are stable and readable.

## Out of Scope

- Agent loop implementation.
- Event Log file IO.
- Provider API calls.
- MCP runtime.
