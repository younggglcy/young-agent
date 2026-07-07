## Context

The first useful capability is coding. It must work locally while respecting the selected workspace boundary and git worktree safety rules.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement local workspace boundary resolution.
- Detect and record git worktree context.
- Implement minimal read file, search files, patch file, and run command tools.
- Keep file operations inside the workspace boundary.
- Defend against path traversal and unsafe symlink escapes.
- Truncate or structure large outputs so events remain readable.

## Acceptance Criteria

- Read/search/patch/run command tools can be invoked through Tool Runtime contracts.
- File reads outside the workspace are rejected.
- Patch operations outside the workspace are rejected.
- Symlink escape attempts are rejected or handled safely.
- Git worktree context is visible to the runtime or event metadata.
- Tests cover normal and rejected file operations.

## Test Notes

- Use temporary workspaces.
- Include path traversal and symlink tests.
- Include a basic patch application test.
- Include a command output truncation test if truncation is implemented here.

## Out of Scope

- Docker or remote sandboxing.
- Full shell sandboxing.
- Advanced code search indexing.
- Multi-file editing strategies beyond the minimal patch tool.
