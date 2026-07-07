# First Phase Uses Local Workspace Boundary

Status: accepted

For the first phase, the Agent Kernel proof will run against the local current working directory with git worktree safety, rather than Docker or a remote workspace. This keeps the first proof focused on the kernel, event log, tool runtime, and CLI proof surface while still requiring file and command tools to respect a concrete workspace boundary.

## Considered Options

- **Local cwd + git worktree safety**: smallest useful boundary for a coding proof, easiest to inspect and debug.
- **Docker workspace**: stronger isolation, but adds image, mount, dependency, and terminal complexity before the kernel is proven.
- **Remote workspace server**: closer to future product scenarios, but too much infrastructure for the first proof.
