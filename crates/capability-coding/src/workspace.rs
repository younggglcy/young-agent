use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct CodingWorkspace {
    context: Arc<WorkspaceContext>,
    root_dir: Arc<Dir>,
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
        let root_dir = Dir::open_ambient_dir(&root, ambient_authority()).map_err(|source| {
            CodingWorkspaceError::OpenRoot {
                path: root.clone(),
                source,
            }
        })?;

        let git_worktree = detect_git_worktree(&root)?;
        Ok(Self {
            context: Arc::new(WorkspaceContext { root, git_worktree }),
            root_dir: Arc::new(root_dir),
        })
    }

    pub fn context(&self) -> &WorkspaceContext {
        &self.context
    }

    pub(crate) fn resolve_existing(
        &self,
        requested_path: &Path,
    ) -> Result<ResolvedWorkspacePath, WorkspacePathError> {
        let relative_path = self.relative_request_path(requested_path)?;
        let canonical_path = self
            .root_dir
            .canonicalize(&relative_path)
            .map_err(|source| self.path_access_error(requested_path, source))?;
        self.root_dir
            .metadata(&canonical_path)
            .map_err(|source| self.path_access_error(requested_path, source))?;
        Ok(ResolvedWorkspacePath {
            relative_path: canonical_path,
            existed: true,
        })
    }

    pub(crate) fn resolve_for_write(
        &self,
        requested_path: &Path,
    ) -> Result<ResolvedWorkspacePath, WorkspacePathError> {
        let relative_path = self.relative_request_path(requested_path)?;
        match self.root_dir.symlink_metadata(&relative_path) {
            Ok(_) => self.resolve_existing(requested_path),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let parent = relative_path.parent().unwrap_or_else(|| Path::new("."));
                let canonical_parent = self
                    .root_dir
                    .canonicalize(parent)
                    .map_err(|source| self.path_access_error(parent, source))?;
                let file_name = relative_path.file_name().ok_or_else(|| {
                    WorkspacePathError::OutsideWorkspace {
                        path: requested_path.to_path_buf(),
                        root: self.context.root.clone(),
                    }
                })?;
                Ok(ResolvedWorkspacePath {
                    relative_path: canonical_parent.join(file_name),
                    existed: false,
                })
            }
            Err(source) => Err(self.path_access_error(requested_path, source)),
        }
    }

    pub(crate) fn open_file(&self, path: &Path) -> io::Result<File> {
        self.root_dir.open(path)
    }

    pub(crate) fn open_dir(&self, path: &Path) -> io::Result<Dir> {
        self.root_dir.open_dir(path)
    }

    pub(crate) fn replace_existing_atomically(
        &self,
        path: &Path,
        content: &[u8],
    ) -> io::Result<()> {
        use std::io::Write;

        static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

        let permissions = self.root_dir.metadata(path)?.permissions();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp_path = None;
        let mut temp_file = None;
        for _ in 0..100 {
            let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".young-agent-patch-{}-{nonce}.tmp",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            match self.root_dir.open_with(&candidate, &options) {
                Ok(file) => {
                    temp_path = Some(candidate);
                    temp_file = Some(file);
                    break;
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => return Err(source),
            }
        }
        let temp_path = temp_path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique patch staging file",
            )
        })?;
        let mut temp_file = temp_file.expect("temp path and file are set together");
        let result = (|| {
            temp_file.write_all(content)?;
            temp_file.sync_all()?;
            self.root_dir.set_permissions(&temp_path, permissions)?;
            self.root_dir.rename(&temp_path, &self.root_dir, path)
        })();
        if result.is_err() {
            let _ = self.root_dir.remove_file(&temp_path);
        }
        result
    }

    pub(crate) fn create_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        use std::io::Write;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = self.root_dir.open_with(path, &options)?;
        file.write_all(content)
    }

    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.root_dir.remove_file(path)
    }

    pub(crate) fn metadata(&self) -> Value {
        self.context.metadata()
    }

    fn relative_request_path(&self, requested_path: &Path) -> Result<PathBuf, WorkspacePathError> {
        let lexical_path = if requested_path.is_absolute() {
            normalize_path(requested_path)
        } else {
            normalize_path(&self.context.root.join(requested_path))
        };
        self.ensure_inside(&lexical_path)?;
        let relative = lexical_path
            .strip_prefix(&self.context.root)
            .expect("inside path has workspace prefix");
        if relative.as_os_str().is_empty() {
            Ok(PathBuf::from("."))
        } else {
            Ok(relative.to_path_buf())
        }
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

    fn path_access_error(&self, path: &Path, source: io::Error) -> WorkspacePathError {
        if source.kind() == io::ErrorKind::PermissionDenied
            && source.to_string().contains("outside of the filesystem")
        {
            WorkspacePathError::OutsideWorkspace {
                path: path.to_path_buf(),
                root: self.context.root.clone(),
            }
        } else {
            WorkspacePathError::Access {
                path: path.to_path_buf(),
                source,
            }
        }
    }
}

impl fmt::Debug for CodingWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodingWorkspace")
            .field("context", &self.context)
            .finish_non_exhaustive()
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
    pub(crate) relative_path: PathBuf,
    pub(crate) existed: bool,
}

#[derive(Debug)]
pub(crate) enum WorkspacePathError {
    OutsideWorkspace { path: PathBuf, root: PathBuf },
    Access { path: PathBuf, source: io::Error },
}

impl WorkspacePathError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::OutsideWorkspace { .. } => "outside_workspace",
            Self::Access { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                "path_not_found"
            }
            Self::Access { .. } => "workspace_io_error",
        }
    }

    pub(crate) fn retryable(&self) -> bool {
        matches!(self, Self::Access { source, .. } if source.kind() == io::ErrorKind::Interrupted)
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
    ResolveRoot { path: PathBuf, source: io::Error },
    RootIsNotDirectory { path: PathBuf },
    OpenRoot { path: PathBuf, source: io::Error },
    StartGitProbe(io::Error),
    GitOutputUtf8(std::string::FromUtf8Error),
    UnexpectedGitOutput { stdout: String },
}

impl fmt::Display for CodingWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolveRoot { path, source } => write!(
                formatter,
                "failed to resolve workspace '{}': {source}",
                path.display()
            ),
            Self::RootIsNotDirectory { path } => write!(
                formatter,
                "workspace root '{}' is not a directory",
                path.display()
            ),
            Self::OpenRoot { path, source } => write!(
                formatter,
                "failed to open workspace root '{}': {source}",
                path.display()
            ),
            Self::StartGitProbe(source) => {
                write!(formatter, "failed to start git worktree probe: {source}")
            }
            Self::GitOutputUtf8(source) => write!(
                formatter,
                "git worktree probe returned invalid UTF-8: {source}"
            ),
            Self::UnexpectedGitOutput { stdout } => write!(
                formatter,
                "git worktree probe returned unexpected output: {stdout:?}"
            ),
        }
    }
}

impl Error for CodingWorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResolveRoot { source, .. }
            | Self::OpenRoot { source, .. }
            | Self::StartGitProbe(source) => Some(source),
            Self::GitOutputUtf8(source) => Some(source),
            Self::RootIsNotDirectory { .. } | Self::UnexpectedGitOutput { .. } => None,
        }
    }
}
