# AGENTS.md

Repo-local instructions for coding agents working on `young-agent`.

- Default to Chinese when talking with the repo owner; keep code, commands,
  proper nouns, and requested artifacts in the clearest original language.
- Start with `CONTEXT.md`, then `README.md`, then the narrow docs or code needed
  for the task.
- Preserve the Agent Kernel boundaries: runtime orchestration, model runtime,
  tool runtime, event store, capability packs, and surfaces should stay distinct.
- Prefer small, reviewable changes with concrete validation over broad rewrites.
- Put durable project knowledge in the right place: ADRs in `docs/adr/`, plans in
  `docs/prd/` or `docs/issues/`, research in `docs/research/`, and lessons in
  `docs/lessons/`.
- Maintain `docs/lessons/` incrementally. Write for human readers first, link new
  entries from `docs/lessons/README.md`, and update older lessons when later work
  changes the conclusion.
- For code changes, run focused checks. For workspace-wide Rust changes, run
  `cargo test --workspace`.
- PR titles must follow `CONTRIBUTING.md`, which mirrors the semantic pull
  request workflow.
