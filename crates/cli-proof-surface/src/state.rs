use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use young_agent_runtime::RunId;
use young_event_store::{EventStoreError, JsonlEventStore};

pub(crate) struct EventLog {
    path: PathBuf,
    store: JsonlEventStore,
}

impl EventLog {
    pub(crate) fn create(
        requested_path: Option<&Path>,
        run_id: &RunId,
    ) -> Result<Self, StateError> {
        let location = location(requested_path, run_id)?;
        let parent = location
            .path
            .parent()
            .ok_or_else(|| StateError::InvalidEventLogPath {
                path: location.path.clone(),
            })?;
        if let Some(state_root) = &location.state_root {
            ensure_private_directory(state_root)?;
            ensure_private_directory(parent)?;
        } else {
            fs::create_dir_all(parent).map_err(|source| StateError::CreateEventLogDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let store = JsonlEventStore::create_new(&location.path)
            .map_err(|source| StateError::CreateEventLog(Box::new(source)))?;
        Ok(Self {
            path: location.path,
            store,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn into_store(self) -> JsonlEventStore {
        self.store
    }
}

struct EventLogLocation {
    path: PathBuf,
    state_root: Option<PathBuf>,
}

fn location(requested_path: Option<&Path>, run_id: &RunId) -> Result<EventLogLocation, StateError> {
    match requested_path {
        Some(path) if path.is_absolute() => Ok(EventLogLocation {
            path: path.to_path_buf(),
            state_root: None,
        }),
        Some(path) => std::env::current_dir()
            .map(|current| EventLogLocation {
                path: current.join(path),
                state_root: None,
            })
            .map_err(StateError::CurrentDirectory),
        None => {
            let state_root = state_directory()?;
            Ok(EventLogLocation {
                path: state_root
                    .join("runs")
                    .join(format!("{}.jsonl", run_id.as_str())),
                state_root: Some(state_root),
            })
        }
    }
}

fn state_directory() -> Result<PathBuf, StateError> {
    if let Some(path) = std::env::var_os("YOUNG_AGENT_STATE_DIR") {
        return absolute_directory("YOUNG_AGENT_STATE_DIR", PathBuf::from(path));
    }

    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return absolute_directory("LOCALAPPDATA", PathBuf::from(path))
            .map(|path| path.join("young-agent"));
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
            return absolute_directory("XDG_STATE_HOME", PathBuf::from(path))
                .map(|path| path.join("young-agent"));
        }
        if let Some(path) = std::env::var_os("HOME") {
            return absolute_directory("HOME", PathBuf::from(path))
                .map(|path| path.join(".local/state/young-agent"));
        }
    }

    #[cfg(unix)]
    {
        Ok(std::env::temp_dir().join(format!(
            "young-agent-state-{}",
            rustix::process::getuid().as_raw()
        )))
    }
    #[cfg(not(unix))]
    {
        Err(StateError::MissingStateDirectory)
    }
}

fn absolute_directory(variable: &'static str, path: PathBuf) -> Result<PathBuf, StateError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(StateError::InvalidStateDirectory { variable, path })
    }
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<(), StateError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let metadata = fs::symlink_metadata(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StateError::UntrustedDirectory {
            path: path.to_path_buf(),
            message: "path must be a real directory, not a symlink or file".to_string(),
        });
    }
    let current_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != current_uid {
        return Err(StateError::UntrustedDirectory {
            path: path.to_path_buf(),
            message: format!(
                "directory owner uid {} does not match current uid {current_uid}",
                metadata.uid()
            ),
        });
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        StateError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let mode = fs::symlink_metadata(path)
        .map_err(|source| StateError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .mode()
        & 0o777;
    if mode != 0o700 {
        return Err(StateError::UntrustedDirectory {
            path: path.to_path_buf(),
            message: format!("directory mode {mode:o} is not private mode 700"),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_directory(path: &Path) -> Result<(), StateError> {
    fs::create_dir_all(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StateError::UntrustedDirectory {
            path: path.to_path_buf(),
            message: "path must be a real directory, not a symlink or file".to_string(),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum StateError {
    CurrentDirectory(io::Error),
    CreateEventLogDirectory {
        path: PathBuf,
        source: io::Error,
    },
    InvalidEventLogPath {
        path: PathBuf,
    },
    InvalidStateDirectory {
        variable: &'static str,
        path: PathBuf,
    },
    #[cfg(not(unix))]
    MissingStateDirectory,
    Io {
        path: PathBuf,
        source: io::Error,
    },
    UntrustedDirectory {
        path: PathBuf,
        message: String,
    },
    CreateEventLog(Box<EventStoreError>),
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectory(source) => {
                write!(formatter, "failed to resolve current directory: {source}")
            }
            Self::CreateEventLogDirectory { path, source } => write!(
                formatter,
                "failed to create Event Log directory '{}': {source}",
                path.display()
            ),
            Self::InvalidEventLogPath { path } => write!(
                formatter,
                "Event Log path '{}' has no parent directory",
                path.display()
            ),
            Self::InvalidStateDirectory { variable, path } => write!(
                formatter,
                "{variable} must be an absolute state directory, got '{}'",
                path.display()
            ),
            #[cfg(not(unix))]
            Self::MissingStateDirectory => formatter.write_str(
                "no private application state directory is available; set YOUNG_AGENT_STATE_DIR",
            ),
            Self::Io { path, source } => write!(
                formatter,
                "failed to prepare private state directory '{}': {source}",
                path.display()
            ),
            Self::UntrustedDirectory { path, message } => write!(
                formatter,
                "refusing untrusted state directory '{}': {message}",
                path.display()
            ),
            Self::CreateEventLog(source) => {
                write!(formatter, "failed to reserve new Event Log: {source}")
            }
        }
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDirectory(source)
            | Self::CreateEventLogDirectory { source, .. }
            | Self::Io { source, .. } => Some(source),
            Self::CreateEventLog(source) => Some(source.as_ref()),
            Self::InvalidEventLogPath { .. }
            | Self::InvalidStateDirectory { .. }
            | Self::UntrustedDirectory { .. } => None,
            #[cfg(not(unix))]
            Self::MissingStateDirectory => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use young_agent_runtime::RunId;

    #[cfg(unix)]
    use super::ensure_private_directory;
    use super::{absolute_directory, location, EventLog, StateError};

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "young-agent-state-unit-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn explicit_locations_preserve_absolute_and_resolve_relative_paths() {
        let run_id = RunId::new("run-location");
        let absolute = std::env::temp_dir().join("explicit-run.jsonl");
        let resolved = location(Some(&absolute), &run_id).expect("absolute path should resolve");
        assert_eq!(resolved.path, absolute);
        assert!(resolved.state_root.is_none());

        let relative = Path::new("relative-run.jsonl");
        let resolved = location(Some(relative), &run_id).expect("relative path should resolve");
        assert_eq!(
            resolved.path,
            std::env::current_dir()
                .expect("current directory should resolve")
                .join(relative)
        );
        assert!(resolved.state_root.is_none());
    }

    #[test]
    fn state_directories_must_be_absolute() {
        let error = absolute_directory("TEST_STATE", PathBuf::from("relative"))
            .expect_err("relative state directory should be rejected");
        assert!(format!("{error}").contains("TEST_STATE must be an absolute state directory"));
        assert!(error.source().is_none());
    }

    #[test]
    fn explicit_log_reports_directory_creation_failures() {
        let directory = TestDirectory::new("blocked-parent");
        let blocked_parent = directory.path.join("not-a-directory");
        fs::write(&blocked_parent, b"file").expect("blocking file should be written");
        let requested = blocked_parent.join("run.jsonl");

        let error = match EventLog::create(Some(&requested), &RunId::new("run-blocked")) {
            Ok(_) => panic!("log beneath a file should be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            StateError::CreateEventLogDirectory { ref path, .. } if path == &blocked_parent
        ));
        assert!(format!("{error}").contains("failed to create Event Log directory"));
        assert!(error.source().is_some());
    }

    #[test]
    fn existing_log_preserves_the_event_store_error_source() {
        let directory = TestDirectory::new("existing-log");
        let requested = directory.path.join("run.jsonl");
        fs::write(&requested, b"existing\n").expect("existing log should be written");

        let error = match EventLog::create(Some(&requested), &RunId::new("run-existing")) {
            Ok(_) => panic!("existing Event Log should not be replaced"),
            Err(error) => error,
        };
        assert!(matches!(&error, StateError::CreateEventLog(_)));
        assert!(format!("{error}").contains("failed to reserve new Event Log"));
        assert!(error.source().is_some());
        assert_eq!(
            fs::read(&requested).expect("existing log should remain readable"),
            b"existing\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_cannot_be_reserved_as_an_event_log() {
        let error = match EventLog::create(Some(Path::new("/")), &RunId::new("run-root")) {
            Ok(_) => panic!("root should not be accepted as an Event Log file"),
            Err(error) => error,
        };
        assert!(matches!(error, StateError::InvalidEventLogPath { .. }));
        assert!(format!("{error}").contains("has no parent directory"));
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_creation_normalizes_nested_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("private");
        let nested = directory.path.join("nested/state");
        ensure_private_directory(&nested).expect("private directory should be prepared");
        let mode = fs::metadata(&nested)
            .expect("private directory metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn state_errors_expose_actionable_messages_and_sources() {
        let current = StateError::CurrentDirectory(io::Error::other("cwd unavailable"));
        assert!(format!("{current}").contains("failed to resolve current directory"));
        assert!(current.source().is_some());

        let directory = StateError::CreateEventLogDirectory {
            path: PathBuf::from("logs"),
            source: io::Error::other("read only"),
        };
        assert!(format!("{directory}").contains("failed to create Event Log directory 'logs'"));
        assert!(directory.source().is_some());

        let invalid_log = StateError::InvalidEventLogPath {
            path: PathBuf::from("run.jsonl"),
        };
        assert!(format!("{invalid_log}").contains("has no parent directory"));
        assert!(invalid_log.source().is_none());

        let invalid_state = StateError::InvalidStateDirectory {
            variable: "YOUNG_AGENT_STATE_DIR",
            path: PathBuf::from("relative"),
        };
        assert!(format!("{invalid_state}").contains("must be an absolute state directory"));
        assert!(invalid_state.source().is_none());

        let io_error = StateError::Io {
            path: PathBuf::from("state"),
            source: io::Error::other("permission denied"),
        };
        assert!(format!("{io_error}").contains("failed to prepare private state directory"));
        assert!(io_error.source().is_some());

        let untrusted = StateError::UntrustedDirectory {
            path: PathBuf::from("state"),
            message: "not private".to_string(),
        };
        assert!(format!("{untrusted}").contains("refusing untrusted state directory"));
        assert!(untrusted.source().is_none());
    }
}
