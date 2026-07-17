use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use young_agent_runtime::{
    AgentRuntime, AgentRuntimeError, RunId, RunRequest, RunStopToken, TerminalRunStatus,
};
use young_capability_coding::{
    register_builtin_coding_capability, CodingCapabilityRegistrationError, CodingWorkspace,
    CodingWorkspaceError,
};
use young_event_store::EventStoreError;
use young_model_runtime::{ModelMessage, ModelToolSpec};
use young_tool_runtime::ToolRuntime;

use crate::approval::InteractiveApprovalControl;
use crate::args::{parse_args, usage, CliOptions, ParseResult};
use crate::fake_provider;
use crate::signals::install_signal_handler;
use crate::state::EventLog;
use crate::terminal::{StreamingEventStore, TerminalOutput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliExitStatus {
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

impl CliExitStatus {
    pub const fn code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Failed => 2,
            Self::Interrupted => 130,
            Self::Cancelled => 125,
        }
    }
}

pub fn run_from_env() -> Result<CliExitStatus, CliError> {
    let stdout = io::stdout();
    let is_terminal = stdout.is_terminal();
    run(std::env::args_os().skip(1), stdout, is_terminal)
}

fn run(
    arguments: impl IntoIterator<Item = OsString>,
    writer: impl Write,
    is_terminal: bool,
) -> Result<CliExitStatus, CliError> {
    let options = match parse_args(arguments).map_err(CliError::Arguments)? {
        ParseResult::Options(options) => options,
        ParseResult::Help => {
            let output = TerminalOutput::new(writer, is_terminal);
            output.line(format_args!("{}", usage()));
            return output
                .take_error()
                .map_or(Ok(CliExitStatus::Completed), |source| {
                    Err(CliError::TerminalOutput(source))
                });
        }
    };
    run_options(options, writer, is_terminal)
}

fn run_options(
    options: CliOptions,
    writer: impl Write,
    is_terminal: bool,
) -> Result<CliExitStatus, CliError> {
    let workspace = CodingWorkspace::resolve(&options.workspace).map_err(CliError::Workspace)?;
    let run_id = new_run_id()?;
    let mut tools = ToolRuntime::default();
    register_builtin_coding_capability(&mut tools, workspace.clone())
        .map_err(CliError::RegisterCapability)?;
    let model_tools = tools
        .definitions()
        .map(|definition| ModelToolSpec {
            name: definition.name.clone(),
            description: definition.description.clone(),
            input_schema: definition.input_schema.clone(),
        })
        .collect();

    let model = fake_provider::load(&options.prompt, options.fake_script.as_deref())
        .map_err(|source| CliError::FakeProvider(Box::new(source)))?;
    let stop = RunStopToken::default();
    install_signal_handler(options.signal_action, stop.clone()).map_err(CliError::InstallSignal)?;
    let event_log = EventLog::create(options.event_log.as_deref(), &run_id)
        .map_err(|source| CliError::State(Box::new(source)))?;

    let output = TerminalOutput::new(writer, is_terminal);
    output.line(format_args!("[event-log] {}", event_log.path().display()));
    if let Some(source) = output.take_error() {
        return Err(CliError::TerminalOutput(source));
    }

    let sink = StreamingEventStore::new(event_log.into_store(), output.clone(), stop.clone());
    let mut runtime = AgentRuntime::new(model, tools, sink);
    let mut control = InteractiveApprovalControl::from_stdin(output.clone());
    let outcome = runtime
        .run_with_control_and_stop(
            RunRequest {
                run_id,
                model: "fake".to_string(),
                messages: vec![ModelMessage::user(options.prompt)],
                tools: model_tools,
                metadata: BTreeMap::from([(
                    "workspace".to_string(),
                    json!(workspace.context().root()),
                )]),
            },
            &mut control,
            &stop,
        )
        .map_err(|source| CliError::Runtime(Box::new(source)))?;

    if let Some(source) = output.take_error() {
        return Err(CliError::TerminalOutput(source));
    }

    Ok(match outcome.status() {
        TerminalRunStatus::Completed { .. } => CliExitStatus::Completed,
        TerminalRunStatus::Failed { .. } => CliExitStatus::Failed,
        TerminalRunStatus::Interrupted { .. } => CliExitStatus::Interrupted,
        TerminalRunStatus::Cancelled { .. } => CliExitStatus::Cancelled,
    })
}

fn new_run_id() -> Result<RunId, CliError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(CliError::SystemClock)?
        .as_nanos();
    Ok(RunId::new(format!("run-{}-{nonce}", std::process::id())))
}

