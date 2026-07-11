//! Append-only JSONL persistence for canonical Agent Events.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use young_agent_runtime::{AgentEvent, AgentEventSink};

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
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| EventStoreError::OpenForAppend {
                path: self.path.clone(),
                source,
            })?;

        let file_length =
            file.seek(SeekFrom::End(0))
                .map_err(|source| EventStoreError::InspectForAppend {
                    path: self.path.clone(),
                    source,
                })?;
        if file_length > 0 {
            let mut last_byte = [0_u8; 1];
            file.seek(SeekFrom::End(-1))
                .and_then(|_| file.read_exact(&mut last_byte))
                .map_err(|source| EventStoreError::InspectForAppend {
                    path: self.path.clone(),
                    source,
                })?;
            if last_byte[0] != b'\n' {
                return Err(EventStoreError::UnterminatedLog {
                    path: self.path.clone(),
                });
            }
        }

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
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut line_number = 0;

        loop {
            let mut record = Vec::new();
            let bytes_read = reader.read_until(b'\n', &mut record).map_err(|source| {
                EventStoreError::ReadRecord {
                    path: self.path.clone(),
                    line: line_number + 1,
                    source,
                }
            })?;
            if bytes_read == 0 {
                break;
            }

            line_number += 1;
            let is_terminated = record.last() == Some(&b'\n');
            if is_terminated {
                record.pop();
            }

            let event = serde_json::from_slice(&record).map_err(|source| {
                EventStoreError::DecodeRecord {
                    path: self.path.clone(),
                    line: line_number,
                    source,
                }
            })?;
            if !is_terminated {
                return Err(EventStoreError::TruncatedRecord {
                    path: self.path.clone(),
                    line: line_number,
                });
            }
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

impl AgentEventSink for JsonlEventStore {
    type Error = EventStoreError;

    fn append(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        JsonlEventStore::append(self, event)
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
    InspectForAppend {
        path: PathBuf,
        source: io::Error,
    },
    UnterminatedLog {
        path: PathBuf,
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
    TruncatedRecord {
        path: PathBuf,
        line: usize,
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
            Self::InspectForAppend { path, source } => write!(
                formatter,
                "failed to inspect Event Log '{}' before append: {source}",
                path.display()
            ),
            Self::UnterminatedLog { path } => write!(
                formatter,
                "cannot append to Event Log '{}': existing record is not terminated by a newline",
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
            Self::TruncatedRecord { path, line } => write!(
                formatter,
                "truncated Agent Event in '{}' at line {line}: record is not terminated by a newline",
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
            | Self::InspectForAppend { source, .. }
            | Self::Append { source, .. }
            | Self::OpenForRead { source, .. }
            | Self::ReadRecord { source, .. } => Some(source),
            Self::UnterminatedLog { .. } | Self::TruncatedRecord { .. } => None,
            Self::Replay { source, .. } => Some(source),
        }
    }
}
