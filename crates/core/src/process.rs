//! Process-level helpers shared by the Codixing binaries.

/// Restore the default `SIGPIPE` disposition on Unix.
///
/// Rust starts every process with `SIGPIPE` ignored, so `println!` panics with
/// "failed printing to stdout: Broken pipe" as soon as a downstream reader
/// such as `head` closes the pipe. For a short-lived CLI the conventional Unix
/// behaviour — exit quietly — is the right one: agents pipe output through
/// `head` constantly, and a panic message plus exit code 101 reads as a
/// failure.
///
/// Call this once at start-up, before spawning threads. Long-running servers
/// should not call it; they must survive a peer closing its end. No-op on
/// non-Unix platforms.
pub fn restore_default_sigpipe() {
    #[cfg(unix)]
    // SAFETY: `signal` is async-signal-safe and is given a valid signal number
    // and the predefined default handler; no Rust state is involved.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Longest Unix socket path Codixing will try to bind.
///
/// `sockaddr_un.sun_path` holds 108 bytes on Linux and 104 on macOS and the
/// BSDs, including the terminating NUL. Staying at or below 100 bytes works
/// on all of them.
#[cfg(unix)]
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;

/// Where the daemon for `root` listens, given the socket's file name
/// (for example `daemon-minimal.sock`).
///
/// The socket normally lives in the index directory,
/// `<root>/.codixing/<file_name>`. A repository checked out under a long path
/// (deep monorepo folders, nested worktrees) pushes that past the kernel's
/// socket path limit, the daemon fails to bind, and every client silently
/// falls back to cold starts. In that case the socket moves to a short,
/// per-user runtime directory and is named after a hash of the canonical
/// root, so the daemon and every client still derive the same path
/// independently.
#[cfg(unix)]
pub fn daemon_socket_path(root: &std::path::Path, file_name: &str) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;

    // Decide on the canonical root so a client that reached the repository
    // through a symlink picks the same location as the daemon.
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let in_index = canonical.join(".codixing").join(file_name);
    if in_index.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_BYTES {
        return root.join(".codixing").join(file_name);
    }

    let digest = xxhash_rust::xxh3::xxh3_64(canonical.as_os_str().as_bytes());
    daemon_runtime_dir().join(format!("{digest:016x}-{file_name}"))
}

/// Short per-user directory for daemon sockets that cannot live in the index
/// directory: `$XDG_RUNTIME_DIR/codixing` when set (already private to the
/// user), otherwise `<tmp>/codixing-<uid>`.
#[cfg(unix)]
pub fn daemon_runtime_dir() -> std::path::PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = std::path::PathBuf::from(runtime).join("codixing");
        if dir.as_os_str().len() <= 60 {
            return dir;
        }
    }
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    std::path::PathBuf::from("/tmp").join(format!("codixing-{uid}"))
}

/// Prepare the directory that will hold a daemon socket.
///
/// The index directory needs nothing. The runtime fallback directory is
/// created with mode `0700` and must be a real directory owned by the current
/// user; anything else (a symlink, another user's directory) is refused so a
/// local attacker cannot stage the location ahead of the daemon.
#[cfg(unix)]
pub fn prepare_daemon_socket_dir(socket_path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let Some(dir) = socket_path.parent() else {
        return Ok(());
    };
    if dir != daemon_runtime_dir() {
        return Ok(());
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to use daemon socket directory {}: not a directory owned by the current user",
                dir.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn short_roots_keep_the_socket_in_the_index_directory() {
        let path = daemon_socket_path(Path::new("/tmp/project"), "daemon-minimal.sock");
        assert_eq!(
            path,
            Path::new("/tmp/project/.codixing/daemon-minimal.sock")
        );
    }

    #[test]
    fn long_roots_fall_back_to_a_bindable_runtime_path() {
        let deep = format!("/tmp/{}", "very-long-directory-name/".repeat(8));
        let root = Path::new(&deep);
        let path = daemon_socket_path(root, "daemon-minimal.sock");

        assert!(
            path.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
            "fallback path must be bindable: {} bytes",
            path.as_os_str().len()
        );
        assert!(path.starts_with(daemon_runtime_dir()), "{}", path.display());
        assert!(
            path.to_string_lossy().ends_with("-daemon-minimal.sock"),
            "{}",
            path.display()
        );
        // Deterministic, and distinct per root and per socket name.
        assert_eq!(path, daemon_socket_path(root, "daemon-minimal.sock"));
        assert_ne!(path, daemon_socket_path(root, "daemon-reviewer.sock"));
        let other = format!("{deep}other");
        assert_ne!(
            path,
            daemon_socket_path(Path::new(&other), "daemon-minimal.sock")
        );
    }

    #[test]
    fn runtime_directory_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let deep = format!("/tmp/{}", "very-long-directory-name/".repeat(8));
        let socket = daemon_socket_path(Path::new(&deep), "daemon-test.sock");
        prepare_daemon_socket_dir(&socket).unwrap();
        let dir = socket.parent().unwrap();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "runtime dir must not be group/world accessible"
        );

        // The index directory is left alone.
        prepare_daemon_socket_dir(Path::new("/tmp/project/.codixing/daemon-minimal.sock")).unwrap();
    }
}
