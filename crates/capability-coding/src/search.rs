use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    display_relative_path, failure, truncate_utf8, workspace_path_failure, ToolArguments,
    MAX_OUTPUT_BYTES,
};
use crate::workspace::CodingWorkspace;

const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_LINE_BYTES: usize = 8 * 1024;

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
        Ok(query) => query,
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
    let files = match collect_files(&resolved.absolute_path, cancellation) {
        Ok(files) => files,
        Err(output) => return output,
    };

    let mut matches = Vec::new();
    let mut output_bytes = 0usize;
    let mut bytes_searched = 0u64;
    let mut binary_files_skipped = 0u64;
    let mut lines_truncated = 0u64;
    let mut truncated = false;

    'files: for file_path in files {
        if cancellation.load(Ordering::Relaxed) {
            return failure("tool_cancelled", "search_files was cancelled", false);
        }
        let file = match File::open(&file_path) {
            Ok(file) => file,
            Err(source) => {
                return failure(
                    "workspace_io_error",
                    format!("failed to open '{}': {source}", file_path.display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            }
        };
        let relative_path = file_path
            .strip_prefix(workspace.context().root())
            .expect("walked file stays inside workspace");
        let display_path = display_relative_path(relative_path);
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut line_number = 0u64;

        loop {
            line.clear();
            let bytes_read = match reader.read_until(b'\n', &mut line) {
                Ok(bytes_read) => bytes_read,
                Err(source) => {
                    return failure(
                        "workspace_io_error",
                        format!("failed to search '{}': {source}", display_path),
                        source.kind() == std::io::ErrorKind::Interrupted,
                    )
                }
            };
            if bytes_read == 0 {
                break;
            }
            bytes_searched = bytes_searched.saturating_add(bytes_read as u64);
            line_number += 1;
            let Ok(text) = std::str::from_utf8(&line) else {
                binary_files_skipped += 1;
                break;
            };
            let text = text.strip_suffix('\n').unwrap_or(text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            if !text.contains(query) {
                continue;
            }

            let visible_text = truncate_utf8(text, MAX_SEARCH_LINE_BYTES);
            if visible_text.len() < text.len() {
                lines_truncated += 1;
                truncated = true;
            }
            let match_bytes = display_path.len().saturating_add(visible_text.len());
            if matches.len() == MAX_SEARCH_MATCHES
                || output_bytes.saturating_add(match_bytes) > MAX_OUTPUT_BYTES
            {
                truncated = true;
                break 'files;
            }
            output_bytes = output_bytes.saturating_add(match_bytes);
            matches.push(json!({
                "path": display_path,
                "line": line_number,
                "text": visible_text,
            }));
        }
    }

    ToolOutput::Success {
        content: vec![ToolContent::Json {
            value: json!({ "matches": matches }),
        }],
        metadata: BTreeMap::from([
            ("path".to_string(), json!(path)),
            ("query".to_string(), json!(query)),
            ("matches".to_string(), json!(matches.len())),
            ("bytes_searched".to_string(), json!(bytes_searched)),
            (
                "binary_files_skipped".to_string(),
                json!(binary_files_skipped),
            ),
            ("lines_truncated".to_string(), json!(lines_truncated)),
            ("truncated".to_string(), json!(truncated)),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    }
}

fn collect_files(start: &Path, cancellation: &AtomicBool) -> Result<Vec<PathBuf>, ToolOutput> {
    if cancellation.load(Ordering::Relaxed) {
        return Err(failure(
            "tool_cancelled",
            "search_files was cancelled",
            false,
        ));
    }
    let metadata = std::fs::metadata(start).map_err(|source| {
        failure(
            "workspace_io_error",
            format!("failed to inspect '{}': {source}", start.display()),
            source.kind() == std::io::ErrorKind::Interrupted,
        )
    })?;
    if metadata.is_file() {
        return Ok(vec![start.to_path_buf()]);
    }
    if !metadata.is_dir() {
        return Err(failure(
            "path_is_not_searchable",
            format!("'{}' is neither a file nor a directory", start.display()),
            false,
        ));
    }

    let mut files = Vec::new();
    let mut directories = vec![start.to_path_buf()];
    while let Some(directory) = directories.pop() {
        if cancellation.load(Ordering::Relaxed) {
            return Err(failure(
                "tool_cancelled",
                "search_files was cancelled",
                false,
            ));
        }
        let entries = std::fs::read_dir(&directory).map_err(|source| {
            failure(
                "workspace_io_error",
                format!("failed to list '{}': {source}", directory.display()),
                source.kind() == std::io::ErrorKind::Interrupted,
            )
        })?;
        let mut entries = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| failure("workspace_io_error", source.to_string(), false))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            let file_type = entry.file_type().map_err(|source| {
                failure(
                    "workspace_io_error",
                    format!("failed to inspect '{}': {source}", entry.path().display()),
                    source.kind() == std::io::ErrorKind::Interrupted,
                )
            })?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if entry.file_name() != ".git" {
                    directories.push(entry.path());
                }
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}
