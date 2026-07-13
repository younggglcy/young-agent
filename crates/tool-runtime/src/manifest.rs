use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::{CapabilityRef, McpCompatibility, ToolApprovalPolicy, ToolDefinition};

pub const BUILT_IN_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub schema_version: u32,
    pub capability: CapabilityMetadata,
    pub tools: Vec<ManifestTool>,
}

impl CapabilityManifest {
    /// Parses metadata supplied by a capability compiled into the binary.
    ///
    /// This is deliberately a string parser rather than a filesystem loader:
    /// phase one supports built-in manifests only and does not discover user
    /// capability packs or plugins.
    pub fn from_toml(source: &str) -> Result<Self, CapabilityManifestError> {
        let manifest: Self = toml::from_str(source).map_err(CapabilityManifestError::Parse)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates the complete manifest contract regardless of how the value
    /// was constructed. Conversion APIs call this too, so direct serde users
    /// cannot bypass manifest-level invariants.
    pub fn validate(&self) -> Result<(), CapabilityManifestError> {
        self.validate_schema_version()?;
        self.validate_required_metadata()?;
        self.validate_unique_tool_names()?;
        self.validate_tool_definitions()
    }

    pub fn tool_definitions(&self) -> Result<Vec<ToolDefinition>, CapabilityManifestError> {
        self.validate()?;
        let capability = CapabilityRef {
            id: self.capability.id.clone(),
            version: self.capability.version.clone(),
        };

        Ok(self
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                output_schema: tool.output_schema.clone(),
                capability: capability.clone(),
                approval_policy: tool.approval_policy(),
                mcp: tool.mcp.clone(),
            })
            .collect())
    }

    /// Converts a validated manifest into runtime definitions without cloning
    /// schema documents or metadata. Built-in registration should prefer this
    /// consuming path because it no longer needs the manifest afterwards.
    pub fn into_tool_definitions(self) -> Result<Vec<ToolDefinition>, CapabilityManifestError> {
        self.validate()?;
        let capability = CapabilityRef {
            id: self.capability.id,
            version: self.capability.version,
        };

        Ok(self
            .tools
            .into_iter()
            .map(|tool| tool.into_definition(capability.clone()))
            .collect())
    }

    fn validate_schema_version(&self) -> Result<(), CapabilityManifestError> {
        if self.schema_version != BUILT_IN_MANIFEST_SCHEMA_VERSION {
            return Err(CapabilityManifestError::Invalid {
                message: format!(
                    "unsupported schema_version {}; expected {BUILT_IN_MANIFEST_SCHEMA_VERSION}",
                    self.schema_version
                ),
            });
        }
        Ok(())
    }

    fn validate_required_metadata(&self) -> Result<(), CapabilityManifestError> {
        require_non_empty("capability.name", &self.capability.name)?;
        require_non_empty("capability.description", &self.capability.description)?;
        if self.tools.is_empty() {
            return Err(CapabilityManifestError::Invalid {
                message: "tools must contain at least one tool".to_string(),
            });
        }

        Ok(())
    }

    fn validate_unique_tool_names(&self) -> Result<(), CapabilityManifestError> {
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if !names.insert(tool.name.as_str()) {
                return Err(CapabilityManifestError::Invalid {
                    message: format!("duplicate tool name '{}'", tool.name),
                });
            }
        }
        Ok(())
    }

    fn validate_tool_definitions(&self) -> Result<(), CapabilityManifestError> {
        let capability = CapabilityRef {
            id: self.capability.id.clone(),
            version: self.capability.version.clone(),
        };

        for tool in &self.tools {
            let approval_policy = tool.approval_policy();
            ToolDefinition::validate_fields(
                &tool.name,
                &tool.description,
                &tool.input_schema,
                tool.output_schema.as_ref(),
                &capability,
                &approval_policy,
                tool.mcp.as_ref(),
            )
            .map_err(|error| CapabilityManifestError::Invalid {
                message: format!("tool '{}': {error}", tool.name),
            })?;
        }
        Ok(())
    }
}

fn require_non_empty(label: &str, value: &str) -> Result<(), CapabilityManifestError> {
    if value.trim().is_empty() {
        return Err(CapabilityManifestError::Invalid {
            message: format!("{label} must not be empty"),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityMetadata {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub safety_class: ToolSafetyClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpCompatibility>,
}

impl ManifestTool {
    fn approval_policy(&self) -> ToolApprovalPolicy {
        match self.safety_class {
            ToolSafetyClass::AlwaysAllow => ToolApprovalPolicy::AlwaysAllow,
            ToolSafetyClass::RequiresApproval => ToolApprovalPolicy::RequiresApproval {
                reason: self.safety_reason.clone().unwrap_or_default(),
            },
            ToolSafetyClass::CallDependent => ToolApprovalPolicy::CallDependent,
            ToolSafetyClass::AlwaysReject => ToolApprovalPolicy::AlwaysReject {
                reason: self.safety_reason.clone().unwrap_or_default(),
            },
        }
    }

    fn into_definition(self, capability: CapabilityRef) -> ToolDefinition {
        let approval_policy = match self.safety_class {
            ToolSafetyClass::AlwaysAllow => ToolApprovalPolicy::AlwaysAllow,
            ToolSafetyClass::RequiresApproval => ToolApprovalPolicy::RequiresApproval {
                reason: self.safety_reason.unwrap_or_default(),
            },
            ToolSafetyClass::CallDependent => ToolApprovalPolicy::CallDependent,
            ToolSafetyClass::AlwaysReject => ToolApprovalPolicy::AlwaysReject {
                reason: self.safety_reason.unwrap_or_default(),
            },
        };

        ToolDefinition {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema,
            output_schema: self.output_schema,
            capability,
            approval_policy,
            mcp: self.mcp,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSafetyClass {
    AlwaysAllow,
    RequiresApproval,
    CallDependent,
    AlwaysReject,
}

#[derive(Debug)]
pub enum CapabilityManifestError {
    Parse(toml::de::Error),
    Invalid { message: String },
}

impl fmt::Display for CapabilityManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => {
                write!(
                    formatter,
                    "failed to parse built-in capability manifest TOML: {error}"
                )
            }
            Self::Invalid { message } => {
                write!(formatter, "invalid built-in capability manifest: {message}")
            }
        }
    }
}

impl Error for CapabilityManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::Invalid { .. } => None,
        }
    }
}
