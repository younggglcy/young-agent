# PRD: Agent Kernel + CLI Proof Surface

## Problem Statement

We want to build our own general-purpose agent, but the first validation surface should be coding because it gives us concrete tools, observable outcomes, and repeatable tests.

The first phase should not attempt to build a full product CLI, a multi-provider platform, or a full custom capability ecosystem. It should prove the smallest durable kernel that can run an agent loop, call tools, persist a canonical event log, enforce local workspace safety, and expose the run through a simple CLI proof surface.

The first real provider integration requirement is Qoder only. DeepSeek API and Codex API remain first-tier provider targets, but they are not required to complete the first phase.

## Solution

Build a Rust Agent Kernel with:

- A provider-neutral model runtime and a Qoder provider adapter.
- A deterministic agent runtime that can drive model output, tool calls, approvals, interruption, cancellation, and final results.
- A tool runtime with a built-in Coding Capability.
- A canonical JSONL Event Log and replay path.
- A minimal Rust CLI Proof Surface that streams kernel events and handles approval requests.
- Focused validation through deterministic fake-model tests, fake-tool tests, replay tests, workspace safety tests, and one Qoder integration smoke.

This is a kernel-first implementation. The CLI is only a proof surface for the kernel, not the long-term product surface.

## User Stories

- As an agent builder, I can run a local coding agent task from a CLI and see model events, tool events, approval requests, errors, and final status.
- As an agent builder, I can inspect a completed run through its canonical event log without relying on provider-specific session formats.
- As an agent builder, I can replay a recorded event log and recover the run timeline deterministically.
- As a future surface developer, I can consume a stable stream of Agent Events without depending on CLI-specific behavior.
- As a provider implementer, I can add a model provider by implementing a small provider adapter contract instead of touching the agent loop.
- As a test author, I can validate the agent loop with a fake model client and fake tools without real network calls.
- As a local user, I can trust that the coding tools stay inside the selected workspace boundary.
- As a local user, I can approve or deny commands that are mutating, destructive, dependency-installing, background-running, or outside the low-risk read and validation class.
- As a future capability author, I can see the shape of a Capability Pack contract even though custom user capability packs are deferred.
- As a future integration author, I can see where MCP-compatible tool boundaries would attach even though MCP runtime support is deferred.

## Implementation Decisions

- Use Rust for the core kernel and first proof surface.
- Keep the kernel general-purpose while validating it with the Coding Capability first.
- Keep the first phase local-only: current working directory plus git worktree safety.
- Use a provider-neutral model runtime with Qoder as the only required real provider in phase one.
- Use fake model clients and fake tools as the main deterministic test seam.
- Treat the canonical Event Log as the only persisted session format in phase one.
- Use JSONL for the Event Log.
- Keep external session import/export out of scope for phase one.
- Use TOML for built-in capability manifests.
- Support only built-in capability manifests in phase one.
- Include only the Coding Capability in phase one.
- Reserve an MCP Boundary in tool contracts.
- Do not implement MCP runtime in phase one.
- Allow low-risk read and validation commands by default.
- Require approval for mutating, destructive, dependency-installing, background-running, and cross-workspace commands.
- Treat the Rust CLI as a proof surface, not the final CLI product.
- Use the Pi monorepo as reference material only, not as a first-phase dependency.

## Testing Decisions

The highest-value test seam is a complete Agent Run through the Agent Kernel using a FakeModelClient, fake tools, and a temporary local workspace. This test should assert the final result and the emitted canonical Event Log.

Required test categories:

- Core contract serialization tests for model events, agent events, tool calls, tool results, and errors.
- Deterministic agent-loop tests with scripted fake model output.
- Tool runtime tests for tool lookup, tool execution, error propagation, and capability metadata.
- Event Log append, read, replay, and corruption handling tests.
- Workspace boundary tests for path traversal, symlinks, and git worktree detection.
- Command approval policy tests for allowed, approval-required, and rejected commands.
- CLI proof surface smoke tests for visible event streaming and approval handling.
- Qoder provider integration smoke, skipped unless credentials and endpoint configuration are present.

Tests should prove externally visible behavior rather than private helper implementation details.

## Out of Scope

- Final product CLI experience.
- TypeScript surface.
- DeepSeek provider implementation.
- Codex provider implementation.
- Provider compatibility matrix.
- MCP runtime.
- User-defined capability packs.
- Remote workspace execution.
- Docker sandboxing.
- Multi-agent orchestration.
- Long-term memory.
- Skills system.
- Session import/export from OpenAI, Anthropic, Codex, Claude Code, or Pi.
- Reusing Pi as a runtime dependency.

## First Batch Implementation Tasks

1. Scaffold the Rust workspace and crate skeleton.
2. Define core model, tool, agent, and event contracts.
3. Implement the JSONL Event Store and replay reader.
4. Implement a deterministic Agent Runtime with FakeModelClient support.
5. Implement Tool Runtime and built-in TOML capability manifest loading.
6. Implement local workspace safety and minimal coding tools.
7. Implement command approval policy and CLI approval handling.
8. Implement the CLI Proof Surface.
9. Implement QoderApiModelClient integration smoke.
10. Add seed coding eval tasks and the end-to-end validation harness.

## Completion Standard

Phase one is complete when:

- A local user can run one coding task from the Rust CLI Proof Surface.
- The run can call read, search, patch, and command tools through the kernel.
- Low-risk read and validation commands run without approval.
- High-risk commands require an approval event and continue only after approval.
- The run produces a complete JSONL Event Log.
- The Event Log can be replayed into a readable run timeline.
- Deterministic tests pass with FakeModelClient and fake tools.
- Local workspace boundary and git worktree safety tests pass.
- The CLI shows Agent Events and handles approval requests.
- Qoder provider smoke can run when configured.

## Supporting Notes

This PRD is synthesized from the current roadmap, glossary, ADRs, and prior research discussions. It intentionally preserves the first-phase narrowing decision: prove the Agent Kernel and CLI Proof Surface before expanding provider breadth or product surface area.
