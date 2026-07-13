use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use young_tool_runtime::{
    CapabilityManifestError, ToolCall, ToolError, ToolHandler, ToolOutput, ToolRegistrationError,
    ToolRuntime,
};

use crate::manifest::coding_manifest;

pub fn register_builtin_coding_capability(
    runtime: &mut ToolRuntime,
) -> Result<(), CodingCapabilityRegistrationError> {
    let manifest = coding_manifest().map_err(CodingCapabilityRegistrationError::Manifest)?;
    let definitions = manifest
        .into_tool_definitions()
        .map_err(CodingCapabilityRegistrationError::Manifest)?;

    // Preflight the complete built-in pack so a duplicate cannot leave a
    // partially registered capability behind.
    for definition in &definitions {
        if runtime.lookup(&definition.name).is_some() {
            return Err(CodingCapabilityRegistrationError::Registration(
                ToolRegistrationError::DuplicateTool {
                    name: definition.name.clone(),
                },
            ));
        }
    }

    for definition in definitions {
        let tool_name = definition.name.clone();
        runtime
            .register(definition, UnimplementedCodingTool { tool_name })
            .map_err(CodingCapabilityRegistrationError::Registration)?;
    }
    Ok(())
}

struct UnimplementedCodingTool {
    tool_name: String,
}

impl ToolHandler for UnimplementedCodingTool {
    fn approval_reason(&self, _call: &ToolCall) -> Option<String> {
        // Phase-one stubs cannot perform side effects. Real call-dependent
        // handlers must replace this explicit classification in their issue.
        None
    }

    fn execute(&mut self, _call: &ToolCall, _cancellation: Arc<AtomicBool>) -> ToolOutput {
        ToolOutput::Failure {
            error: ToolError {
                code: "tool_not_implemented".to_string(),
                message: format!(
                    "coding tool '{}' is declared but not implemented",
                    self.tool_name
                ),
                retryable: false,
            },
            extensions: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub enum CodingCapabilityRegistrationError {
    Manifest(CapabilityManifestError),
    Registration(ToolRegistrationError),
}

impl fmt::Display for CodingCapabilityRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => {
                write!(formatter, "coding capability manifest failed: {error}")
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
        }
    }
}
