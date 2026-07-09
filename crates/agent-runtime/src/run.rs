use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use young_model_runtime::stream::ModelStreamEvent;
use young_tool_runtime::execution::{ToolCall, ToolResult};

use crate::turn::TurnId;

/// A stable run identifier that keeps Rust type-safety while serializing as its
/// inner string in persisted contracts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Event envelopes are forward-readable so older surfaces and replay readers can
/// tolerate additive fields written by newer kernels. Stable semantics still
/// belong in typed fields, not ad hoc unknown fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        run_id: RunId,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    TurnStarted {
        run_id: RunId,
        turn_id: TurnId,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ModelOutput {
        run_id: RunId,
        turn_id: TurnId,
        event: ModelStreamEvent,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolCallRequested {
        run_id: RunId,
        turn_id: TurnId,
        call: ToolCall,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ApprovalRequested {
        run_id: RunId,
        turn_id: TurnId,
        request: ApprovalRequest,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolResult {
        run_id: RunId,
        turn_id: TurnId,
        result: ToolResult,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Error {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        error: AgentError,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    /// The final event for a run. Terminal outcomes, including interruption and
    /// cancellation, are represented only through this status.
    RunFinished {
        run_id: RunId,
        status: TerminalRunStatus,
        #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunStatus {
    Running,
    AwaitingApproval,
    Finished { terminal_status: TerminalRunStatus },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalRunStatus {
    Completed { final_message: String },
    Failed { error: AgentError },
    Interrupted { reason: String },
    Cancelled { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentError {
    pub code: String,
    pub message: String,
    /// Whether the Agent Run can continue after this high-level error.
    pub recoverable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub call: ToolCall,
    pub reason: String,
}
