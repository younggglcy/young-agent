use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use young_tool_runtime::{ToolCall, ToolContent, ToolOutput};

use crate::tool_support::{
    display_relative_path, failure, finalize_output, truncate_json_string, workspace_path_failure,
    ToolArguments, MAX_OUTPUT_BYTES, MAX_TOOL_CONTENT_SERIALIZED_BYTES,
};
use crate::workspace::CodingWorkspace;

const MAX_READ_PATH_BYTES: usize = 8 * 1024;

pub(crate) fn execute(
    workspace: &CodingWorkspace,
    call: &ToolCall,
    cancellation: &AtomicBool,
) -> ToolOutput {
    if cancellation.load(Ordering::Relaxed) {
        return failure("tool_cancelled", "read_file was cancelled", false);
    }
    let arguments = match ToolArguments::parse(&call.arguments, &["path"]) {
        Ok(arguments) => arguments,
        Err(output) => return output,
    };
    let path = match arguments.required_string("path") {
        Ok(path) if path.len() <= MAX_READ_PATH_BYTES => path,
        Ok(_) => {
            return failure(
                "invalid_arguments",
                format!("argument 'path' exceeds {MAX_READ_PATH_BYTES} bytes"),
                false,
            )
        }
        Err(output) => return output,
    };
    let resolved = match workspace.resolve_existing(Path::new(path)) {
        Ok(resolved) => resolved,
        Err(error) => return workspace_path_failure(error),
    };
    let (mut file, metadata) = match workspace.open_regular_file(&resolved.relative_path) {
        Ok(opened) => opened,
        Err(source) if source.kind() == std::io::ErrorKind::InvalidInput => {
            return failure(
                "path_is_not_file",
                format!("'{}' is not a file", resolved.relative_path.display()),
                false,
            )
        }
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
    let mut bytes = Vec::with_capacity(MAX_OUTPUT_BYTES.saturating_add(4));
    if let Err(source) = file
        .by_ref()
        .take((MAX_OUTPUT_BYTES + 4) as u64)
        .read_to_end(&mut bytes)
    {
        return failure(
            "workspace_io_error",
            format!(
                "failed to read '{}': {source}",
                resolved.relative_path.display()
            ),
            source.kind() == std::io::ErrorKind::Interrupted,
        );
    }
    if cancellation.load(Ordering::Relaxed) {
        return failure("tool_cancelled", "read_file was cancelled", false);
    }

    let truncated = metadata.len() > MAX_OUTPUT_BYTES as u64 || bytes.len() > MAX_OUTPUT_BYTES;
    let visible_bytes = if truncated {
        &bytes[..bytes.len().min(MAX_OUTPUT_BYTES)]
    } else {
        &bytes
    };
    let text = match utf8_prefix(visible_bytes, truncated) {
        Ok(text) => text,
        Err(message) => return failure("file_not_utf8", message, false),
    };
    let (text, serialization_truncated) =
        truncate_json_string(text, MAX_TOOL_CONTENT_SERIALIZED_BYTES);
    let truncated = truncated || serialization_truncated;
    let relative_path = display_relative_path(&resolved.relative_path);
    let mut output_metadata = BTreeMap::from([
        ("path".to_string(), json!(relative_path)),
        ("bytes".to_string(), json!(metadata.len())),
        ("returned_bytes".to_string(), json!(text.len())),
        ("truncated".to_string(), json!(truncated)),
        ("workspace".to_string(), workspace.metadata()),
    ]);
    if truncated {
        output_metadata.insert(
            "truncation_limit_bytes".to_string(),
            json!(MAX_TOOL_CONTENT_SERIALIZED_BYTES),
        );
    }

    finalize_output(ToolOutput::Success {
        content: vec![ToolContent::Text {
            text: text.to_string(),
        }],
        metadata: output_metadata,
        extensions: BTreeMap::new(),
    })
}

fn utf8_prefix(bytes: &[u8], truncated: bool) -> Result<&str, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|nested| nested.to_string())
        }
        Err(error) => Err(format!("file content is not valid UTF-8: {error}")),
    }
}
