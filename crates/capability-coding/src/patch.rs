use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use cap_std::fs::File;
use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolError, ToolOutput};

use crate::tool_support::{
    display_relative_path, failure, finalize_output, truncate_json_string, truncate_utf8,
    ToolArguments,
};
use crate::workspace::{
    ensure_atomic_patch_supported, AtomicCreateError, AtomicReplaceError, CodingWorkspace,
    PublishedRecovery, WorkspacePathError, MAX_FILE_SNAPSHOT_BYTES,
};

const MAX_PATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATCH_LINES: usize = 200_000;
const MAX_PATCH_FILE_BYTES: u64 = MAX_FILE_SNAPSHOT_BYTES;
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
    if let Err(error) = ensure_atomic_patch_supported() {
        return failure("unsupported_file_metadata", error.to_string(), false);
    }
    let result = match apply_unified_patch(workspace, patch, cancellation) {
        Ok(result) => result,
        Err(error) => return error.into_output(),
    };
    finalize_output(ToolOutput::Success {
        content: vec![ToolContent::Json {
            value: json!({
                "changed_files": result.changed_files,
                "recovery_files": result.recovery_files,
            }),
        }],
        metadata: BTreeMap::from([
            (
                "files_changed".to_string(),
                json!(result.changed_files.len()),
            ),
            (
                "recovery_files".to_string(),
                json!(result.recovery_files.len()),
            ),
            (
                "recovery_policy".to_string(),
                json!("ignored_by_search_and_git_until_caller_removes"),
            ),
            ("workspace".to_string(), workspace.metadata()),
        ]),
        extensions: BTreeMap::new(),
    })
}

fn apply_unified_patch(
    workspace: &CodingWorkspace,
    patch: &str,
    cancellation: &AtomicBool,
) -> Result<PatchResult, PatchError> {
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
    let mut original_snapshot = None;
    let original = if resolved.existed {
        let (file, metadata) = workspace
            .open_regular_file(&resolved.relative_path)
            .map_err(|source| PatchError::Io {
                path: resolved.relative_path.clone(),
                source,
            })?;
        if metadata.len() > MAX_PATCH_FILE_BYTES {
            return Err(PatchError::Limit(format!(
                "patch target '{}' exceeds {MAX_PATCH_FILE_BYTES} bytes",
                resolved.relative_path.display()
            )));
        }
        let (content, snapshot) = read_patch_target(file, &resolved.relative_path, cancellation)?;
        original_snapshot = Some(snapshot);
        Some(content)
    } else {
        None
    };

    let change = file_patch.prepare_change(original.as_deref(), cancellation)?;
    if cancellation.load(Ordering::Relaxed) {
        return Err(PatchError::Cancelled);
    }
    let FileChange::Write(content) = &change;
    if content.len() as u64 > MAX_PATCH_FILE_BYTES {
        return Err(PatchError::Limit(format!(
            "patch result '{}' exceeds {MAX_PATCH_FILE_BYTES} bytes",
            resolved.relative_path.display()
        )));
    }
    let recovery = match change {
        FileChange::Write(content) if resolved.existed => workspace
            .replace_existing_atomically(
                &resolved.relative_path,
                content.as_bytes(),
                original_snapshot.expect("existing patch targets have a snapshot"),
            )
            .map_err(|error| match error {
                AtomicReplaceError::BeforePublication {
                    source,
                    recovery: Some(recovery),
                } => PatchError::Preserved {
                    target: resolved.relative_path.clone(),
                    recovery,
                    source,
                },
                AtomicReplaceError::BeforePublication {
                    source,
                    recovery: None,
                } => PatchError::Io {
                    path: resolved.relative_path.clone(),
                    source,
                },
                AtomicReplaceError::Published {
                    source,
                    target,
                    recovery,
                } => PatchError::Published {
                    source,
                    target,
                    recovery,
                },
            })?
            .into_iter()
            .map(|path| display_relative_path(&path))
            .collect(),
        FileChange::Write(content) => {
            workspace
                .create_new(&resolved.relative_path, content.as_bytes())
                .map_err(|error| match error {
                    AtomicCreateError::BeforePublication {
                        source,
                        recovery: Some(recovery),
                    } => PatchError::Preserved {
                        target: resolved.relative_path.clone(),
                        recovery,
                        source,
                    },
                    AtomicCreateError::BeforePublication {
                        source,
                        recovery: None,
                    } => PatchError::Io {
                        path: resolved.relative_path.clone(),
                        source,
                    },
                    AtomicCreateError::Published { source, target } => PatchError::Published {
                        target,
                        recovery: PublishedRecovery::NotApplicableNewFile,
                        source,
                    },
                })?;
            Vec::new()
        }
    };
    Ok(PatchResult {
        changed_files: vec![display_relative_path(&resolved.relative_path)],
        recovery_files: recovery,
    })
}

