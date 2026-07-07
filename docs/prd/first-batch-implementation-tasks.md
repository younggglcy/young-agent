# First Batch Implementation Tasks

## Issue Plan

These issues implement the first phase of the Agent Kernel roadmap. The intended dependency order is:

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

1. Scaffold Rust workspace.
2. Define core contracts.
3. Implement Event Store.
4. Implement deterministic Agent Runtime.
5. Implement Tool Runtime and built-in capability manifest.
6. Implement workspace safety and coding tools.
7. Implement command approval policy and CLI approval handling.
8. Implement CLI Proof Surface.
9. Implement Qoder provider smoke.
10. Add seed eval tasks and validation harness.

The first two issues establish shared interfaces. Issues after that should remain small enough for agent handoff, but some will depend on the contracts from earlier issues.

## Testing Seam

The primary seam is an end-to-end Agent Run through the Agent Kernel with a FakeModelClient, fake tools, and a temporary workspace. This gives the project one stable place to validate model events, tool calls, approvals, event logging, replay, and final status without a network provider.

## First Provider Scope

Only Qoder is required in this first batch. DeepSeek API and Codex API are first-tier future providers but should not be implemented as part of this batch.

## Published Issues

| Order | Issue | Title |
| --- | --- | --- |
| 0 | https://github.com/younggglcy/young-agent/issues/1 | PRD: Agent Kernel + CLI Proof Surface |
| 1 | https://github.com/younggglcy/young-agent/issues/2 | Task: Scaffold Rust workspace for Agent Kernel |
| 2 | https://github.com/younggglcy/young-agent/issues/3 | Task: Define Agent Kernel contracts and event model |
| 3 | https://github.com/younggglcy/young-agent/issues/4 | Task: Implement JSONL Event Store and replay |
| 4 | https://github.com/younggglcy/young-agent/issues/5 | Task: Implement deterministic Agent Runtime with FakeModelClient |
| 5 | https://github.com/younggglcy/young-agent/issues/6 | Task: Implement Tool Runtime and built-in capability manifest |
| 6 | https://github.com/younggglcy/young-agent/issues/7 | Task: Implement workspace safety and minimal coding tools |
| 7 | https://github.com/younggglcy/young-agent/issues/8 | Task: Implement command approval policy |
| 8 | https://github.com/younggglcy/young-agent/issues/9 | Task: Implement CLI Proof Surface |
| 9 | https://github.com/younggglcy/young-agent/issues/10 | Task: Implement Qoder provider integration smoke |
| 10 | https://github.com/younggglcy/young-agent/issues/11 | Task: Add seed evals and validation harness |
