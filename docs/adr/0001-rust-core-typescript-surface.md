# Rust Core With TypeScript Surfaces

Status: accepted

We will build the Agent Kernel and high-control runtime layers in Rust, while user-facing surfaces such as desktop, IDE, web, and richer configuration UI will be TypeScript. This preserves Rust's strengths for local execution, process control, event durability, and distribution as a binary, while avoiding forcing UI and extension work into Rust where the ecosystem fit is weaker.

## Considered Options

- **All TypeScript**: faster early UI iteration, but weaker for process/runtime boundaries and local binary distribution.
- **All Rust**: strong runtime discipline, but poor fit for desktop/IDE/web surface iteration.
- **Rust core + TypeScript surfaces**: higher protocol-design cost, but gives the right ownership boundary between kernel and surfaces.
