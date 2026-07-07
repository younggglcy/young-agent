## Context

Qoder is the only required real provider for the first phase. This issue validates the provider adapter boundary without expanding into a provider matrix.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement QoderApiModelClient behind the provider-neutral model contract.
- Normalize Qoder streaming responses into model stream events.
- Support basic configuration through environment variables or a minimal config mechanism.
- Add one integration smoke that is skipped unless required configuration is present.
- Ensure provider errors are surfaced through kernel error contracts.

## Acceptance Criteria

- The Agent Runtime can use QoderApiModelClient through the same model contract as FakeModelClient.
- A configured smoke test can send a minimal request and receive model output.
- Missing credentials or endpoint configuration skips the smoke test instead of failing default local test runs.
- Provider errors produce actionable error events.
- No DeepSeek or Codex provider implementation is added in this issue.

## Test Notes

- Add unit tests for response normalization where fixtures are available.
- Add a skipped-by-default integration smoke.
- Keep secrets out of logs and event payloads.

## Out of Scope

- DeepSeek provider.
- Codex provider.
- Provider selection UI.
- Provider compatibility matrix.
