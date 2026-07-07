## Context

The core value of the project is the Agent Kernel loop. Before using a real provider, the loop should be proven with deterministic fake model output and fake tools.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement the first deterministic Agent Runtime loop.
- Add a FakeModelClient that can script model text, tool calls, errors, and final responses.
- Add fake tool execution support for runtime tests.
- Emit Agent Events for each externally visible step.
- Integrate Event Store appends during a run.
- Support basic stop conditions, model errors, tool errors, interruption, and cancellation.

## Acceptance Criteria

- A scripted fake model can request a fake tool and then produce a final answer.
- The runtime emits model, tool, error, approval, and final-status events as applicable.
- Runtime tests can assert final state and Event Log contents.
- Tool errors are surfaced as Agent Events and fed back to the model loop when appropriate.
- Interruption and cancellation produce distinct terminal states.

## Test Notes

- Add at least one happy-path run test with a tool call.
- Add one model-error test.
- Add one tool-error test.
- Add one cancellation or interruption test.

## Out of Scope

- Real provider networking.
- Real coding tools.
- CLI interaction.
- Sophisticated planning or memory.
