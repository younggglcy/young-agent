use std::error::Error;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use young_tool_runtime::{
    CapabilityManifestError, ToolCall, ToolHandler, ToolOutput, ToolRegistrationError, ToolRuntime,
};

use crate::manifest::coding_manifest;
use crate::workspace::CodingWorkspace;

const BUILTIN_TOOL_NAMES: [&str; 4] = ["read_file", "search_files", "apply_patch", "run_command"];

pub fn register_builtin_coding_capability(
    runtime: &mut ToolRuntime,
    workspace: CodingWorkspace,
) -> Result<(), CodingCapabilityRegistrationError> {
    let manifest = coding_manifest().map_err(CodingCapabilityRegistrationError::Manifest)?;
    let definitions = manifest
        .into_tool_definitions()
        .map_err(CodingCapabilityRegistrationError::Manifest)?;

    // Validate the entire built-in pack before registering any handler so a
    // stale manifest or duplicate cannot leave a partially registered pack.
    for definition in &definitions {
        if !BUILTIN_TOOL_NAMES.contains(&definition.name.as_str()) {
            return Err(CodingCapabilityRegistrationError::UnsupportedManifestTool {
                name: definition.name.clone(),
            });
        }
        if runtime.lookup(&definition.name).is_some() {
            return Err(CodingCapabilityRegistrationError::Registration(
                ToolRegistrationError::DuplicateTool {
                    name: definition.name.clone(),
                },
            ));
        }
    }

    for definition in definitions {
        let handler = BuiltinCodingTool::new(&definition.name, workspace.clone());
        runtime
            .register(definition, handler)
            .map_err(CodingCapabilityRegistrationError::Registration)?;
    }
    Ok(())
}

enum BuiltinCodingTool {
    ReadFile(CodingWorkspace),
    SearchFiles(CodingWorkspace),
    ApplyPatch(CodingWorkspace),
    RunCommand(CodingWorkspace),
}

impl BuiltinCodingTool {
    fn new(name: &str, workspace: CodingWorkspace) -> Self {
        match name {
            "read_file" => Self::ReadFile(workspace),
            "search_files" => Self::SearchFiles(workspace),
            "apply_patch" => Self::ApplyPatch(workspace),
            "run_command" => Self::RunCommand(workspace),
            _ => unreachable!("manifest tool names were preflighted"),
        }
    }
}

impl ToolHandler for BuiltinCodingTool {
    fn approval_reason(&self, _call: &ToolCall) -> Option<String> {
        match self {
            Self::RunCommand(_) => Some(crate::command::APPROVAL_REASON.to_string()),
            Self::ReadFile(_) | Self::SearchFiles(_) | Self::ApplyPatch(_) => None,
        }
    }

    fn execute(&mut self, call: &ToolCall, cancellation: Arc<AtomicBool>) -> ToolOutput {
        match self {
            Self::ReadFile(workspace) => crate::read::execute(workspace, call, &cancellation),
            Self::SearchFiles(workspace) => crate::search::execute(workspace, call, &cancellation),
            Self::ApplyPatch(workspace) => crate::patch::execute(workspace, call, &cancellation),
            Self::RunCommand(workspace) => crate::command::execute(workspace, call, &cancellation),
        }
    }
}

#[derive(Debug)]
pub enum CodingCapabilityRegistrationError {
    Manifest(CapabilityManifestError),
    UnsupportedManifestTool { name: String },
    Registration(ToolRegistrationError),
}

impl fmt::Display for CodingCapabilityRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => {
                write!(formatter, "coding capability manifest failed: {error}")
            }
            Self::UnsupportedManifestTool { name } => {
                write!(
                    formatter,
                    "coding capability manifest declares unsupported tool '{name}'"
                )
            }
            Self::Registration(error) => {
                write!(formatter, "coding capability registration failed: {error}")
            }
        }
    }
}

impl Error for CodingCapabilityRegistrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Registration(error) => Some(error),
            Self::UnsupportedManifestTool { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CodingCapabilityRegistrationError;
    use young_tool_runtime::{CapabilityManifestError, ToolRegistrationError};

    #[test]
    fn registration_errors_preserve_their_context_and_sources() {
        let cases = vec![
            (
                CodingCapabilityRegistrationError::Manifest(
                    CapabilityManifestError::Invalid {
                        message: "invalid tool".to_string(),
                    },
                ),
                "coding capability manifest failed: invalid built-in capability manifest: invalid tool",
                true,
            ),
            (
                CodingCapabilityRegistrationError::UnsupportedManifestTool {
                    name: "unknown_tool".to_string(),
                },
                "coding capability manifest declares unsupported tool 'unknown_tool'",
                false,
            ),
            (
                CodingCapabilityRegistrationError::Registration(
                    ToolRegistrationError::DuplicateTool {
                        name: "read_file".to_string(),
                    },
                ),
                "coding capability registration failed: tool 'read_file' is already registered",
                true,
            ),
        ];

        for (error, expected, has_source) in cases {
            assert_eq!(error.to_string(), expected);
            assert_eq!(std::error::Error::source(&error).is_some(), has_source);
        }
    }
}
