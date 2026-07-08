use serde::{Deserialize, Serialize};
use young_model_runtime::stream::ModelStreamEvent;
use young_tool_runtime::execution::{ToolCall, ToolResult};

use crate::turn::TurnId;

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
#[serde(tag = "type", rename_all = "snake_case")]
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
    RunFinished {
        run_id: RunId,
        status: RunStatus,
    },
    Interrupted {
        run_id: RunId,
        reason: String,
    },
    Cancelled {
        run_id: RunId,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    AwaitingApproval,
    Completed { final_message: String },
    Failed { error: AgentError },
    Interrupted,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub call: ToolCall,
    pub reason: String,
}
