# NAPI-RS TypeScript CLI vs Rust CLI Proof Surface

Date: 2026-07-07

## Question

Should the Rust CLI Proof Surface stay inside the Rust workspace, or should the Rust core be exposed through NAPI-RS while the CLI is implemented in TypeScript?

## Recommendation

Keep `cli-proof-surface` inside the Rust workspace for the first Agent Kernel phase. Do not move the first CLI Proof Surface to TypeScript via NAPI-RS yet.

The reason is not that NAPI-RS is a poor fit. NAPI-RS is a credible future bridge for TypeScript surfaces because Node-API is ABI-stable across Node.js versions and is intended to let native addons survive JavaScript engine changes without recompilation across later major versions ([Node-API docs](https://nodejs.org/api/n-api.html#node-api), [Node.js ABI stability guide](https://nodejs.org/learn/modules/abi-stability#n-api)). The issue is phase order: this repo's first-phase plan is explicitly kernel-first, with a minimal Rust CLI Proof Surface, a canonical JSONL event log, deterministic fake-model tests, local workspace safety, and Qoder smoke validation before TypeScript surfaces enter scope ([PRD](../prd/agent-kernel-cli-proof-surface.md), [CLI issue](../issues/agent-kernel-cli-proof-surface/08-cli-proof-surface.md), [first batch plan](../prd/first-batch-implementation-tasks.md)).

## Repository constraints

The current accepted architecture is "Rust core + TypeScript surfaces": Rust owns high-control runtime layers; TypeScript is intended for richer user-facing surfaces such as desktop, IDE, web, and configuration UI ([ADR 0001](../adr/0001-rust-core-typescript-surface.md)). That ADR supports a future TypeScript surface, but it does not require the first proof CLI to be TypeScript.

The first-phase PRD is narrower: build the Agent Kernel, canonical event log, tool runtime, approval policy, and a minimal Rust CLI Proof Surface that streams Agent Events and handles approval requests ([PRD solution and decisions](../prd/agent-kernel-cli-proof-surface.md)). It also says TypeScript surface work and final product CLI UX are out of scope for phase one ([PRD out of scope](../prd/agent-kernel-cli-proof-surface.md), [CLI issue out of scope](../issues/agent-kernel-cli-proof-surface/08-cli-proof-surface.md)).

The scaffold task specifically asks for a Rust workspace with crate skeletons for model runtime, agent runtime, tool runtime, event store, coding capability, and CLI Proof Surface, plus one workspace build command and a placeholder CLI binary ([scaffold issue](../issues/agent-kernel-cli-proof-surface/01-scaffold-rust-workspace.md)). Cargo workspaces are designed for this shape: common commands can run across members, members share one `Cargo.lock`, and package commands can operate on all or selected workspace packages ([Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html#workspaces)).

## Option A: keep the first CLI in Rust

This option keeps `cli-proof-surface` as a Rust binary target in the same workspace as the kernel crates. Cargo binary targets can use the package library API, can be run with `cargo run --bin`, and can be installed with `cargo install` ([Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#binaries), [cargo install](https://doc.rust-lang.org/cargo/commands/cargo-install.html#description)).

Benefits:

- The API boundary stays inside Rust until the core contracts are proven. This matches the core-contract issue, which needs provider-neutral model stream events, tool call/result contracts, Agent Events, run status/error types, and serialization before concrete surfaces become important ([core contracts issue](../issues/agent-kernel-cli-proof-surface/02-core-contracts.md)).
- Event streaming, replay, and approval can be validated with the same Rust event types that the Event Store persists. The roadmap makes the canonical Event Log the first-phase source of truth and defers external session formats until after the kernel event model is stable ([ADR 0007](../adr/0007-canonical-event-log-first.md), [event store issue](../issues/agent-kernel-cli-proof-surface/03-jsonl-event-store.md)).
- The main deterministic test seam remains one Rust end-to-end Agent Run using FakeModelClient, fake tools, and a temporary workspace, which is exactly what the first-batch plan calls out ([first batch testing seam](../prd/first-batch-implementation-tasks.md), [runtime issue](../issues/agent-kernel-cli-proof-surface/04-deterministic-agent-runtime.md)).
- Distribution can start as normal Rust build/install behavior instead of native npm package distribution. That matters because first phase is a proof surface, not the final user-facing CLI ([PRD completion standard](../prd/agent-kernel-cli-proof-surface.md)).

Costs:

- Later TypeScript surfaces will still need a boundary: NAPI-RS, a local JSON-RPC process, WASM, or another adapter.
- Terminal UX iteration may be slower than a TypeScript CLI stack.
- A Rust CLI Proof Surface should avoid growing into the final product CLI, because the repo terminology explicitly defines it as a proof surface, not the long-term surface ([CONTEXT.md](../../CONTEXT.md)).

## Option B: expose Rust via NAPI-RS and write the CLI in TypeScript now

This option adds a native addon package around the Rust Agent Kernel and implements the CLI in TypeScript that loads the generated `.node` addon.

Benefits:

- It aligns with the long-term "TypeScript surfaces" direction from ADR 0001 ([ADR 0001](../adr/0001-rust-core-typescript-surface.md)).
- NAPI-RS can generate JavaScript binding files and TypeScript definition files alongside the native `.node` addon after build ([NAPI-RS simple package](https://napi.rs/docs/introduction/simple-package#create-napi-rscool)).
- NAPI-RS exposes CLI and programmatic build APIs, including generation of JS bindings, `.d.ts` files, and native addon outputs ([NAPI-RS build](https://napi.rs/docs/cli/build#usage), [NAPI-RS programmatic API](https://napi.rs/docs/cli/programmatic-api#output-types)).

Costs:

- The Agent Kernel API would need an FFI-safe JavaScript-facing shape before the Rust contracts have earned stability. This conflicts with the first-phase goal of proving provider-neutral model events, Agent Events, tool contracts, serialization, replay, and approval behavior inside the kernel first ([core contracts issue](../issues/agent-kernel-cli-proof-surface/02-core-contracts.md), [runtime issue](../issues/agent-kernel-cli-proof-surface/04-deterministic-agent-runtime.md)).
- Streaming Agent Events over NAPI is not just a type export. NAPI-RS supports `async fn` by converting Tokio futures into JavaScript Promises, but event streams and callbacks that cross native threads require `AsyncTask` or `ThreadsafeFunction` designs ([NAPI-RS async fn](https://napi.rs/docs/concepts/async-fn#tokio-integration), [NAPI-RS AsyncTask](https://napi.rs/docs/concepts/async-task#task), [NAPI-RS ThreadsafeFunction](https://napi.rs/docs/concepts/threadsafe-function#thread-safefunction)).
- Thread-safe callback lifecycles are real design surface. Node's docs say JavaScript functions normally only run from the addon's main thread; additional native threads must communicate with that main thread, and thread-safe functions carry queue, lifecycle, ref/unref, acquire/release, abort, and cleanup behavior ([Node-API thread-safe calls](https://nodejs.org/api/n-api.html#asynchronous-thread-safe-function-calls), [Node.js Thread-Safe Functions guide](https://nodejs.org/learn/node-api/special-topics/thread-safe-functions#thread-safe-functions)).
- Error typing for callbacks is not free. NAPI-RS notes that `ThreadsafeFunction` is complex enough that generated TypeScript types sometimes need explicit overrides ([NAPI-RS type overrides](https://napi.rs/docs/concepts/types-overwrite#types-overwrite)), and its `CalleeHandled: false` mode cannot pass Rust-thread errors back to JavaScript and can crash the process on synchronous JS callback errors ([NAPI-RS ThreadsafeFunction error strategy](https://napi.rs/docs/concepts/threadsafe-function#calleehandled-false)).
- Native npm distribution adds a second release system. NAPI-RS recommends platform-specific npm packages because Rust addon source distribution creates toolchain and compile-time burden for users, but that approach requires per-platform packages and extra author-side release management ([NAPI-RS release deep dive](https://napi.rs/docs/deep-dive/release#release-native-packages)).
- NAPI-RS helps with that release work, but the shape is still larger than a proof CLI: generated projects can choose many target triples, create platform packages, use optional dependencies, and load the right native binding at runtime ([NAPI-RS simple package targets](https://napi.rs/docs/introduction/simple-package#create-napi-rscool), [NAPI-RS getting started deep dive](https://napi.rs/docs/introduction/getting-started#deep-dive), [NAPI-RS artifacts](https://napi.rs/docs/cli/artifacts#how-does-it-work)).
- Cross-build behavior introduces CI/toolchain details before the kernel is proven. NAPI-RS documents experimental cross-compilation flags, target-specific linker behavior, and environment variable interactions with Cargo and `RUSTFLAGS` ([NAPI-RS build cross-compilation flags](https://napi.rs/docs/cli/build#cross-compilation-flags), [NAPI-RS build linker behavior](https://napi.rs/docs/cli/build#default-linkers-for-less-common-targets)).
- Testing would split across Rust kernel tests and JavaScript/native-addon tests. NAPI-RS scaffolding includes a JavaScript test-framework option, currently only `ava`, but the repo's first phase asks for deterministic Rust fake-model/fake-tool/event-log tests as the primary seam ([NAPI-RS new options](https://napi.rs/docs/cli/new#options), [first batch testing seam](../prd/first-batch-implementation-tasks.md)).

## Tradeoff matrix

| Dimension | Rust CLI in workspace | TypeScript CLI over NAPI-RS |
| --- | --- | --- |
| API boundary | Keeps first boundary as Rust traits/types and serialized Agent Events until contracts stabilize ([core contracts issue](../issues/agent-kernel-cli-proof-surface/02-core-contracts.md)). | Forces a JS-facing ABI/API early; likely needs class/factory/callback wrappers before the event model has stabilized. |
| Distribution | Uses normal Cargo workspace, binary target, and install path ([Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html#workspaces), [Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#binaries)). | Adds npm package, native `.node` artifacts, platform packages, optional dependencies, and binding loader logic ([NAPI-RS getting started](https://napi.rs/docs/introduction/getting-started#deep-dive)). |
| Streaming/events | CLI can consume Rust `AgentEvent` stream directly and persist the same events to JSONL ([event store issue](../issues/agent-kernel-cli-proof-surface/03-jsonl-event-store.md)). | Needs Promise/callback/thread-safe-function design for event streaming and cancellation ([NAPI-RS async fn](https://napi.rs/docs/concepts/async-fn#tokio-integration), [Node-API thread-safe calls](https://nodejs.org/api/n-api.html#asynchronous-thread-safe-function-calls)). |
| Async/runtime | One Rust async/runtime ownership story for model streaming, tool execution, approval, interruption, and cancellation ([runtime issue](../issues/agent-kernel-cli-proof-surface/04-deterministic-agent-runtime.md)). | Bridges Rust async/Tokio and Node event loop; NAPI-RS supports this, but `async fn`, `AsyncTask`, and `ThreadsafeFunction` have distinct constraints ([NAPI-RS async fn](https://napi.rs/docs/concepts/async-fn#tokio-integration), [NAPI-RS AsyncTask](https://napi.rs/docs/concepts/async-task#task)). |
| Packaging | One Rust build graph during proof. | Adds native addon CI/package matrix; `targets` drives scaffolding/package creation but does not make `napi build` compile all targets by itself ([NAPI-RS config](https://napi.rs/docs/cli/napi-config#schema)). |
| Testing | Matches required deterministic Rust test seam and CLI smoke tests ([PRD testing decisions](../prd/agent-kernel-cli-proof-surface.md)). | Requires Rust tests plus JS/native load/streaming tests; NAPI-RS scaffolding defaults its JS test-framework option to AVA-only ([NAPI-RS new options](https://napi.rs/docs/cli/new#options)). |
| Roadmap fit | Directly implements issues 1-10 without moving TypeScript surface into scope ([first batch plan](../prd/first-batch-implementation-tasks.md)). | Pulls TypeScript surface, native addon packaging, and cross-language ABI design into phase one, despite TypeScript surfaces being out of scope ([PRD out of scope](../prd/agent-kernel-cli-proof-surface.md)). |

## Suggested path

1. Keep the first CLI Proof Surface as a Rust workspace member.
2. Make the Rust CLI intentionally thin: it should start runs, render Agent Events, handle approval, show event-log location, and support fake-provider validation, matching the CLI issue acceptance criteria ([CLI issue](../issues/agent-kernel-cli-proof-surface/08-cli-proof-surface.md)).
3. Stabilize these Rust-owned contracts first: model stream events, Agent Events, tool contracts, approval request/decision events, event-log serialization, replay model, and cancellation/interruption states ([core contracts issue](../issues/agent-kernel-cli-proof-surface/02-core-contracts.md), [event store issue](../issues/agent-kernel-cli-proof-surface/03-jsonl-event-store.md), [approval issue](../issues/agent-kernel-cli-proof-surface/07-command-approval-policy.md)).
4. After the first phase completes, add a deliberate "surface adapter" design decision. At that point, compare NAPI-RS against a process boundary such as JSON-RPC/stdout events. NAPI-RS should be preferred if TypeScript surfaces need low-latency in-process calls or a typed npm package; a process boundary should be preferred if crash isolation, independent release cadence, and simpler streaming/backpressure are more important.
5. If NAPI-RS is chosen later, implement it as a separate adapter package/crate, not as the owner of kernel contracts. Export the same canonical event wire format used by the JSONL Event Log so TypeScript surfaces do not define a second event model ([ADR 0007](../adr/0007-canonical-event-log-first.md)).

## Bottom line

For phase one, NAPI-RS would solve a future surface problem before the kernel contract exists. The current roadmap needs the opposite: prove the Rust Agent Kernel and a small Rust CLI Proof Surface first, then expose the stabilized event/run API to TypeScript through a consciously chosen adapter.
