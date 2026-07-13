use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::execution::{
    normalize_dispatcher_output, PreparedToolCall, ToolCall, ToolDispatcher, ToolError,
    ToolExecutionAuthorization, ToolHandler, ToolOutput,
};

#[derive(Clone, Debug, Default)]
pub struct FakeToolHandler {
    outputs: VecDeque<ToolOutput>,
    calls: Vec<ToolCall>,
    approval_reason: Option<String>,
}

impl FakeToolHandler {
    pub fn new(outputs: impl IntoIterator<Item = ToolOutput>) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
            calls: Vec::new(),
            approval_reason: None,
        }
    }

    pub fn requiring_approval(
        reason: impl Into<String>,
        outputs: impl IntoIterator<Item = ToolOutput>,
    ) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
            calls: Vec::new(),
            approval_reason: Some(reason.into()),
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
    fn approval_reason(&self, _call: &ToolCall) -> Option<String> {
        self.approval_reason.clone()
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
#[derive(Clone, Debug, Default)]
pub struct FakeToolDispatcher {
    handler: FakeToolHandler,
}

impl FakeToolDispatcher {
    pub fn new(outputs: impl IntoIterator<Item = ToolOutput>) -> Self {
        Self {
            handler: FakeToolHandler::new(outputs),
        }
    }

    pub fn requiring_approval(
        reason: impl Into<String>,
        outputs: impl IntoIterator<Item = ToolOutput>,
    ) -> Self {
        Self {
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
        match self.handler.approval_reason(&call) {
            Some(reason) => PreparedToolCall::requiring_approval(call, reason),
            None => PreparedToolCall::ready(call),
        }
    }

    fn execute_prepared(
        &mut self,
        prepared: PreparedToolCall,
        authorization: ToolExecutionAuthorization,
        cancellation: Arc<AtomicBool>,
    ) -> ToolOutput {
        match prepared.into_authorized_call(authorization) {
            Ok(call) => normalize_dispatcher_output(self.handler.execute(&call, cancellation)),
            Err(output) => output,
        }
    }
}
