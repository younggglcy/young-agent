use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    /// Stable invocation id shared with model-runtime tool-call events and
    /// the corresponding ToolResult.call_id.
    pub id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// Correlates this result to the ToolCall.id that was executed.
    pub call_id: String,
    pub output: ToolOutput,
}

/// Output envelopes are forward-readable so older consumers can tolerate
/// additive fields. Durable producer data belongs in Success.metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutput {
    Success {
        content: Vec<ToolContent>,
        /// Producer-defined object metadata for logs, UI hints, and metrics.
        /// Core tool semantics must not depend on producer-specific keys.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        metadata: BTreeMap<String, Value>,
    },
    Failure {
        error: ToolError,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolContent {
    Text { text: String },
    Json { value: Value },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    /// Whether retrying the same low-level tool call is expected to help.
    pub retryable: bool,
}
