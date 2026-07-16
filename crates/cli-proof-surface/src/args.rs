use std::ffi::OsString;
use std::path::PathBuf;

const USAGE: &str = "Usage: young-agent --fake --prompt <PROMPT> --workspace <PATH> [--event-log <PATH>] [--fake-script <PATH>] [--on-signal <interrupt|cancel>]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SignalAction {
    #[default]
    Interrupt,
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CliOptions {
    pub(crate) prompt: String,
    pub(crate) workspace: PathBuf,
    pub(crate) event_log: Option<PathBuf>,
    pub(crate) fake_script: Option<PathBuf>,
    pub(crate) signal_action: SignalAction,
}

pub(crate) enum ParseResult {
    Options(CliOptions),
    Help,
}

pub(crate) fn parse_args(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<ParseResult, String> {
    let mut arguments = arguments.into_iter();
    let mut fake = false;
    let mut prompt = None;
    let mut workspace = None;
    let mut event_log = None;
    let mut fake_script = None;
    let mut signal_action = SignalAction::default();

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--fake") => fake = true,
            Some("--prompt") => {
                prompt = Some(next_utf8_value(&mut arguments, "--prompt")?);
            }
            Some("--workspace") => {
                workspace = Some(PathBuf::from(next_value(&mut arguments, "--workspace")?));
            }
            Some("--event-log") => {
                event_log = Some(PathBuf::from(next_value(&mut arguments, "--event-log")?));
            }
            Some("--fake-script") => {
                fake_script = Some(PathBuf::from(next_value(&mut arguments, "--fake-script")?));
            }
            Some("--on-signal") => {
                signal_action = match next_utf8_value(&mut arguments, "--on-signal")?.as_str() {
                    "interrupt" => SignalAction::Interrupt,
                    "cancel" => SignalAction::Cancel,
                    value => {
                        return Err(format!(
                            "--on-signal must be 'interrupt' or 'cancel', got '{value}'\n{USAGE}"
                        ));
                    }
                };
            }
            Some("--help" | "-h") => return Ok(ParseResult::Help),
            Some(flag) if flag.starts_with('-') => {
                return Err(format!("unknown option '{flag}'\n{USAGE}"));
            }
            Some(value) => return Err(format!("unexpected argument '{value}'\n{USAGE}")),
            None => return Err(format!("arguments must be valid UTF-8 options\n{USAGE}")),
        }
    }

    if !fake {
        return Err(format!(
            "--fake is required until a real provider adapter is available\n{USAGE}"
        ));
    }
    let prompt = prompt.ok_or_else(|| format!("--prompt is required\n{USAGE}"))?;
    if prompt.trim().is_empty() {
        return Err(format!("--prompt must not be empty\n{USAGE}"));
    }
    let workspace = workspace.ok_or_else(|| format!("--workspace is required\n{USAGE}"))?;

    Ok(ParseResult::Options(CliOptions {
        prompt,
        workspace,
        event_log,
        fake_script,
        signal_action,
    }))
}

pub(crate) fn usage() -> &'static str {
    USAGE
}

fn next_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<OsString, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value\n{USAGE}"))
}

fn next_utf8_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<String, String> {
    next_value(arguments, option)?
        .into_string()
        .map_err(|_| format!("{option} must be valid UTF-8\n{USAGE}"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{parse_args, CliOptions, ParseResult, SignalAction};

    #[test]
    fn parses_the_minimal_fake_provider_invocation() {
        let parsed = parse_args([
            OsString::from("--fake"),
            OsString::from("--prompt"),
            OsString::from("inspect"),
            OsString::from("--workspace"),
            OsString::from("/tmp/workspace"),
        ])
        .expect("arguments should parse");

        assert!(matches!(
            parsed,
            ParseResult::Options(CliOptions {
                prompt,
                workspace,
                event_log: None,
                fake_script: None,
                signal_action: SignalAction::Interrupt,
            }) if prompt == "inspect" && workspace.as_path() == Path::new("/tmp/workspace")
        ));
    }
}