#[derive(Debug)]
pub enum CliError {
    Arguments(String),
    Workspace(CodingWorkspaceError),
    FakeProvider(Box<dyn Error + Send + Sync>),
    State(Box<dyn Error + Send + Sync>),
    InstallSignal(ctrlc::Error),
    RegisterCapability(CodingCapabilityRegistrationError),
    Runtime(Box<AgentRuntimeError<EventStoreError>>),
    SystemClock(std::time::SystemTimeError),
    TerminalOutput(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(message) => formatter.write_str(message),
            Self::Workspace(source) => write!(formatter, "failed to open workspace: {source}"),
            Self::FakeProvider(source) => write!(formatter, "{source}"),
            Self::State(source) => write!(formatter, "{source}"),
            Self::InstallSignal(source) => {
                write!(
                    formatter,
                    "failed to install process signal handler: {source}"
                )
            }
            Self::RegisterCapability(source) => write!(formatter, "{source}"),
            Self::Runtime(source) => write!(formatter, "Agent Run failed: {source}"),
            Self::SystemClock(source) => write!(formatter, "system clock is invalid: {source}"),
            Self::TerminalOutput(source) => write!(formatter, "terminal output failed: {source}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Arguments(_) => None,
            Self::TerminalOutput(source) => Some(source),
            Self::FakeProvider(source) | Self::State(source) => Some(source.as_ref()),
            Self::InstallSignal(source) => Some(source),
            Self::Workspace(source) => Some(source),
            Self::RegisterCapability(source) => Some(source),
            Self::Runtime(source) => Some(source.as_ref()),
            Self::SystemClock(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use young_capability_coding::{register_builtin_coding_capability, CodingWorkspace};
    use young_tool_runtime::ToolRuntime;

    use super::{run, CliError, CliExitStatus};

    #[test]
    fn exit_statuses_keep_their_process_contract() {
        assert_eq!(CliExitStatus::Completed.code(), 0);
        assert_eq!(CliExitStatus::Failed.code(), 2);
        assert_eq!(CliExitStatus::Interrupted.code(), 130);
        assert_eq!(CliExitStatus::Cancelled.code(), 125);
    }

    #[test]
    fn help_completes_without_initializing_a_run() {
        assert_eq!(
            run([OsString::from("--help")], Vec::new(), false)
                .expect("help should render successfully"),
            CliExitStatus::Completed
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("stdout closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("stdout closed"))
        }
    }

    #[test]
    fn help_surfaces_terminal_write_failures() {
        let error = run([OsString::from("--help")], FailingWriter, false)
            .expect_err("terminal failure should be returned");
        assert!(matches!(error, CliError::TerminalOutput(_)));
        assert!(format!("{error}").contains("terminal output failed: stdout closed"));
        assert!(error.source().is_some());
    }

    #[test]
    fn cli_error_wrappers_preserve_context_and_sources() {
        let arguments = CliError::Arguments("bad arguments".to_string());
        assert_eq!(arguments.to_string(), "bad arguments");
        assert!(arguments.source().is_none());

        let provider = CliError::FakeProvider(Box::new(io::Error::other("provider failed")));
        assert_eq!(provider.to_string(), "provider failed");
        assert!(provider.source().is_some());

        let state = CliError::State(Box::new(io::Error::other("state failed")));
        assert_eq!(state.to_string(), "state failed");
        assert!(state.source().is_some());

        let terminal = CliError::TerminalOutput(io::Error::other("terminal failed"));
        assert_eq!(
            terminal.to_string(),
            "terminal output failed: terminal failed"
        );
        assert!(terminal.source().is_some());
    }

    fn unique_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "young-agent-app-unit-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn workspace_and_registration_errors_keep_their_typed_sources() {
        let missing = unique_path("missing");
        let workspace_error = match CodingWorkspace::resolve(&missing) {
            Ok(_) => panic!("missing workspace should be rejected"),
            Err(error) => error,
        };
        let workspace_error = CliError::Workspace(workspace_error);
        assert!(workspace_error
            .to_string()
            .contains("failed to open workspace"));
        assert!(workspace_error.source().is_some());

        let root = unique_path("duplicate-registration");
        fs::create_dir(&root).expect("workspace should be created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace should resolve");
        let mut tools = ToolRuntime::default();
        register_builtin_coding_capability(&mut tools, workspace.clone())
            .expect("first capability registration should succeed");
        let registration_error = register_builtin_coding_capability(&mut tools, workspace)
            .expect_err("duplicate capability registration should fail");
        let registration_error = CliError::RegisterCapability(registration_error);
        assert!(!registration_error.to_string().is_empty());
        assert!(registration_error.source().is_some());
        drop(tools);
        fs::remove_dir_all(&root).expect("workspace should be removed");
    }
}
