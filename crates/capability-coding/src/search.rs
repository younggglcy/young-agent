use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::{Dir, DirEntry, File};
use serde_json::{json, Value};
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    display_relative_path, failure, finalize_output, truncate_json_string, workspace_path_failure,
    ToolArguments, MAX_OUTPUT_BYTES, MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::{CodingWorkspace, RECOVERY_DIRECTORY};

const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_LINE_BYTES: usize = 8 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 8 * 1024;
const MAX_SEARCH_QUERY_METADATA_SERIALIZED_BYTES: usize = 4 * 1024;
const MAX_SEARCH_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_SEARCH_DIRECTORIES: u64 = 10_000;
const MAX_SEARCH_ENTRIES: u64 = 100_000;
const MAX_SEARCH_DEPTH: usize = 256;
const MAX_SEARCH_FILES: u64 = 100_000;
const MAX_SEARCH_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn execute(
    workspace: &CodingWorkspace,
    call: &ToolCall,
    cancellation: &AtomicBool,
) -> ToolOutput {
    let arguments = match ToolArguments::parse(&call.arguments, &["query", "path"]) {
        Ok(arguments) => arguments,
        Err(output) => return output,
    };
    let query = match arguments.required_string("query") {
        Ok("") => {
            return failure(
                "invalid_arguments",
                "argument 'query' must not be empty",
                false,
            )
        }
        Ok(query) if query.len() <= MAX_SEARCH_QUERY_BYTES => query,
        Ok(_) => {
            return failure(
                "invalid_arguments",
                format!("argument 'query' exceeds {MAX_SEARCH_QUERY_BYTES} bytes"),
                false,
            )
        }
        Err(output) => return output,
    };
    let path = match arguments.optional_string("path") {
        Ok(Some(path)) => path,
        Ok(None) => ".",
        Err(output) => return output,
    };
    let resolved = match workspace.resolve_existing(Path::new(path)) {
        Ok(resolved) => resolved,
        Err(error) => return workspace_path_failure(error),
    };
    let pattern = LiteralPattern::new(query.as_bytes());
    let mut results = SearchResults::default();

    match workspace.open_dir(&resolved.relative_path) {
        Ok(directory) => {
            if let Err(output) = search_directory(
                directory,
                resolved.relative_path,
                &pattern,
                cancellation,
                &mut results,
            ) {
                return output;
            }
        }
        Err(_) => {
            let file = match workspace.open_regular_file(&resolved.relative_path) {
                Ok((file, _)) => file,
                Err(source) => {
                    return failure(
                        "workspace_io_error",
                        format!(
                            "failed to open '{}': {source}",
                            resolved.relative_path.display()
                        ),
                        source.kind() == std::io::ErrorKind::Interrupted,
                    )
                }
            };
            let display_path = display_relative_path(&resolved.relative_path);
            if let Err(output) =
                search_file(file, &display_path, &pattern, cancellation, &mut results)
            {
                return output;
            }
        }
    }

    bounded_search_output(workspace, path, query, results)
}

struct DirectoryFrame {
    relative_path: PathBuf,
    entries: std::vec::IntoIter<DirEntry>,
}

