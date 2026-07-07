# Defer MCP Runtime

Status: accepted

The first phase will not implement an MCP Runtime. The Tool Runtime should keep a clean enough contract that MCP tools can be mapped into it later, but the first proof will not connect to MCP servers, perform dynamic tool discovery, manage MCP process lifecycles, or handle MCP-specific output framing.

## Considered Options

- **Implement MCP in phase one**: attractive for extensibility, but introduces external process lifecycle, dynamic schemas, permission boundaries, and untrusted output handling before the Agent Kernel is proven.
- **Ignore MCP entirely**: simpler, but risks designing tool contracts that are hard to adapt later.
- **Reserve an MCP Boundary and defer runtime**: keeps the first proof small while preserving future compatibility.
