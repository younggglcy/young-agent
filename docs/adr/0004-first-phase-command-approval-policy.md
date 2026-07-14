# First Phase Command Approval Policy

Status: accepted

For the first phase, `run_command` will allow read-only and low-risk validation commands by default, but commands that mutate files, change permissions, alter git history, install dependencies, manage background processes, or operate outside the workspace boundary must request approval. This gives the CLI proof surface enough autonomy to validate coding tasks while keeping side effects visible in the event log and under human control.

The policy classifies one concrete `ToolCall` exactly once and stores the result in the Tool
Runtime's prepared plan. A call-dependent handler must explicitly return `Allow`,
`RequiresApproval`, or `Reject`. The Agent Runtime owns `ApprovalRequested` and
`ApprovalResolved` events and consumes the same prepared plan after a decision, so the command
shown to the user cannot drift from the command that executes. Approval and rejection reasons
must contain non-whitespace text; an invalid dynamic policy result is rejected before it can
produce an empty approval prompt or failure event.

Classification is deliberately conservative rather than a complete shell parser. It scans a
bounded command, allows only named read and validation forms, and requires approval for unknown
programs, dynamic expansion, redirection, background execution, tool-specific helper hooks, and
explicit cross-workspace paths. Malformed, over-limit, overly complex, privilege-elevating, and
clearly root-targeting commands are rejected before execution. Safe shell composition is allowed
only when every simple command is independently low-risk.

Git commands that read or refresh the index (`status`, `diff`, `ls-files`, and `grep`) require
approval because repository or inherited `core.fsmonitor` configuration can execute a helper.
Likewise, signature verification and explicit pager, text-conversion, or external-diff modes are
not automatically allowed even when the outer Git command appears read-only. The unconditional
fsmonitor decision is made before workspace path probing so an already-known approval result does
not consume the bounded filesystem-inspection budget.

Workspace discovery and command execution share the same executable-selection and inherited Git
environment boundary. Repository-location, config-injection, and `GIT_TRACE*` destinations are
removed, so the initial Git probe cannot execute workspace content and automatically allowed
`git rev-parse` or exact `git branch --show-current` forms stay bound to the selected workspace
without hidden external writes. Compressed-file inspection modes of `file` require approval
because the utility delegates decompression to programs such as `zstd` and `lzip`.

Before execution, the command runner also removes shell startup, exported-function, and dynamic
loader injection variables. Its `PATH` contains only canonical absolute directories outside the
workspace, and drops any directory whose low-risk executable resolves back into the workspace.
This keeps the executable selected by an automatically allowed command from being replaced by
workspace content while preserving externally installed toolchains. Path inspection is bounded,
deduplicated before filesystem probes, and completed before reserving command-supervision
capacity. The command runner protects the complete low-risk program set, while workspace
discovery probes only `git` and therefore avoids unrelated executable checks. If no safe entry
remains or the budget is exhausted, the runner uses an absolute non-directory sentinel instead of
an empty `PATH`, because an empty shell path denotes cwd.

The scanner recognizes only the lexical detail needed for that decision: POSIX space and tab word
separation, single and double quotes, backslash escaping and line continuation, and composition by
newline, `;`, `&&`, `||`, or `|`. It does not semantically interpret here-documents, process
substitution, assignment prefixes, control structures, or shell function definitions. Those forms
must fail closed as `RequiresApproval` or `Reject`; adding an automatically allowed shell form
requires an explicit contract change plus classification and real-execution regression tests.

This policy is an approval boundary, not a shell sandbox. Binding the child cwd to the workspace
handle prevents cwd handoff races, and the sanitized environment closes known inherited-execution
paths, but neither makes an approved shell incapable of accessing the rest of the host. A stronger
filesystem or process isolation boundary remains future work.

## Considered Options

- **Approve every command**: safest, but too slow to prove an agent loop that can inspect and validate work.
- **Allow every command**: fastest, but too risky for a local cwd proof without Docker isolation.
- **Allow low-risk commands and approve mutating/high-risk commands**: preserves useful autonomy while keeping side effects explicit.
