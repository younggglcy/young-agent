use std::error::Error;
use std::fmt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use cap_std::ambient_authority;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use cap_std::fs::Permissions;
use cap_std::fs::{Dir, DirEntry, File, Metadata, OpenOptions};
use serde_json::{json, Value};

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
use std::cell::Cell;

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
thread_local! {
    static INJECT_RECOVERY_POLICY_FAILURE_AFTER_EXCHANGE: Cell<bool> = const { Cell::new(false) };
    static INJECT_NEW_FILE_VALIDATION_FAILURE_AFTER_RENAME: Cell<bool> = const { Cell::new(false) };
    static INJECT_RECOVERY_NAMESPACE_SWAP_AFTER_CLEANUP_MOVE: Cell<bool> = const { Cell::new(false) };
    static INJECT_RECOVERY_CONTENT_REPLACEMENT_AFTER_EXCHANGE: Cell<bool> = const { Cell::new(false) };
    static INJECT_RECOVERY_CONTENT_REPLACEMENT_BEFORE_EXCHANGE_FAILURE: Cell<bool> = const { Cell::new(false) };
    static INJECT_RECOVERY_SECURITY_METADATA_AFTER_EXCHANGE: Cell<bool> = const { Cell::new(false) };
    static INJECT_RECOVERY_SYMLINK_AFTER_EXCHANGE: Cell<bool> = const { Cell::new(false) };
}

