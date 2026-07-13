use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::{Dir, File, ReadDir};
use serde_json::{json, Value};
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    display_relative_path, failure, workspace_path_failure, ToolArguments,
    MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::CodingWorkspace;

const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_LINE_BYTES: usize = 8 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 8 * 1024;

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
            let file = match workspace.open_file(&resolved.relative_path) {
                Ok(file) => file,
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

    ToolOutput::Success {
        content: vec![ToolContent::Json {
            value: json!({ "matches": results.matches }),
        }],
        metadata: BTreeMap::from([
            ("path".to_string(), json!(path)),
            ("query".to_string(), json!(query)),
            ("matches".to_string(), json!(results.matches.len())),
            ("bytes_searched".to_string(), json!(results.bytes_searched)),
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
    }
}

struct DirectoryFrame {
    relative_path: PathBuf,
    entries: ReadDir,
}

fn search_directory(
    root: Dir,
    relative_path: PathBuf,
    pattern: &LiteralPattern,
    cancellation: &AtomicBool,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    let entries = root.entries().map_err(|source| {
        failure(
            "workspace_io_error",
            format!("failed to list '{}': {source}", relative_path.display()),
            source.kind() == std::io::ErrorKind::Interrupted,
        )
    })?;
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
        let entry = entry.map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to read a directory entry: {source}"),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
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
        let entry_path = frame.relative_path.join(&file_name);
        if file_type.is_dir() {
            if file_name == ".git" {
                continue;
            }
            let directory = entry.open_dir().map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to open '{}': {source}", entry_path.display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            let entries = directory.entries().map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to list '{}': {source}", entry_path.display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            stack.push(DirectoryFrame {
                relative_path: entry_path,
                entries,
            });
        } else if file_type.is_file() {
            let file = entry.open().map_err(|source| {
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

fn search_file(
    mut file: File,
    display_path: &str,
    pattern: &LiteralPattern,
    cancellation: &AtomicBool,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    let mut buffer = [0u8; 8 * 1024];
    let mut line = LineState::default();
    let mut line_number = 1u64;

    loop {
        if cancellation.load(Ordering::Relaxed) {
            return Err(failure(
                "tool_cancelled",
                "search_files was cancelled",
                false,
            ));
        }
        let bytes_read = file.read(&mut buffer).map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to search '{display_path}': {source}"),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
        if bytes_read == 0 {
            if line.bytes_seen > 0 {
                finish_line(display_path, line_number, &line, results)?;
            }
            return Ok(());
        }
        results.bytes_searched = results.bytes_searched.saturating_add(bytes_read as u64);

        let mut start = 0usize;
        for newline in buffer[..bytes_read]
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
        {
            line.feed(&buffer[start..newline], pattern);
            finish_line(display_path, line_number, &line, results)?;
            if results.limit_reached {
                return Ok(());
            }
            line = LineState::default();
            line_number += 1;
            start = newline + 1;
        }
        line.feed(&buffer[start..bytes_read], pattern);
    }
}

fn finish_line(
    display_path: &str,
    line_number: u64,
    line: &LineState,
    results: &mut SearchResults,
) -> Result<(), ToolOutput> {
    if !line.matched {
        return Ok(());
    }
    let text = match visible_utf8_prefix(&line.visible) {
        Ok(text) => text.strip_suffix('\r').unwrap_or(text),
        Err(()) => {
            results.binary_files_skipped += 1;
            return Ok(());
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
    binary_files_skipped: u64,
    lines_truncated: u64,
    truncated: bool,
    limit_reached: bool,
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

fn visible_utf8_prefix(bytes: &[u8]) -> Result<&str, ()> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|_| ())
        }
        Err(_) => Err(()),
    }
}
