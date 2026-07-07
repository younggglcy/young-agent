## Context

The first phase needs a small Tool Runtime and a built-in Coding Capability. Capability manifests are TOML and built-in only.

Parent PRD: https://github.com/younggglcy/young-agent/issues/1

## Scope

- Implement Tool Runtime registration and lookup.
- Implement tool execution dispatch through the core contracts.
- Define the built-in TOML manifest format for capability metadata.
- Add the Coding Capability manifest.
- Ensure manifest metadata can describe tool names, descriptions, input schemas, safety class, and MCP boundary metadata.

## Acceptance Criteria

- Built-in capability metadata can be loaded from TOML.
- The Coding Capability can register its initial tool definitions with the Tool Runtime.
- Tool Runtime can dispatch a tool call and return a Tool Result.
- Unknown tools fail with a clear tool error.
- Manifest tests validate required fields and useful error messages.

## Test Notes

- Add manifest parse tests.
- Add unknown-tool and successful-dispatch tests.
- Add invalid-manifest tests.

## Out of Scope

- User-defined capability packs.
- Plugin loading.
- MCP runtime.
- Final coding tool implementations beyond minimal stubs if needed.
