use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::File;
use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{display_relative_path, failure, truncate_utf8, ToolArguments};
use crate::workspace::{CodingWorkspace, WorkspacePathError};

const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATCH_LINES: usize = 200_000;
const MAX_PATCH_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PATCH_FILE_LINES: usize = 1_000_000;
const MAX_CONFLICT_LINE_BYTES: usize = 1024;

pub(crate) fn execute(
    workspace: &CodingWorkspace,
    call: &ToolCall,
    cancellation: &AtomicBool,
) -> ToolOutput {
    let arguments = match ToolArguments::parse(&call.arguments, &["patch"]) {
        Ok(arguments) => arguments,
        Err(output) => return output,
    };
    let patch = match arguments.required_string("patch") {
        Ok(patch) if patch.len() <= MAX_PATCH_BYTES => patch,
        Ok(_) => {
            return failure(
                "patch_too_large",
                format!("patch exceeds {MAX_PATCH_BYTES} bytes"),
                false,
            )
        }
        Err(output) => return output,
    };
    let changed_files = match apply_unified_patch(workspace, patch, cancellation) {
        Ok(changed_files) => changed_files,
        Err(error) => return failure(error.code(), error.to_string(), error.retryable()),
    };
    ToolOutput::Success {
        content: vec![ToolContent::Json {
            value: json!({ "changed_files": changed_files }),
        }],
        metadata: BTreeMap::from([
            ("files_changed".to_string(), json!(changed_files.len())),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    }
}

fn apply_unified_patch(
    workspace: &CodingWorkspace,
    patch: &str,
    cancellation: &AtomicBool,
) -> Result<Vec<String>, PatchError> {
    let mut file_patches = parse_patch(patch, cancellation)?;
    if file_patches.len() != 1 {
        return Err(PatchError::InvalidPatch(
            "the minimal patch tool accepts exactly one file patch".to_string(),
        ));
    }
    if cancellation.load(Ordering::Relaxed) {
        return Err(PatchError::Cancelled);
    }
    let file_patch = file_patches.pop().expect("exactly one patch was checked");
    let path = file_patch.target_path()?;
    let resolved = workspace
        .resolve_for_write(path)
        .map_err(PatchError::Workspace)?;
    let original = if resolved.existed {
        let file = workspace
            .open_file(&resolved.relative_path)
            .map_err(|source| PatchError::Io {
                path: resolved.relative_path.clone(),
                source,
            })?;
        let metadata = file.metadata().map_err(|source| PatchError::Io {
            path: resolved.relative_path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(PatchError::Conflict(format!(
                "'{}' is not a file",
                resolved.relative_path.display()
            )));
        }
        if metadata.len() > MAX_PATCH_FILE_BYTES {
            return Err(PatchError::Limit(format!(
                "patch target '{}' exceeds {MAX_PATCH_FILE_BYTES} bytes",
                resolved.relative_path.display()
            )));
        }
        Some(read_patch_target(
            file,
            &resolved.relative_path,
            cancellation,
        )?)
    } else {
        None
    };

    let change = file_patch.prepare_change(original.as_deref(), cancellation)?;
    if cancellation.load(Ordering::Relaxed) {
        return Err(PatchError::Cancelled);
    }
    match change {
        FileChange::Write(content) if resolved.existed => workspace
            .replace_existing_atomically(&resolved.relative_path, content.as_bytes())
            .map_err(|source| PatchError::Io {
                path: resolved.relative_path.clone(),
                source,
            })?,
        FileChange::Write(content) => {
            if let Err(source) = workspace.create_new(&resolved.relative_path, content.as_bytes()) {
                let _ = workspace.remove_file(&resolved.relative_path);
                return Err(PatchError::Io {
                    path: resolved.relative_path.clone(),
                    source,
                });
            }
        }
        FileChange::Delete => workspace
            .remove_file(&resolved.relative_path)
            .map_err(|source| PatchError::Io {
                path: resolved.relative_path.clone(),
                source,
            })?,
    }
    Ok(vec![display_relative_path(&resolved.relative_path)])
}

#[derive(Debug)]
struct FilePatch {
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    hunks: Vec<Hunk>,
}

impl FilePatch {
    fn target_path(&self) -> Result<&Path, PatchError> {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old == new => Ok(new),
            (None, Some(new)) => Ok(new),
            (Some(old), None) => Ok(old),
            (Some(old), Some(new)) => Err(PatchError::InvalidPatch(format!(
                "renaming '{}' to '{}' is not supported by the minimal patch tool",
                old.display(),
                new.display()
            ))),
            (None, None) => Err(PatchError::InvalidPatch(
                "a file patch cannot use /dev/null for both paths".to_string(),
            )),
        }
    }

    fn prepare_change(
        &self,
        original: Option<&str>,
        cancellation: &AtomicBool,
    ) -> Result<FileChange, PatchError> {
        match (&self.old_path, &self.new_path, original) {
            (None, Some(_), Some(_)) => Err(PatchError::Conflict(
                "new-file patch target already exists".to_string(),
            )),
            (Some(_), _, None) => Err(PatchError::Conflict(
                "patch source file does not exist".to_string(),
            )),
            _ => {
                let updated = apply_hunks(original.unwrap_or_default(), &self.hunks, cancellation)?;
                if self.new_path.is_none() {
                    if !updated.is_empty() {
                        return Err(PatchError::Conflict(
                            "delete-file patch did not remove all content".to_string(),
                        ));
                    }
                    Ok(FileChange::Delete)
                } else {
                    Ok(FileChange::Write(updated))
                }
            }
        }
    }
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_count: usize,
    lines: Vec<PatchLine>,
}

#[derive(Debug)]
enum PatchLine {
    Context(String),
    Remove(String),
    Add(String),
}

enum FileChange {
    Write(String),
    Delete,
}

fn parse_patch(patch: &str, cancellation: &AtomicBool) -> Result<Vec<FilePatch>, PatchError> {
    if patch.is_empty() {
        return Err(PatchError::InvalidPatch(
            "patch must not be empty".to_string(),
        ));
    }
    let mut lines = Vec::new();
    for (index, line) in patch.split_inclusive('\n').enumerate() {
        if index % 1024 == 0 && cancellation.load(Ordering::Relaxed) {
            return Err(PatchError::Cancelled);
        }
        if lines.len() == MAX_PATCH_LINES {
            return Err(PatchError::Limit(format!(
                "patch exceeds {MAX_PATCH_LINES} lines"
            )));
        }
        lines.push(line);
    }
    let mut index = 0usize;
    let mut files = Vec::new();

    while index < lines.len() {
        if cancellation.load(Ordering::Relaxed) {
            return Err(PatchError::Cancelled);
        }
        while index < lines.len() && !lines[index].starts_with("--- ") {
            index += 1;
        }
        if index == lines.len() {
            break;
        }
        let old_path = parse_header_path(lines[index], "--- ")?;
        index += 1;
        let new_header = lines
            .get(index)
            .ok_or_else(|| PatchError::InvalidPatch("missing +++ file header".to_string()))?;
        let new_path = parse_header_path(new_header, "+++ ")?;
        index += 1;

        let mut hunks = Vec::new();
        while index < lines.len() && lines[index].starts_with("@@") {
            let (old_start, old_count, new_count) = parse_hunk_header(lines[index])?;
            index += 1;
            let mut hunk_lines = Vec::new();
            let mut old_seen = 0usize;
            let mut new_seen = 0usize;
            while old_seen < old_count || new_seen < new_count {
                if cancellation.load(Ordering::Relaxed) {
                    return Err(PatchError::Cancelled);
                }
                let line = lines.get(index).ok_or_else(|| {
                    PatchError::InvalidPatch("hunk ended before its declared counts".to_string())
                })?;
                if line.starts_with("\\ No newline at end of file") {
                    strip_last_line_ending(&mut hunk_lines)?;
                    index += 1;
                    continue;
                }
                index += 1;
                let (prefix, content) = line.split_at(1);
                match prefix {
                    " " => {
                        old_seen += 1;
                        new_seen += 1;
                        hunk_lines.push(PatchLine::Context(content.to_string()));
                    }
                    "-" => {
                        old_seen += 1;
                        hunk_lines.push(PatchLine::Remove(content.to_string()));
                    }
                    "+" => {
                        new_seen += 1;
                        hunk_lines.push(PatchLine::Add(content.to_string()));
                    }
                    _ => {
                        return Err(PatchError::InvalidPatch(format!(
                            "invalid hunk line {line:?}"
                        )))
                    }
                }
                if old_seen > old_count || new_seen > new_count {
                    return Err(PatchError::InvalidPatch(
                        "hunk content exceeds its declared counts".to_string(),
                    ));
                }
            }
            if lines
                .get(index)
                .is_some_and(|line| line.starts_with("\\ No newline at end of file"))
            {
                strip_last_line_ending(&mut hunk_lines)?;
                index += 1;
            }
            hunks.push(Hunk {
                old_start,
                old_count,
                new_count,
                lines: hunk_lines,
            });
        }
        if hunks.is_empty() {
            return Err(PatchError::InvalidPatch(
                "file patch must contain at least one hunk".to_string(),
            ));
        }
        if let Some(unaccounted) = lines.get(index) {
            if !unaccounted.starts_with("--- ") && !is_diff_metadata(unaccounted) {
                return Err(PatchError::InvalidPatch(format!(
                    "unaccounted content after hunk: {unaccounted:?}"
                )));
            }
        }
        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }

    if files.is_empty() {
        return Err(PatchError::InvalidPatch(
            "patch does not contain a unified diff file header".to_string(),
        ));
    }
    Ok(files)
}

fn is_diff_metadata(line: &str) -> bool {
    [
        "diff --git ",
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn parse_header_path(line: &str, prefix: &str) -> Result<Option<PathBuf>, PatchError> {
    let raw = line
        .strip_prefix(prefix)
        .ok_or_else(|| PatchError::InvalidPatch(format!("expected {prefix:?} file header")))?
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .next()
        .expect("split always yields one field");
    if raw == "/dev/null" {
        return Ok(None);
    }
    let raw = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    if raw.is_empty() {
        return Err(PatchError::InvalidPatch(
            "patch file path must not be empty".to_string(),
        ));
    }
    Ok(Some(PathBuf::from(raw)))
}

fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize), PatchError> {
    let line = line.trim_end_matches(['\r', '\n']);
    let body = line
        .strip_prefix("@@")
        .and_then(|body| body.split_once("@@").map(|(ranges, _)| ranges.trim()))
        .ok_or_else(|| PatchError::InvalidPatch(format!("invalid hunk header {line:?}")))?;
    let mut ranges = body.split_whitespace();
    let old = ranges.next().ok_or_else(|| {
        PatchError::InvalidPatch(format!("missing old range in hunk header {line:?}"))
    })?;
    let new = ranges.next().ok_or_else(|| {
        PatchError::InvalidPatch(format!("missing new range in hunk header {line:?}"))
    })?;
    if ranges.next().is_some() || !old.starts_with('-') || !new.starts_with('+') {
        return Err(PatchError::InvalidPatch(format!(
            "invalid ranges in hunk header {line:?}"
        )));
    }
    let (old_start, old_count) = parse_range(&old[1..])?;
    let (_, new_count) = parse_range(&new[1..])?;
    Ok((old_start, old_count, new_count))
}

fn parse_range(range: &str) -> Result<(usize, usize), PatchError> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, count),
        None => (range, "1"),
    };
    let start = start
        .parse::<usize>()
        .map_err(|_| PatchError::InvalidPatch(format!("invalid hunk range start {start:?}")))?;
    let count = count
        .parse::<usize>()
        .map_err(|_| PatchError::InvalidPatch(format!("invalid hunk range count {count:?}")))?;
    Ok((start, count))
}

