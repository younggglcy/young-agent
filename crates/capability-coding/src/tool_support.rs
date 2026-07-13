use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};
use young_tool_runtime::{ToolError, ToolOutput};

use crate::workspace::WorkspacePathError;

pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TOOL_CONTENT_SERIALIZED_BYTES: usize = 48 * 1024;
const MAX_ERROR_MESSAGE_SERIALIZED_BYTES: usize = 8 * 1024;

pub(crate) struct ToolArguments<'a> {
    values: &'a Map<String, Value>,
}

impl<'a> ToolArguments<'a> {
    pub(crate) fn parse(arguments: &'a Value, allowed: &[&str]) -> Result<Self, ToolOutput> {
        let Some(values) = arguments.as_object() else {
            return Err(failure(
                "invalid_arguments",
                "tool arguments must be a JSON object",
                false,
            ));
        };
        if let Some(unknown) = values.keys().find(|name| !allowed.contains(&name.as_str())) {
            return Err(failure(
                "invalid_arguments",
                format!("unknown argument '{unknown}'"),
                false,
            ));
        }
        Ok(Self { values })
    }

    pub(crate) fn required_string(&self, name: &str) -> Result<&'a str, ToolOutput> {
        match self.values.get(name).and_then(Value::as_str) {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(failure(
                "invalid_arguments",
                format!("argument '{name}' must be a non-empty string"),
                false,
            )),
        }
    }

    pub(crate) fn optional_string(&self, name: &str) -> Result<Option<&'a str>, ToolOutput> {
        match self.values.get(name) {
            None => Ok(None),
            Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
            Some(_) => Err(failure(
                "invalid_arguments",
                format!("argument '{name}' must be a non-empty string when provided"),
                false,
            )),
        }
    }
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

pub(crate) fn truncate_json_string(value: &str, max_serialized_bytes: usize) -> (&str, bool) {
    let mut serialized_bytes = 2usize;
    let mut boundary = 0usize;
    for (index, character) in value.char_indices() {
        let character_bytes = match character {
            '"' | '\\' | '\u{0008}' | '\u{0009}' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        if serialized_bytes.saturating_add(character_bytes) > max_serialized_bytes {
            return (&value[..boundary], true);
        }
        serialized_bytes = serialized_bytes.saturating_add(character_bytes);
        boundary = index + character.len_utf8();
    }
    (value, false)
}

pub(crate) fn display_relative_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.display().to_string()
    }
}

pub(crate) fn workspace_path_failure(error: WorkspacePathError) -> ToolOutput {
    failure(error.code(), error.to_string(), error.retryable())
}

pub(crate) fn failure(code: &str, message: impl Into<String>, retryable: bool) -> ToolOutput {
    let message = message.into();
    let (message, _) = truncate_json_string(&message, MAX_ERROR_MESSAGE_SERIALIZED_BYTES);
    ToolOutput::Failure {
        error: ToolError {
            code: code.to_string(),
            message: message.to_string(),
            retryable,
        },
        extensions: BTreeMap::new(),
    }
}
