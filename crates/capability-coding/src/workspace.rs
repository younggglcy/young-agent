use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct CodingWorkspace {
    context: Arc<WorkspaceContext>,
}

impl CodingWorkspace {
    pub fn resolve(selected_root: impl AsRef<Path>) -> Result<Self, CodingWorkspaceError> {
        let selected_root = selected_root.as_ref();
        let root = fs::canonicalize(selected_root).map_err(|source| {
            CodingWorkspaceError::ResolveRoot {
                path: selected_root.to_path_buf(),
                source,
            }
        })?;
        let metadata = fs::metadata(&root).map_err(|source| CodingWorkspaceError::ResolveRoot {
            path: root.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(CodingWorkspaceError::RootIsNotDirectory { path: root });
        }

        let git_worktree = detect_git_worktree(&root)?;
        Ok(Self {
            context: Arc::new(WorkspaceContext { root, git_worktree }),
        })
    }

    pub fn context(&self) -> &WorkspaceContext {
        &self.context
    }

    pub(crate) fn resolve_existing(
        &self,
        requested_path: &Path,
    ) -> Result<ResolvedWorkspacePath, WorkspacePathError> {
        let lexical_path = if requested_path.is_absolute() {
            normalize_path(requested_path)
        } else {
            normalize_path(&self.context.root.join(requested_path))
        };
        self.ensure_inside(&lexical_path)?;

        let resolved_path =
            fs::canonicalize(&lexical_path).map_err(|source| WorkspacePathError::Access {
                path: requested_path.to_path_buf(),
                source,
            })?;
        self.ensure_inside(&resolved_path)?;

        let relative_path = resolved_path
            .strip_prefix(&self.context.root)
            .expect("inside path has workspace prefix")
            .to_path_buf();
        Ok(ResolvedWorkspacePath {
            absolute_path: resolved_path,
            relative_path,
            existed: true,
        })
    }

    pub(crate) fn resolve_for_write(
        &self,
        requested_path: &Path,
    ) -> Result<ResolvedWorkspacePath, WorkspacePathError> {
        let lexical_path = if requested_path.is_absolute() {
            normalize_path(requested_path)
        } else {
            normalize_path(&self.context.root.join(requested_path))
        };
        self.ensure_inside(&lexical_path)?;

        match fs::symlink_metadata(&lexical_path) {
            Ok(_) => self.resolve_existing(requested_path),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let parent =
                    lexical_path
                        .parent()
                        .ok_or_else(|| WorkspacePathError::OutsideWorkspace {
                            path: lexical_path.clone(),
                            root: self.context.root.clone(),
                        })?;
                let resolved_parent =
                    fs::canonicalize(parent).map_err(|source| WorkspacePathError::Access {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                self.ensure_inside(&resolved_parent)?;
                let file_name = lexical_path.file_name().ok_or_else(|| {
                    WorkspacePathError::OutsideWorkspace {
                        path: lexical_path.clone(),
                        root: self.context.root.clone(),
                    }
                })?;
                let absolute_path = resolved_parent.join(file_name);
                self.ensure_inside(&absolute_path)?;
                let relative_path = absolute_path
                    .strip_prefix(&self.context.root)
                    .expect("inside path has workspace prefix")
                    .to_path_buf();
                Ok(ResolvedWorkspacePath {
                    absolute_path,
                    relative_path,
                    existed: false,
                })
            }
            Err(source) => Err(WorkspacePathError::Access {
                path: requested_path.to_path_buf(),
                source,
            }),
        }
    }

    pub(crate) fn metadata(&self) -> Value {
        self.context.metadata()
    }

    fn ensure_inside(&self, path: &Path) -> Result<(), WorkspacePathError> {
        if path.starts_with(&self.context.root) {
            Ok(())
        } else {
            Err(WorkspacePathError::OutsideWorkspace {
                path: path.to_path_buf(),
                root: self.context.root.clone(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceContext {
    root: PathBuf,
    git_worktree: Option<GitWorktreeContext>,
}

impl WorkspaceContext {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_worktree(&self) -> Option<&GitWorktreeContext> {
        self.git_worktree.as_ref()
    }

    fn metadata(&self) -> Value {
        let git_worktree = self.git_worktree.as_ref().map(|git| {
            json!({
                "worktree_root": git.worktree_root.display().to_string(),
                "git_dir": git.git_dir.display().to_string(),
                "common_dir": git.common_dir.display().to_string(),
                "linked": git.is_linked_worktree(),
            })
        });
        json!({
            "root": self.root.display().to_string(),
            "git_worktree": git_worktree,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktreeContext {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl GitWorktreeContext {
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    pub fn is_linked_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedWorkspacePath {
    pub(crate) absolute_path: PathBuf,
    pub(crate) relative_path: PathBuf,
    pub(crate) existed: bool,
}

#[derive(Debug)]
pub(crate) enum WorkspacePathError {
    OutsideWorkspace {
        path: PathBuf,
        root: PathBuf,
    },
    Access {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl WorkspacePathError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::OutsideWorkspace { .. } => "outside_workspace",
            Self::Access { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                "path_not_found"
            }
            Self::Access { .. } => "workspace_io_error",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(self, Self::Access { source, .. } if source.kind() == std::io::ErrorKind::Interrupted)
    }
}

impl fmt::Display for WorkspacePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace { path, root } => write!(
                formatter,
                "path '{}' escapes workspace boundary '{}'",
                path.display(),
                root.display()
            ),
            Self::Access { path, source } => {
                write!(formatter, "failed to access '{}': {source}", path.display())
            }
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn detect_git_worktree(root: &Path) -> Result<Option<GitWorktreeContext>, CodingWorkspaceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
        ])
        .output()
        .map_err(CodingWorkspaceError::StartGitProbe)?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).map_err(CodingWorkspaceError::GitOutputUtf8)?;
    let mut paths = stdout.lines().map(PathBuf::from);
    let worktree_root = paths.next();
    let git_dir = paths.next();
    let common_dir = paths.next();
    if paths.next().is_some()
        || worktree_root.is_none()
        || git_dir.is_none()
        || common_dir.is_none()
    {
        return Err(CodingWorkspaceError::UnexpectedGitOutput { stdout });
    }

    Ok(Some(GitWorktreeContext {
        worktree_root: worktree_root.expect("checked above"),
        git_dir: git_dir.expect("checked above"),
        common_dir: common_dir.expect("checked above"),
    }))
}

#[derive(Debug)]
pub enum CodingWorkspaceError {
    ResolveRoot {
        path: PathBuf,
        source: std::io::Error,
    },
    RootIsNotDirectory {
        path: PathBuf,
    },
    StartGitProbe(std::io::Error),
    GitOutputUtf8(std::string::FromUtf8Error),
    UnexpectedGitOutput {
        stdout: String,
    },
}

impl fmt::Display for CodingWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolveRoot { path, source } => {
                write!(
                    formatter,
                    "failed to resolve workspace '{}': {source}",
                    path.display()
                )
            }
            Self::RootIsNotDirectory { path } => {
                write!(
                    formatter,
                    "workspace root '{}' is not a directory",
                    path.display()
                )
            }
            Self::StartGitProbe(source) => {
                write!(formatter, "failed to start git worktree probe: {source}")
            }
            Self::GitOutputUtf8(source) => {
                write!(
                    formatter,
                    "git worktree probe returned invalid UTF-8: {source}"
                )
            }
            Self::UnexpectedGitOutput { stdout } => {
                write!(
                    formatter,
                    "git worktree probe returned unexpected output: {stdout:?}"
                )
            }
        }
    }
}

impl Error for CodingWorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResolveRoot { source, .. } | Self::StartGitProbe(source) => Some(source),
            Self::GitOutputUtf8(source) => Some(source),
            Self::RootIsNotDirectory { .. } | Self::UnexpectedGitOutput { .. } => None,
        }
    }
}
