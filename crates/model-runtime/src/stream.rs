use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    Started {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_request_id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCall {
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
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
