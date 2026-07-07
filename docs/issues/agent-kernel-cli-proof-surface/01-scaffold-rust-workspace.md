## Context

This is the foundation issue for the first Agent Kernel phase. It creates the Rust workspace and crate boundaries needed by the rest of the implementation tasks.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Create a Rust workspace for the Agent Kernel project.
- Add crate skeletons for model runtime, agent runtime, tool runtime, event store, coding capability, and CLI proof surface.
- Add minimal public module structure and placeholder crate-level documentation.
- Add a basic workspace test command that compiles all crates.
- Keep implementations minimal; this issue should establish shape, not behavior.

## Acceptance Criteria

- The workspace builds with a single cargo command.
- Each planned crate exists and has a clear responsibility boundary.
- The CLI crate can compile a placeholder binary.
- No provider-specific implementation is introduced.
- No tool behavior is implemented beyond placeholders.

## Test Notes

- Add the smallest possible compile smoke test.
- Do not add broad integration tests in this issue.

## Out of Scope

- Agent loop behavior.
- Event log persistence.
- Tool execution.
- Qoder provider integration.
