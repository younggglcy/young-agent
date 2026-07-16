# Contributing

## Pull request titles

Pull request titles must pass
[`.github/workflows/semantic-pull-request.yml`](.github/workflows/semantic-pull-request.yml).

Use this shape:

```text
[revert: ]<type>[optional scope][optional !]: <subject>
```

Allowed types:

```text
feat, fix, docs, style, refactor, perf, test, build, ci, chore
```

The subject must be 1-50 characters, must not start with an uppercase character,
and must not end with a period.

Valid examples:

```text
docs: add contributing guide
feat(cli): add run summary
fix(runtime)!: enforce approval policy
revert: fix(runtime): handle missing event log
```

Invalid examples:

```text
Docs: Add contributing guide
docs: add contributing guide.
docs: Add contributing guide
```

## Coverage policy

Coverage has three separate responsibilities:

1. [`.github/workflows/coverage.yml`](.github/workflows/coverage.yml) runs the tests, generates
   `lcov.info`, and uploads it. The `upload` job fails when report generation or transport fails;
   it does not own a coverage threshold.
2. [`codecov.yml`](codecov.yml) is the single source of truth for the merge-blocking coverage
   policy. Codecov patch coverage must meet 90%; overall project coverage remains visible through
   the Codecov dashboard, PR report, and README badge.
3. GitHub branch protection is the enforcement layer. `main` accepts changes only through pull
   requests and requires `upload` and `codecov/patch` from their expected GitHub Apps.
   Administrators do not bypass these rules.

The workflow still uploads coverage after a merge to `main` so Codecov has the default-branch
baseline used for comparisons and the README badge. The Codecov statuses use `only_pulls: true`
because direct pushes to `main` are not an allowed delivery path.

`codecov/project` is intentionally not a required context. The current Codecov GitHub integration
does not publish it consistently, including when a verified probe lowers overall coverage below
90%. Requiring a context that Codecov omits would permanently block unrelated pull requests. A
future hard project-coverage floor requires either a Codecov integration that reliably publishes
that status or a separate local gate; the latter would no longer be a Codecov-only policy.

If `upload` or `codecov/patch` is unexpectedly missing or stuck, inspect the upload log and Codecov
PR report, then rerun the failed workflow after the service recovers. Do not bypass the incident by
removing a required check. Any temporary branch-protection exception requires an explicit repository
owner decision; restore the required checks and repeat the failing-and-recovery probe afterward.

Changing a workflow, job, or Codecov status name also requires updating branch protection in the
same rollout. Verify both a failing coverage change and its tested recovery before treating a new
status context as enforced.
