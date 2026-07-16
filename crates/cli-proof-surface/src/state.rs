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