fn search_directory(
    root: Dir,
    relative_path: PathBuf,
    pattern: &LiteralPattern,
    cancellation: &AtomicBool,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    let _ = results.record_directory();
    let entries = sorted_entries(&root, &relative_path, cancellation, results)?;
    if results.limit_reached {
        return Ok(());
    }
    let mut stack = vec![DirectoryFrame {
        relative_path,
        entries,
    }];

    while let Some(frame) = stack.last_mut() {
        if cancellation.load(Ordering::Relaxed) {
            return Err(failure(
                "tool_cancelled",
                "search_files was cancelled",
                false,
            ));
        }
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let file_type = entry.file_type().map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to inspect a directory entry: {source}"),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
        if file_type.is_symlink() {
            continue;
        }
        let file_name = entry.file_name();
        if file_name
            .to_string_lossy()
            .starts_with(".young-agent-patch-")
        {
            continue;
        }
        let entry_path = frame.relative_path.join(&file_name);
        if file_type.is_dir() {
            if file_name == ".git" || file_name == RECOVERY_DIRECTORY {
                continue;
            }
            if stack.len() == MAX_SEARCH_DEPTH {
                results.mark_limit_reached();
                return Ok(());
            }
            if !results.record_directory() {
                return Ok(());
            }
            let directory = entry.open_dir().map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to open '{}': {source}", entry_path.display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            let entries = sorted_entries(&directory, &entry_path, cancellation, results)?;
            if results.limit_reached {
                return Ok(());
            }
            stack.push(DirectoryFrame {
                relative_path: entry_path,
                entries,
            });
        } else if file_type.is_file() {
            let (file, _) = CodingWorkspace::open_regular_entry(&entry).map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to open '{}': {source}", entry_path.display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            let display_path = display_relative_path(&entry_path);
            search_file(file, &display_path, pattern, cancellation, results)?;
            if results.limit_reached {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn sorted_entries(
    directory: &Dir,
    relative_path: &Path,
    cancellation: &AtomicBool,
    results: &mut SearchResults,
) -> Result<std::vec::IntoIter<DirEntry>, ToolOutput> {
    let entries = directory.entries().map_err(|source| {
        failure(
            "workspace_io_error",
            format!("failed to list '{}': {source}", relative_path.display()),
            source.kind() == std::io::ErrorKind::Interrupted,
        )
    })?;
    let mut sorted = Vec::new();
    for entry in entries {
        if cancellation.load(Ordering::Relaxed) {
            return Err(failure(
                "tool_cancelled",
                "search_files was cancelled",
                false,
            ));
        }
        if sorted.len() == MAX_SEARCH_DIRECTORY_ENTRIES {
            return Err(failure(
                "search_limit_exceeded",
                format!(
                    "directory '{}' exceeds {MAX_SEARCH_DIRECTORY_ENTRIES} entries",
                    relative_path.display()
                ),
                false,
            ));
        }
        let entry = entry.map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to read a directory entry: {source}"),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
        if !results.record_entry() {
            break;
        }
        sorted.push(entry);
    }
    sorted.sort_by_key(DirEntry::file_name);
    Ok(sorted.into_iter())
}

fn search_file(
    mut file: File,
    display_path: &str,
    pattern: &LiteralPattern,
    cancellation: &AtomicBool,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    if results.files_searched == MAX_SEARCH_FILES {
        results.truncated = true;
        results.limit_reached = true;
        return Ok(());
    }
    results.files_searched = results.files_searched.saturating_add(1);
    let checkpoint = results.checkpoint();
    let mut buffer = [0u8; 8 * 1024];
    let mut line = LineState::default();
    let mut line_number = 1u64;
    let mut utf8 = Utf8Validator::default();

    loop {
        if cancellation.load(Ordering::Relaxed) {
            return Err(failure(
                "tool_cancelled",
                "search_files was cancelled",
                false,
            ));
        }
        let remaining_bytes = MAX_SEARCH_BYTES.saturating_sub(results.bytes_searched);
        if remaining_bytes == 0 {
            let mut probe = [0u8; 1];
            let bytes_read = file.read(&mut probe).map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to search '{display_path}': {source}"),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            if bytes_read == 0 {
                return finish_search_file(
                    display_path,
                    line_number,
                    &line,
                    &utf8,
                    checkpoint,
                    results,
                );
            }
            results.truncated = true;
            results.limit_reached = true;
            return Ok(());
        }
        let read_limit = buffer.len().min(remaining_bytes as usize);
        let bytes_read = file.read(&mut buffer[..read_limit]).map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to search '{display_path}': {source}"),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
        if bytes_read == 0 {
            return finish_search_file(
                display_path,
                line_number,
                &line,
                &utf8,
                checkpoint,
                results,
            );
        }
        results.bytes_searched = results.bytes_searched.saturating_add(bytes_read as u64);
        if !utf8.feed(&buffer[..bytes_read]) {
            results.discard_binary_file(checkpoint);
            return Ok(());
        }

        let mut start = 0usize;
        for newline in buffer[..bytes_read]
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
        {
            line.feed(&buffer[start..newline], pattern);
            finish_line(display_path, line_number, &line, results)?;
            line = LineState::default();
            line_number += 1;
            start = newline + 1;
        }
        line.feed(&buffer[start..bytes_read], pattern);
    }
}

fn finish_search_file(
    display_path: &str,
    line_number: u64,
    line: &LineState,
    utf8: &Utf8Validator,
    checkpoint: SearchCheckpoint,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    if !utf8.finish() {
        results.discard_binary_file(checkpoint);
        return Ok(());
    }
    if line.bytes_seen > 0 {
        finish_line(display_path, line_number, line, results)?;
    }
    Ok(())
}

fn finish_line(
    display_path: &str,
    line_number: u64,
    line: &LineState,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    if !line.matched || results.limit_reached {
        return Ok(());
    }
    let text = match visible_utf8_prefix(&line.visible) {
        Ok(text) => text.strip_suffix('\r').unwrap_or(text),
        Err(()) => {
            return Err(failure(
                "workspace_io_error",
                "UTF-8 validation drifted",
                false,
            ))
        }
    };
    if line.truncated {
        results.lines_truncated += 1;
        results.truncated = true;
    }
    let value = json!({
        "path": display_path,
        "line": line_number,
        "text": text,
    });
    let serialized_bytes = serde_json::to_vec(&value)
        .expect("search match JSON serializes")
        .len();
    if results.matches.len() == MAX_SEARCH_MATCHES
        || results.serialized_bytes.saturating_add(serialized_bytes)
            > MAX_TOOL_CONTENT_SERIALIZED_BYTES
    {
        results.truncated = true;
        results.limit_reached = true;
        return Ok(());
    }
    results.serialized_bytes = results.serialized_bytes.saturating_add(serialized_bytes);
    results.matches.push(value);
    Ok(())
}

#[derive(Default)]
struct SearchResults {
    matches: Vec<Value>,
    serialized_bytes: usize,
    bytes_searched: u64,
    files_searched: u64,
    directories_visited: u64,
    entries_visited: u64,
    binary_files_skipped: u64,
    lines_truncated: u64,
    truncated: bool,
    limit_reached: bool,
}

#[derive(Clone, Copy)]
struct SearchCheckpoint {
    matches_len: usize,
    serialized_bytes: usize,
    lines_truncated: u64,
    truncated: bool,
    limit_reached: bool,
}

impl SearchResults {
    fn record_directory(&mut self) -> bool {
        if self.directories_visited == MAX_SEARCH_DIRECTORIES {
            self.mark_limit_reached();
            return false;
        }
        self.directories_visited = self.directories_visited.saturating_add(1);
        true
    }

    fn record_entry(&mut self) -> bool {
        if self.entries_visited == MAX_SEARCH_ENTRIES {
            self.mark_limit_reached();
            return false;
        }
        self.entries_visited = self.entries_visited.saturating_add(1);
        true
    }

    fn mark_limit_reached(&mut self) {
        self.truncated = true;
        self.limit_reached = true;
    }

    fn checkpoint(&self) -> SearchCheckpoint {
        SearchCheckpoint {
            matches_len: self.matches.len(),
            serialized_bytes: self.serialized_bytes,
            lines_truncated: self.lines_truncated,
            truncated: self.truncated,
            limit_reached: self.limit_reached,
        }
    }

    fn discard_binary_file(&mut self, checkpoint: SearchCheckpoint) {
        self.matches.truncate(checkpoint.matches_len);
        self.serialized_bytes = checkpoint.serialized_bytes;
        self.lines_truncated = checkpoint.lines_truncated;
        self.truncated = checkpoint.truncated;
        self.limit_reached = checkpoint.limit_reached;
        self.binary_files_skipped = self.binary_files_skipped.saturating_add(1);
    }
}

#[derive(Default)]
struct LineState {
    visible: Vec<u8>,
    bytes_seen: usize,
    matcher_state: usize,
    matched: bool,
    truncated: bool,
}

impl LineState {
    fn feed(&mut self, bytes: &[u8], pattern: &LiteralPattern) {
        self.bytes_seen = self.bytes_seen.saturating_add(bytes.len());
        let remaining = MAX_SEARCH_LINE_BYTES.saturating_sub(self.visible.len());
        let retained = remaining.min(bytes.len());
        self.visible.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained < bytes.len();

        if !self.matched {
            for byte in bytes {
                self.matcher_state = pattern.advance(self.matcher_state, *byte);
                if self.matcher_state == pattern.needle.len() {
                    self.matched = true;
                    break;
                }
            }
        }
    }
}

struct LiteralPattern {
    needle: Vec<u8>,
    fallback: Vec<usize>,
}

impl LiteralPattern {
    fn new(needle: &[u8]) -> Self {
        let mut fallback = vec![0usize; needle.len()];
        let mut matched = 0usize;
        for index in 1..needle.len() {
            while matched > 0 && needle[index] != needle[matched] {
                matched = fallback[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            fallback[index] = matched;
        }
        Self {
            needle: needle.to_vec(),
            fallback,
        }
    }

    fn advance(&self, mut matched: usize, byte: u8) -> usize {
        while matched > 0 && byte != self.needle[matched] {
            matched = self.fallback[matched - 1];
        }
        if byte == self.needle[matched] {
            matched += 1;
        }
        matched
    }
}

#[derive(Default)]
struct Utf8Validator {
    pending: Vec<u8>,
    validation_buffer: Vec<u8>,
}

impl Utf8Validator {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        self.validation_buffer.clear();
        self.validation_buffer.extend_from_slice(&self.pending);
        self.validation_buffer.extend_from_slice(bytes);
        self.pending.clear();

        match std::str::from_utf8(&self.validation_buffer) {
            Ok(_) => true,
            Err(error) if error.error_len().is_none() => {
                self.pending
                    .extend_from_slice(&self.validation_buffer[error.valid_up_to()..]);
                true
            }
            Err(_) => false,
        }
    }

    fn finish(&self) -> bool {
        self.pending.is_empty()
    }
}

fn bounded_search_output(
    workspace: &CodingWorkspace,
    path: &str,
    query: &str,
    mut results: SearchResults,
) -> ToolOutput {
    let (metadata_query, query_truncated) =
        truncate_json_string(query, MAX_SEARCH_QUERY_METADATA_SERIALIZED_BYTES);
    loop {
        let output = ToolOutput::Success {
            content: vec![ToolContent::Json {
                value: json!({ "matches": &results.matches }),
            }],
            metadata: BTreeMap::from([
                ("path".to_string(), json!(path)),
                ("query".to_string(), json!(metadata_query)),
                ("query_bytes".to_string(), json!(query.len())),
                ("query_truncated".to_string(), json!(query_truncated)),
                ("matches".to_string(), json!(results.matches.len())),
                ("bytes_searched".to_string(), json!(results.bytes_searched)),
                ("files_searched".to_string(), json!(results.files_searched)),
                (
                    "directories_visited".to_string(),
                    json!(results.directories_visited),
                ),
                (
                    "entries_visited".to_string(),
                    json!(results.entries_visited),
                ),
                (
                    "binary_files_skipped".to_string(),
                    json!(results.binary_files_skipped),
                ),
                (
                    "lines_truncated".to_string(),
                    json!(results.lines_truncated),
                ),
                ("truncated".to_string(), json!(results.truncated)),
                ("workspace".to_string(), workspace.metadata()),
            ]),
            extensions: BTreeMap::new(),
        };
        if serde_json::to_vec(&output)
            .expect("search output JSON serializes")
            .len()
            <= MAX_OUTPUT_BYTES
        {
            return finalize_output(output);
        }
        if results.matches.pop().is_none() {
            return failure(
                "output_too_large",
                "search metadata exceeds the tool output budget",
                false,
            );
        }
        results.truncated = true;
    }
}

fn visible_utf8_prefix(bytes: &[u8]) -> Result<&str, ()> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|_| ())
        }
        Err(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchResults, MAX_SEARCH_DIRECTORIES, MAX_SEARCH_ENTRIES};

    #[test]
    fn global_directory_budget_stops_before_opening_another_directory() {
        let mut results = SearchResults {
            directories_visited: MAX_SEARCH_DIRECTORIES,
            ..SearchResults::default()
        };

        assert!(!results.record_directory());
        assert!(results.truncated);
        assert!(results.limit_reached);
    }

    #[test]
    fn global_entry_budget_stops_before_retaining_another_entry() {
        let mut results = SearchResults {
            entries_visited: MAX_SEARCH_ENTRIES,
            ..SearchResults::default()
        };

        assert!(!results.record_entry());
        assert!(results.truncated);
        assert!(results.limit_reached);
    }
}
