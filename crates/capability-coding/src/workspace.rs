use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, DirEntry, File, Metadata, OpenOptions, Permissions};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct CodingWorkspace {
    context: Arc<WorkspaceContext>,
    root_dir: Arc<Dir>,
}

impl CodingWorkspace {
    pub fn resolve(selected_root: impl AsRef<Path>) -> Result<Self, CodingWorkspaceError> {
        let selected_root = selected_root.as_ref();
        let root_dir =
            Dir::open_ambient_dir(selected_root, ambient_authority()).map_err(|source| {
                CodingWorkspaceError::OpenRoot {
                    path: selected_root.to_path_buf(),
                    source,
                }
            })?;
        let opened_metadata =
            root_dir
                .metadata(".")
                .map_err(|source| CodingWorkspaceError::InspectOpenedRoot {
                    path: selected_root.to_path_buf(),
                    source,
                })?;
        if !opened_metadata.is_dir() {
            return Err(CodingWorkspaceError::RootIsNotDirectory {
                path: selected_root.to_path_buf(),
            });
        }
        let root = fs::canonicalize(selected_root).map_err(|source| {
            CodingWorkspaceError::ResolveRoot {
                path: selected_root.to_path_buf(),
                source,
            }
        })?;
        ensure_opened_root_matches_path(&root_dir, &root)?;

        let git_worktree = detect_git_worktree(&root_dir, &root)?;
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
        let metadata = self
            .root_dir
            .symlink_metadata(&relative_path)
            .map_err(|source| self.path_access_error(requested_path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspacePathError::OutsideWorkspace {
                path: requested_path.to_path_buf(),
                root: self.context.root.clone(),
            });
        }
        Ok(ResolvedWorkspacePath {
            relative_path,
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
                let parent = relative_path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let directory = self
                    .root_dir
                    .open_dir(parent)
                    .map_err(|source| self.path_access_error(parent, source))?;
                drop(directory);
                relative_path
                    .file_name()
                    .ok_or_else(|| WorkspacePathError::OutsideWorkspace {
                        path: requested_path.to_path_buf(),
                        root: self.context.root.clone(),
                    })?;
                Ok(ResolvedWorkspacePath {
                    relative_path,
                    existed: false,
                })
            }
            Err(source) => Err(self.path_access_error(requested_path, source)),
        }
    }

    pub(crate) fn open_regular_file(&self, path: &Path) -> io::Result<(File, Metadata)> {
        let file = self.root_dir.open_with(path, &regular_file_options())?;
        require_regular_file(file)
    }

    pub(crate) fn open_regular_entry(entry: &DirEntry) -> io::Result<(File, Metadata)> {
        let file = entry.open_with(&regular_file_options())?;
        require_regular_file(file)
    }

    pub(crate) fn file_snapshot(file: &File) -> io::Result<FileSnapshot> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt as _;

