# Audit Platform Process Hooks in One Unsafe Boundary

Status: accepted

The workspace continues to forbid unsafe Rust by default. Local command execution nevertheless
needs three post-fork hooks that Rust exposes through unsafe `CommandExt::pre_exec`: binding the
child cwd with `fchdir` on an already-open directory capability, enabling Linux
`PR_SET_NO_NEW_PRIVS` before `exec`, and duplicating a close-on-exec ownership token into the child
so normally inherited descendants keep the token alive until exit.

## Decision

`young-platform-process` is the only workspace crate allowed to lower `unsafe_code = "forbid"`
to `deny` and then allow individual audited blocks. It exposes safe functions only. The cwd hook
owns a cloned directory handle for the full lifetime of the registered hook. The token hook owns
its close-on-exec source descriptor, duplicates it without close-on-exec after fork, and transfers
that duplicate to the exec'd process. `PreparedTrackedCommand::spawn_group` consumes the command
builder, so the parent-side source is necessarily dropped before the spawned child is returned to
the capability layer. All hooks are limited to async-signal-safe syscalls with no allocation or
shared-state access after fork.

All capability, runtime, event-store, model, and surface crates continue to inherit the workspace
lint unchanged. Any new unsafe block requires updating this ADR's scope and reviewing whether it
belongs in the platform adapter.

## Considered Options

- Ambient cwd paths were rejected because they reopen the check-then-use race the workspace
  capability is intended to close.
- Keeping local `allow(unsafe_code)` attributes in the coding capability was rejected because it
  silently weakened the workspace policy across a large security-sensitive module.
- Treating a fixed number of successful process-group signals as proof of cleanup was rejected:
  a descendant fork can complete after the last signal. The inherited token gives the parent a
  positive completion signal; without token closure, ownership stays with the supervisor.
- Docker or a remote workspace remains a later isolation boundary and does not remove the need to
  make this first-phase local process handoff race-safe.
