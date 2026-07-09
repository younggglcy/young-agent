use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::ModelToolCallId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<ModelMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ModelToolSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        /// Optional text emitted by the assistant. A tool-call-only assistant
        /// message leaves this absent instead of forcing an empty string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// Tool calls emitted by the assistant message. Provider adapters need
        /// this history when the following Tool messages report results.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        content: Vec<ModelMessageContent>,
        /// Tool definition name used to produce this tool result message.
        name: String,
        /// Correlates this message to the model-emitted tool call id in the
        /// preceding assistant message.
        tool_call_id: ModelToolCallId,
    },
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ModelToolCall>,
    ) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self::Assistant {
            content: None,
            tool_calls,
        }
    }

    pub fn tool(
        content: impl Into<String>,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self::Tool {
            content: vec![ModelMessageContent::text(content)],
            name: name.into(),
            tool_call_id: ModelToolCallId::new(tool_call_id),
        }
    }

    pub fn tool_content(
        content: Vec<ModelMessageContent>,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self::Tool {
            content,
            name: name.into(),
            tool_call_id: ModelToolCallId::new(tool_call_id),
        }
    }

    pub fn role(&self) -> ModelMessageRole {
        match self {
            Self::System { .. } => ModelMessageRole::System,
            Self::User { .. } => ModelMessageRole::User,
            Self::Assistant { .. } => ModelMessageRole::Assistant,
            Self::Tool { .. } => ModelMessageRole::Tool,
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        match self {
            Self::System { content } | Self::User { content } => Some(content),
            Self::Assistant { content, .. } => content.as_deref(),
            Self::Tool { content, .. } => content.iter().find_map(ModelMessageContent::as_text),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelMessageContent {
    Text { text: String },
    Json { value: Value },
}

impl ModelMessageContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn json(value: Value) -> Self {
        Self::Json { value }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Json { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolCall {
    pub id: ModelToolCallId,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
