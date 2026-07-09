use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentEvent {
    RunStarted {
        run_id: RunId,
    },
    TurnStarted {
        run_id: RunId,
        turn_id: TurnId,
    },
    ModelOutput {
        run_id: RunId,
        turn_id: TurnId,
        event: ModelStreamEvent,
    },
    ToolCallRequested {
        run_id: RunId,
        turn_id: TurnId,
        call: ToolCall,
    },
    ApprovalRequested {
        run_id: RunId,
        turn_id: TurnId,
        request: ApprovalRequest,
    },
    ToolResult {
        run_id: RunId,
        turn_id: TurnId,
        result: ToolResult,
    },
    Error {
        run_id: RunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        error: AgentError,
    },
    /// The final event for a run. Terminal outcomes, including interruption and
    /// cancellation, are represented only through this status.
    RunFinished {
        run_id: RunId,
        status: TerminalRunStatus,
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
