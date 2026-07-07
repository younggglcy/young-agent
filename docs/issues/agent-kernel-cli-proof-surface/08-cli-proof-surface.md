## Context

The CLI is the first proof surface for the kernel. It should expose the Agent Kernel behavior clearly without becoming the final product CLI.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement a minimal CLI entrypoint for starting an Agent Run.
- Stream Agent Events in a readable terminal format.
- Show the Event Log location for each run.
- Handle approval requests from the kernel.
- Support interrupt and cancel behavior.
- Support a fake-provider mode for deterministic local validation.

## Acceptance Criteria

- A user can start a run with a prompt and workspace.
- The CLI displays model output, tool calls, tool results, approval prompts, errors, and final status.
- Approval prompts can be granted or denied.
- The CLI can run with FakeModelClient for deterministic tests.
- The CLI reports the Event Log path.
- Interrupt or cancel produces a distinct terminal status.

## Test Notes

- Add CLI smoke tests around fake-provider mode if practical.
- Keep terminal formatting tests lightweight.
- Prefer asserting event flow over exact decorative output.

## Out of Scope

- Final UX polish.
- TUI.
- TypeScript or GUI surfaces.
- Multi-session management.