fn strip_last_line_ending(lines: &mut [PatchLine]) -> Result<(), PatchError> {
    let line = lines.last_mut().ok_or_else(|| {
        PatchError::InvalidPatch("newline marker has no preceding hunk line".to_string())
    })?;
    let content = match line {
        PatchLine::Context(content) | PatchLine::Remove(content) | PatchLine::Add(content) => {
            content
        }
    };
    if content.ends_with('\n') {
        content.pop();
        if content.ends_with('\r') {
            content.pop();
        }
    }
    Ok(())
}

fn apply_hunks(
    original: &str,
    hunks: &[Hunk],
    cancellation: &AtomicBool,
) -> Result<String, PatchError> {
    let original_lines = original.split_inclusive('\n').collect::<Vec<_>>();
    let mut output = String::with_capacity(original.len());
    let mut cursor = 0usize;

    for hunk in hunks {
        if cancellation.load(Ordering::Relaxed) {
            return Err(PatchError::Cancelled);
        }
        let start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };
        if start < cursor || start > original_lines.len() {
            return Err(PatchError::Conflict(format!(
                "hunk starting at old line {} is out of order or outside the file",
                hunk.old_start
            )));
        }
        for (index, line) in original_lines[cursor..start].iter().enumerate() {
            if index % 1024 == 0 && cancellation.load(Ordering::Relaxed) {
                return Err(PatchError::Cancelled);
            }
            output.push_str(line);
        }
        cursor = start;
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;
        for line in &hunk.lines {
            if cancellation.load(Ordering::Relaxed) {
                return Err(PatchError::Cancelled);
            }
            match line {
                PatchLine::Context(expected) => {
                    require_original_line(&original_lines, cursor, expected)?;
                    output.push_str(expected);
                    cursor += 1;
                    old_seen += 1;
                    new_seen += 1;
                }
                PatchLine::Remove(expected) => {
                    require_original_line(&original_lines, cursor, expected)?;
                    cursor += 1;
                    old_seen += 1;
                }
                PatchLine::Add(added) => {
                    output.push_str(added);
                    new_seen += 1;
                }
            }
        }
        if old_seen != hunk.old_count || new_seen != hunk.new_count {
            return Err(PatchError::InvalidPatch(
                "parsed hunk counts do not match its header".to_string(),
            ));
        }
    }
    for (index, line) in original_lines[cursor..].iter().enumerate() {
        if index % 1024 == 0 && cancellation.load(Ordering::Relaxed) {
            return Err(PatchError::Cancelled);
        }
        output.push_str(line);
    }
    Ok(output)
}

