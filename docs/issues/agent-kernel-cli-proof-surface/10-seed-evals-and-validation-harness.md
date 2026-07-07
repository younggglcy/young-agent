## Context

The first phase needs a tiny evaluation harness to prove that the kernel can solve repeatable coding tasks and that regressions are visible.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Add a small set of seed coding tasks for local validation.
- Run seed tasks through the CLI Proof Surface or kernel test harness.
- Capture expected outcomes and event-log assertions.
- Include at least one fake-model deterministic eval.
- Include optional Qoder smoke wiring when provider configuration is present.

## Acceptance Criteria

- There is a documented command for running first-phase validation.
- Seed evals can run without provider credentials using FakeModelClient.
- The validation harness checks final status and meaningful event-log content.
- Optional Qoder smoke is clearly separated from default deterministic validation.
- Failures produce enough output for a developer or agent to diagnose the failing step.

## Test Notes

- Keep seed tasks small.
- Prefer temp workspaces or fixtures that reset cleanly.
- Avoid making network-backed evals part of the default required test suite.

## Out of Scope

- Benchmark dashboards.
- Large eval corpus.
- Provider comparison.
- Multi-agent evals.
