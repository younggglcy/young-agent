use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::execution::{
    PreparedToolCall, ToolCall, ToolDispatcher, ToolError, ToolExecutionAuthorization, ToolHandler,
    ToolOutput, ToolResult,
};

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

impl ToolDefinition {
    pub fn validate(&self) -> Result<(), ToolDefinitionError> {
        Self::validate_fields(
            &self.name,
            &self.description,
            &self.input_schema,
            self.output_schema.as_ref(),
            &self.capability,
            &self.approval_policy,
            self.mcp.as_ref(),
        )
    }

    pub(crate) fn validate_fields(
        name: &str,
        description: &str,
        input_schema: &Value,
        output_schema: Option<&Value>,
        capability: &CapabilityRef,
        approval_policy: &ToolApprovalPolicy,
        mcp: Option<&McpCompatibility>,
    ) -> Result<(), ToolDefinitionError> {
        require_non_empty("name", name)?;
        require_non_empty("description", description)?;
        require_non_empty("capability.id", &capability.id)?;
        require_non_empty("capability.version", &capability.version)?;
        if !input_schema.is_object() {
            return Err(ToolDefinitionError::new("input_schema must be an object"));
        }
        if output_schema.is_some_and(|schema| !schema.is_object()) {
            return Err(ToolDefinitionError::new("output_schema must be an object"));
        }
        match approval_policy {
            ToolApprovalPolicy::RequiresApproval { reason }
            | ToolApprovalPolicy::AlwaysReject { reason } => {
                require_non_empty("approval policy reason", reason)?;
            }
            ToolApprovalPolicy::AlwaysAllow | ToolApprovalPolicy::CallDependent => {}
        }
        if let Some(mcp) = mcp {
            require_non_empty("mcp.server", &mcp.server)?;
            require_non_empty("mcp.tool_name", &mcp.tool_name)?;
            require_non_empty("mcp.protocol_version", &mcp.protocol_version)?;
        }
        Ok(())
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), ToolDefinitionError> {
    if value.trim().is_empty() {
        return Err(ToolDefinitionError::new(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDefinitionError {
    message: String,
}

impl ToolDefinitionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolDefinitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ToolDefinitionError {}

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
    RequiresApproval {
        reason: String,
    },
    /// The concrete handler classifies each call through
    /// [`ToolHandler::approval_reason`].
    CallDependent,
    AlwaysReject {
        reason: String,
    },
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
    handler: Box<dyn ToolHandler>,
}

impl ToolRuntime {
    pub fn register<H>(
        &mut self,
        definition: ToolDefinition,
        handler: H,
    ) -> Result<(), ToolRegistrationError>
    where
        H: ToolHandler + 'static,
    {
        let definition_name = definition.name.clone();
        definition
            .validate()
            .map_err(|source| ToolRegistrationError::InvalidDefinition {
                name: definition_name,
                source,
            })?;
        if self.tools.contains_key(&definition.name) {
            return Err(ToolRegistrationError::DuplicateTool {
                name: definition.name,
            });
        }

        self.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                handler: Box::new(handler),
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

    /// One-shot convenience for non-interactive callers that already know the
    /// authorization outcome. Interactive callers should retain the plan from
    /// [`ToolDispatcher::prepare`] and execute that exact plan after approval.
    pub fn dispatch(
        &mut self,
        call: ToolCall,
        authorization: ToolExecutionAuthorization,
        cancellation: Arc<AtomicBool>,
    ) -> ToolResult {
        let call_id = call.id.clone();
        let prepared = self.prepare(call);
        let output = self.execute_prepared(prepared, authorization, cancellation);
        ToolResult { call_id, output }
    }
}

impl crate::execution::sealed::Sealed for ToolRuntime {}

impl ToolDispatcher for ToolRuntime {
    fn prepare(&self, call: ToolCall) -> PreparedToolCall {
        let Some(tool) = self.tools.get(&call.tool_name) else {
            let message = format!("tool '{}' is not registered", call.tool_name);
            return PreparedToolCall::rejected(
                call,
                ToolError {
                    code: "unknown_tool".to_string(),
                    message,
                    retryable: false,
                },
            );
        };

        match &tool.definition.approval_policy {
            ToolApprovalPolicy::AlwaysAllow => PreparedToolCall::ready(call),
            ToolApprovalPolicy::RequiresApproval { reason } => {
                PreparedToolCall::requiring_approval(call, reason.clone())
            }
            ToolApprovalPolicy::CallDependent => match tool.handler.approval_reason(&call) {
                Some(reason) => PreparedToolCall::requiring_approval(call, reason),
                None => PreparedToolCall::ready(call),
            },
            ToolApprovalPolicy::AlwaysReject { reason } => PreparedToolCall::rejected(
                call,
                ToolError {
                    code: "tool_rejected".to_string(),
                    message: reason.clone(),
                    retryable: false,
                },
            ),
        }
    }

    fn execute_prepared(
        &mut self,
        prepared: PreparedToolCall,
        authorization: ToolExecutionAuthorization,
        cancellation: Arc<AtomicBool>,
    ) -> ToolOutput {
        match prepared.into_authorized_call(authorization) {
            Ok(call) => match self.tools.get_mut(&call.tool_name) {
                Some(tool) => normalize_handler_output(tool.handler.execute(&call, cancellation)),
                None => ToolOutput::Failure {
                    error: ToolError {
                        code: "unknown_tool".to_string(),
                        message: format!("tool '{}' is not registered", call.tool_name),
                        retryable: false,
                    },
                    extensions: BTreeMap::new(),
                },
            },
            Err(output) => output,
        }
    }
}

fn normalize_handler_output(output: ToolOutput) -> ToolOutput {
    match output {
        ToolOutput::Failure { error, .. } if error.code == "approval_denied" => {
            ToolOutput::Failure {
                error: ToolError {
                    code: "reserved_tool_error_code".to_string(),
                    message: "tool handler returned reserved error code 'approval_denied'"
                        .to_string(),
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            }
        }
        output => output,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRegistrationError {
    InvalidDefinition {
        name: String,
        source: ToolDefinitionError,
    },
    DuplicateTool {
        name: String,
    },
}

impl fmt::Display for ToolRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDefinition { name, source } => {
                write!(formatter, "invalid tool '{name}': {source}")
            }
            Self::DuplicateTool { name } => {
                write!(formatter, "tool '{name}' is already registered")
            }
        }
    }
}

impl Error for ToolRegistrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidDefinition { source, .. } => Some(source),
            Self::DuplicateTool { .. } => None,
        }
    }
}
