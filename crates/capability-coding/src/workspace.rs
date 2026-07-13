use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions, Permissions};
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

    #[cfg(unix)]
    #[allow(unsafe_code)]
    pub(crate) fn bind_command_working_directory(&self, command: &mut Command) -> io::Result<()> {
        use std::os::unix::process::CommandExt;

        let directory = self.root_dir.try_clone()?;
        // SAFETY: the closure performs only the async-signal-safe fchdir syscall on an
        // already-open directory handle. It allocates nothing and touches no shared state.
        unsafe {
            command.pre_exec(move || rustix::process::fchdir(&directory).map_err(io::Error::from));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub(crate) fn bind_command_working_directory(&self, _command: &mut Command) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-bound command working directories are not supported on this platform",
        ))
    }

    pub(crate) fn replace_existing_atomically(
        &self,
        path: &Path,
        content: &[u8],
    ) -> io::Result<()> {
        let metadata = self.replacement_metadata(path)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let temp_path = self.stage_content(parent, content, Some(metadata))?;
        let result = self.root_dir.rename(&temp_path, &self.root_dir, path);
        if result.is_err() {
            let _ = self.root_dir.remove_file(&temp_path);
        }
        result
    }

    pub(crate) fn create_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let temp_path = self.stage_content(parent, content, None)?;
        let result = self.commit_new_file(&temp_path, path);
        if result.is_err() {
            let _ = self.root_dir.remove_file(&temp_path);
        }
        result
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

    fn stage_content(
        &self,
        parent: &Path,
        content: &[u8],
        replacement: Option<ReplacementMetadata>,
    ) -> io::Result<PathBuf> {
        use std::io::Write;

        static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

        for _ in 0..100 {
            let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".young-agent-patch-{}-{nonce}.tmp",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = match self.root_dir.open_with(&candidate, &options) {
                Ok(file) => file,
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(source),
            };
            let result = (|| {
                if let Some(replacement) = replacement {
                    #[cfg(unix)]
                    {
                        use cap_std::fs::MetadataExt as _;

                        let staging = file.metadata()?;
                        if staging.uid() != replacement.uid || staging.gid() != replacement.gid {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "atomic patch cannot preserve the target owner or group",
                            ));
                        }
                        if file_has_extended_attributes(&file)? || has_extended_acl(&file)? {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "atomic patch staging file inherited unsupported security metadata",
                            ));
                        }
                    }
                    file.set_permissions(replacement.permissions)?;
                }
                file.write_all(content)?;
                file.sync_all()
            })();
            drop(file);
            if let Err(source) = result {
                let _ = self.root_dir.remove_file(&candidate);
                return Err(source);
            }
            return Ok(candidate);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique patch staging file",
        ))
    }

    fn commit_new_file(&self, temp_path: &Path, path: &Path) -> io::Result<()> {
        #[cfg(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        ))]
        {
            rustix::fs::renameat_with(
                &*self.root_dir,
                temp_path,
                &*self.root_dir,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)
        }
        #[cfg(not(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        )))]
        {
            self.root_dir.hard_link(temp_path, &self.root_dir, path)?;
            if let Err(source) = self.root_dir.remove_file(temp_path) {
                let _ = self.root_dir.remove_file(path);
                return Err(source);
            }
            Ok(())
        }
    }

    fn replacement_metadata(&self, path: &Path) -> io::Result<ReplacementMetadata> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt as _;

            let file = self.root_dir.open(path)?;
            let metadata = file.metadata()?;
            if metadata.nlink() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic patch refuses files with multiple hard links",
                ));
            }
            if file_has_extended_attributes(&file)? {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic patch refuses files with extended attributes",
                ));
            }
            if has_extended_acl(&file)? {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic patch refuses files with an extended ACL",
                ));
            }
            Ok(ReplacementMetadata {
                permissions: metadata.permissions(),
                uid: metadata.uid(),
                gid: metadata.gid(),
            })
        }
        #[cfg(not(unix))]
        {
            let metadata = self.root_dir.metadata(path)?;
            Ok(ReplacementMetadata {
                permissions: metadata.permissions(),
            })
        }
    }
}

#[derive(Clone)]
struct ReplacementMetadata {
    permissions: Permissions,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
fn has_extended_acl(file: &File) -> io::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "macos")]
    let path = PathBuf::from(
        rustix::fs::getpath(file)
            .map_err(io::Error::from)?
            .to_string_lossy()
            .into_owned(),
    );
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let path = if cfg!(target_os = "linux") {
        PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
    } else {
        PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
    };
    let entries = exacl::getfacl(path, None)?;
    #[cfg(target_os = "macos")]
    return Ok(!entries.is_empty());
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    return Ok(entries.iter().any(|entry| {
        !entry.name.is_empty()
            || !entry.flags.is_empty()
            || matches!(entry.kind, exacl::AclEntryKind::Mask)
    }));
}

