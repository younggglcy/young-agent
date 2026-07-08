use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<ModelMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ModelToolSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelMessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
pub struct ModelToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
