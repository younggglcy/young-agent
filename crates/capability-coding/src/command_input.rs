use serde_json::Value;

const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunCommandInputError {
    ArgumentsNotObject,
    CommandMissingOrNotString,
    UnknownArguments,
    CommandTooLarge,
    CommandEmpty,
}

impl RunCommandInputError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::ArgumentsNotObject => "run_command arguments must be an object",
            Self::CommandMissingOrNotString => "run_command requires a string 'command' argument",
            Self::UnknownArguments => "run_command does not accept unknown arguments",
            Self::CommandTooLarge => "command exceeds the 65536 bytes policy limit",
            Self::CommandEmpty => "command must not be empty",
        }
    }
}

pub(crate) fn parse_run_command_arguments(arguments: &Value) -> Result<&str, RunCommandInputError> {
    let Some(arguments) = arguments.as_object() else {
        return Err(RunCommandInputError::ArgumentsNotObject);
    };
    let Some(command) = arguments
        .get("command")
        .and_then(|command| command.as_str())
    else {
        return Err(RunCommandInputError::CommandMissingOrNotString);
    };
    if arguments.len() != 1 {
        return Err(RunCommandInputError::UnknownArguments);
    }
    validate_run_command_text(command)?;
    Ok(command)
}

pub(crate) fn validate_run_command_text(command: &str) -> Result<(), RunCommandInputError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(RunCommandInputError::CommandTooLarge);
    }
    if command.trim().is_empty() {
        return Err(RunCommandInputError::CommandEmpty);
    }
    Ok(())
}
