# TOML Manifests for Built-In Capabilities

Status: accepted

For the first phase, Capability Packs will be declared with TOML manifests, and only built-in capabilities will be loaded. TOML is straightforward to maintain by hand in a Rust workspace, avoids YAML's implicit-type pitfalls, and is more readable than JSON for configuration-style metadata.

## Considered Options

- **JSON**: simple and ubiquitous, but noisy for human-authored configuration.
- **YAML**: human-friendly, but implicit typing and parser differences are unnecessary risk for kernel contracts.
- **TOML**: a good fit for Rust configuration files and explicit enough for first-phase manifests.