struct PatchResult {
    changed_files: Vec<String>,
    recovery_files: Vec<String>,
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
            (Some(_), None, Some(_)) => Err(PatchError::InvalidPatch(
                "delete-file patches are not supported by the minimal patch tool".to_string(),
            )),
            _ => {
                let updated = apply_hunks(original.unwrap_or_default(), &self.hunks, cancellation)?;
                Ok(FileChange::Write(updated))
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
) -> Result<(String, crate::workspace::FileSnapshot), PatchError> {
    let before = CodingWorkspace::begin_file_snapshot(&file).map_err(|source| PatchError::Io {
        path: path.to_path_buf(),
        source,
    })?;
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
    let snapshot = CodingWorkspace::finish_file_snapshot_from_content(&file, before, &bytes)
        .map_err(|source| PatchError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let content = String::from_utf8(bytes).map_err(|source| PatchError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })?;
    Ok((content, snapshot))
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
    Preserved {
        target: PathBuf,
        recovery: PublishedRecovery,
        source: std::io::Error,
    },
    Published {
        target: PathBuf,
        recovery: PublishedRecovery,
        source: std::io::Error,
    },
    Cancelled,
}

impl PatchError {
    fn into_output(self) -> ToolOutput {
        match self {
            Self::Preserved {
                target,
                recovery,
                source,
            } => {
                let target = display_relative_path(&target);
                let (code, recoveries, recovery_candidates, recovery_state, recovery_policy) =
                    match recovery {
                        PublishedRecovery::LocatedVerified(path) => (
                            "patch_not_published_with_recovery",
                            vec![display_relative_path(&path)],
                            Vec::new(),
                            "located_verified",
                            "verified_search_and_git_ignored",
                        ),
                        PublishedRecovery::LocatedContentUnverified(path) => (
                            "patch_not_published_with_recovery_candidate",
                            Vec::new(),
                            vec![display_relative_path(&path)],
                            "located_content_unverified",
                            "verified_search_and_git_ignored",
                        ),
                        PublishedRecovery::LocatedPolicyUnverified(path) => (
                            "patch_not_published_with_recovery",
                            vec![display_relative_path(&path)],
                            Vec::new(),
                            "located_policy_unverified",
                            "unverified",
                        ),
                        PublishedRecovery::LocatedContentAndPolicyUnverified(path) => (
                            "patch_not_published_with_recovery_candidate",
                            Vec::new(),
                            vec![display_relative_path(&path)],
                            "located_content_and_policy_unverified",
                            "unverified",
                        ),
                        PublishedRecovery::Unlocated => (
                            "patch_not_published_recovery_unlocated",
                            Vec::new(),
                            Vec::new(),
                            "unlocated",
                            "unverified",
                        ),
                        PublishedRecovery::NotApplicableNewFile => (
                            "patch_not_published_without_recovery",
                            Vec::new(),
                            Vec::new(),
                            "not_applicable_new_file",
                            "not_applicable",
                        ),
                    };
                let message = format!(
                    "patch was not published at '{target}', and recovery evidence was recorded: {source}"
                );
                let (message, _) = truncate_json_string(&message, 8 * 1024);
                ToolOutput::Failure {
                    error: ToolError {
                        code: code.to_string(),
                        message: message.to_string(),
                        retryable: false,
                    },
                    extensions: BTreeMap::from([
                        ("publication_state".to_string(), json!("not_published")),
                        ("changed_files".to_string(), json!([])),
                        ("recovery_files".to_string(), json!(recoveries)),
                        (
                            "recovery_candidates".to_string(),
                            json!(recovery_candidates),
                        ),
                        ("recovery_state".to_string(), json!(recovery_state)),
                        ("recovery_policy".to_string(), json!(recovery_policy)),
                    ]),
                }
            }
            Self::Published {
                target,
                recovery,
                source,
            } => {
                let changed = display_relative_path(&target);
                let (
                    code,
                    publication_state,
                    recoveries,
                    recovery_candidates,
                    recovery_state,
                    recovery_policy,
                ) = match recovery {
                    PublishedRecovery::LocatedVerified(path) => (
                        "patch_published_with_recovery",
                        "published_with_recovery",
                        vec![display_relative_path(&path)],
                        Vec::new(),
                        "located_verified",
                        "verified_search_and_git_ignored",
                    ),
                    PublishedRecovery::LocatedContentUnverified(path) => (
                        "patch_published_with_recovery_candidate",
                        "published_recovery_candidate",
                        Vec::new(),
                        vec![display_relative_path(&path)],
                        "located_content_unverified",
                        "verified_search_and_git_ignored",
                    ),
                    PublishedRecovery::LocatedPolicyUnverified(path) => (
                        "patch_published_with_recovery",
                        "published_with_recovery",
                        vec![display_relative_path(&path)],
                        Vec::new(),
                        "located_policy_unverified",
                        "unverified",
                    ),
                    PublishedRecovery::LocatedContentAndPolicyUnverified(path) => (
                        "patch_published_with_recovery_candidate",
                        "published_recovery_candidate",
                        Vec::new(),
                        vec![display_relative_path(&path)],
                        "located_content_and_policy_unverified",
                        "unverified",
                    ),
                    PublishedRecovery::NotApplicableNewFile => (
                        "patch_published_without_recovery",
                        "published_without_recovery",
                        Vec::new(),
                        Vec::new(),
                        "not_applicable_new_file",
                        "not_applicable",
                    ),
                    PublishedRecovery::Unlocated => (
                        "patch_published_recovery_unlocated",
                        "published_recovery_unlocated",
                        Vec::new(),
                        Vec::new(),
                        "unlocated",
                        "unverified",
                    ),
                };
                let message = format!(
                    "patch was published at '{changed}', but commit validation failed: {source}"
                );
                let (message, _) = truncate_json_string(&message, 8 * 1024);
                ToolOutput::Failure {
                    error: ToolError {
                        code: code.to_string(),
                        message: message.to_string(),
                        retryable: false,
                    },
                    extensions: BTreeMap::from([
                        ("publication_state".to_string(), json!(publication_state)),
                        ("changed_files".to_string(), json!([changed])),
                        ("recovery_files".to_string(), json!(recoveries)),
                        (
                            "recovery_candidates".to_string(),
                            json!(recovery_candidates),
                        ),
                        ("recovery_state".to_string(), json!(recovery_state)),
                        ("recovery_policy".to_string(), json!(recovery_policy)),
                    ]),
                }
            }
            error => failure(error.code(), error.to_string(), error.retryable()),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidPatch(_) => "invalid_patch",
            Self::Limit(_) => "patch_too_large",
            Self::Conflict(_) => "patch_conflict",
            Self::Workspace(error) => error.code(),
            Self::Io { source, .. } if source.kind() == std::io::ErrorKind::Unsupported => {
                "unsupported_file_metadata"
            }
            Self::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock => {
                "patch_conflict"
            }
            Self::Io { source, .. } if source.kind() == std::io::ErrorKind::InvalidInput => {
                "patch_conflict"
            }
            Self::Io { .. } => "workspace_io_error",
            Self::Preserved {
                recovery: PublishedRecovery::Unlocated,
                ..
            } => "patch_not_published_recovery_unlocated",
            Self::Preserved {
                recovery: PublishedRecovery::NotApplicableNewFile,
                ..
            } => "patch_not_published_without_recovery",
            Self::Preserved {
                recovery:
                    PublishedRecovery::LocatedContentUnverified(_)
                    | PublishedRecovery::LocatedContentAndPolicyUnverified(_),
                ..
            } => "patch_not_published_with_recovery_candidate",
            Self::Preserved { .. } => "patch_not_published_with_recovery",
            Self::Published {
                recovery: PublishedRecovery::Unlocated,
                ..
            } => "patch_published_recovery_unlocated",
            Self::Published {
                recovery: PublishedRecovery::NotApplicableNewFile,
                ..
            } => "patch_published_without_recovery",
            Self::Published {
                recovery:
                    PublishedRecovery::LocatedContentUnverified(_)
                    | PublishedRecovery::LocatedContentAndPolicyUnverified(_),
                ..
            } => "patch_published_with_recovery_candidate",
            Self::Published { .. } => "patch_published_with_recovery",
            Self::Cancelled => "tool_cancelled",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::Workspace(error) => error.retryable(),
            Self::Io { source, .. } => source.kind() == std::io::ErrorKind::Interrupted,
            Self::InvalidPatch(_)
            | Self::Limit(_)
            | Self::Conflict(_)
            | Self::Preserved { .. }
            | Self::Published { .. }
            | Self::Cancelled => false,
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
            Self::Preserved { target, source, .. } => write!(
                formatter,
                "patch was not published at '{}', and staged data was preserved: {source}",
                target.display()
            ),
            Self::Published { target, source, .. } => write!(
                formatter,
                "patch was published at '{}', but commit validation failed: {source}",
                target.display()
            ),
            Self::Cancelled => formatter.write_str("apply_patch was cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use young_tool_runtime::ToolOutput;

    use super::{PatchError, PublishedRecovery};

    #[test]
    fn published_failure_exposes_nested_recovery_state_as_extensions() {
        let output = PatchError::Published {
            target: "src/target.txt".into(),
            recovery: PublishedRecovery::LocatedPolicyUnverified(
                "src/.young-agent-recovery/.young-agent-patch-displaced-00000000000000000000000000000000.tmp"
                    .into(),
            ),
            source: std::io::Error::other("injected policy drift"),
        }
        .into_output();

        let ToolOutput::Failure { error, extensions } = output else {
            panic!("published patch validation must fail structurally");
        };
        assert_eq!(error.code, "patch_published_with_recovery");
        assert_eq!(
            extensions["publication_state"],
            json!("published_with_recovery")
        );
        assert_eq!(extensions["changed_files"], json!(["src/target.txt"]));
        assert_eq!(
            extensions["recovery_files"],
            json!([
                "src/.young-agent-recovery/.young-agent-patch-displaced-00000000000000000000000000000000.tmp"
            ])
        );
        assert_eq!(
            extensions["recovery_state"],
            json!("located_policy_unverified")
        );
        assert_eq!(extensions["recovery_policy"], json!("unverified"));
    }

    #[test]
    fn pre_publication_failure_exposes_preserved_recovery_as_extensions() {
        let output = PatchError::Preserved {
            target: "src/target.txt".into(),
            recovery: PublishedRecovery::LocatedVerified(
                "src/.young-agent-recovery/.young-agent-patch-displaced-00000000000000000000000000000000.tmp"
                    .into(),
            ),
            source: std::io::Error::other("injected exchange failure"),
        }
        .into_output();

        let ToolOutput::Failure { error, extensions } = output else {
            panic!("pre-publication preservation must fail structurally");
        };
        assert_eq!(error.code, "patch_not_published_with_recovery");
        assert_eq!(extensions["publication_state"], json!("not_published"));
        assert_eq!(extensions["changed_files"], json!([]));
        assert_eq!(
            extensions["recovery_files"],
            json!([
                "src/.young-agent-recovery/.young-agent-patch-displaced-00000000000000000000000000000000.tmp"
            ])
        );
        assert_eq!(extensions["recovery_state"], json!("located_verified"));
        assert_eq!(
            extensions["recovery_policy"],
            json!("verified_search_and_git_ignored")
        );
    }

    #[test]
    fn published_failure_distinguishes_an_unlocated_recovery() {
        let output = PatchError::Published {
            target: "src/target.txt".into(),
            recovery: PublishedRecovery::Unlocated,
            source: std::io::Error::other("injected namespace identity loss"),
        }
        .into_output();

        let ToolOutput::Failure { error, extensions } = output else {
            panic!("published patch validation must fail structurally");
        };
        assert_eq!(error.code, "patch_published_recovery_unlocated");
        assert_eq!(
            extensions["publication_state"],
            json!("published_recovery_unlocated")
        );
        assert_eq!(extensions["recovery_files"], json!([]));
        assert_eq!(extensions["recovery_state"], json!("unlocated"));
        assert_eq!(extensions["recovery_policy"], json!("unverified"));
    }

    #[test]
    fn content_unverified_path_is_exposed_only_as_a_recovery_candidate() {
        let candidate = "src/.young-agent-recovery/.young-agent-patch-displaced-00000000000000000000000000000000.tmp";
        let output = PatchError::Published {
            target: "src/target.txt".into(),
            recovery: PublishedRecovery::LocatedContentUnverified(candidate.into()),
            source: std::io::Error::other("injected recovery inspection failure"),
        }
        .into_output();

        let ToolOutput::Failure { error, extensions } = output else {
            panic!("published patch validation must fail structurally");
        };
        assert_eq!(error.code, "patch_published_with_recovery_candidate");
        assert_eq!(
            extensions["publication_state"],
            json!("published_recovery_candidate")
        );
        assert_eq!(extensions["recovery_files"], json!([]));
        assert_eq!(extensions["recovery_candidates"], json!([candidate]));
        assert_eq!(
            extensions["recovery_state"],
            json!("located_content_unverified")
        );
    }

    #[test]
    fn published_new_file_failure_reports_that_recovery_is_not_applicable() {
        let output = PatchError::Published {
            target: "src/created.txt".into(),
            recovery: PublishedRecovery::NotApplicableNewFile,
            source: std::io::Error::other("injected post-publication validation failure"),
        }
        .into_output();

        let ToolOutput::Failure { error, extensions } = output else {
            panic!("published new-file validation must fail structurally");
        };
        assert_eq!(error.code, "patch_published_without_recovery");
        assert_eq!(
            extensions["publication_state"],
            json!("published_without_recovery")
        );
        assert_eq!(extensions["changed_files"], json!(["src/created.txt"]));
        assert_eq!(extensions["recovery_files"], json!([]));
        assert_eq!(
            extensions["recovery_state"],
            json!("not_applicable_new_file")
        );
        assert_eq!(extensions["recovery_policy"], json!("not_applicable"));
    }
}