fn read_patch_target(
    mut file: File,
    path: &Path,
    cancellation: &AtomicBool,
) -> Result<String, PatchError> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    let mut newline_count = 0usize;

    loop {
        if cancellation.load(Ordering::Relaxed) {
            return Err(PatchError::Cancelled);
        }
        let bytes_read = file.read(&mut buffer).map_err(|source| PatchError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if bytes_read == 0 {
            break;
        }
        if bytes.len().saturating_add(bytes_read) > MAX_PATCH_FILE_BYTES as usize {
            return Err(PatchError::Limit(format!(
                "patch target '{}' exceeds {MAX_PATCH_FILE_BYTES} bytes",
                path.display()
            )));
        }
        newline_count = newline_count.saturating_add(
            buffer[..bytes_read]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
        );
        if newline_count > MAX_PATCH_FILE_LINES {
            return Err(PatchError::Limit(format!(
                "patch target '{}' exceeds {MAX_PATCH_FILE_LINES} lines",
                path.display()
            )));
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let lines = newline_count + usize::from(bytes.last().is_some_and(|byte| *byte != b'\n'));
    if lines > MAX_PATCH_FILE_LINES {
        return Err(PatchError::Limit(format!(
            "patch target '{}' exceeds {MAX_PATCH_FILE_LINES} lines",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|source| PatchError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })
}

fn require_original_line(
    original_lines: &[&str],
    index: usize,
    expected: &str,
) -> Result<(), PatchError> {
    match original_lines.get(index) {
        Some(actual) if *actual == expected => Ok(()),
        Some(actual) => Err(PatchError::Conflict(format!(
            "patch context mismatch at old line {}: expected {:?}, found {:?}",
            index + 1,
            truncate_utf8(expected, MAX_CONFLICT_LINE_BYTES),
            truncate_utf8(actual, MAX_CONFLICT_LINE_BYTES)
        ))),
        None => Err(PatchError::Conflict(format!(
            "patch expects old line {}, but the file ended",
            index + 1
        ))),
    }
}

#[derive(Debug)]
pub(crate) enum PatchError {
    InvalidPatch(String),
    Limit(String),
    Conflict(String),
    Workspace(WorkspacePathError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Cancelled,
}

impl PatchError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidPatch(_) => "invalid_patch",
            Self::Limit(_) => "patch_too_large",
            Self::Conflict(_) => "patch_conflict",
            Self::Workspace(error) => error.code(),
            Self::Io { .. } => "workspace_io_error",
            Self::Cancelled => "tool_cancelled",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::Workspace(error) => error.retryable(),
            Self::Io { source, .. } => source.kind() == std::io::ErrorKind::Interrupted,
            Self::InvalidPatch(_) | Self::Limit(_) | Self::Conflict(_) | Self::Cancelled => false,
        }
    }
}

impl fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPatch(message) => write!(formatter, "invalid patch: {message}"),
            Self::Limit(message) => write!(formatter, "patch limit exceeded: {message}"),
            Self::Conflict(message) => write!(formatter, "patch conflict: {message}"),
            Self::Workspace(error) => error.fmt(formatter),
            Self::Io { path, source } => {
                write!(formatter, "failed to update '{}': {source}", path.display())
            }
            Self::Cancelled => formatter.write_str("apply_patch was cancelled"),
        }
    }
}
