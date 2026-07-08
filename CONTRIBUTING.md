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
