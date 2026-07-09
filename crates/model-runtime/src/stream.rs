use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelStreamEvent {
    Started {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_request_id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        /// Provider tool-call id. This must match the final ToolCall id and
        /// the later tool-runtime ToolResult.call_id for the same invocation.
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCall {
        /// Provider tool-call id. This must match the tool-runtime ToolCall.id
        /// dispatched by the agent for the same invocation.
        id: String,
        name: String,
        arguments: Value,
    },
    Usage {
        usage: ModelUsage,
    },
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
    },
    Failed {
        error: ModelError,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelError {
    pub code: String,
    pub message: String,
    /// Whether retrying the same provider request is expected to help.
    pub retryable: bool,
}
