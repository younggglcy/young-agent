use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use young_model_runtime::{FakeModelClient, ModelStreamEvent, ScriptedModelTurn};

const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SCRIPT_TURNS: usize = 128;
const MAX_EVENTS_PER_TURN: usize = 4 * 1024;
const MAX_SCRIPT_EVENTS: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FakeScript {
    turns: Vec<Vec<ModelStreamEvent>>,
}

pub(crate) fn load(
    prompt: &str,
    script_path: Option<&Path>,
) -> Result<FakeModelClient, FakeProviderError> {
    let Some(path) = script_path else {
        return Ok(FakeModelClient::new([ScriptedModelTurn::events([
            ModelStreamEvent::TextDelta {
                delta: format!("Fake provider response for: {prompt}"),
                extensions: BTreeMap::new(),
            },
            ModelStreamEvent::Completed {
                finish_reason: Some("stop".to_string()),
                extensions: BTreeMap::new(),
            },
        ])]));
    };

    let file = File::open(path).map_err(|source| FakeProviderError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut source = Vec::new();
    file.take(MAX_SCRIPT_BYTES + 1)
        .read_to_end(&mut source)
        .map_err(|source| FakeProviderError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if source.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(FakeProviderError::Limit {
            path: path.to_path_buf(),
            message: format!("script exceeds {MAX_SCRIPT_BYTES} bytes"),
        });
    }
    let script: FakeScript =
        serde_json::from_slice(&source).map_err(|source| FakeProviderError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    validate(path, &script)?;
    Ok(FakeModelClient::new(
        script.turns.into_iter().map(ScriptedModelTurn::events),
    ))
}

fn validate(path: &Path, script: &FakeScript) -> Result<(), FakeProviderError> {
    if script.turns.is_empty() {
        return Err(FakeProviderError::Empty {
            path: path.to_path_buf(),
        });
    }
    if script.turns.len() > MAX_SCRIPT_TURNS {
        return Err(FakeProviderError::Limit {
            path: path.to_path_buf(),
            message: format!("script exceeds {MAX_SCRIPT_TURNS} turns"),
        });
    }
    let mut total_events = 0usize;
    for (index, turn) in script.turns.iter().enumerate() {
        if turn.len() > MAX_EVENTS_PER_TURN {
            return Err(FakeProviderError::Limit {
                path: path.to_path_buf(),
                message: format!(
                    "turn {} exceeds {MAX_EVENTS_PER_TURN} model events",
                    index + 1
                ),
            });
        }
        total_events = total_events.saturating_add(turn.len());
        if total_events > MAX_SCRIPT_EVENTS {
            return Err(FakeProviderError::Limit {
                path: path.to_path_buf(),
                message: format!("script exceeds {MAX_SCRIPT_EVENTS} total model events"),
            });
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum FakeProviderError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    Empty {
        path: PathBuf,
    },
    Limit {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for FakeProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(
                formatter,
                "failed to read fake-provider script '{}': {source}",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                formatter,
                "failed to decode fake-provider script '{}': {source}",
                path.display()
            ),
            Self::Empty { path } => write!(
                formatter,
                "fake-provider script '{}' must contain at least one turn",
                path.display()
            ),
            Self::Limit { path, message } => write!(
                formatter,
                "fake-provider script '{}' is too large: {message}",
                path.display()
            ),
        }
    }
}

impl Error for FakeProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Empty { .. } | Self::Limit { .. } => None,
        }
    }
}
