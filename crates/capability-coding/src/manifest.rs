use young_tool_runtime::{CapabilityManifest, CapabilityManifestError};

pub const CODING_CAPABILITY_MANIFEST_TOML: &str = include_str!("../coding-capability.toml");

pub fn coding_manifest() -> Result<CapabilityManifest, CapabilityManifestError> {
    CapabilityManifest::from_toml(CODING_CAPABILITY_MANIFEST_TOML)
}
