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