            let metadata = file.metadata()?;
            Ok(FileSnapshot {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.size(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stable patch target identities are not supported on this platform",
            ))
        }
    }

    pub(crate) fn open_dir(&self, path: &Path) -> io::Result<Dir> {
        self.root_dir.open_dir(path)
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    pub(crate) fn bind_command_working_directory(&self, command: &mut Command) -> io::Result<()> {
        bind_process_working_directory(command, &self.root_dir)
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
        expected_snapshot: FileSnapshot,
    ) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "patch target has no file name")
        })?;
        let directory = self.root_dir.open_dir(parent)?;
        let metadata = self.replacement_metadata(&directory, Path::new(file_name))?;
        if metadata.snapshot != expected_snapshot {
            return Err(patch_target_changed());
        }
        let staged = self.stage_content(&directory, content, Some(metadata.clone()))?;
        self.commit_existing_file(&directory, &staged, Path::new(file_name), metadata)
    }

    pub(crate) fn create_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "patch target has no file name")
        })?;
        let directory = self.root_dir.open_dir(parent)?;
        let staged = self.stage_content(&directory, content, None)?;
        self.commit_new_file(&directory, &staged, Path::new(file_name))
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
        directory: &Dir,
        content: &[u8],
        replacement: Option<ReplacementMetadata>,
    ) -> io::Result<StagedFile> {
        use std::io::Write;

        static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

        for _ in 0..100 {
            let nonce = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let candidate = PathBuf::from(format!(
                ".young-agent-patch-{}-{nonce}.tmp",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt as _;

                options.mode(0o600);
            }
            let mut file = match directory.open_with(&candidate, &options) {
                Ok(file) => file,
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(source),
            };
            let result = (|| {
                if let Some(replacement) = replacement.as_ref() {
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
                }
                file.write_all(content)?;
                file.sync_all()?;
                if let Some(replacement) = replacement.as_ref() {
                    file.set_permissions(replacement.permissions.clone())?;
                    #[cfg(unix)]
                    {
                        use cap_std::fs::MetadataExt as _;

                        if file.metadata()?.mode() != replacement.mode {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "atomic patch could not preserve the target mode",
                            ));
                        }
                    }
                    file.sync_all()?;
                }
                Ok(())
            })();
            if let Err(source) = result {
                return cleanup_open_staging_after_error(directory, &candidate, &file, source);
            }
            let snapshot = match Self::file_snapshot(&file) {
                Ok(snapshot) => snapshot,
                Err(source) => {
                    return cleanup_open_staging_after_error(directory, &candidate, &file, source)
                }
            };
            return Ok(StagedFile {
                path: candidate,
                file,
                snapshot,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique patch staging file",
        ))
    }

    fn commit_new_file(&self, directory: &Dir, staged: &StagedFile, path: &Path) -> io::Result<()> {
        #[cfg(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        ))]
        {
            self.validate_staging_slot(directory, staged)?;
            let result = rustix::fs::renameat_with(
                directory,
                &staged.path,
                directory,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from);
            if let Err(source) = result {
                return cleanup_owned_staging_after_error(directory, staged, source);
            }
            self.validate_installed_staging(directory, path, staged)
        }
        #[cfg(not(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        )))]
        {
            let _ = (directory, staged, path);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic no-replace patch commits are not supported on this platform",
            ))
        }
    }

    fn commit_existing_file(
        &self,
        directory: &Dir,
        staged: &StagedFile,
        path: &Path,
        expected: ReplacementMetadata,
    ) -> io::Result<()> {
        #[cfg(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        ))]
        {
            self.validate_staging_slot(directory, staged)?;
            if let Err(source) = exchange_files(directory, &staged.path, path) {
                return cleanup_owned_staging_after_error(directory, staged, source);
            }

            let validation = self
                .validate_installed_staging(directory, path, staged)
                .and_then(|()| self.replacement_metadata(directory, &staged.path))
                .and_then(|metadata| {
                    if metadata.matches(&expected) {
                        Ok(())
                    } else {
                        Err(patch_target_changed())
                    }
                });
            if let Err(source) = validation {
                return rollback_existing_exchange(directory, staged, path, source);
            }

            let final_validation = self
                .validate_installed_staging(directory, path, staged)
                .and_then(|()| self.replacement_metadata(directory, &staged.path))
                .and_then(|displaced| {
                    if displaced.matches(&expected) {
                        Ok(())
                    } else {
                        Err(patch_target_changed())
                    }
                });
            if let Err(source) = final_validation {
                return rollback_existing_exchange(directory, staged, path, source);
            }
            if let Err(source) = directory.remove_file(&staged.path) {
                return rollback_existing_exchange(directory, staged, path, source);
            }
            Ok(())
        }
        #[cfg(not(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        )))]
        {
            let _ = (directory, staged, path, expected);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic identity-bound patch commits are not supported on this platform",
            ))
        }
    }

    fn validate_staging_slot(&self, directory: &Dir, staged: &StagedFile) -> io::Result<()> {
        let handle_snapshot = Self::file_snapshot(&staged.file)?;
        let (slot, _) =
            require_regular_file(directory.open_with(&staged.path, &regular_file_options())?)?;
        let slot_snapshot = Self::file_snapshot(&slot)?;
        if handle_snapshot == staged.snapshot && slot_snapshot == staged.snapshot {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "patch staging file changed before commit",
            ))
        }
    }

    fn validate_installed_staging(
        &self,
        directory: &Dir,
        path: &Path,
        staged: &StagedFile,
    ) -> io::Result<()> {
        let handle_snapshot = Self::file_snapshot(&staged.file)?;
        let (installed, _) =
            require_regular_file(directory.open_with(path, &regular_file_options())?)?;
        let installed_snapshot = Self::file_snapshot(&installed)?;
        if handle_snapshot == staged.snapshot && installed_snapshot == staged.snapshot {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "patch target changed during commit; displaced file was preserved as '{}'",
                    staged.path.display()
                ),
            ))
        }
    }

    fn replacement_metadata(
        &self,
        directory: &Dir,
        path: &Path,
    ) -> io::Result<ReplacementMetadata> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt as _;

            let (file, metadata) =
                require_regular_file(directory.open_with(path, &regular_file_options())?)?;
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
                snapshot: FileSnapshot {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    size: metadata.size(),
                    modified_seconds: metadata.mtime(),
                    modified_nanoseconds: metadata.mtime_nsec(),
                    uid: metadata.uid(),
                    gid: metadata.gid(),
                    mode: metadata.mode(),
                },
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (directory, path);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "safe replacement metadata validation is not supported on this platform",
            ))
        }
    }
}

