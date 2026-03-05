use std::sync::Arc;

use tokio::sync::RwLock;

use codixing_core::Engine;

/// Shared application state — an `Engine` behind an async `RwLock`.
///
/// Readers (search, symbols, status) acquire a read lock; writers (reindex,
/// remove) acquire a write lock.
pub type AppState = Arc<RwLock<Engine>>;

/// Construct a new `AppState` wrapping the given engine.
pub fn new_state(engine: Engine) -> AppState {
    Arc::new(RwLock::new(engine))
}