pub(crate) const MAX_FILE_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const RECOVERY_DIRECTORY: &str = ".young-agent-recovery";

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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn file_snapshot(file: &File) -> io::Result<FileSnapshot> {
        #[cfg(unix)]
        {
            let before = Self::begin_file_snapshot(file)?;
            let digest = file_digest(file, before.size)?;
            let after = file_stat_snapshot(&file.metadata()?);
            if before != after {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "file changed while its content snapshot was being captured",
                ));
            }
            Ok(FileSnapshot {
                stat: after,
                digest,
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

    #[cfg(unix)]
    pub(crate) fn begin_file_snapshot(file: &File) -> io::Result<FileStatSnapshot> {
        let before = file_stat_snapshot(&file.metadata()?);
        if before.size > MAX_FILE_SNAPSHOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("safe file snapshots are limited to {MAX_FILE_SNAPSHOT_BYTES} bytes"),
            ));
        }
        Ok(before)
    }

    #[cfg(unix)]
    pub(crate) fn finish_file_snapshot_from_content(
        file: &File,
        before: FileStatSnapshot,
        content: &[u8],
    ) -> io::Result<FileSnapshot> {
        if content.len() as u64 != before.size {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file length did not match the captured content",
            ));
        }
        let after = file_stat_snapshot(&file.metadata()?);
        if before != after {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file changed while its content snapshot was being captured",
            ));
        }
        Ok(FileSnapshot {
            stat: after,
            digest: *blake3::hash(content).as_bytes(),
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn begin_file_snapshot(_file: &File) -> io::Result<FileStatSnapshot> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe file snapshots are not supported on this platform",
        ))
    }

    #[cfg(not(unix))]
    pub(crate) fn finish_file_snapshot_from_content(
        _file: &File,
        _before: FileStatSnapshot,
        _content: &[u8],
    ) -> io::Result<FileSnapshot> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe file snapshots are not supported on this platform",
        ))
    }

    pub(crate) fn open_dir(&self, path: &Path) -> io::Result<Dir> {
        self.root_dir.open_dir(path)
    }

    pub(crate) fn bind_command_working_directory(&self, command: &mut Command) -> io::Result<()> {
        bind_process_working_directory(command, &self.root_dir)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn replace_existing_atomically(
        &self,
        path: &Path,
        content: &[u8],
        expected_snapshot: FileSnapshot,
    ) -> Result<Option<PathBuf>, AtomicReplaceError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "patch target has no file name")
            })
            .map_err(|source| AtomicReplaceError::before_within(source, parent))?;
        let directory = self
            .root_dir
            .open_dir(parent)
            .map_err(|source| AtomicReplaceError::before_within(source, parent))?;
        let metadata = self
            .replacement_metadata(&directory, Path::new(file_name), expected_snapshot)
            .map_err(|source| AtomicReplaceError::before_within(source, parent))?;
        if metadata.snapshot != expected_snapshot {
            return Err(AtomicReplaceError::before_within(
                patch_target_changed(),
                parent,
            ));
        }
        let recovery_directory = open_recovery_namespace(&directory)
            .map_err(|source| AtomicReplaceError::before_within(source, parent))?;
        let staged = self
            .stage_content(&directory, content, Some(&metadata))
            .map_err(|source| AtomicReplaceError::before_within(source, parent))?;
        let recovery = self
            .commit_existing_file(
                &directory,
                &recovery_directory,
                staged,
                Path::new(file_name),
                metadata,
            )
            .map_err(|error| match error {
                CommitError::BeforePublication { source, recovery } => {
                    AtomicReplaceError::BeforePublication {
                        source,
                        recovery: recovery.map(|recovery| recovery.within_parent(parent)),
                    }
                }
                CommitError::Published { source, recovery } => AtomicReplaceError::Published {
                    source,
                    target: path.to_path_buf(),
                    recovery: recovery.within_parent(parent),
                },
            })?;
        Ok(Some(parent.join(recovery)))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn replace_existing_atomically(
        &self,
        path: &Path,
        _content: &[u8],
        _expected_snapshot: FileSnapshot,
    ) -> Result<Option<PathBuf>, AtomicReplaceError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Err(AtomicReplaceError::before_within(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "safe atomic replacement is not supported on this platform",
            ),
            parent,
        ))
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn create_new(&self, path: &Path, content: &[u8]) -> Result<(), AtomicCreateError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "patch target has no file name")
            })
            .map_err(|source| AtomicCreateError::before_within(source, parent))?;
        let directory = self
            .root_dir
            .open_dir(parent)
            .map_err(|source| AtomicCreateError::before_within(source, parent))?;
        let staged = self
            .stage_content(&directory, content, None)
            .map_err(|source| AtomicCreateError::before_within(source, parent))?;
        self.commit_new_file(&directory, staged, Path::new(file_name))
            .map_err(|error| match error {
                CommitNewError::BeforePublication(source) => {
                    AtomicCreateError::before_within(source, parent)
                }
                CommitNewError::Published(source) => AtomicCreateError::Published {
                    source,
                    target: path.to_path_buf(),
                },
            })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn create_new(
        &self,
        _path: &Path,
        _content: &[u8],
    ) -> Result<(), AtomicCreateError> {
        Err(AtomicCreateError::before_within(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic no-replace patch commits are not supported on this platform",
            ),
            Path::new("."),
        ))
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn stage_content(
        &self,
        directory: &Dir,
        content: &[u8],
        replacement: Option<&ReplacementMetadata>,
    ) -> io::Result<StagedFile> {
        use std::io::Write;

        validate_staging_content_size(content.len())?;
        for _ in 0..100 {
            let candidate = random_patch_path("stage")?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
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
                if let Some(replacement) = replacement {
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
                if let Some(replacement) = replacement {
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
                validate_security_metadata(&file, "patch staging file")?;
                Ok(())
            })();
            if let Err(source) = result {
                return cleanup_open_staging_after_error(directory, &candidate, &file, source);
            }
            let snapshot = match Self::begin_file_snapshot(&file)
                .and_then(|before| Self::finish_file_snapshot_from_content(&file, before, content))
            {
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn commit_new_file(
        &self,
        directory: &Dir,
        mut staged: StagedFile,
        path: &Path,
    ) -> Result<(), CommitNewError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if let Err(source) = self.validate_staging_slot(directory, &staged) {
                return cleanup_owned_staging_after_error(directory, &staged, source)
                    .map_err(CommitNewError::BeforePublication);
            }
            let result = rustix::fs::renameat_with(
                directory,
                &staged.path,
                directory,
                path,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from);
            if let Err(source) = result {
                return cleanup_owned_staging_after_error(directory, &staged, source)
                    .map_err(CommitNewError::BeforePublication);
            }
            #[cfg(test)]
            if INJECT_NEW_FILE_VALIDATION_FAILURE_AFTER_RENAME
                .with(|injected| injected.replace(false))
            {
                return Err(CommitNewError::Published(io::Error::other(
                    "injected new-file validation failure after publication",
                )));
            }
            if let Err(source) = refresh_snapshot_after_rename(
                &staged.file,
                &mut staged.snapshot,
                "patch staging file",
            ) {
                return Err(CommitNewError::Published(published_target_changed(
                    path, None, source,
                )));
            }
            if let Err(source) = validate_installed_staging_slot(directory, path, &staged) {
                return Err(CommitNewError::Published(published_target_changed(
                    path, None, source,
                )));
            }
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (directory, staged, path);
            Err(CommitNewError::BeforePublication(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic no-replace patch commits are not supported on this platform",
            )))
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn commit_existing_file(
        &self,
        directory: &Dir,
        recovery: &RecoveryNamespace,
        mut staged: StagedFile,
        path: &Path,
        mut expected: ReplacementMetadata,
    ) -> Result<PathBuf, CommitError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if let Err(source) = self
                .validate_staging_slot(directory, &staged)
                .and_then(|()| validate_replacement_slot(directory, path, &expected))
            {
                return cleanup_owned_staging_after_error(directory, &staged, source)
                    .map_err(CommitError::before);
            }
            let recovery_path = match move_to_recovery_namespace(
                directory,
                &staged.path,
                &recovery.directory,
                "displaced",
            ) {
                Ok(path) => path,
                Err(source) => {
                    return cleanup_owned_staging_after_error(directory, &staged, source)
                        .map_err(CommitError::before)
                }
            };
            staged.path = recovery_path;
            if let Err(source) = refresh_snapshot_after_rename(
                &staged.file,
                &mut staged.snapshot,
                "patch staging file",
            )
            .and_then(|()| {
                validate_installed_staging_slot(&recovery.directory, &staged.path, &staged)
            }) {
                return Err(preserved_content_failure(
                    directory, recovery, &staged, source,
                ));
            }
            if let Err(error) = validate_recovery_namespace_slot(directory, recovery) {
                return Err(preserved_commit_failure(
                    directory,
                    recovery,
                    &staged,
                    recovery_namespace_error_source(error),
                ));
            }
            #[cfg(test)]
            let exchange = if INJECT_RECOVERY_CONTENT_REPLACEMENT_BEFORE_EXCHANGE_FAILURE
                .with(|injected| injected.replace(false))
            {
                inject_replaced_recovery_content(
                    &recovery.directory,
                    &staged.path,
                    "moved-staging-before-exchange",
                )
                .and_then(|()| Err(io::Error::other("injected exchange failure")))
            } else {
                exchange_files_between(&recovery.directory, &staged.path, directory, path)
            };
            #[cfg(not(test))]
            let exchange =
                exchange_files_between(&recovery.directory, &staged.path, directory, path);
            if let Err(source) = exchange {
                return Err(preserved_commit_failure(
                    directory, recovery, &staged, source,
                ));
            }
            #[cfg(test)]
            if INJECT_RECOVERY_POLICY_FAILURE_AFTER_EXCHANGE
                .with(|injected| injected.replace(false))
            {
                if let Err(source) = inject_invalid_recovery_policy(&recovery.directory) {
                    return Err(published_commit_failure(
                        directory, recovery, &staged, &expected, source,
                    ));
                }
            }
            #[cfg(test)]
            if INJECT_RECOVERY_CONTENT_REPLACEMENT_AFTER_EXCHANGE
                .with(|injected| injected.replace(false))
            {
                if let Err(source) = inject_replaced_recovery_content(
                    &recovery.directory,
                    &staged.path,
                    "moved-original-after-exchange",
                ) {
                    return Err(published_commit_failure(
                        directory, recovery, &staged, &expected, source,
                    ));
                }
            }
            #[cfg(test)]
            if INJECT_RECOVERY_SECURITY_METADATA_AFTER_EXCHANGE
                .with(|injected| injected.replace(false))
            {
                if let Err(source) = inject_recovery_extended_attribute(&expected.file) {
                    return Err(published_commit_failure(
                        directory, recovery, &staged, &expected, source,
                    ));
                }
            }
            #[cfg(test)]
            if INJECT_RECOVERY_SYMLINK_AFTER_EXCHANGE.with(|injected| injected.replace(false)) {
                if let Err(source) =
                    inject_symlink_recovery_content(&recovery.directory, &staged.path)
                {
                    return Err(published_commit_failure(
                        directory, recovery, &staged, &expected, source,
                    ));
                }
            }
            if let Err(source) = refresh_snapshot_after_rename(
                &staged.file,
                &mut staged.snapshot,
                "patch staging file",
            )
            .and_then(|()| refresh_replacement_after_rename(&mut expected))
            {
                return Err(published_commit_failure(
                    directory, recovery, &staged, &expected, source,
                ));
            }

            let validation =
                validate_installed_staging_slot(directory, path, &staged).and_then(|()| {
                    validate_replacement_slot_identity(&recovery.directory, &staged.path, &expected)
                });
            if let Err(source) = validation {
                return Err(published_commit_failure(
                    directory, recovery, &staged, &expected, source,
                ));
            }
            if let Err(error) = validate_recovery_namespace_slot(directory, recovery) {
                return Err(published_commit_failure(
                    directory,
                    recovery,
                    &staged,
                    &expected,
                    recovery_namespace_error_source(error),
                ));
            }
            Ok(PathBuf::from(RECOVERY_DIRECTORY).join(staged.path))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (directory, recovery, staged, path, expected);
            Err(CommitError::before(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic identity-bound patch commits are not supported on this platform",
            )))
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn validate_staging_slot(&self, directory: &Dir, staged: &StagedFile) -> io::Result<()> {
        validate_security_metadata(&staged.file, "patch staging file")?;
        validate_retained_snapshot_metadata(
            &staged.file,
            &staged.snapshot,
            "patch staging file changed before commit",
        )?;
        validate_snapshot_slot(
            directory,
            &staged.path,
            &staged.snapshot,
            "patch staging file",
            "patch staging file changed before commit",
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn replacement_metadata(
        &self,
        directory: &Dir,
        path: &Path,
        expected_snapshot: FileSnapshot,
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
            validate_security_metadata(&file, "atomic patch target")?;
            let metadata = file.metadata()?;
            if metadata.nlink() != 1 || file_stat_snapshot(&metadata) != expected_snapshot.stat {
                return Err(patch_target_changed());
            }
            validate_security_metadata(&file, "atomic patch target")?;
            let final_metadata = file.metadata()?;
            if final_metadata.nlink() != 1
                || file_stat_snapshot(&final_metadata) != expected_snapshot.stat
            {
                return Err(patch_target_changed());
            }
            Ok(ReplacementMetadata {
                permissions: final_metadata.permissions(),
                snapshot: expected_snapshot,
                file,
                uid: final_metadata.uid(),
                gid: final_metadata.gid(),
                mode: final_metadata.mode(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (directory, path, expected_snapshot);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "safe replacement metadata validation is not supported on this platform",
            ))
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ReplacementMetadata {
    permissions: Permissions,
    snapshot: FileSnapshot,
    file: File,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct StagedFile {
    path: PathBuf,
    file: File,
    snapshot: FileSnapshot,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct RecoveryNamespace {
    directory: Dir,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

pub(crate) enum AtomicReplaceError {
    BeforePublication {
        source: io::Error,
        recovery: Option<PublishedRecovery>,
    },
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        expect(
            dead_code,
            reason = "shared patch error contract; unsupported targets fail before publication"
        )
    )]
    Published {
        source: io::Error,
        target: PathBuf,
        recovery: PublishedRecovery,
    },
}

impl AtomicReplaceError {
    fn before_within(source: io::Error, parent: &Path) -> Self {
        let recovery = preserved_recovery_from_io_error(&source)
            .map(|recovery| recovery.within_parent(parent));
        Self::BeforePublication { source, recovery }
    }
}

pub(crate) enum AtomicCreateError {
    BeforePublication {
        source: io::Error,
        recovery: Option<PublishedRecovery>,
    },
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        expect(
            dead_code,
            reason = "shared patch error contract; unsupported targets fail before publication"
        )
    )]
    Published { source: io::Error, target: PathBuf },
}

impl AtomicCreateError {
    fn before_within(source: io::Error, parent: &Path) -> Self {
        let recovery = preserved_recovery_from_io_error(&source)
            .map(|recovery| recovery.within_parent(parent));
        Self::BeforePublication { source, recovery }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
impl AtomicCreateError {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::BeforePublication { source, .. } | Self::Published { source, .. } => {
                source.kind()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PublishedRecovery {
    LocatedVerified(PathBuf),
    LocatedContentUnverified(PathBuf),
    LocatedPolicyUnverified(PathBuf),
    LocatedContentAndPolicyUnverified(PathBuf),
    NotApplicableNewFile,
    Unlocated,
}

impl PublishedRecovery {
    fn within_parent(self, parent: &Path) -> Self {
        match self {
            Self::LocatedVerified(path) => Self::LocatedVerified(parent.join(path)),
            Self::LocatedContentUnverified(path) => {
                Self::LocatedContentUnverified(parent.join(path))
            }
            Self::LocatedPolicyUnverified(path) => Self::LocatedPolicyUnverified(parent.join(path)),
            Self::LocatedContentAndPolicyUnverified(path) => {
                Self::LocatedContentAndPolicyUnverified(parent.join(path))
            }
            Self::NotApplicableNewFile => Self::NotApplicableNewFile,
            Self::Unlocated => Self::Unlocated,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum CommitNewError {
    BeforePublication(io::Error),
    Published(io::Error),
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
impl CommitNewError {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::BeforePublication(source) | Self::Published(source) => source.kind(),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum CommitError {
    BeforePublication {
        source: io::Error,
        recovery: Option<PublishedRecovery>,
    },
    Published {
        source: io::Error,
        recovery: PublishedRecovery,
    },
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl CommitError {
    fn before(source: io::Error) -> Self {
        let recovery = preserved_recovery_from_io_error(&source);
        Self::BeforePublication { source, recovery }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
impl CommitError {
    fn kind(&self) -> io::ErrorKind {
        match self {
            Self::BeforePublication { source, .. } | Self::Published { source, .. } => {
                source.kind()
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum RecoveryNamespaceError {
    Identity(io::Error),
    Policy(io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    #[cfg(unix)]
    stat: FileStatSnapshot,
    #[cfg(unix)]
    digest: [u8; 32],
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStatSnapshot {
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
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    mode: u32,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStatSnapshot;

#[cfg(unix)]
fn file_stat_snapshot(metadata: &Metadata) -> FileStatSnapshot {
    use cap_std::fs::MetadataExt as _;

    FileStatSnapshot {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", all(test, unix)))]
fn file_digest(file: &File, size: u64) -> io::Result<[u8; 32]> {
    use cap_std::fs::FileExt as _;

    let mut hasher = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while offset < size {
        let remaining = usize::try_from((size - offset).min(buffer.len() as u64))
            .expect("bounded digest chunk fits usize");
        let bytes_read = file.read_at(&mut buffer[..remaining], offset)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file became shorter while its content snapshot was being captured",
            ));
        }
        hasher.update(&buffer[..bytes_read]);
        offset = offset.saturating_add(bytes_read as u64);
    }
    let mut probe = [0u8; 1];
    if file.read_at(&mut probe, size)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file grew while its content snapshot was being captured",
        ));
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl FileSnapshot {
    fn same_payload_and_metadata(&self, other: &Self) -> bool {
        self.digest == other.digest
            && self.stat.device == other.stat.device
            && self.stat.inode == other.stat.inode
            && self.stat.size == other.stat.size
            && self.stat.modified_seconds == other.stat.modified_seconds
            && self.stat.modified_nanoseconds == other.stat.modified_nanoseconds
            && self.stat.uid == other.stat.uid
            && self.stat.gid == other.stat.gid
            && self.stat.mode == other.stat.mode
    }

    fn matches_metadata(&self, metadata: &Metadata) -> bool {
        self.stat == file_stat_snapshot(metadata)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_security_metadata(file: &File, subject: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        if file_has_extended_attributes(file)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{subject} has unsupported extended attributes"),
            ));
        }
        if has_extended_acl(file)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{subject} has an unsupported extended ACL"),
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, subject);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe patch security metadata validation is not supported on this platform",
        ))
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn validate_staging_content_size(size: usize) -> io::Result<()> {
    if size as u64 > MAX_FILE_SNAPSHOT_BYTES {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("patch result exceeds {MAX_FILE_SNAPSHOT_BYTES} bytes"),
        ))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refresh_snapshot_after_rename(
    file: &File,
    snapshot: &mut FileSnapshot,
    subject: &str,
) -> io::Result<()> {
    validate_security_metadata(file, subject)?;
    let refreshed = CodingWorkspace::file_snapshot(file)?;
    if !refreshed.same_payload_and_metadata(snapshot) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{subject} changed during an atomic rename"),
        ));
    }
    *snapshot = refreshed;
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refresh_replacement_after_rename(expected: &mut ReplacementMetadata) -> io::Result<()> {
    refresh_snapshot_after_rename(
        &expected.file,
        &mut expected.snapshot,
        "atomic patch target",
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_replacement_slot_identity(
    directory: &Dir,
    path: &Path,
    expected: &ReplacementMetadata,
) -> io::Result<()> {
    validate_snapshot_slot(
        directory,
        path,
        &expected.snapshot,
        "atomic patch target",
        "patch target changed during commit",
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_replacement_slot(
    directory: &Dir,
    path: &Path,
    expected: &ReplacementMetadata,
) -> io::Result<()> {
    validate_security_metadata(&expected.file, "atomic patch target")?;
    if CodingWorkspace::file_snapshot(&expected.file)? != expected.snapshot {
        return Err(patch_target_changed());
    }
    validate_replacement_slot_identity(directory, path, expected)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_retained_snapshot_metadata(
    file: &File,
    expected: &FileSnapshot,
    changed_message: &str,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        if file_stat_snapshot(&file.metadata()?) == expected.stat {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, changed_message))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, expected, changed_message);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe retained snapshot validation is not supported on this platform",
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_installed_staging_slot(
    directory: &Dir,
    path: &Path,
    staged: &StagedFile,
) -> io::Result<()> {
    validate_snapshot_slot(
        directory,
        path,
        &staged.snapshot,
        "installed patch file",
        "patch target changed during commit; automatic cleanup was skipped",
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_snapshot_slot(
    directory: &Dir,
    path: &Path,
    expected: &FileSnapshot,
    subject: &str,
    changed_message: &str,
) -> io::Result<()> {
    let entry_before = directory.symlink_metadata(path)?;
    if !entry_before.is_file() || entry_before.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, changed_message));
    }
    let (slot, metadata) =
        require_regular_file(directory.open_with(path, &regular_file_options())?)?;
    let entry_after = directory.symlink_metadata(path)?;
    if !entry_after.is_file() || entry_after.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, changed_message));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;

        if entry_before.dev() != metadata.dev()
            || entry_before.ino() != metadata.ino()
            || entry_after.dev() != metadata.dev()
            || entry_after.ino() != metadata.ino()
        {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, changed_message));
        }
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic patch refuses files with multiple hard links",
            ));
        }
    }
    validate_security_metadata(&slot, subject)?;
    let after = slot.metadata()?;
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;

        if after.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic patch refuses files with multiple hard links",
            ));
        }
    }
    if expected.matches_metadata(&after) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::WouldBlock, changed_message))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn random_patch_path(kind: &str) -> io::Result<PathBuf> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|source| io::Error::other(format!("failed to generate patch nonce: {source}")))?;
    let mut name = format!(".young-agent-patch-{kind}-");
    for byte in random {
        write!(&mut name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name.push_str(".tmp");
    Ok(PathBuf::from(name))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn claim_path(directory: &Dir, path: &Path, kind: &str) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let claim = random_patch_path(kind)?;
        match rustix::fs::renameat_with(
            directory,
            path,
            directory,
            &claim,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => return Ok(claim),
            Err(source) if io::Error::from(source).kind() == io::ErrorKind::AlreadyExists => {
                continue
            }
            Err(source) => return Err(io::Error::from(source)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique patch recovery path",
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn move_claimed_to_recovery_namespace(
    directory: &Dir,
    claimed_path: &Path,
    kind: &str,
) -> io::Result<(RecoveryNamespace, PathBuf)> {
    let recovery = open_recovery_namespace(directory)?;
    let destination =
        move_to_recovery_namespace(directory, claimed_path, &recovery.directory, kind)?;
    Ok((recovery, destination))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn move_to_recovery_namespace(
    directory: &Dir,
    source_path: &Path,
    recovery: &Dir,
    kind: &str,
) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let destination = random_patch_path(kind)?;
        match rustix::fs::renameat_with(
            directory,
            source_path,
            recovery,
            &destination,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => return Ok(destination),
            Err(source) if io::Error::from(source).kind() == io::ErrorKind::AlreadyExists => {
                continue
            }
            Err(source) => return Err(io::Error::from(source)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique namespaced recovery path",
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_recovery_namespace(directory: &Dir) -> io::Result<RecoveryNamespace> {
    use cap_std::fs::MetadataExt as _;

    match directory.create_dir(RECOVERY_DIRECTORY) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(source),
    }
    let recovery = open_recovery_directory_slot(directory)?;
    ensure_recovery_gitignore(&recovery)?;
    let metadata = recovery.metadata(".")?;
    Ok(RecoveryNamespace {
        directory: recovery,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_recovery_namespace_slot(
    parent: &Dir,
    recovery: &RecoveryNamespace,
) -> Result<(), RecoveryNamespaceError> {
    use cap_std::fs::MetadataExt as _;

    let slot = open_recovery_directory_slot(parent).map_err(|source| {
        RecoveryNamespaceError::Identity(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("recovery namespace path changed ({source})"),
        ))
    })?;
    let slot_metadata = slot.metadata(".").map_err(|source| {
        RecoveryNamespaceError::Identity(io::Error::new(
            source.kind(),
            format!("failed to inspect recovery namespace slot ({source})"),
        ))
    })?;
    let handle_metadata = recovery.directory.metadata(".").map_err(|source| {
        RecoveryNamespaceError::Identity(io::Error::new(
            source.kind(),
            format!("failed to inspect retained recovery namespace ({source})"),
        ))
    })?;
    if slot_metadata.dev() != recovery.device
        || slot_metadata.ino() != recovery.inode
        || handle_metadata.dev() != recovery.device
        || handle_metadata.ino() != recovery.inode
    {
        return Err(RecoveryNamespaceError::Identity(io::Error::new(
            io::ErrorKind::WouldBlock,
            "recovery namespace identity changed",
        )));
    }
    ensure_recovery_gitignore(&recovery.directory).map_err(RecoveryNamespaceError::Policy)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn preserved_recovery_evidence(
    parent: &Dir,
    recovery: &RecoveryNamespace,
    path: &Path,
    file: &File,
    expected: FileSnapshot,
) -> PublishedRecovery {
    let policy_before = match validate_recovery_namespace_slot(parent, recovery) {
        Ok(()) => true,
        Err(RecoveryNamespaceError::Policy(_)) => false,
        Err(RecoveryNamespaceError::Identity(_)) => return PublishedRecovery::Unlocated,
    };
    let content = inspect_preserved_recovery_content(recovery, path, file, expected);
    if matches!(content, RecoveryContentEvidence::Mismatch) {
        return PublishedRecovery::Unlocated;
    }
    let policy_after = match validate_recovery_namespace_slot(parent, recovery) {
        Ok(()) => true,
        Err(RecoveryNamespaceError::Policy(_)) => false,
        Err(RecoveryNamespaceError::Identity(_)) => return PublishedRecovery::Unlocated,
    };
    let policy_verified = policy_before && policy_after;
    let located = PathBuf::from(RECOVERY_DIRECTORY).join(path);
    match (content, policy_verified) {
        (RecoveryContentEvidence::Verified, true) => PublishedRecovery::LocatedVerified(located),
        (RecoveryContentEvidence::InspectionFailed, true) => {
            PublishedRecovery::LocatedContentUnverified(located)
        }
        (RecoveryContentEvidence::Verified, false) => {
            PublishedRecovery::LocatedPolicyUnverified(located)
        }
        (RecoveryContentEvidence::InspectionFailed, false) => {
            PublishedRecovery::LocatedContentAndPolicyUnverified(located)
        }
        (RecoveryContentEvidence::Mismatch, _) => {
            unreachable!("content mismatches return before path publication")
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Clone, Copy)]
enum RecoveryContentEvidence {
    Verified,
    InspectionFailed,
    Mismatch,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn inspect_preserved_recovery_content(
    recovery: &RecoveryNamespace,
    path: &Path,
    file: &File,
    mut expected: FileSnapshot,
) -> RecoveryContentEvidence {
    if let Err(source) =
        refresh_snapshot_after_rename(file, &mut expected, "preserved patch staging file")
    {
        return recovery_content_failure_kind(&source);
    }
    match validate_snapshot_slot(
        &recovery.directory,
        path,
        &expected,
        "preserved patch staging file",
        "preserved patch staging entry changed during recovery",
    ) {
        Ok(()) => RecoveryContentEvidence::Verified,
        Err(source) => recovery_content_failure_kind(&source),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn recovery_content_failure_kind(source: &io::Error) -> RecoveryContentEvidence {
    if source.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
        return RecoveryContentEvidence::Mismatch;
    }
    match source.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => {
            RecoveryContentEvidence::Mismatch
        }
        _ => RecoveryContentEvidence::InspectionFailed,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn preserved_content_failure(
    parent: &Dir,
    recovery: &RecoveryNamespace,
    staged: &StagedFile,
    source: io::Error,
) -> CommitError {
    CommitError::BeforePublication {
        source,
        recovery: Some(preserved_recovery_evidence(
            parent,
            recovery,
            &staged.path,
            &staged.file,
            staged.snapshot,
        )),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn preserved_commit_failure(
    parent: &Dir,
    recovery: &RecoveryNamespace,
    staged: &StagedFile,
    source: io::Error,
) -> CommitError {
    CommitError::BeforePublication {
        source,
        recovery: Some(preserved_recovery_evidence(
            parent,
            recovery,
            &staged.path,
            &staged.file,
            staged.snapshot,
        )),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn published_commit_failure(
    parent: &Dir,
    recovery: &RecoveryNamespace,
    staged: &StagedFile,
    expected: &ReplacementMetadata,
    source: io::Error,
) -> CommitError {
    CommitError::Published {
        source,
        recovery: preserved_recovery_evidence(
            parent,
            recovery,
            &staged.path,
            &expected.file,
            expected.snapshot,
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn recovery_namespace_error_source(error: RecoveryNamespaceError) -> io::Error {
    match error {
        RecoveryNamespaceError::Policy(source) | RecoveryNamespaceError::Identity(source) => source,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_recovery_directory_slot(parent: &Dir) -> io::Result<Dir> {
    use cap_std::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        (rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK)
            .bits() as i32,
    );
    let file = parent.open_with(RECOVERY_DIRECTORY, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("recovery namespace '{RECOVERY_DIRECTORY}' is not a real directory"),
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ensure_recovery_gitignore(recovery: &Dir) -> io::Result<()> {
    use std::io::{Read as _, Write as _};

    const CONTENT: &[u8] = b"*\n";
    let mut create = OpenOptions::new();
    create.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        create.mode(0o600);
    }
    match recovery.open_with(".gitignore", &create) {
        Ok(mut file) => {
            file.write_all(CONTENT)?;
            file.sync_all()?;
            Ok(())
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let (file, _) =
                require_regular_file(recovery.open_with(".gitignore", &regular_file_options())?)?;
            let mut content = Vec::with_capacity(CONTENT.len() + 1);
            file.take((CONTENT.len() + 1) as u64)
                .read_to_end(&mut content)?;
            if content == CONTENT {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recovery namespace .gitignore has unexpected content",
                ))
            }
        }
        Err(source) => Err(source),
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn inject_invalid_recovery_policy(recovery: &Dir) -> io::Result<()> {
    use std::io::Write as _;

    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    let mut file = recovery.open_with(".gitignore", &options)?;
    file.write_all(b"unexpected\n")?;
    file.sync_all()
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn inject_replaced_recovery_content(
    recovery: &Dir,
    path: &Path,
    moved_name: &str,
) -> io::Result<()> {
    use std::io::Write as _;

    recovery.rename(path, recovery, moved_name)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut replacement = recovery.open_with(path, &options)?;
    replacement.write_all(b"concurrent replacement\n")?;
    replacement.sync_all()
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn inject_recovery_extended_attribute(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let attribute = "com.young-agent.concurrent-recovery";
    #[cfg(target_os = "linux")]
    let attribute = "user.young-agent.concurrent-recovery";
    rustix::fs::fsetxattr(
        file,
        attribute,
        b"injected",
        rustix::fs::XattrFlags::empty(),
    )
    .map_err(io::Error::from)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn inject_symlink_recovery_content(recovery: &Dir, path: &Path) -> io::Result<()> {
    const MOVED_ORIGINAL: &str = "moved-original-before-recovery-symlink";
    recovery.rename(path, recovery, MOVED_ORIGINAL)?;
    rustix::fs::symlinkat(MOVED_ORIGINAL, recovery, path).map_err(io::Error::from)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_no_replace(directory: &Dir, from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        from,
        directory,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn patch_target_changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "patch target changed while the patch was being applied",
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn published_target_changed(
    target: &Path,
    recovery: Option<&Path>,
    source: io::Error,
) -> io::Error {
    let recovery = recovery.map_or_else(String::new, |path| {
        format!("; displaced data was preserved as '{}'", path.display())
    });
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "patch was atomically published at '{}', but post-commit validation failed ({source}); the published target was preserved{recovery}",
            target.display()
        ),
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn claimed_staging_matches(
    directory: &Dir,
    claimed_path: &Path,
    staging_file: &File,
    expected: FileSnapshot,
) -> io::Result<bool> {
    let (claimed, _) =
        require_regular_file(directory.open_with(claimed_path, &regular_file_options())?)?;
    if validate_security_metadata(staging_file, "patch staging file").is_err()
        || validate_security_metadata(&claimed, "claimed patch staging file").is_err()
    {
        return Ok(false);
    }
    let retained = CodingWorkspace::file_snapshot(staging_file)?;
    let claimed_snapshot = CodingWorkspace::file_snapshot(&claimed)?;
    Ok(retained.same_payload_and_metadata(&expected)
        && claimed_snapshot.same_payload_and_metadata(&expected))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cleanup_open_staging_after_error<T>(
    directory: &Dir,
    staging_path: &Path,
    staging_file: &File,
    source: io::Error,
) -> io::Result<T> {
    let expected = CodingWorkspace::file_snapshot(staging_file).map_err(|validation| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "patch operation failed ({source}); staging file '{}' could not be validated and was preserved ({validation})",
                staging_path.display()
            ),
        )
    })?;
    cleanup_expected_staging_after_error(directory, staging_path, staging_file, expected, source)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cleanup_expected_staging_after_error<T>(
    directory: &Dir,
    staging_path: &Path,
    staging_file: &File,
    expected: FileSnapshot,
    source: io::Error,
) -> io::Result<T> {
    let claimed = match claim_path(directory, staging_path, "cleanup") {
        Ok(path) => path,
        Err(claim) if claim.kind() == io::ErrorKind::NotFound => return Err(source),
        Err(claim) => {
            return Err(io::Error::new(
                claim.kind(),
                format!(
                    "patch operation failed ({source}); staging path '{}' could not be claimed for cleanup ({claim})",
                    staging_path.display()
                ),
            ))
        }
    };
    match claimed_staging_matches(directory, &claimed, staging_file, expected) {
        Ok(true) => {}
        Ok(false) => {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            restore_claimed_path(
                directory,
                &claimed,
                staging_path,
                &format!("patch operation failed ({source}); staging identity changed"),
            )?;
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "patch operation failed ({source}); concurrent staging entry was restored as '{}'",
                    staging_path.display()
                ),
            ));
        }
        Err(validation) => {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            restore_claimed_path(
                directory,
                &claimed,
                staging_path,
                &format!(
                    "patch operation failed ({source}); staging ownership was indeterminate ({validation})"
                ),
            )?;
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "patch operation failed ({source}); indeterminate staging entry was restored as '{}' ({validation})",
                    staging_path.display()
                ),
            ));
        }
    }
    let (recovery_namespace, recovery_path) =
        move_claimed_to_recovery_namespace(directory, &claimed, "failed")
        .map_err(|recovery| {
            io::Error::new(
                recovery.kind(),
                format!(
                    "patch operation failed ({source}); owned staging data remained at '{}' because it could not enter the recovery namespace ({recovery})",
                    claimed.display()
                ),
            )
        })?;
    #[cfg(test)]
    if INJECT_RECOVERY_NAMESPACE_SWAP_AFTER_CLEANUP_MOVE.with(|injected| injected.replace(false)) {
        directory.rename(
            RECOVERY_DIRECTORY,
            directory,
            "moved-recovery-after-cleanup",
        )?;
        drop(open_recovery_namespace(directory)?);
    }
    let recovery = preserved_recovery_evidence(
        directory,
        &recovery_namespace,
        &recovery_path,
        staging_file,
        expected,
    );
    Err(io::Error::new(
        source.kind(),
        PreservedStagingError { source, recovery },
    ))
}

#[derive(Debug)]
struct PreservedStagingError {
    source: io::Error,
    recovery: PublishedRecovery,
}

impl fmt::Display for PreservedStagingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "patch operation failed ({})", self.source)?;
        match &self.recovery {
            PublishedRecovery::LocatedVerified(path)
            | PublishedRecovery::LocatedPolicyUnverified(path) => write!(
                formatter,
                "; owned staging data was preserved as recovery file '{}'",
                path.display()
            ),
            PublishedRecovery::LocatedContentUnverified(path)
            | PublishedRecovery::LocatedContentAndPolicyUnverified(path) => write!(
                formatter,
                "; a possible recovery path was retained at '{}', but its content could not be verified",
                path.display()
            ),
            PublishedRecovery::Unlocated => formatter.write_str(
                "; owned staging data was preserved, but its recovery location could not be verified",
            ),
            PublishedRecovery::NotApplicableNewFile => formatter.write_str(
                "; owned staging data has no applicable recovery location",
            ),
        }
    }
}

impl Error for PreservedStagingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

fn preserved_recovery_from_io_error(source: &io::Error) -> Option<PublishedRecovery> {
    source
        .get_ref()
        .and_then(|source| source.downcast_ref::<PreservedStagingError>())
        .map(|source| source.recovery.clone())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cleanup_owned_staging_after_error<T>(
    directory: &Dir,
    staged: &StagedFile,
    source: io::Error,
) -> io::Result<T> {
    cleanup_expected_staging_after_error(
        directory,
        &staged.path,
        &staged.file,
        staged.snapshot,
        source,
    )
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

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn exchange_files(directory: &Dir, left: &Path, right: &Path) -> io::Result<()> {
    exchange_files_between(directory, left, directory, right)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn exchange_files_between(
    left_directory: &Dir,
    left: &Path,
    right_directory: &Dir,
    right: &Path,
) -> io::Result<()> {
    rustix::fs::renameat_with(
        left_directory,
        left,
        right_directory,
        right,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn restore_claimed_path(
    directory: &Dir,
    claimed_path: &Path,
    path: &Path,
    context: &str,
) -> io::Result<()> {
    rename_no_replace(directory, claimed_path, path).map_err(|restore| {
        io::Error::new(
            restore.kind(),
            format!(
                "{context}; '{}' could not be restored to '{}' and was preserved ({restore})",
                claimed_path.display(),
                path.display()
            ),
        )
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn has_extended_acl(file: &File) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "macos")]
    let path = PathBuf::from(
        rustix::fs::getpath(file)
            .map_err(io::Error::from)?
            .to_string_lossy()
            .into_owned(),
    );
    #[cfg(target_os = "linux")]
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let entries = exacl::getfacl(path, None)?;
    #[cfg(target_os = "macos")]
    return Ok(!entries.is_empty());
    #[cfg(target_os = "linux")]
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
    let mut names = vec![0u8; 64 * 1024];
    let written = rustix::fs::flistxattr(file, &mut names).map_err(io::Error::from)?;
    for name in names[..written]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        #[cfg(target_os = "macos")]
        if name == b"com.apple.provenance" {
            // APFS attaches this OS-managed attribute to ordinary new files, including
            // our staging file. It is not user-authored security metadata to preserve.
            continue;
        }
        return Ok(true);
    }
    Ok(false)
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

fn bind_process_working_directory(command: &mut Command, directory: &Dir) -> io::Result<()> {
    young_platform_process::bind_working_directory(command, directory)
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
    #[cfg(unix)]
    let matches = {
        use cap_std::fs::MetadataExt as _;
        use std::os::unix::fs::MetadataExt as _;

        let ambient =
            fs::metadata(root_path).map_err(|source| CodingWorkspaceError::ResolveRoot {
                path: root_path.to_path_buf(),
                source,
            })?;

        opened.dev() == ambient.dev() && opened.ino() == ambient.ino()
    };
    #[cfg(windows)]
    let matches = {
        use cap_fs_ext::MetadataExt as _;

        let ambient = Dir::open_ambient_dir(root_path, ambient_authority())
            .and_then(|directory| directory.metadata("."))
            .map_err(|source| CodingWorkspaceError::ResolveRoot {
                path: root_path.to_path_buf(),
                source,
            })?;
        opened.dev() == ambient.dev() && opened.ino() == ambient.ino()
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
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::git_probe_command;
    #[cfg(unix)]
    use super::CodingWorkspace;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn replacement_metadata(
        workspace: &CodingWorkspace,
        directory: &cap_std::fs::Dir,
        path: &std::path::Path,
    ) -> super::ReplacementMetadata {
        let (target, _) = workspace
            .open_regular_file(path)
            .expect("target opens for its initial snapshot");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("target snapshot reads");
        workspace
            .replacement_metadata(directory, path, snapshot)
            .expect("replacement metadata validates")
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn assert_invalid_recovery_namespace_does_not_publish(root: &std::path::Path) {
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("target snapshot reads");

        workspace
            .replace_existing_atomically(std::path::Path::new("target.txt"), b"patched\n", snapshot)
            .expect_err("invalid recovery namespace must reject the patch");

        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "original\n"
        );
    }

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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn recovery_namespace_file_is_rejected_before_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-file-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join(super::RECOVERY_DIRECTORY), "occupied\n")
            .expect("namespace is occupied by a file");

        assert_invalid_recovery_namespace_does_not_publish(&root);

        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn recovery_namespace_symlink_is_rejected_before_publication() {
        use std::os::unix::fs::symlink;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-symlink-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::create_dir(root.join("other")).expect("symlink target is created");
        symlink("other", root.join(super::RECOVERY_DIRECTORY))
            .expect("recovery namespace symlink is created");

        assert_invalid_recovery_namespace_does_not_publish(&root);

        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn unexpected_recovery_ignore_rule_is_rejected_before_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-ignore-{}-{nonce}",
            std::process::id()
        ));
        let recovery = root.join(super::RECOVERY_DIRECTORY);
        std::fs::create_dir_all(&recovery).expect("recovery namespace is created");
        std::fs::write(recovery.join(".gitignore"), "unexpected\n")
            .expect("unexpected ignore rule is written");

        assert_invalid_recovery_namespace_does_not_publish(&root);

        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn recovery_namespace_identity_drift_is_rejected_before_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-drift-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let metadata =
            replacement_metadata(&workspace, &directory, std::path::Path::new("target.txt"));
        let staged = workspace
            .stage_content(&directory, b"patched\n", Some(&metadata))
            .expect("content is staged");
        let recovery =
            super::open_recovery_namespace(&directory).expect("recovery namespace opens");
        directory
            .rename(super::RECOVERY_DIRECTORY, &directory, "moved-recovery")
            .expect("opened recovery namespace is renamed");
        directory
            .create_dir(super::RECOVERY_DIRECTORY)
            .expect("replacement namespace is created");

        let error = workspace
            .commit_existing_file(
                &directory,
                &recovery,
                staged,
                std::path::Path::new("target.txt"),
                metadata,
            )
            .expect_err("namespace identity drift must reject publication");

        let super::CommitError::BeforePublication {
            recovery: Some(super::PublishedRecovery::Unlocated),
            ..
        } = error
        else {
            panic!("pre-publication namespace drift must retain an unlocated recovery state");
        };

        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "original\n"
        );
        assert!(std::fs::read_dir(root.join("moved-recovery"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".young-agent-patch-displaced-")));

        drop((recovery, directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn post_publication_policy_failure_reports_nested_recovery_state() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-policy-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("nested workspace is created");
        std::fs::write(root.join("src/target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("src/target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("snapshot reads");
        super::INJECT_RECOVERY_POLICY_FAILURE_AFTER_EXCHANGE.with(|injected| injected.set(true));

        let error = workspace
            .replace_existing_atomically(
                std::path::Path::new("src/target.txt"),
                b"patched\n",
                snapshot,
            )
            .expect_err("post-publication policy drift must be structured");

        let super::AtomicReplaceError::Published {
            target, recovery, ..
        } = error
        else {
            panic!("failure must retain published state");
        };
        assert_eq!(target, std::path::Path::new("src/target.txt"));
        let super::PublishedRecovery::LocatedPolicyUnverified(recovery) = recovery else {
            panic!("stable namespace with invalid policy retains an unverified recovery path");
        };
        assert!(recovery.starts_with("src/.young-agent-recovery"));
        assert_eq!(
            std::fs::read_to_string(root.join("src/target.txt")).unwrap(),
            "patched\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(recovery)).unwrap(),
            "original\n"
        );

        drop((target, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn post_publication_recovery_replacement_is_not_reported_as_verified() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-content-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("snapshot reads");
        super::INJECT_RECOVERY_CONTENT_REPLACEMENT_AFTER_EXCHANGE
            .with(|injected| injected.set(true));

        let error = workspace
            .replace_existing_atomically(std::path::Path::new("target.txt"), b"patched\n", snapshot)
            .expect_err("replaced recovery entry must fail post-publication validation");

        let super::AtomicReplaceError::Published {
            recovery: super::PublishedRecovery::Unlocated,
            ..
        } = error
        else {
            panic!("a known recovery mismatch must not expose the replacement as original data");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "patched\n"
        );
        assert_eq!(
            std::fs::read_to_string(
                std::fs::read_dir(root.join(super::RECOVERY_DIRECTORY))
                    .unwrap()
                    .filter_map(Result::ok)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".young-agent-patch-displaced-")
                    })
                    .expect("concurrent replacement remains visible")
                    .path()
            )
            .unwrap(),
            "concurrent replacement\n"
        );
        assert_eq!(
            std::fs::read_to_string(
                root.join(super::RECOVERY_DIRECTORY)
                    .join("moved-original-after-exchange")
            )
            .unwrap(),
            "original\n"
        );

        drop((target, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn post_publication_recovery_security_metadata_is_reported_as_a_candidate() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-security-metadata-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("snapshot reads");
        super::INJECT_RECOVERY_SECURITY_METADATA_AFTER_EXCHANGE.with(|injected| injected.set(true));

        let error = workspace
            .replace_existing_atomically(std::path::Path::new("target.txt"), b"patched\n", snapshot)
            .expect_err("recovery security metadata must fail post-publication validation");

        let super::AtomicReplaceError::Published {
            recovery: super::PublishedRecovery::LocatedContentUnverified(candidate),
            ..
        } = error
        else {
            panic!("security inspection failure must preserve a recovery candidate");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "patched\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(&candidate)).unwrap(),
            "original\n"
        );
        let (candidate_file, _) = workspace
            .open_regular_file(&candidate)
            .expect("candidate remains addressable");
        assert!(super::file_has_extended_attributes(&candidate_file).unwrap());

        drop((candidate_file, target, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn post_publication_recovery_symlink_is_reported_as_unlocated() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-recovery-symlink-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("snapshot reads");
        super::INJECT_RECOVERY_SYMLINK_AFTER_EXCHANGE.with(|injected| injected.set(true));

        let error = workspace
            .replace_existing_atomically(std::path::Path::new("target.txt"), b"patched\n", snapshot)
            .expect_err("recovery symlink replacement must fail post-publication validation");

        let super::AtomicReplaceError::Published {
            recovery: super::PublishedRecovery::Unlocated,
            ..
        } = error
        else {
            panic!("a structural recovery entry mismatch must be unlocated");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "patched\n"
        );
        let recovery = root.join(super::RECOVERY_DIRECTORY);
        let symlink = std::fs::read_dir(&recovery)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".young-agent-patch-displaced-")
            })
            .expect("replacement symlink remains present");
        assert!(std::fs::symlink_metadata(symlink.path())
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(recovery.join("moved-original-before-recovery-symlink"))
                .unwrap(),
            "original\n"
        );

        drop((target, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn exchange_failure_does_not_verify_a_replaced_recovery_entry() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-pre-publication-recovery-content-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (target, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let snapshot = CodingWorkspace::file_snapshot(&target).expect("snapshot reads");
        super::INJECT_RECOVERY_CONTENT_REPLACEMENT_BEFORE_EXCHANGE_FAILURE
            .with(|injected| injected.set(true));

        let error = workspace
            .replace_existing_atomically(std::path::Path::new("target.txt"), b"patched\n", snapshot)
            .expect_err("injected exchange failure must preserve publication state");

        let super::AtomicReplaceError::BeforePublication {
            recovery: Some(super::PublishedRecovery::Unlocated),
            ..
        } = error
        else {
            panic!("a replaced pre-publication recovery entry must be unlocated");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(
                root.join(super::RECOVERY_DIRECTORY)
                    .join("moved-staging-before-exchange")
            )
            .unwrap(),
            "patched\n"
        );

        drop((target, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn create_new_never_removes_an_existing_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-create-new-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("nested test workspace is created");
        std::fs::write(root.join("src/owned.txt"), "concurrent owner\n")
            .expect("existing file is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");

        let error = workspace
            .create_new(std::path::Path::new("src/owned.txt"), b"patch content\n")
            .expect_err("create-new commit must not replace an existing file");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let super::AtomicCreateError::BeforePublication {
            recovery: Some(super::PublishedRecovery::LocatedVerified(recovery)),
            ..
        } = error
        else {
            panic!("preserved create-new staging data must have a structured recovery path");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("src/owned.txt")).unwrap(),
            "concurrent owner\n"
        );
        let recoveries = std::fs::read_dir(root.join("src").join(super::RECOVERY_DIRECTORY))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".young-agent-patch-failed-")
            })
            .collect::<Vec<_>>();
        assert_eq!(recoveries.len(), 1);
        assert_eq!(recoveries[0].path(), root.join(&recovery));
        assert_eq!(
            std::fs::read_to_string(recoveries[0].path()).unwrap(),
            "patch content\n"
        );

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn cleanup_namespace_swap_downgrades_recovery_to_unlocated() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-cleanup-namespace-swap-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("owned.txt"), "concurrent owner\n")
            .expect("existing file is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        super::INJECT_RECOVERY_NAMESPACE_SWAP_AFTER_CLEANUP_MOVE
            .with(|injected| injected.set(true));

        let error = workspace
            .create_new(std::path::Path::new("owned.txt"), b"patch content\n")
            .expect_err("namespace swap must keep publication failure structured");

        let super::AtomicCreateError::BeforePublication {
            recovery: Some(super::PublishedRecovery::Unlocated),
            ..
        } = error
        else {
            panic!("renamed recovery namespace must not retain a verified ambient path");
        };
        assert_eq!(
            std::fs::read_to_string(root.join("owned.txt")).unwrap(),
            "concurrent owner\n"
        );
        assert!(std::fs::read_dir(root.join("moved-recovery-after-cleanup"))
            .unwrap()
            .filter_map(Result::ok)
            .any(
                |entry| std::fs::read_to_string(entry.path()).ok().as_deref()
                    == Some("patch content\n")
            ));

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn new_file_post_publication_failure_retains_published_state() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-create-published-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        super::INJECT_NEW_FILE_VALIDATION_FAILURE_AFTER_RENAME.with(|injected| injected.set(true));

        let error = workspace
            .create_new(std::path::Path::new("created.txt"), b"published\n")
            .expect_err("post-publication validation failure must remain structured");

        let super::AtomicCreateError::Published { target, .. } = error else {
            panic!("failure must retain new-file publication state");
        };
        assert_eq!(target, std::path::Path::new("created.txt"));
        assert_eq!(
            std::fs::read_to_string(root.join("created.txt")).unwrap(),
            "published\n"
        );

        drop(workspace);
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn file_snapshot_digest_detects_equal_length_rewrites() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-digest-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "first\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (file, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let before = CodingWorkspace::file_snapshot(&file).expect("first snapshot succeeds");

        std::fs::write(root.join("target.txt"), "other\n").expect("equal-length rewrite succeeds");
        let after = CodingWorkspace::file_snapshot(&file).expect("second snapshot succeeds");

        assert_eq!(before.stat.size, after.stat.size);
        assert_ne!(before.digest, after.digest);
        drop((file, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(unix)]
    #[test]
    fn file_digest_rejects_growth_past_the_captured_size() {
        use std::io::Write as _;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-growing-digest-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "first\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (file, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");
        let captured_size = file.metadata().unwrap().len();
        let mut appender = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("target.txt"))
            .expect("append handle opens");
        appender.write_all(b"growth\n").expect("target grows");

        let error = super::file_digest(&file, captured_size)
            .expect_err("digest must not chase a moving EOF");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        drop((appender, file, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn file_snapshot_rejects_files_above_the_transaction_limit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-snapshot-limit-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let target = std::fs::File::create(root.join("target.txt")).expect("target is created");
        target
            .set_len(super::MAX_FILE_SNAPSHOT_BYTES + 1)
            .expect("sparse target exceeds the limit");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let (file, _) = workspace
            .open_regular_file(std::path::Path::new("target.txt"))
            .expect("target opens");

        let error = CodingWorkspace::file_snapshot(&file)
            .expect_err("oversized snapshots must fail before hashing");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        drop((file, workspace, target));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[test]
    fn staging_rejects_an_oversized_result_before_creating_a_file() {
        let oversized =
            usize::try_from(super::MAX_FILE_SNAPSHOT_BYTES + 1).expect("snapshot limit fits usize");

        let error = super::validate_staging_content_size(oversized)
            .expect_err("oversized staging content must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn new_file_validation_failure_preserves_a_concurrently_modified_target() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-new-rollback-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let mut staged = workspace
            .stage_content(&directory, b"owned\n", None)
            .expect("content is staged");
        super::rename_no_replace(
            &directory,
            &staged.path,
            std::path::Path::new("created.txt"),
        )
        .expect("staging is published");
        super::refresh_snapshot_after_rename(
            &staged.file,
            &mut staged.snapshot,
            "patch staging file",
        )
        .expect("controlled rename refreshes ctime");
        std::fs::write(root.join("created.txt"), "changed\n")
            .expect("installed staging is concurrently changed");

        let error = super::validate_installed_staging_slot(
            &directory,
            std::path::Path::new("created.txt"),
            &staged,
        )
        .expect_err("concurrent target mutation must fail validation");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read_to_string(root.join("created.txt")).unwrap(),
            "changed\n"
        );
        drop((staged, directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn new_file_commit_rechecks_staging_extended_attributes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-staging-xattr-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let staged = workspace
            .stage_content(&directory, b"owned\n", None)
            .expect("content is staged");
        let staged_path = staged.path.clone();
        #[cfg(target_os = "macos")]
        let attribute = "com.young-agent.concurrent";
        #[cfg(target_os = "linux")]
        let attribute = "user.young-agent.concurrent";
        rustix::fs::fsetxattr(
            &staged.file,
            attribute,
            b"injected",
            rustix::fs::XattrFlags::empty(),
        )
        .expect("concurrent xattr is injected");

        let error = workspace
            .commit_new_file(&directory, staged, std::path::Path::new("created.txt"))
            .expect_err("staging xattrs must be rejected at commit");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!root.join("created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(&staged_path)).unwrap(),
            "owned\n"
        );
        let (preserved, _) = workspace
            .open_regular_file(&staged_path)
            .expect("staging with concurrent metadata is preserved");
        assert!(super::file_has_extended_attributes(&preserved).unwrap());
        drop((directory, workspace));
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
        let metadata =
            replacement_metadata(&workspace, &directory, std::path::Path::new("target.txt"));
        let staging = workspace
            .stage_content(&directory, b"patched\n", Some(&metadata))
            .expect("content is staged");

        directory
            .rename("target.txt", &directory, "original.txt")
            .expect("original target is moved");
        std::fs::write(root.join("target.txt"), "concurrent\n")
            .expect("concurrent target is written");
        let recovery =
            super::open_recovery_namespace(&directory).expect("recovery namespace opens");

        let error = workspace
            .commit_existing_file(
                &directory,
                &recovery,
                staging,
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
        let recoveries = std::fs::read_dir(root.join(super::RECOVERY_DIRECTORY))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".young-agent-patch-failed-")
            })
            .collect::<Vec<_>>();
        assert_eq!(recoveries.len(), 1);
        assert_eq!(
            std::fs::read_to_string(recoveries[0].path()).unwrap(),
            "patched\n"
        );

        drop((directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn existing_commit_rechecks_the_target_digest_before_exchange() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "young-workspace-target-digest-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test workspace is created");
        std::fs::write(root.join("target.txt"), "original\n").expect("target is written");
        let workspace = CodingWorkspace::resolve(&root).expect("workspace resolves");
        let directory = workspace.root_dir.open_dir(".").expect("root dir opens");
        let mut metadata =
            replacement_metadata(&workspace, &directory, std::path::Path::new("target.txt"));
        let staging = workspace
            .stage_content(&directory, b"patched\n", Some(&metadata))
            .expect("content is staged");
        std::fs::write(root.join("target.txt"), "changed!\n")
            .expect("equal-length concurrent rewrite succeeds");
        metadata.snapshot.stat =
            super::file_stat_snapshot(&metadata.file.metadata().expect("target metadata reads"));
        let recovery =
            super::open_recovery_namespace(&directory).expect("recovery namespace opens");

        let error = workspace
            .commit_existing_file(
                &directory,
                &recovery,
                staging,
                std::path::Path::new("target.txt"),
                metadata,
            )
            .expect_err("stale digest must fail even when stat metadata matches");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "changed!\n"
        );

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
        let staged_path = staged.path.clone();
        directory
            .rename(&staged.path, &directory, "owned-staging.tmp")
            .expect("owned staging file is moved");
        std::fs::write(root.join(&staged.path), "concurrent\n")
            .expect("replacement staging path is written");

        let error = workspace
            .commit_new_file(&directory, staged, std::path::Path::new("created.txt"))
            .expect_err("replaced staging slot must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!root.join("created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(&staged_path)).unwrap(),
            "concurrent\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("owned-staging.tmp")).unwrap(),
            "owned\n"
        );

        drop((directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn post_commit_validation_preserves_a_second_concurrent_target_replacement() {
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
        let expected =
            replacement_metadata(&workspace, &directory, std::path::Path::new("target.txt"));
        let staged = workspace
            .stage_content(&directory, b"patched\n", Some(&expected))
            .expect("content is staged");
        super::exchange_files(&directory, &staged.path, std::path::Path::new("target.txt"))
            .expect("initial exchange succeeds");
        directory
            .rename("target.txt", &directory, "published-staging.tmp")
            .expect("installed staging file is moved");
        std::fs::write(root.join("target.txt"), "second concurrent\n")
            .expect("second concurrent target is written");

        let error = super::validate_installed_staging_slot(
            &directory,
            std::path::Path::new("target.txt"),
            &staged,
        )
        .expect_err("post-commit validation must detect a second replacement");

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
        let expected =
            replacement_metadata(&workspace, &directory, std::path::Path::new("target.txt"));
        let staged = workspace
            .stage_content(&directory, b"patched\n", Some(&expected))
            .expect("content is staged");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
            .expect("concurrent mode is set");
        let recovery =
            super::open_recovery_namespace(&directory).expect("recovery namespace opens");

        let error = workspace
            .commit_existing_file(
                &directory,
                &recovery,
                staged,
                std::path::Path::new("target.txt"),
                expected,
            )
            .expect_err("concurrent mode change must conflict");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o640);

        drop((directory, workspace));
        std::fs::remove_dir_all(root).expect("test workspace is removed");
    }
}
