#![doc = "Built-in Coding Capability boundary for the Agent Kernel."]

pub mod manifest;
pub mod tools;

pub use manifest::{coding_manifest, CODING_CAPABILITY_MANIFEST_TOML};
pub use tools::{register_builtin_coding_capability, CodingCapabilityRegistrationError};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-capability-coding");
    }
}