#[cfg(unix)]
fn file_has_extended_attributes(file: &File) -> io::Result<bool> {
    let mut names: Vec<u8> = Vec::with_capacity(64 * 1024);
    rustix::fs::flistxattr(file, &mut names).map_err(io::Error::from)?;
    Ok(!names.is_empty())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))
))]
fn has_extended_acl(_file: &File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic patch ACL validation is not supported on this platform",
    ))
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
        let (root, mut metadata_truncated) = bounded_metadata_path(&self.root);
        let git_worktree = self.git_worktree.as_ref().map(|git| {
            let (worktree_root, worktree_truncated) = bounded_metadata_path(&git.worktree_root);
            let (git_dir, git_dir_truncated) = bounded_metadata_path(&git.git_dir);
            let (common_dir, common_dir_truncated) = bounded_metadata_path(&git.common_dir);
            metadata_truncated |= worktree_truncated || git_dir_truncated || common_dir_truncated;
            json!({
                "worktree_root": worktree_root,
                "git_dir": git_dir,
                "common_dir": common_dir,
                "linked": git.is_linked_worktree(),
            })
        });
        json!({
            "root": root,
            "git_worktree": git_worktree,
            "metadata_truncated": metadata_truncated,
        })
    }
}

fn bounded_metadata_path(path: &Path) -> (String, bool) {
    const MAX_SERIALIZED_BYTES: usize = 2 * 1024;

    let value = path.display().to_string();
    let mut serialized_bytes = 2usize;
    let mut boundary = 0usize;
    for (index, character) in value.char_indices() {
        let character_bytes = match character {
            '"' | '\\' | '\u{0008}' | '\u{0009}' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        if serialized_bytes.saturating_add(character_bytes) > MAX_SERIALIZED_BYTES {
            return (value[..boundary].to_string(), true);
        }
        serialized_bytes = serialized_bytes.saturating_add(character_bytes);
        boundary = index + character.len_utf8();
    }
    (value, false)
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
    let output = git_probe_command(root)
        .output()
        .map_err(CodingWorkspaceError::StartGitProbe)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(CodingWorkspaceError::GitProbeFailed {
            exit_code: output.status.code(),
            stderr,
        });
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

fn git_probe_command(root: &Path) -> Command {
    const REPOSITORY_ENVIRONMENT: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_INTERNAL_SUPER_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_QUARANTINE_PATH",
        "GIT_PREFIX",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ];

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
        ])
        .env("LC_ALL", "C");
    for name in REPOSITORY_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

#[derive(Debug)]
pub enum CodingWorkspaceError {
    ResolveRoot {
        path: PathBuf,
        source: io::Error,
    },
    RootIsNotDirectory {
        path: PathBuf,
    },
    OpenRoot {
        path: PathBuf,
        source: io::Error,
    },
    StartGitProbe(io::Error),
    GitProbeFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    GitOutputUtf8(std::string::FromUtf8Error),
    UnexpectedGitOutput {
        stdout: String,
    },
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
            Self::GitProbeFailed { exit_code, stderr } => write!(
                formatter,
                "git worktree probe failed with exit code {exit_code:?}: {}",
                stderr.trim()
            ),
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
            Self::RootIsNotDirectory { .. }
            | Self::GitProbeFailed { .. }
            | Self::UnexpectedGitOutput { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{git_probe_command, CodingWorkspace};

    #[test]
    fn git_probe_clears_repository_environment_overrides() {
        let command = git_probe_command(std::path::Path::new("."));
        let environment = command.get_envs().collect::<Vec<_>>();

        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
        ] {
            assert!(
                environment
                    .iter()
                    .any(|(key, value)| { *key == std::ffi::OsStr::new(name) && value.is_none() }),
                "{name} must be removed from the git probe environment"
            );
        }
    }

    #[test]
    fn create_new_never_removes_an_existing_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-create-new-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("owned.txt"), "concurrent owner\n")
            .expect("existing file is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");

        let error = workspace
            .create_new(std::path::Path::new("owned.txt"), b"patch content\n")
            .expect_err("create-new commit must not replace an existing file");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(root.join("owned.txt")).unwrap(),
            "concurrent owner\n"
        );
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".young-agent-patch-")
            }),
            "failed no-replace commits must clean their staging file"
        );

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
