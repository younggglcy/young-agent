use std::collections::VecDeque;

use crate::execution::{ToolCall, ToolError, ToolExecutor, ToolOutput, ToolResult};

#[derive(Clone, Debug, Default)]
pub struct FakeToolExecutor {
    outputs: VecDeque<ToolOutput>,
    calls: Vec<ToolCall>,
    approval_reason: Option<String>,
}

impl FakeToolExecutor {
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

impl ToolExecutor for FakeToolExecutor {
    fn approval_reason(&self, _call: &ToolCall) -> Option<String> {
        self.approval_reason.clone()
    }

    fn execute(&mut self, call: &ToolCall) -> ToolResult {
        self.calls.push(call.clone());
        let output = self
            .outputs
            .pop_front()
            .unwrap_or_else(|| ToolOutput::Failure {
                error: ToolError {
                    code: "fake_script_exhausted".to_string(),
                    message: "FakeToolExecutor has no scripted output left".to_string(),
                    retryable: false,
                },
                extensions: Default::default(),
            });

        ToolResult {
            call_id: call.id.clone(),
            output,
        }
    }
}
