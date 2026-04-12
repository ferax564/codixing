//! Transient-error retry helper for Windows filesystem races.
//!
//! ## Why this exists
//!
//! On Windows, operations that create and then immediately lock files
//! (Tantivy's `Index::open_or_create` → `index.writer(...)` → `.tantivy-writer.lock`)
//! intermittently fail with "Access is denied" (Windows error 5) or
//! "The process cannot access the file because it is being used by another
//! process" (Windows error 32, `ERROR_SHARING_VIOLATION`).
//!
//! Root cause: Windows Defender real-time scanning opens newly-created files
//! to inspect them. While Defender holds a handle, any attempt to re-open the
//! file with write+lock semantics fails. Other background processes (backup
//! agents, cloud sync clients, indexers) can do the same.
//!
//! Unlike Unix, where file locks are advisory and processes don't fight over
//! newly-written files, Windows file sharing flags are mandatory and a
//! microsecond-long antivirus scan can race any tightly-coupled create+lock
//! sequence.
//!
//! ## How this helper works
//!
//! [`retry_transient_io`] runs a closure repeatedly with exponential backoff
//! when it sees a known transient Windows error. Each retry waits roughly
//! `1ms, 2ms, 4ms, 8ms, 16ms, 32ms, 64ms, 128ms, 256ms, 512ms` — about 1 s of
//! total wait across 10 attempts. After that, the last error is returned.
//!
//! On Unix the helper compiles to a single call of the closure with zero
//! overhead: the retry loop only runs on Windows.
//!
//! ## When to use it
//!
//! Wrap operations that (a) create and immediately lock Tantivy files,
//! (b) open the writer on an existing index, or (c) perform atomic file
//! renames. Do NOT wrap read-only mmap opens — those should never hit this
//! race and the retry would just slow down the non-Windows path.
//!
//! ## Limitations
//!
//! Retrying is a correctness band-aid, not a fix for pathological AV
//! interference. If Defender is scanning every write for seconds at a time,
//! 1 s of retries won't save us and the test will still fail. The alternative
//! is single-threaded test execution (`--test-threads=1`) which is what the
//! project was doing before — much slower but deterministic.

use std::io;

/// Upper bound on retry attempts. 10 attempts × exponential backoff ≈ 1 s
/// total wait in the pathological case.
#[cfg(windows)]
const MAX_ATTEMPTS: u32 = 10;

/// Run `op` with transient-error retries on Windows, once on Unix.
///
/// Returns the first successful result, or the last error after exhausting
/// retries. On Unix the closure is called exactly once.
#[cfg(windows)]
pub fn retry_transient_io<T, F, E>(mut op: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    E: IsTransientIo,
{
    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !e.is_transient_io() {
                    return Err(e);
                }
                last_err = Some(e);
                // Exponential backoff: 1ms, 2ms, 4ms, ..., capped at 512ms.
                let wait_ms = 1u64 << attempt.min(9);
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
            }
        }
    }
    // Exhausted retries — return the last seen error.
    Err(last_err.expect("loop ran at least once"))
}

/// Run `op` directly on Unix — the retry loop is never needed.
#[cfg(not(windows))]
pub fn retry_transient_io<T, F, E>(mut op: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    E: IsTransientIo,
{
    op()
}

/// Trait implemented by error types that can be inspected for transient
/// Windows filesystem failures.
pub trait IsTransientIo {
    fn is_transient_io(&self) -> bool;
}

impl IsTransientIo for io::Error {
    fn is_transient_io(&self) -> bool {
        is_transient_raw_error(self.raw_os_error())
            || matches!(self.kind(), io::ErrorKind::PermissionDenied)
    }
}

impl IsTransientIo for crate::error::CodixingError {
    fn is_transient_io(&self) -> bool {
        use crate::error::CodixingError;
        match self {
            CodixingError::Io(e) => e.is_transient_io(),
            // Tantivy wraps io errors in its own variant; the string form is
            // our only signal when the underlying io::Error isn't exposed.
            CodixingError::Tantivy(e) => {
                let msg = e.to_string();
                msg.contains("Access is denied")
                    || msg.contains("PermissionDenied")
                    || msg.contains("os error 5")
                    || msg.contains("os error 32")
                    || msg.contains("os error 33")
                    || msg.contains("being used by another process")
            }
            CodixingError::Index(msg) => {
                msg.contains("Access is denied")
                    || msg.contains("os error 5")
                    || msg.contains("os error 32")
                    || msg.contains("os error 33")
            }
            _ => false,
        }
    }
}

/// Check whether a raw OS error code corresponds to a known transient
/// Windows filesystem failure.
///
/// Error codes of interest on Windows:
///   5  — ERROR_ACCESS_DENIED
///   32 — ERROR_SHARING_VIOLATION
///   33 — ERROR_LOCK_VIOLATION
fn is_transient_raw_error(code: Option<i32>) -> bool {
    matches!(code, Some(5) | Some(32) | Some(33))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[cfg(windows)]
    use std::io::Error;
    use std::io::ErrorKind;

    #[test]
    fn returns_ok_on_first_success() {
        let calls = Cell::new(0);
        let result = retry_transient_io::<_, _, io::Error>(|| {
            calls.set(calls.get() + 1);
            Ok::<_, io::Error>(42)
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn propagates_non_transient_error_immediately() {
        let calls = Cell::new(0);
        let result: Result<(), io::Error> = retry_transient_io(|| {
            calls.set(calls.get() + 1);
            Err(io::Error::new(ErrorKind::NotFound, "missing"))
        });
        assert!(result.is_err());
        // NotFound is never transient, so only one attempt.
        assert_eq!(calls.get(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn retries_transient_then_succeeds() {
        let calls = Cell::new(0);
        let result: Result<i32, io::Error> = retry_transient_io(|| {
            let n = calls.get() + 1;
            calls.set(n);
            if n < 3 {
                // Fake "Access is denied" for the first two attempts.
                Err(Error::from_raw_os_error(5))
            } else {
                Ok(99)
            }
        });
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn transient_detection_covers_permission_denied() {
        let err = io::Error::new(ErrorKind::PermissionDenied, "nope");
        assert!(err.is_transient_io());
    }

    #[test]
    fn transient_detection_ignores_not_found() {
        let err = io::Error::new(ErrorKind::NotFound, "missing");
        assert!(!err.is_transient_io());
    }
}
