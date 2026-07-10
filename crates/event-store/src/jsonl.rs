//! Append-only JSONL persistence for canonical Agent Events.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use young_agent_runtime::AgentEvent;

use crate::replay::{replay_events, ReplayError, RunReplay};

/// A path-backed, append-only store with one canonical Agent Event per line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonlEventStore {
    path: PathBuf,
}

impl JsonlEventStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one complete JSON record and flushes it to the operating system.
    pub fn append(&self, event: &AgentEvent) -> Result<(), EventStoreError> {
        let mut record = serde_json::to_vec(event).map_err(|source| EventStoreError::Encode {
            path: self.path.clone(),
            source,
        })?;
        record.push(b'\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| EventStoreError::OpenForAppend {
                path: self.path.clone(),
                source,
            })?;

        file.write_all(&record)
            .and_then(|()| file.flush())
            .map_err(|source| EventStoreError::Append {
                path: self.path.clone(),
                source,
            })
    }

    /// Reads and decodes every record in physical line order.
    pub fn read_all(&self) -> Result<Vec<AgentEvent>, EventStoreError> {
        let file = File::open(&self.path).map_err(|source| EventStoreError::OpenForRead {
            path: self.path.clone(),
            source,
        })?;
        let mut events = Vec::new();

        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line_number = index + 1;
            let line = line.map_err(|source| EventStoreError::ReadRecord {
                path: self.path.clone(),
                line: line_number,
                source,
            })?;
            let event =
                serde_json::from_str(&line).map_err(|source| EventStoreError::DecodeRecord {
                    path: self.path.clone(),
                    line: line_number,
                    source,
                })?;
            events.push(event);
        }

        Ok(events)
    }

    /// Reads the complete log and reconstructs its observable run state.
    pub fn replay(&self) -> Result<RunReplay, EventStoreError> {
        let events = self.read_all()?;
        replay_events(events).map_err(|source| EventStoreError::Replay {
            path: self.path.clone(),
            source,
        })
    }
}

/// Failures include the log path and, for record-level failures, a one-based
/// line number so callers can locate the corrupt record.
#[derive(Debug)]
#[non_exhaustive]
pub enum EventStoreError {
    Encode {
        path: PathBuf,
        source: serde_json::Error,
    },
    OpenForAppend {
        path: PathBuf,
        source: io::Error,
    },
    Append {
        path: PathBuf,
        source: io::Error,
    },
    OpenForRead {
        path: PathBuf,
        source: io::Error,
    },
    ReadRecord {
        path: PathBuf,
        line: usize,
        source: io::Error,
    },
    DecodeRecord {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    Replay {
        path: PathBuf,
        source: ReplayError,
    },
}

impl fmt::Display for EventStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode { path, source } => write!(
                formatter,
                "failed to encode Agent Event for '{}': {source}",
                path.display()
            ),
            Self::OpenForAppend { path, source } => write!(
                formatter,
                "failed to open Event Log '{}' for append: {source}",
                path.display()
            ),
            Self::Append { path, source } => write!(
                formatter,
                "failed to append to Event Log '{}': {source}",
                path.display()
            ),
            Self::OpenForRead { path, source } => write!(
                formatter,
                "failed to open Event Log '{}' for reading: {source}",
                path.display()
            ),
            Self::ReadRecord { path, line, source } => write!(
                formatter,
                "failed to read Event Log '{}' at line {line}: {source}",
                path.display()
            ),
            Self::DecodeRecord { path, line, source } => write!(
                formatter,
                "failed to decode Agent Event in '{}' at line {line}: {source}",
                path.display()
            ),
            Self::Replay { path, source } => write!(
                formatter,
                "failed to replay Event Log '{}': {source}",
                path.display()
            ),
        }
    }
}

impl Error for EventStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode { source, .. } | Self::DecodeRecord { source, .. } => Some(source),
            Self::OpenForAppend { source, .. }
            | Self::Append { source, .. }
            | Self::OpenForRead { source, .. }
            | Self::ReadRecord { source, .. } => Some(source),
            Self::Replay { source, .. } => Some(source),
        }
    }
}
