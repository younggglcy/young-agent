# young-agent

`young-agent` is a Rust Agent Kernel experiment. The first implementation phase validates the kernel through a minimal CLI Proof Surface and built-in Coding Capability.

## Workspace

The workspace contains kernel crates, one built-in capability crate, and one proof surface crate. The CLI is a surface that consumes the kernel boundary; it is not part of the Agent Kernel concept.

| Crate | Responsibility |
| --- | --- |
| `young-model-runtime` | Provider-neutral model runtime boundary. |
| `young-agent-runtime` | Agent Run orchestration boundary. |
| `young-tool-runtime` | Tool definition, policy, and execution boundary. |
| `young-event-store` | Canonical Event Log storage boundary. |
| `young-capability-coding` | Built-in Coding Capability boundary. |
| `young-cli-proof-surface` | Rust CLI Proof Surface scaffold. |

## Docs

- [`CONTEXT.md`](CONTEXT.md): shared vocabulary for the Agent Kernel.
- [`docs/courses/`](docs/courses/): human-readable courses that trace how we build the agent from zero to one.
- [`docs/lessons/`](docs/lessons/): durable standalone lessons learned during implementation.

## Validate

```sh
cargo test --workspace
cargo run -p young-cli-proof-surface
```
