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
2. [`codecov.yml`](codecov.yml) is the single source of truth for coverage policy. Project and
   patch coverage must each meet 90%. Codecov publishes the stable `codecov/project` and
   `codecov/patch` commit statuses; GitHub Checks mode stays disabled so both policy results have
   explicit status contexts.
3. GitHub branch protection is the enforcement layer. `main` accepts changes only through pull
   requests and requires `upload`, `codecov/project`, and `codecov/patch` from their expected
   GitHub Apps. Administrators do not bypass these rules.

The workflow still uploads coverage after a merge to `main` so Codecov has the default-branch
baseline used for comparisons and the README badge. The Codecov statuses use `only_pulls: true`
because direct pushes to `main` are not an allowed delivery path.

Changing a workflow, job, or Codecov status name also requires updating branch protection in the
same rollout. Verify both a failing coverage change and its tested recovery before treating a new
status context as enforced.
