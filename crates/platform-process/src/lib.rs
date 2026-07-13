#![doc = "Audited platform process hooks for the Agent Kernel."]

use std::io;
use std::process::Command;

use cap_std::fs::Dir;

/// Binds a child process to an already-open directory capability.
///
/// The directory handle is cloned into the registered child hook, so this safe
/// API does not rely on the caller keeping the original handle alive until
/// `spawn`.
#[cfg(unix)]
pub fn bind_working_directory(command: &mut Command, directory: &Dir) -> io::Result<()> {
    use std::os::unix::process::CommandExt as _;

    let directory = directory.try_clone()?;
    // SAFETY: the owned directory handle outlives the hook, and the hook only
    // performs the async-signal-safe fchdir syscall. It does not allocate or
    // touch shared process state after fork.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || rustix::process::fchdir(&directory).map_err(io::Error::from));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn bind_working_directory(_command: &mut Command, _directory: &Dir) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-bound command working directories are not supported on this platform",
    ))
}

/// Prevents Linux descendants from gaining privileges across `exec`.
#[cfg(target_os = "linux")]
pub fn block_exec_privilege_gain(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: rustix issues the single prctl(PR_SET_NO_NEW_PRIVS) syscall and
    // does not allocate or touch shared process state in the post-fork child.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| rustix::thread::set_no_new_privs(true).map_err(io::Error::from));
    }
}

#[cfg(not(target_os = "linux"))]
pub fn block_exec_privilege_gain(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "young-platform-process");
    }
}
