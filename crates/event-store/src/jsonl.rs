//! Append-only JSONL persistence for canonical Agent Events.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use young_agent_runtime::{AgentEvent, AgentEventSink};

use crate::replay::{
    replay_events, replay_events_for_recovery, replay_events_with_compatibility,
    ReplayCompatibility, ReplayError, RunReplay,
};

/// A path-backed, append-only store with one canonical Agent Event per line.
#[derive(Clone)]
pub struct JsonlEventStore {
    path: PathBuf,
    append_file: Arc<Mutex<Option<AppendFile>>>,
}

struct AppendFile {
    file: File,
    parent_directory_needs_sync: bool,
}

impl fmt::Debug for JsonlEventStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsonlEventStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl PartialEq for JsonlEventStore {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for JsonlEventStore {}

impl JsonlEventStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            append_file: Arc::new(Mutex::new(None)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one complete JSON record and flushes it to the operating system.
    pub fn append(&self, event: &AgentEvent) -> Result<(), EventStoreError> {
        self.append_with_durability(event, false)
    }

    /// Appends one complete JSON record and synchronizes its bytes and newline
    /// commit marker to stable storage before returning.
    pub fn append_durable(&self, event: &AgentEvent) -> Result<(), EventStoreError> {
        self.append_with_durability(event, true)
    }

    fn append_with_durability(
        &self,
        event: &AgentEvent,
        durable: bool,
    ) -> Result<(), EventStoreError> {
        let mut record = serde_json::to_vec(event).map_err(|source| EventStoreError::Encode {
            path: self.path.clone(),
            source,
        })?;
        record.push(b'\n');

        let mut append_file = self
            .append_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if append_file.is_none() {
            *append_file = Some(self.open_append_file()?);
        }

        let state = append_file.as_mut().expect("append file is initialized");
        let result = (|| {
            state.file.write_all(&record)?;
            state.file.flush()?;
            if durable {
                state.file.sync_data()?;
                if state.parent_directory_needs_sync {
                    self.sync_parent_directory()?;
                    state.parent_directory_needs_sync = false;
                }
            }
            Ok(())
        })();
        if let Err(source) = result {
            // Force the next append to re-open and validate the commit marker;
            // a failed write may have left an uncommitted partial record.
            *append_file = None;
            return Err(EventStoreError::Append {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }

    fn sync_parent_directory(&self) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    }

    fn open_append_file(&self) -> Result<AppendFile, EventStoreError> {
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

        Ok(AppendFile {
            file,
            parent_directory_needs_sync: true,
        })
    }

    /// Removes only the final record when it lacks the newline commit marker.
    ///
    /// This is an explicit recovery operation: the caller must ensure no live
    /// runtime can append and must reconcile any indeterminate tool side effect
    /// before deciding whether to restore a `ToolResult` event.
    pub fn repair_truncated_tail(&self) -> Result<u64, EventStoreError> {
        *self
            .append_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|source| EventStoreError::OpenForRepair {
                path: self.path.clone(),
                source,
            })?;
        let file_length =
            file.seek(SeekFrom::End(0))
                .map_err(|source| EventStoreError::RepairTail {
                    path: self.path.clone(),
                    source,
                })?;
        if file_length == 0 {
            return Ok(0);
        }

        let mut last_byte = [0_u8; 1];
        file.seek(SeekFrom::End(-1))
            .and_then(|_| file.read_exact(&mut last_byte))
            .map_err(|source| EventStoreError::RepairTail {
                path: self.path.clone(),
                source,
            })?;
        if last_byte[0] == b'\n' {
            return Ok(0);
        }

        const SEARCH_CHUNK_SIZE: usize = 8 * 1024;
        let mut search_end = file_length;
        let mut committed_length = 0_u64;
        let mut buffer = [0_u8; SEARCH_CHUNK_SIZE];
        while search_end > 0 {
            let chunk_start = search_end.saturating_sub(SEARCH_CHUNK_SIZE as u64);
            let chunk_length = (search_end - chunk_start) as usize;
            file.seek(SeekFrom::Start(chunk_start))
                .and_then(|_| file.read_exact(&mut buffer[..chunk_length]))
                .map_err(|source| EventStoreError::RepairTail {
                    path: self.path.clone(),
                    source,
                })?;
            if let Some(newline_index) = buffer[..chunk_length]
                .iter()
                .rposition(|byte| *byte == b'\n')
            {
                committed_length = chunk_start + newline_index as u64 + 1;
                break;
            }
            search_end = chunk_start;
        }

        let removed_bytes = file_length - committed_length;
        file.set_len(committed_length)
            .and_then(|()| file.sync_all())
            .map_err(|source| EventStoreError::RepairTail {
                path: self.path.clone(),
                source,
            })?;
        Ok(removed_bytes)
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

    /// Replays a log with an explicit compatibility policy. Prefer strict
    /// [`Self::replay`]; legacy mode is only for pre-`ApprovalResolved` logs.
    pub fn replay_with_compatibility(
        &self,
        compatibility: ReplayCompatibility,
    ) -> Result<RunReplay, EventStoreError> {
        let events = self.read_all()?;
        replay_events_with_compatibility(events, compatibility).map_err(|source| {
            EventStoreError::Replay {
                path: self.path.clone(),
                source,
            }
        })
    }

    /// Replays an inactive log and exposes tool calls whose results require
    /// reconciliation. The caller must ensure no live runtime can append.
    pub fn replay_for_recovery(&self) -> Result<RunReplay, EventStoreError> {
        let events = self.read_all()?;
        replay_events_for_recovery(events).map_err(|source| EventStoreError::Replay {
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

    fn append_durable(&mut self, event: &AgentEvent) -> Result<(), Self::Error> {
        JsonlEventStore::append_durable(self, event)
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
    OpenForRepair {
        path: PathBuf,
        source: io::Error,
    },
    RepairTail {
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
            Self::OpenForRepair { path, source } => write!(
                formatter,
                "failed to open Event Log '{}' for tail repair: {source}",
                path.display()
            ),
            Self::RepairTail { path, source } => write!(
                formatter,
                "failed to repair truncated tail in Event Log '{}': {source}",
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
            | Self::OpenForRepair { source, .. }
            | Self::RepairTail { source, .. }
            | Self::Append { source, .. }
            | Self::OpenForRead { source, .. }
            | Self::ReadRecord { source, .. } => Some(source),
            Self::UnterminatedLog { .. } | Self::TruncatedRecord { .. } => None,
            Self::Replay { source, .. } => Some(source),
        }
    }
}
