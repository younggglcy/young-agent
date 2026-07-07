# Pi Monorepo Is Reference Only In Phase One

Status: accepted

`pi mono` refers to the existing Pi monorepo at `~/projects/pi`. For the first phase, Pi is an architectural reference for module boundaries and prior art, but the Agent Kernel proof will not depend on Pi packages, Pi runtime types, or Pi as a required provider gateway.

## Consequences

`QoderApiModelClient` may still use Qoder-specific API knowledge, but the first-phase PRD should not bind the kernel to Pi monorepo internals. If Pi becomes useful later, it can be integrated as a Provider Adapter or comparative implementation after the kernel contracts are stable.
