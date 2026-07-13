use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    /// Kernel-owned invocation id for the concrete tool execution.
    pub id: ToolCallId,
    pub tool_name: String,
    pub arguments: Value,
}

/// Orchestration state showing whether the Agent Runtime granted approval for
/// one exact Tool Call. This is an in-process correctness guard, not a
/// cryptographic capability or sandbox boundary.
///
/// `NotRequired` is the safe default: a policy-aware Tool Runtime must still
/// reject a call whose static or call-dependent policy requires approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolExecutionAuthorization {
    NotRequired,
    ApprovalGranted { call_id: ToolCallId },
}

impl ToolExecutionAuthorization {
    fn is_granted_for(&self, call: &ToolCall) -> bool {
        matches!(
            self,
            Self::ApprovalGranted { call_id } if call_id == &call.id
        )
    }
}

/// Immutable result of one Tool Runtime lookup and policy classification.
/// The plan owns the exact call so approval display and execution cannot drift
/// to different arguments or invocation ids.
#[derive(Debug, PartialEq)]
pub struct PreparedToolCall {
    dispatcher_identity: ToolDispatcherIdentity,
    call: ToolCall,
    disposition: ToolExecutionDisposition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolDispatcherIdentity(u64);

impl ToolDispatcherIdentity {
    pub(crate) fn fresh() -> Self {
        static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);
        let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed);
        assert_ne!(identity, 0, "tool dispatcher identity space exhausted");
        Self(identity)
    }
}

#[derive(Debug, PartialEq)]
enum ToolExecutionDisposition {
    Ready,
    RequiresApproval { reason: String },
    Reject { error: ToolError },
}

impl PreparedToolCall {
    pub(crate) fn ready(dispatcher_identity: ToolDispatcherIdentity, call: ToolCall) -> Self {
        Self {
            dispatcher_identity,
            call,
            disposition: ToolExecutionDisposition::Ready,
        }
    }

    pub(crate) fn requiring_approval(
        dispatcher_identity: ToolDispatcherIdentity,
        call: ToolCall,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            dispatcher_identity,
            call,
            disposition: ToolExecutionDisposition::RequiresApproval {
                reason: reason.into(),
            },
        }
    }

    pub(crate) fn rejected(
        dispatcher_identity: ToolDispatcherIdentity,
        call: ToolCall,
        error: ToolError,
    ) -> Self {
        Self {
            dispatcher_identity,
            call,
            disposition: ToolExecutionDisposition::Reject { error },
        }
    }

    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    pub fn approval_reason(&self) -> Option<&str> {
        match &self.disposition {
            ToolExecutionDisposition::RequiresApproval { reason } => Some(reason),
            ToolExecutionDisposition::Ready | ToolExecutionDisposition::Reject { .. } => None,
        }
    }

    /// Consumes this exact plan and validates the Agent Runtime's decision.
    pub(crate) fn into_authorized_call(
        self,
        dispatcher_identity: ToolDispatcherIdentity,
        authorization: ToolExecutionAuthorization,
    ) -> Result<ToolCall, ToolOutput> {
        if self.dispatcher_identity != dispatcher_identity {
            return Err(ToolOutput::Failure {
                error: ToolError {
                    code: "invalid_prepared_tool_call".to_string(),
                    message: "prepared tool call belongs to a different dispatcher".to_string(),
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            });
        }

        match self.disposition {
            ToolExecutionDisposition::Ready => Ok(self.call),
            ToolExecutionDisposition::RequiresApproval { .. }
                if authorization.is_granted_for(&self.call) =>
            {
                Ok(self.call)
            }
            ToolExecutionDisposition::RequiresApproval { reason } => Err(ToolOutput::Failure {
                error: ToolError {
                    code: "approval_required".to_string(),
                    message: reason,
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            }),
            ToolExecutionDisposition::Reject { error } => Err(ToolOutput::Failure {
                error,
                extensions: BTreeMap::new(),
            }),
        }
    }
}

/// Internal seam implemented by one concrete registered tool adapter.
pub trait ToolHandler {
    /// Classifies a call only when its definition uses a call-dependent policy.
    /// Every handler must make the allow decision explicit so changing a
    /// definition to `CallDependent` cannot silently inherit a fail-open
    /// default.
    fn approval_reason(&self, call: &ToolCall) -> Option<String>;

    /// Executes one invocation. Implementations that can block on external
    /// work must observe `cancellation` and return promptly once it is set;
    /// cancellation is cooperative, not forced.
    fn execute(&mut self, call: &ToolCall, cancellation: Arc<AtomicBool>) -> ToolOutput;
}

/// External seam consumed by the Agent Runtime. Lookup, policy classification,
/// authorization enforcement, and handler dispatch stay behind this interface.
///
/// This synchronous trait is an intentionally unstable first-phase proof
/// seam for deterministic fake dispatchers. Long-lived or remote tool execution
/// should move this seam to an async future before it becomes a stable API.
pub(crate) mod sealed {
    pub trait Sealed {}
}

pub trait ToolDispatcher: sealed::Sealed {
    fn prepare(&self, call: ToolCall) -> PreparedToolCall;

    fn execute_prepared(
        &mut self,
        prepared: PreparedToolCall,
        authorization: ToolExecutionAuthorization,
        cancellation: Arc<AtomicBool>,
    ) -> ToolOutput;
}

/// Normalizes output at the Tool Runtime boundary before it reaches the Agent
/// Runtime. Every sealed dispatcher must apply this to adapter-produced output.
pub(crate) fn normalize_dispatcher_output(output: ToolOutput) -> ToolOutput {
    match output {
        ToolOutput::Failure { error, .. } if error.code == "approval_denied" => {
            ToolOutput::Failure {
                error: ToolError {
                    code: "reserved_tool_error_code".to_string(),
                    message: "tool dispatcher returned reserved error code 'approval_denied'"
                        .to_string(),
                    retryable: false,
                },
                extensions: BTreeMap::new(),
            }
        }
        output => output,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// Correlates this result to the ToolCall.id that was executed.
    pub call_id: ToolCallId,
    pub output: ToolOutput,
}

/// Output envelopes are forward-readable so older consumers can tolerate
/// additive fields. Durable producer data belongs in Success.metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolOutput {
    Success {
        content: Vec<ToolContent>,
        /// Producer-defined object metadata for logs, UI hints, and metrics.
        /// Core tool semantics must not depend on producer-specific keys.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        metadata: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
    },
    Failure {
        error: ToolError,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extensions: BTreeMap<String, Value>,
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
    /// `approval_denied` is reserved for the Agent Runtime's canonical denial
    /// result and must not be returned by a Tool Runtime adapter.
    pub code: String,
    pub message: String,
    /// Whether retrying the same low-level tool call is expected to help.
    pub retryable: bool,
}