#[derive(Clone)]
struct ReplacementMetadata {
    permissions: Permissions,
    snapshot: FileSnapshot,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
}

#[cfg(unix)]
impl ReplacementMetadata {
    fn matches(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot
            && self.uid == other.uid
            && self.gid == other.gid
            && self.mode == other.mode
    }
}

struct StagedFile {
    path: PathBuf,
    file: File,
    snapshot: FileSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    size: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
}

fn patch_target_changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "patch target changed while the patch was being applied",
    )
}

fn cleanup_staging_after_error<T>(
    directory: &Dir,
    staging_path: &Path,
    source: io::Error,
) -> io::Result<T> {
    match directory.remove_file(staging_path) {
        Ok(()) => Err(source),
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(source),
        Err(cleanup) => Err(io::Error::new(
            cleanup.kind(),
            format!(
                "patch operation failed ({source}); staging file '{}' could not be removed ({cleanup})",
                staging_path.display()
            ),
        )),
    }
}

fn cleanup_open_staging_after_error<T>(
    directory: &Dir,
    staging_path: &Path,
    staging_file: &File,
    source: io::Error,
) -> io::Result<T> {
    let expected = CodingWorkspace::file_snapshot(staging_file)?;
    let (slot, _) =
        require_regular_file(directory.open_with(staging_path, &regular_file_options())?)?;
    if CodingWorkspace::file_snapshot(&slot)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "patch operation failed ({source}); staging path changed and was preserved as '{}'",
                staging_path.display()
            ),
        ));
    }
    cleanup_staging_after_error(directory, staging_path, source)
}

fn cleanup_owned_staging_after_error<T>(
    directory: &Dir,
    staged: &StagedFile,
    source: io::Error,
) -> io::Result<T> {
    cleanup_open_staging_after_error(directory, &staged.path, &staged.file, source)
}

fn regular_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;

        let flags = rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW;
        options.custom_flags(flags.bits() as i32);
    }
    options
}

fn require_regular_file(file: File) -> io::Result<(File, Metadata)> {
    let metadata = file.metadata()?;
    if metadata.is_file() {
        Ok((file, metadata))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ))
    }
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
))]
fn exchange_files(directory: &Dir, left: &Path, right: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        left,
        directory,
        right,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
))]
fn rollback_existing_exchange(
    directory: &Dir,
    staged: &StagedFile,
    path: &Path,
    source: io::Error,
) -> io::Result<()> {
    let (installed, _) = require_regular_file(directory.open_with(path, &regular_file_options())?)?;
    if CodingWorkspace::file_snapshot(&staged.file)? != staged.snapshot
        || CodingWorkspace::file_snapshot(&installed)? != staged.snapshot
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "patch commit failed ({source}); target changed again, so displaced file was preserved as '{}'",
                staged.path.display()
            ),
        ));
    }
    if let Err(rollback) = exchange_files(directory, &staged.path, path) {
        return Err(io::Error::other(format!(
            "patch commit failed ({source}) and restoring the original target also failed ({rollback})"
        )));
    }
    let (rolled_back_staging, _) =
        require_regular_file(directory.open_with(&staged.path, &regular_file_options())?)?;
    if CodingWorkspace::file_snapshot(&rolled_back_staging)? != staged.snapshot {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "patch target was restored after failure ({source}), but staging changed again and was preserved as '{}'",
                staged.path.display()
            ),
        ));
    }
    if let Err(cleanup) = directory.remove_file(&staged.path) {
        return Err(io::Error::new(
            cleanup.kind(),
            format!("patch commit failed ({source}); target was restored but staging cleanup failed ({cleanup})"),
        ));
    }
    Err(source)
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

