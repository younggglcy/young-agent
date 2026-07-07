# Canonical Event Log First

Status: accepted

For the first phase, Agent Runs will be represented only by our own Canonical Event Log. We will not import or export OpenAI, Anthropic, Codex, Claude Code, or other session formats in the first proof; future compatibility should be implemented through adapters after the kernel event model is stable.

## Considered Options

- **Adopt an external session format**: faster interop, but would leak another product's message, reasoning, tool-call, and compaction semantics into the kernel.
- **Support import/export in phase one**: useful eventually, but distracts from proving run, trace, replay, and tool execution.
- **Define our own Canonical Event Log first**: gives the kernel a stable internal model, with adapters possible later.
