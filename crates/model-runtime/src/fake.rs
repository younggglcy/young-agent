use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::client::{ModelClient, ModelRequest};
use crate::stream::{ModelError, ModelStreamEvent};

#[derive(Clone, Debug, PartialEq)]
pub enum ScriptedModelTurn {
    Events(Vec<ModelStreamEvent>),
    Error(ModelError),
}

impl ScriptedModelTurn {
    pub fn events(events: impl IntoIterator<Item = ModelStreamEvent>) -> Self {
        Self::Events(events.into_iter().collect())
    }

    pub fn error(error: ModelError) -> Self {
        Self::Error(error)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FakeModelClient {
    turns: VecDeque<ScriptedModelTurn>,
    request_count: usize,
    last_request: Option<ModelRequest>,
}

impl FakeModelClient {
    pub fn new(turns: impl IntoIterator<Item = ScriptedModelTurn>) -> Self {
        Self {
            turns: turns.into_iter().collect(),
            request_count: 0,
            last_request: None,
        }
    }

    pub fn request_count(&self) -> usize {
        self.request_count
    }

    /// Returns only the latest request snapshot so scripted runs use memory
    /// proportional to current history rather than all cumulative histories.
    pub fn last_request(&self) -> Option<&ModelRequest> {
        self.last_request.as_ref()
    }

    pub fn remaining_turns(&self) -> usize {
        self.turns.len()
    }
}

impl ModelClient for FakeModelClient {
    type Stream = std::vec::IntoIter<ModelStreamEvent>;

    fn stream(
        &mut self,
        request: &ModelRequest,
        _cancellation: Arc<AtomicBool>,
    ) -> Result<Self::Stream, ModelError> {
        self.request_count += 1;
        self.last_request = Some(request.clone());
        match self.turns.pop_front() {
            Some(ScriptedModelTurn::Events(events)) => Ok(events.into_iter()),
            Some(ScriptedModelTurn::Error(error)) => Err(error),
            None => Err(ModelError {
                code: "fake_script_exhausted".to_string(),
                message: "FakeModelClient has no scripted turn left".to_string(),
                retryable: false,
            }),
        }
    }
}
