# young-agent

[![codecov](https://codecov.io/gh/younggglcy/young-agent/graph/badge.svg?branch=main)](https://codecov.io/gh/younggglcy/young-agent)

`young-agent` is a Rust Agent Kernel experiment. The first implementation phase validates the kernel through a minimal CLI Proof Surface and built-in Coding Capability.

## Workspace

The workspace contains kernel crates, one audited platform adapter, one built-in capability crate,
and one proof surface crate. The CLI is a surface that consumes the kernel boundary; it is not
part of the Agent Kernel concept.

| Crate | Responsibility |
| --- | --- |
| `young-model-runtime` | Provider-neutral model runtime boundary. |
| `young-agent-runtime` | Agent Run orchestration boundary. |
| `young-tool-runtime` | Tool definition, policy, and execution boundary. |
| `young-event-store` | Canonical Event Log storage boundary. |
| `young-platform-process` | Audited safe API over the local process pre-exec boundary. |
| `young-capability-coding` | Built-in Coding Capability boundary. |
| `young-cli-proof-surface` | Rust CLI Proof Surface scaffold. |

## Docs

- [`CONTEXT.md`](CONTEXT.md): shared vocabulary for the Agent Kernel.
- [`docs/courses/`](docs/courses/): human-readable courses that trace how we build the agent from zero to one.
- [`docs/lessons/`](docs/lessons/): durable standalone lessons learned during implementation.

## Validate

```sh
cargo test --workspace
cargo run -p young-cli-proof-surface -- --fake --prompt "Validate the workspace" --workspace .
```

## CLI Proof Surface

The first Surface currently runs with the deterministic fake provider:

```sh
cargo run -p young-cli-proof-surface -- \
  --fake \
  --prompt "Summarize this workspace" \
  --workspace .
```

The CLI prints each Agent Event after it is committed to the Canonical Event Log and reports the
log path before the run starts. Default logs live in an application state directory outside the
selected workspace (`$YOUNG_AGENT_STATE_DIR`, `$XDG_STATE_HOME/young-agent`, or
`$HOME/.local/state/young-agent`, in that order on Unix). Pass `--event-log <PATH>` to reserve a
different new path. New runs keep the originally created file identity open so a path replacement
cannot redirect later events. Unix state directories are owner-checked, reject symlinks, and use
mode `0700`; Event Logs use mode `0600`. If no state environment is available, the Unix fallback
is isolated by uid under the system temporary directory.

Use `--fake-script <PATH>` to replay deterministic model turns. The file contains arrays of
`ModelStreamEvent` payloads, one array per model turn:

```json
{
  "turns": [
    [
      {
        "type": "text_delta",
        "delta": "Deterministic response."
      },
      {
        "type": "completed",
        "finish_reason": "stop"
      }
    ]
  ]
}
```

Fake scripts are limited to 8 MiB, 128 turns, 4,096 events per turn, and 16,384 events overall so
deterministic validation cannot consume unbounded startup memory.

Tool calls that require approval prompt on stdin and accept `y`/`yes`; `n`/`no`, a blank line, EOF,
or oversized input denies the exact prepared call, while other input is prompted again. Terminal
control and bidirectional-format characters from model, tool, and error text are escaped before
display. Process signals produce an `interrupted` terminal status by default. Use `--on-signal
cancel` when the host should record `cancelled` instead. The corresponding exit codes are `130`
and `125`; provider/runtime failure uses `2`.
