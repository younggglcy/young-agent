# Agent Kernel

This context defines the shared language for building a general-purpose agent system whose first proof is a coding capability.

## Language

**Agent Kernel**:
The generic agent core that owns runs, turns, model interaction, tool dispatch, policy decisions, and durable events without depending on any one capability.
_Avoid_: Coding agent core, framework core

**Capability Pack**:
A loadable bundle of tools, instructions, evaluation seeds, and policy defaults that teaches the Agent Kernel a domain-specific skill area.
_Avoid_: Plugin, feature pack, module

**Capability Manifest**:
The TOML metadata file that declares a built-in Capability Pack's identity, tools, instructions, evaluation seeds, and policy defaults.
_Avoid_: Plugin config, extension manifest

**Coding Capability**:
The first Capability Pack, focused on repository reading, file edits, patches, commands, tests, and coding-oriented evaluation tasks.
_Avoid_: Coding kernel, coding-only agent

**Surface**:
A user-facing host for the Agent Kernel, such as a CLI, desktop app, IDE extension, or web console.
_Avoid_: Frontend, UI shell

**CLI Proof Surface**:
The first minimal Surface used to prove the Agent Kernel end to end from a terminal, without committing to the long-term desktop, IDE, or web experience.
_Avoid_: Product CLI, final CLI

**Provider Adapter**:
An implementation that maps a model provider API into the Agent Kernel's model event contract.
_Avoid_: SDK wrapper, model client

**Pi Monorepo**:
The existing reference implementation at `~/projects/pi`, used as an architectural reference but not as a first-phase dependency.
_Avoid_: Pi mono, pi gateway

**Agent Run**:
A single execution of the Agent Kernel against a user goal, producing a durable stream of events and a final outcome.
_Avoid_: Chat session, job

**Event Log**:
The append-only record of an Agent Run, used for replay, debugging, evaluation, and later surface rendering.
_Avoid_: Transcript, conversation history

**Canonical Event Log**:
The first-phase source of truth for Agent Runs; external session formats may be adapted later but do not define the kernel model.
_Avoid_: Imported session, chat transcript

**Tool Runtime**:
The shared host for tool definitions, permissions, execution, result normalization, and approval boundaries.
_Avoid_: Tool registry only, tools package

**MCP Boundary**:
The reserved compatibility shape that allows future MCP tools to be mapped into the Tool Runtime without implementing MCP in the first phase.
_Avoid_: MCP support, MCP runtime

**MCP Runtime**:
The future runtime that would connect to MCP servers, discover dynamic tools, execute them, and frame their outputs.
_Avoid_: Tool Runtime

**Workspace Boundary**:
The file-system scope within which an Agent Run is allowed to read, write, and execute commands for the first proof.
_Avoid_: Sandbox, project folder

**Git Worktree Safety**:
The first-phase safety posture that treats the active git worktree as the unit of local mutation, audit, and recovery.
_Avoid_: Docker sandbox, remote workspace safety

**Command Approval Policy**:
The rule set that decides whether a command tool call may execute immediately or must pause the Agent Run for human approval.
_Avoid_: Shell permissions, command filter
