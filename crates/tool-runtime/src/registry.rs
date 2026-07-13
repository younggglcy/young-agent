use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::{ToolCall, ToolError, ToolExecutor, ToolOutput, ToolResult};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Local output contract for the normalized ToolOutput content shape.
    /// This is independent of MCP compatibility metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub capability: CapabilityRef,
    /// Runtime approval policy for executing the tool. This gate applies
    /// regardless of whether the tool is local-only or MCP-compatible.
    pub approval_policy: ToolApprovalPolicy,
    /// Reserved mapping metadata for future MCP boundary compatibility.
    /// Presence here does not imply that an MCP runtime exists in this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpCompatibility>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRef {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolApprovalPolicy {
    AlwaysAllow,
    RequiresApproval { reason: String },
    AlwaysReject { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCompatibility {
    pub server: String,
    pub tool_name: String,
    pub protocol_version: String,
}

#[derive(Default)]
pub struct ToolRuntime {
    tools: BTreeMap<String, RegisteredTool>,
}

struct RegisteredTool {
    definition: ToolDefinition,
    executor: Box<dyn ToolExecutor>,
}

impl ToolRuntime {
    pub fn register<E>(
        &mut self,
        definition: ToolDefinition,
        executor: E,
    ) -> Result<(), ToolRegistrationError>
    where
        E: ToolExecutor + 'static,
    {
        if definition.name.trim().is_empty() {
            return Err(ToolRegistrationError::EmptyName);
        }
        if self.tools.contains_key(&definition.name) {
            return Err(ToolRegistrationError::DuplicateTool {
                name: definition.name,
            });
        }

        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                executor: Box::new(executor),
            },
        );
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name).map(|tool| &tool.definition)
    }

    pub fn definitions(&self) -> impl ExactSizeIterator<Item = &ToolDefinition> {
        self.tools.values().map(|tool| &tool.definition)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Dispatches a call after the caller has honored [`Self::approval_reason`].
    /// Approval prompting stays in the Agent Runtime; this method owns lookup,
    /// static rejection, implementation dispatch, and result correlation.
    pub fn dispatch(&mut self, call: &ToolCall, cancellation: Arc<AtomicBool>) -> ToolResult {
        let output = match self.tools.get_mut(&call.tool_name) {
            Some(tool) => match &tool.definition.approval_policy {
                ToolApprovalPolicy::AlwaysReject { reason } => ToolOutput::Failure {
                    error: ToolError {
                        code: "tool_rejected".to_string(),
                        message: reason.clone(),
                        retryable: false,
                    },
                    extensions: BTreeMap::new(),
                },
                ToolApprovalPolicy::AlwaysAllow | ToolApprovalPolicy::RequiresApproval { .. } => {
                    tool.executor.execute(call, cancellation)
                }
            },
            None => ToolOutput::Failure {
                error: ToolError {
                    code: "unknown_tool".to_string(),
                    message: format!("tool '{}' is not registered", call.tool_name),
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            },
        };

        ToolResult {
            call_id: call.id.clone(),
            output,
        }
    }
}

impl ToolExecutor for ToolRuntime {
    fn approval_reason(&self, call: &ToolCall) -> Option<String> {
        let tool = self.tools.get(&call.tool_name)?;
        match &tool.definition.approval_policy {
            ToolApprovalPolicy::RequiresApproval { reason } => Some(reason.clone()),
            ToolApprovalPolicy::AlwaysAllow => tool.executor.approval_reason(call),
            ToolApprovalPolicy::AlwaysReject { .. } => None,
        }
    }

    fn execute(&mut self, call: &ToolCall, cancellation: Arc<AtomicBool>) -> ToolOutput {
        self.dispatch(call, cancellation).output
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRegistrationError {
    EmptyName,
    DuplicateTool { name: String },
}

impl fmt::Display for ToolRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => write!(formatter, "tool name must not be empty"),
            Self::DuplicateTool { name } => {
                write!(formatter, "tool '{name}' is already registered")
            }
        }
    }
}

impl Error for ToolRegistrationError {}
