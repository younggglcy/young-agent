use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use young_model_runtime::{stream::ModelStreamEvent, ModelToolCallId};
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentEvent {
    RunStarted {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    TurnStarted {
        run_id: RunId,
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ModelOutput {
        run_id: RunId,
        turn_id: TurnId,
        event: ModelStreamEvent,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolCallRequested {
        run_id: RunId,
        turn_id: TurnId,
        /// Bridges the model-emitted tool call to the kernel-owned tool
        /// invocation id carried by `call.id`.
        model_tool_call_id: ModelToolCallId,
        call: ToolCall,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ApprovalRequested {
        run_id: RunId,
        turn_id: TurnId,
        request: ApprovalRequest,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    ToolResult {
        run_id: RunId,
        turn_id: TurnId,
        result: ToolResult,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Error {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        error: AgentError,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    /// The final event for a run. Terminal outcomes, including interruption and
    /// cancellation, are represented only through this status.
    RunFinished {
        run_id: RunId,
        status: TerminalRunStatus,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
}

impl AgentEvent {
    /// Returns the Agent Run that owns this canonical event.
    pub fn run_id(&self) -> &RunId {
        match self {
            Self::RunStarted { run_id, .. }
            | Self::TurnStarted { run_id, .. }
            | Self::ModelOutput { run_id, .. }
            | Self::ToolCallRequested { run_id, .. }
            | Self::ApprovalRequested { run_id, .. }
            | Self::ToolResult { run_id, .. }
            | Self::Error { run_id, .. }
            | Self::RunFinished { run_id, .. } => run_id,
        }
    }
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
