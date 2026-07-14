use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::execution::{
    normalize_dispatcher_output, PreparedToolCall, ToolCall, ToolCallPolicy, ToolDispatcher,
    ToolDispatcherIdentity, ToolError, ToolExecutionAuthorization, ToolHandler, ToolOutput,
};

#[derive(Clone, Debug, Default)]
pub struct FakeToolHandler {
    outputs: VecDeque<ToolOutput>,
    calls: Vec<ToolCall>,
    policy: ToolCallPolicy,
}

impl FakeToolHandler {
    pub fn new(outputs: impl IntoIterator<Item = ToolOutput>) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
            calls: Vec::new(),
            policy: ToolCallPolicy::Allow,
        }
    }

    pub fn requiring_approval(
        reason: impl Into<String>,
        outputs: impl IntoIterator<Item = ToolOutput>,
    ) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
            calls: Vec::new(),
            policy: ToolCallPolicy::RequiresApproval {
                reason: reason.into(),
            },
        }
    }

    pub fn rejecting(
        reason: impl Into<String>,
        outputs: impl IntoIterator<Item = ToolOutput>,
    ) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
            calls: Vec::new(),
            policy: ToolCallPolicy::Reject {
                reason: reason.into(),
            },
        }
    }

    pub fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    pub fn remaining_outputs(&self) -> usize {
        self.outputs.len()
    }
}

impl ToolHandler for FakeToolHandler {
    fn classify(&self, _call: &ToolCall) -> ToolCallPolicy {
        self.policy.clone()
    }

    fn execute(&mut self, call: &ToolCall, _cancellation: Arc<AtomicBool>) -> ToolOutput {
        self.calls.push(call.clone());
        self.outputs
            .pop_front()
            .unwrap_or_else(|| ToolOutput::Failure {
                error: ToolError {
                    code: "fake_script_exhausted".to_string(),
                    message: "FakeToolHandler has no scripted output left".to_string(),
                    retryable: false,
                },
                extensions: Default::default(),
            })
    }
}

/// Dedicated external fake for deterministic Agent Runtime tests.
#[derive(Debug)]
pub struct FakeToolDispatcher {
    dispatcher_identity: ToolDispatcherIdentity,
    handler: FakeToolHandler,
}

impl Clone for FakeToolDispatcher {
    fn clone(&self) -> Self {
        Self {
            dispatcher_identity: ToolDispatcherIdentity::fresh(),
            handler: self.handler.clone(),
        }
    }
}

impl Default for FakeToolDispatcher {
    fn default() -> Self {
        Self {
            dispatcher_identity: ToolDispatcherIdentity::fresh(),
            handler: FakeToolHandler::default(),
        }
    }
}

impl FakeToolDispatcher {
    pub fn new(outputs: impl IntoIterator<Item = ToolOutput>) -> Self {
        Self {
            dispatcher_identity: ToolDispatcherIdentity::fresh(),
            handler: FakeToolHandler::new(outputs),
        }
    }

    pub fn requiring_approval(
        reason: impl Into<String>,
        outputs: impl IntoIterator<Item = ToolOutput>,
    ) -> Self {
        Self {
            dispatcher_identity: ToolDispatcherIdentity::fresh(),
            handler: FakeToolHandler::requiring_approval(reason, outputs),
        }
    }

    pub fn calls(&self) -> &[ToolCall] {
        self.handler.calls()
    }

    pub fn remaining_outputs(&self) -> usize {
        self.handler.remaining_outputs()
    }
}

impl crate::execution::sealed::Sealed for FakeToolDispatcher {}

impl ToolDispatcher for FakeToolDispatcher {
    fn prepare(&self, call: ToolCall) -> PreparedToolCall {
        match self.handler.classify(&call) {
            ToolCallPolicy::Allow => PreparedToolCall::ready(self.dispatcher_identity, call),
            ToolCallPolicy::RequiresApproval { reason } => {
                PreparedToolCall::requiring_approval(self.dispatcher_identity, call, reason)
            }
            ToolCallPolicy::Reject { reason } => PreparedToolCall::rejected(
                self.dispatcher_identity,
                call,
                ToolError {
                    code: "tool_rejected".to_string(),
                    message: reason,
                    retryable: false,
                },
            ),
        }
    }

    fn execute_prepared(
        &mut self,
        prepared: PreparedToolCall,
        authorization: ToolExecutionAuthorization,
        cancellation: Arc<AtomicBool>,
    ) -> ToolOutput {
        match prepared.into_authorized_call(self.dispatcher_identity, authorization) {
            Ok(call) => normalize_dispatcher_output(self.handler.execute(&call, cancellation)),
            Err(output) => output,
        }
    }
}
