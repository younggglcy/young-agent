# Contract Review Should Harden Wire Shape First

## Context

Issue 3 introduced the first persisted contracts for model, tool, and agent run
events. Review feedback mixed concrete wire-shape risks with broader provider
evolution ideas.

## Lesson

For early protocol work, prioritize feedback that makes persisted data less
ambiguous without guessing future provider behavior. In this case, object-shaped
metadata and a single terminal run status were worth fixing immediately because
they prevent divergent consumers. Broader ideas such as multimodal message
parts, richer stream deltas, and provider-specific lifecycle states should wait
for implementation pressure from real adapters.

## Next Time

When reviewing a contract PR, classify comments into:

- wire-shape ambiguity to fix now;
- semantic rules to document now;
- future expressiveness to defer until a real consumer needs it.
