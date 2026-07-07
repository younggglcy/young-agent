## Context

The first phase command policy allows low-risk read and validation commands by default, while requiring approval for mutating, destructive, dependency-installing, background-running, and cross-workspace commands.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement command classification for the first approval policy.
- Integrate classification with the run command tool.
- Emit approval request Agent Events when approval is required.
- Support approval granted and denied outcomes.
- Record approval decisions in the Event Log.
- Provide enough data for the CLI Proof Surface to prompt the user.

## Acceptance Criteria

- Low-risk read and validation commands can run without approval.
- Mutating commands require approval.
- Destructive commands require approval or are rejected when clearly unsafe.
- Dependency-installing commands require approval.
- Background-running commands require approval.
- Cross-workspace commands are rejected or require approval according to the workspace boundary policy.
- Denied approval stops the command and emits a clear event.

## Test Notes

- Add table-driven command classification tests.
- Add run command tests for allowed, approval-required, denied, and rejected cases.
- Keep the policy conservative when classification is uncertain.

## Out of Scope

- Full shell parser correctness.
- Remote sandboxing.
- Per-user policy customization.