#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "hurd"
))]
fn file_has_extended_attributes(file: &File) -> io::Result<bool> {
    let mut names: Vec<u8> = Vec::with_capacity(64 * 1024);
    rustix::fs::flistxattr(file, &mut names).map_err(io::Error::from)?;
    Ok(!names.is_empty())
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "hurd"
)))]
fn file_has_extended_attributes(_file: &File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic patch extended-attribute validation is not supported on this platform",
    ))
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

fn detect_git_worktree(
    root_dir: &Dir,
    root_path: &Path,
) -> Result<Option<GitWorktreeContext>, CodingWorkspaceError> {
    let mut command = git_probe_command();
    #[cfg(unix)]
    bind_process_working_directory(&mut command, root_dir)
        .map_err(CodingWorkspaceError::BindGitProbe)?;
    #[cfg(not(unix))]
    command.current_dir(root_path);
    let output = command
        .output()
        .map_err(CodingWorkspaceError::StartGitProbe)?;
    ensure_opened_root_matches_path(root_dir, root_path)?;

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

fn git_probe_command() -> Command {
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

#[cfg(unix)]
#[allow(unsafe_code)]
fn bind_process_working_directory(command: &mut Command, directory: &Dir) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    let directory = directory.try_clone()?;
    // SAFETY: the closure performs only the async-signal-safe fchdir syscall on an
    // already-open directory handle. It allocates nothing and touches no shared state.
    unsafe {
        command.pre_exec(move || rustix::process::fchdir(&directory).map_err(io::Error::from));
    }
    Ok(())
}

fn ensure_opened_root_matches_path(
    root_dir: &Dir,
    root_path: &Path,
) -> Result<(), CodingWorkspaceError> {
    let opened =
        root_dir
            .metadata(".")
            .map_err(|source| CodingWorkspaceError::InspectOpenedRoot {
                path: root_path.to_path_buf(),
                source,
            })?;
    let ambient = fs::metadata(root_path).map_err(|source| CodingWorkspaceError::ResolveRoot {
        path: root_path.to_path_buf(),
        source,
    })?;

    #[cfg(unix)]
    let matches = {
        use cap_std::fs::MetadataExt as _;
        use std::os::unix::fs::MetadataExt as _;

        opened.dev() == ambient.dev() && opened.ino() == ambient.ino()
    };
    #[cfg(windows)]
    let matches = {
        use cap_std::fs::MetadataExt as _;
        use std::os::windows::fs::MetadataExt as _;

        opened.volume_serial_number() == ambient.volume_serial_number()
            && opened.file_index() == ambient.file_index()
            && opened.volume_serial_number().is_some()
            && opened.file_index().is_some()
    };
    #[cfg(not(any(unix, windows)))]
    let matches = false;

    if matches {
        Ok(())
    } else {
        Err(CodingWorkspaceError::RootChanged {
            path: root_path.to_path_buf(),
        })
    }
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
    InspectOpenedRoot {
        path: PathBuf,
        source: io::Error,
    },
    RootChanged {
        path: PathBuf,
    },
    BindGitProbe(io::Error),
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
            Self::InspectOpenedRoot { path, source } => write!(
                formatter,
                "failed to inspect opened workspace '{}': {source}",
                path.display()
            ),
            Self::RootChanged { path } => write!(
                formatter,
                "workspace path '{}' changed while it was being opened",
                path.display()
            ),
            Self::BindGitProbe(source) => {
                write!(
                    formatter,
                    "failed to bind git probe to workspace handle: {source}"
                )
            }
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
            | Self::InspectOpenedRoot { source, .. }
            | Self::BindGitProbe(source)
            | Self::StartGitProbe(source) => Some(source),
            Self::GitOutputUtf8(source) => Some(source),
            Self::RootIsNotDirectory { .. }
            | Self::RootChanged { .. }
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
        let command = git_probe_command();
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
    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn existing_commit_rolls_back_when_the_target_identity_changes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-target-swap-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let metadata = workspace
            .replacement_metadata(&directory, std::path::Path::new("target.txt"))
            .expect("metadata validates");
        let staging = workspace
            .stage_content(&directory, b"patched\n", Some(metadata.clone()))
            .expect("content is staged");

        directory
            .rename("target.txt", &directory, "original.txt")
            .expect("original target is moved");
        std::fs::write(root.join("target.txt"), "concurrent\n")
            .expect("concurrent target is written");

        let error = workspace
            .commit_existing_file(
                &directory,
                &staging,
                std::path::Path::new("target.txt"),
                metadata,
            )
            .expect_err("identity mismatch must fail the commit");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "concurrent\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("original.txt")).unwrap(),
            "original\n"
        );
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".young-agent-patch-")
        }));

        drop((directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(unix)]
    #[test]
    fn opened_root_identity_detects_an_ambient_path_replacement() {
        use cap_std::ambient_authority;
        use cap_std::fs::Dir;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let parent = std::env::temp_dir().join(format!(
            "young-workspace-root-swap-{}-{nonce}",
            std::process::id()
        ));
        let root = parent.join("selected");
        let moved = parent.join("moved");
        std::fs::create_dir_all(&root).expect("test workspace is created");
        let opened = Dir::open_ambient_dir(&root, ambient_authority()).expect("root handle opens");
        std::fs::rename(&root, &moved).expect("opened root is moved");
        std::fs::create_dir(&root).expect("replacement root is created");

        let error = super::ensure_opened_root_matches_path(&opened, &root)
            .expect_err("replacement path must not match the opened handle");

        assert!(matches!(
            error,
            super::CodingWorkspaceError::RootChanged { .. }
        ));
        drop(opened);
        std::fs::remove_dir_all(parent).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn new_file_commit_rejects_a_replaced_staging_slot() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-staging-swap-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let staged = workspace
            .stage_content(&directory, b"owned\n", None)
            .expect("content is staged");
        directory
            .rename(&staged.path, &directory, "owned-staging.tmp")
            .expect("owned staging file is moved");
        std::fs::write(root.join(&staged.path), "concurrent\n")
            .expect("replacement staging path is written");

        let error = workspace
            .commit_new_file(&directory, &staged, std::path::Path::new("created.txt"))
            .expect_err("replaced staging slot must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!root.join("created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(&staged.path)).unwrap(),
            "concurrent\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("owned-staging.tmp")).unwrap(),
            "owned\n"
        );

        drop((staged, directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rollback_preserves_a_second_concurrent_target_replacement() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-rollback-race-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let expected = workspace
            .replacement_metadata(&directory, std::path::Path::new("target.txt"))
            .expect("metadata validates");
        let staged = workspace
            .stage_content(&directory, b"patched\n", Some(expected))
            .expect("content is staged");
        super::exchange_files(&directory, &staged.path, std::path::Path::new("target.txt"))
            .expect("initial exchange succeeds");
        directory
            .rename("target.txt", &directory, "published-staging.tmp")
            .expect("installed staging file is moved");
        std::fs::write(root.join("target.txt"), "second concurrent\n")
            .expect("second concurrent target is written");

        let error = super::rollback_existing_exchange(
            &directory,
            &staged,
            std::path::Path::new("target.txt"),
            super::patch_target_changed(),
        )
        .expect_err("rollback must not overwrite a second replacement");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "second concurrent\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(&staged.path)).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("published-staging.tmp")).unwrap(),
            "patched\n"
        );

        drop((staged, directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn existing_commit_preserves_a_concurrent_mode_change() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-mode-race-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let target = root.join("target.txt");
        std::fs::write(&target, "original\n").expect("target is written");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .expect("initial mode is set");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let expected = workspace
            .replacement_metadata(&directory, std::path::Path::new("target.txt"))
            .expect("metadata validates");
        let staged = workspace
            .stage_content(&directory, b"patched\n", Some(expected.clone()))
            .expect("content is staged");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
            .expect("concurrent mode is set");

        let error = workspace
            .commit_existing_file(
                &directory,
                &staged,
                std::path::Path::new("target.txt"),
                expected,
            )
            .expect_err("concurrent mode change must conflict");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);

        drop((staged, directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
