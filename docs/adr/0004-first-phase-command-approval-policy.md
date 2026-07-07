# First Phase Command Approval Policy

Status: accepted

For the first phase, `run_command` will allow read-only and low-risk validation commands by default, but commands that mutate files, change permissions, alter git history, install dependencies, manage background processes, or operate outside the workspace boundary must request approval. This gives the CLI proof surface enough autonomy to validate coding tasks while keeping side effects visible in the event log and under human control.

## Considered Options

- **Approve every command**: safest, but too slow to prove an agent loop that can inspect and validate work.
- **Allow every command**: fastest, but too risky for a local cwd proof without Docker isolation.
- **Allow low-risk commands and approve mutating/high-risk commands**: preserves useful autonomy while keeping side effects explicit.
