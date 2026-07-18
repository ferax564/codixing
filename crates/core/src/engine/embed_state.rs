use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

/// Tracks progress of background embedding after Engine::init() returns.
pub struct EmbedState {
    pub(super) total: AtomicUsize,
    pub(super) completed: AtomicUsize,
    pub(super) ready: AtomicBool,
    pub(super) failed: AtomicBool,
    pub(super) cancel: AtomicBool,
    pub(super) handle: Mutex<Option<JoinHandle<()>>>,
}

impl EmbedState {
    pub fn new(total: usize) -> Self {
        Self {
            total: AtomicUsize::new(total),
            completed: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    pub fn progress(&self) -> (usize, usize) {
        (
            self.completed.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
        self.ready.store(true, Ordering::Release);
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn increment_completed(&self, n: usize) {
        self.completed.fetch_add(n, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::EmbedState;

    #[test]
    fn failure_is_terminal_and_distinct_from_success() {
        let failed = EmbedState::new(10);
        failed.mark_failed();
        assert!(failed.is_ready());
        assert!(failed.has_failed());

        let succeeded = EmbedState::new(10);
        succeeded.mark_ready();
        assert!(succeeded.is_ready());
        assert!(!succeeded.has_failed());
    }
}
