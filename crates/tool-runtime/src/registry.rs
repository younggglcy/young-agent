use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
pub struct CapabilityRef {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum ToolApprovalPolicy {
    AlwaysAllow,
    RequiresApproval { reason: String },
    AlwaysReject { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCompatibility {
    pub server: String,
    pub tool_name: String,
    pub protocol_version: String,
}
