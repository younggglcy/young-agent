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
        manifest.validate_schema_version()?;
        manifest.validate_required_metadata()?;
        manifest.validate_unique_tool_names()?;
        manifest.validate_safety_reasons()?;
        Ok(manifest)
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let capability = CapabilityRef {
            id: self.capability.id.clone(),
            version: self.capability.version.clone(),
        };

        self.tools
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
            .collect()
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
        require_non_empty("capability.id", &self.capability.id)?;
        require_non_empty("capability.version", &self.capability.version)?;
        require_non_empty("capability.name", &self.capability.name)?;
        require_non_empty("capability.description", &self.capability.description)?;
        if self.tools.is_empty() {
            return Err(CapabilityManifestError::Invalid {
                message: "tools must contain at least one tool".to_string(),
            });
        }

        for tool in &self.tools {
            require_non_empty("tool.name", &tool.name)?;
            require_non_empty(
                &format!("tool '{}'.description", tool.name),
                &tool.description,
            )?;
            if !tool.input_schema.is_object() {
                return Err(CapabilityManifestError::Invalid {
                    message: format!("tool '{}' input_schema must be a TOML table", tool.name),
                });
            }
            if tool
                .output_schema
                .as_ref()
                .is_some_and(|schema| !schema.is_object())
            {
                return Err(CapabilityManifestError::Invalid {
                    message: format!("tool '{}' output_schema must be a TOML table", tool.name),
                });
            }
            if let Some(mcp) = &tool.mcp {
                require_non_empty(&format!("tool '{}'.mcp.server", tool.name), &mcp.server)?;
                require_non_empty(
                    &format!("tool '{}'.mcp.tool_name", tool.name),
                    &mcp.tool_name,
                )?;
                require_non_empty(
                    &format!("tool '{}'.mcp.protocol_version", tool.name),
                    &mcp.protocol_version,
                )?;
            }
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

    fn validate_safety_reasons(&self) -> Result<(), CapabilityManifestError> {
        for tool in &self.tools {
            let needs_reason = matches!(
                tool.safety_class,
                ToolSafetyClass::RequiresApproval | ToolSafetyClass::AlwaysReject
            );
            let has_reason = tool
                .safety_reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty());
            if needs_reason && !has_reason {
                let safety_class = match tool.safety_class {
                    ToolSafetyClass::RequiresApproval => "requires_approval",
                    ToolSafetyClass::AlwaysReject => "always_reject",
                    ToolSafetyClass::AlwaysAllow | ToolSafetyClass::CallDependent => {
                        unreachable!("these safety classes need no static reason")
                    }
                };
                return Err(CapabilityManifestError::Invalid {
                    message: format!(
                        "tool '{}' safety_class '{safety_class}' requires safety_reason",
                        tool.name
                    ),
                });
            }
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
                reason: self
                    .safety_reason
                    .clone()
                    .unwrap_or_else(|| format!("tool '{}' requires approval", self.name)),
            },
            ToolSafetyClass::CallDependent => ToolApprovalPolicy::CallDependent,
            ToolSafetyClass::AlwaysReject => ToolApprovalPolicy::AlwaysReject {
                reason: self.safety_reason.clone().unwrap_or_else(|| {
                    format!("tool '{}' is rejected by its safety policy", self.name)
                }),
            },
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
