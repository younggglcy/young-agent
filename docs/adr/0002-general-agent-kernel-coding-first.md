# General Agent Kernel With Coding First

Status: accepted

We will build a general Agent Kernel and validate it first through a Coding Capability. Coding is the first proof because it stresses long context, tool use, file mutation, command execution, approval, replay, and evaluation, but coding concepts must not become kernel concepts.

## Consequences

The Agent Kernel must depend on generic capability manifests and tool contracts, not on repository, patch, git, or test concepts directly. Future research, browser, desktop automation, memory, scheduler, messaging, and data-analysis capabilities should be additive Capability Packs rather than rewrites of the kernel.
