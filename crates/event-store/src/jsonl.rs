//! Append-only JSONL persistence for canonical Agent Events.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use young_agent_runtime::{AgentEvent, AgentEventSink, EventDurability, EventSequence};

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
    fully_validated: bool,
    line_count: usize,
}

enum AppendPosition {
    Empty,
    Legacy,
    Sequenced(EventSequence),
}

struct PersistedAgentEvent {
    sequence: Option<EventSequence>,
    event: AgentEvent,
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
        self.append_with_durability(None, event, false)
    }

    /// Appends one complete JSON record and synchronizes its bytes and newline
    /// commit marker to stable storage before returning.
    pub fn append_durable(&self, event: &AgentEvent) -> Result<(), EventStoreError> {
        self.append_with_durability(None, event, true)
    }

    /// Idempotently establishes one sequenced canonical event after an
    /// ambiguous append failure.
    ///
    /// The caller must ensure no live runtime or other recovery worker can
    /// append to this log. A committed matching record is synchronized again;
    /// an uncommitted tail is repaired before a missing record is appended.
    pub fn reconcile(
        &self,
        sequence: EventSequence,
        event: &AgentEvent,
        durability: EventDurability,
    ) -> Result<(), EventStoreError> {
        let attempted_event = event.clone().with_event_sequence(sequence);
        let records = if self.path.exists() {
            self.repair_truncated_tail()?;
            self.read_all_records()?
        } else {
            Vec::new()
        };
        if records.iter().any(|record| record.sequence.is_none()) {
            return Err(EventStoreError::ReconciliationConflict {
                path: self.path.clone(),
                sequence,
                persisted: records.into_iter().map(|record| record.event).collect(),
                attempted: Box::new(attempted_event),
            });
        }

        let persisted = records
            .iter()
            .filter(|record| {
                record.sequence == Some(sequence)
                    || (durability == EventDurability::Durable
                        && has_same_durable_identity(&record.event, &attempted_event))
            })
            .collect::<Vec<_>>();
        if persisted.len() == 1
            && persisted[0].sequence == Some(sequence)
            && persisted[0].event == attempted_event
        {
            if durability == EventDurability::Durable {
                self.sync_existing_log()?;
            }
            return Ok(());
        }
        if !persisted.is_empty() {
            return Err(EventStoreError::ReconciliationConflict {
                path: self.path.clone(),
                sequence,
                persisted: persisted
                    .into_iter()
                    .map(|record| record.event.clone())
                    .collect(),
                attempted: Box::new(attempted_event.clone()),
            });
        }
        let next_sequence = EventSequence::new(records.len() as u64 + 1);
        if sequence != next_sequence {
            return Err(EventStoreError::ReconciliationConflict {
                path: self.path.clone(),
                sequence,
                persisted: records
                    .last()
                    .map(|record| vec![record.event.clone()])
                    .unwrap_or_default(),
                attempted: Box::new(attempted_event),
            });
        }

        self.append_with_durability(
            Some(sequence),
            event,
            durability == EventDurability::Durable,
        )
    }

    fn append_with_durability(
        &self,
        sequence: Option<EventSequence>,
        event: &AgentEvent,
        durable: bool,
    ) -> Result<(), EventStoreError> {
        let sequence = sequence.or_else(|| event.event_sequence());
        let mut record = if let Some(sequence) = sequence {
            serde_json::to_vec(&event.clone().with_event_sequence(sequence))
        } else {
            serde_json::to_vec(event)
        }
        .map_err(|source| EventStoreError::Encode {
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

        let result = {
            let state = append_file.as_mut().expect("append file is initialized");
            state
                .file
                .lock()
                .map_err(|source| EventStoreError::InspectForAppend {
                    path: self.path.clone(),
                    source,
                })?;
            let append_result = (|| {
                self.validate_append_sequence(state, sequence)?;
                state
                    .file
                    .write_all(&record)
                    .and_then(|()| state.file.flush())
                    .map_err(|source| EventStoreError::Append {
                        path: self.path.clone(),
                        source,
                    })?;
                if durable {
                    state
                        .file
                        .sync_data()
                        .map_err(|source| EventStoreError::Append {
                            path: self.path.clone(),
                            source,
                        })?;
                    if state.parent_directory_needs_sync {
                        self.sync_parent_directory()
                            .map_err(|source| EventStoreError::Append {
                                path: self.path.clone(),
                                source,
                            })?;
                        state.parent_directory_needs_sync = false;
                    }
                }
                state.line_count += 1;
                Ok(())
            })();
            let unlock_result = state
                .file
                .unlock()
                .map_err(|source| EventStoreError::Append {
                    path: self.path.clone(),
                    source,
                });
            append_result.and(unlock_result)
        };
        if let Err(error) = result {
            // Force the next append to re-open and validate the commit marker;
            // a failed write may have left an uncommitted partial record.
            *append_file = None;
            return Err(error);
        }
        Ok(())
    }

    fn validate_append_sequence(
        &self,
        state: &mut AppendFile,
        found: Option<EventSequence>,
    ) -> Result<(), EventStoreError> {
        let (position, line) = self.inspect_append_position(state)?;
        let expected = match position {
            AppendPosition::Empty => {
                let first = EventSequence::new(1);
                if found.is_none() || found == Some(first) {
                    return Ok(());
                }
                Some(first)
            }
            AppendPosition::Legacy => {
                if found.is_none() {
                    return Ok(());
                }
                None
            }
            AppendPosition::Sequenced(expected) => {
                if found == Some(expected) {
                    return Ok(());
                }
                Some(expected)
            }
        };
        Err(EventStoreError::InvalidEventSequence {
            path: self.path.clone(),
            line,
            expected,
            found,
        })
    }

    fn inspect_append_position(
        &self,
        state: &mut AppendFile,
    ) -> Result<(AppendPosition, usize), EventStoreError> {
        let file_length = state.file.seek(SeekFrom::End(0)).map_err(|source| {
            EventStoreError::InspectForAppend {
                path: self.path.clone(),
                source,
            }
        })?;
        if file_length == 0 {
            state.fully_validated = true;
            state.line_count = 0;
            return Ok((AppendPosition::Empty, 1));
        }

        let mut last_byte = [0_u8; 1];
        state
            .file
            .seek(SeekFrom::End(-1))
            .and_then(|_| state.file.read_exact(&mut last_byte))
            .map_err(|source| EventStoreError::InspectForAppend {
                path: self.path.clone(),
                source,
            })?;
        if last_byte[0] != b'\n' {
            return Err(EventStoreError::UnterminatedLog {
                path: self.path.clone(),
            });
        }

        let last_event = if state.fully_validated {
            self.read_last_event(&mut state.file, file_length, state.line_count.max(1))?
        } else {
            let records = self.read_all_records()?;
            state.line_count = records.len();
            state.fully_validated = true;
            records
                .last()
                .expect("a non-empty log has a final record")
                .event
                .clone()
        };
        let line = match last_event.event_sequence() {
            Some(sequence) => sequence.as_u64().saturating_add(1) as usize,
            None => state.line_count.saturating_add(1),
        };
        let position = match last_event.event_sequence() {
            Some(sequence) => {
                AppendPosition::Sequenced(EventSequence::new(sequence.as_u64().saturating_add(1)))
            }
            None => AppendPosition::Legacy,
        };
        Ok((position, line))
    }

    fn read_last_event(
        &self,
        file: &mut File,
        file_length: u64,
        line: usize,
    ) -> Result<AgentEvent, EventStoreError> {
        const SEARCH_CHUNK_SIZE: usize = 8 * 1024;
        let mut search_end = file_length - 1;
        let mut record_start = 0_u64;
        let mut buffer = [0_u8; SEARCH_CHUNK_SIZE];
        while search_end > 0 {
            let chunk_start = search_end.saturating_sub(SEARCH_CHUNK_SIZE as u64);
            let chunk_length = (search_end - chunk_start) as usize;
            file.seek(SeekFrom::Start(chunk_start))
                .and_then(|_| file.read_exact(&mut buffer[..chunk_length]))
                .map_err(|source| EventStoreError::InspectForAppend {
                    path: self.path.clone(),
                    source,
                })?;
            if let Some(newline_index) = buffer[..chunk_length]
                .iter()
                .rposition(|byte| *byte == b'\n')
            {
                record_start = chunk_start + newline_index as u64 + 1;
                break;
            }
            search_end = chunk_start;
        }

        let record_length = (file_length - record_start - 1) as usize;
        let mut record = vec![0_u8; record_length];
        file.seek(SeekFrom::Start(record_start))
            .and_then(|_| file.read_exact(&mut record))
            .map_err(|source| EventStoreError::InspectForAppend {
                path: self.path.clone(),
                source,
            })?;
        serde_json::from_slice(&record).map_err(|source| EventStoreError::DecodeRecord {
            path: self.path.clone(),
            line,
            source,
        })
    }

    fn sync_existing_log(&self) -> Result<(), EventStoreError> {
        let file = OpenOptions::new()
            .read(true)
            .open(&self.path)
            .map_err(|source| EventStoreError::OpenForAppend {
                path: self.path.clone(),
                source,
            })?;
        file.sync_data()
            .and_then(|()| self.sync_parent_directory())
            .map_err(|source| EventStoreError::Append {
                path: self.path.clone(),
                source,
            })
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
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| EventStoreError::OpenForAppend {
                path: self.path.clone(),
                source,
            })?;

        Ok(AppendFile {
            file,
            parent_directory_needs_sync: true,
            fully_validated: false,
            line_count: 0,
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
        Ok(self
            .read_all_records()?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    fn read_all_records(&self) -> Result<Vec<PersistedAgentEvent>, EventStoreError> {
        let file = File::open(&self.path).map_err(|source| EventStoreError::OpenForRead {
            path: self.path.clone(),
            source,
        })?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut line_number = 0;
        let mut expected_sequence = None::<Option<EventSequence>>;

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

            let event: AgentEvent = serde_json::from_slice(&record).map_err(|source| {
                EventStoreError::DecodeRecord {
                    path: self.path.clone(),
                    line: line_number,
                    source,
                }
            })?;
            let sequence = event.event_sequence();
            match expected_sequence {
                None => {
                    if let Some(found) = sequence {
                        let expected = EventSequence::new(1);
                        if found != expected {
                            return Err(EventStoreError::InvalidEventSequence {
                                path: self.path.clone(),
                                line: line_number,
                                expected: Some(expected),
                                found: Some(found),
                            });
                        }
                    }
                }
                Some(expected) if sequence != expected => {
                    return Err(EventStoreError::InvalidEventSequence {
                        path: self.path.clone(),
                        line: line_number,
                        expected,
                        found: sequence,
                    });
                }
                Some(_) => {}
            }
            expected_sequence = Some(
                sequence.map(|sequence| EventSequence::new(sequence.as_u64().saturating_add(1))),
            );
            if !is_terminated {
                return Err(EventStoreError::TruncatedRecord {
                    path: self.path.clone(),
                    line: line_number,
                });
            }
            events.push(PersistedAgentEvent { sequence, event });
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

fn has_same_durable_identity(left: &AgentEvent, right: &AgentEvent) -> bool {
    match (left, right) {
        (
            AgentEvent::ToolCallRequested {
                run_id: left_run,
                call: left_call,
                ..
            },
            AgentEvent::ToolCallRequested {
                run_id: right_run,
                call: right_call,
                ..
            },
        ) => left_run == right_run && left_call.id == right_call.id,
        (
            AgentEvent::ApprovalRequested {
                run_id: left_run,
                request: left_request,
                ..
            },
            AgentEvent::ApprovalRequested {
                run_id: right_run,
                request: right_request,
                ..
            },
        ) => left_run == right_run && left_request.id == right_request.id,
        (
            AgentEvent::ApprovalResolved {
                run_id: left_run,
                approval_id: left_approval,
                ..
            },
            AgentEvent::ApprovalResolved {
                run_id: right_run,
                approval_id: right_approval,
                ..
            },
        ) => left_run == right_run && left_approval == right_approval,
        (
            AgentEvent::ToolResult {
                run_id: left_run,
                result: left_result,
                ..
            },
            AgentEvent::ToolResult {
                run_id: right_run,
                result: right_result,
                ..
            },
        ) => left_run == right_run && left_result.call_id == right_result.call_id,
        (
            AgentEvent::RunFinished {
                run_id: left_run, ..
            },
            AgentEvent::RunFinished {
                run_id: right_run, ..
            },
        ) => left_run == right_run,
        _ => false,
    }
}

impl AgentEventSink for JsonlEventStore {
    type Error = EventStoreError;

    fn append(&mut self, sequence: EventSequence, event: &AgentEvent) -> Result<(), Self::Error> {
        self.append_with_durability(Some(sequence), event, false)
    }

    fn append_durable(
        &mut self,
        sequence: EventSequence,
        event: &AgentEvent,
    ) -> Result<(), Self::Error> {
        self.append_with_durability(Some(sequence), event, true)
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
    InvalidEventSequence {
        path: PathBuf,
        line: usize,
        expected: Option<EventSequence>,
        found: Option<EventSequence>,
    },
    Replay {
        path: PathBuf,
        source: ReplayError,
    },
    ReconciliationConflict {
        path: PathBuf,
        sequence: EventSequence,
        persisted: Vec<AgentEvent>,
        attempted: Box<AgentEvent>,
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
            Self::InvalidEventSequence {
                path,
                line,
                expected,
                found,
            } => write!(
                formatter,
                "invalid Agent Event sequence in '{}' at line {line}: expected {expected:?}, found {found:?}",
                path.display()
            ),
            Self::Replay { path, source } => write!(
                formatter,
                "failed to replay Event Log '{}': {source}",
                path.display()
            ),
            Self::ReconciliationConflict {
                path,
                sequence,
                persisted,
                attempted,
            } => write!(
                formatter,
                "Agent Event sequence {} or its durable lifecycle identity has conflicting records in '{}'; persisted {persisted:?}, attempted {attempted:?}",
                sequence.as_u64(),
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
            Self::UnterminatedLog { .. }
            | Self::TruncatedRecord { .. }
            | Self::InvalidEventSequence { .. }
            | Self::ReconciliationConflict { .. } => None,
            Self::Replay { source, .. } => Some(source),
        }
    }
}
