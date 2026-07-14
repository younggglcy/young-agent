# First Phase Command Approval Policy

Status: accepted

For the first phase, `run_command` will allow read-only and low-risk validation commands by default, but commands that mutate files, change permissions, alter git history, install dependencies, manage background processes, or operate outside the workspace boundary must request approval. This gives the CLI proof surface enough autonomy to validate coding tasks while keeping side effects visible in the event log and under human control.

The policy classifies one concrete `ToolCall` exactly once and stores the result in the Tool
Runtime's prepared plan. A call-dependent handler must explicitly return `Allow`,
`RequiresApproval`, or `Reject`. The Agent Runtime owns `ApprovalRequested` and
`ApprovalResolved` events and consumes the same prepared plan after a decision, so the command
shown to the user cannot drift from the command that executes.

Classification is deliberately conservative rather than a complete shell parser. It scans a
bounded command, allows only named read and validation forms, and requires approval for unknown
programs, dynamic expansion, redirection, background execution, tool-specific helper hooks, and
explicit cross-workspace paths. Malformed, over-limit, overly complex, privilege-elevating, and
clearly root-targeting commands are rejected before execution. Safe shell composition is allowed
only when every simple command is independently low-risk.

This policy is an approval boundary, not a shell sandbox. Binding the child cwd to the workspace
handle prevents cwd handoff races but does not make an approved shell incapable of accessing the
rest of the host. A stronger filesystem or process isolation boundary remains future work.

## Considered Options

- **Approve every command**: safest, but too slow to prove an agent loop that can inspect and validate work.
- **Allow every command**: fastest, but too risky for a local cwd proof without Docker isolation.
- **Allow low-risk commands and approve mutating/high-risk commands**: preserves useful autonomy while keeping side effects explicit.
