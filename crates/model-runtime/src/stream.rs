use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{ModelRequestId, ModelToolCallId};

/// Stream event envelopes are forward-readable for provider adapters and
/// surfaces. Stable extension data must be modeled explicitly or carried in
/// metadata-bearing payloads, not inferred from ignored additive fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStreamEvent {
    Started {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<ModelRequestId>,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    TextDelta {
        delta: String,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolCallDelta {
        /// Model-emitted tool-call id. This correlates model messages with
        /// model stream events; the tool runtime owns its own invocation id.
        id: ModelToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolCall {
        /// Model-emitted tool-call id. This correlates model messages with
        /// model stream events; the tool runtime owns its own invocation id.
        id: ModelToolCallId,
        name: String,
        arguments: Value,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Usage {
        usage: ModelUsage,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Failed {
        error: ModelError,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
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
