//! Multi-agent shared session store.
//!
//! Enables multiple MCP clients (AI agents) to share session context. When one
//! agent searches or edits code, other agents benefit from boosted relevance of
//! recently accessed files. Thread-safe via `Arc<RwLock<>>`.
//!
//! Two persistence modes:
//! - **In-memory only** (default via [`SharedSession::default_new`] / [`SharedSession::new`]):
//!   events live in a fixed-capacity `VecDeque`, FIFO eviction at the cap.
//! - **Append-log JSONL** (opt-in via [`SharedSession::with_persistence`]):
//!   every [`SharedSession::record`] additionally writes a wall-clock-timestamped
//!   JSON line to a file (typically `.codixing/shared_session.jsonl`). On
//!   startup the file is replayed; events older than `3× decay_minutes` are
//!   dropped on load. The persisted log is what
//!   `codixing sync --learn-reformulations` consumes for v0.42 session-mining.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The type of session event recorded by an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedEventType {
    Search,
    FileRead,
    FileWrite,
    SymbolLookup,
    Navigation,
}

/// On-disk representation of a single event in the JSONL append log.
///
/// Differs from [`SharedSessionEvent`] in two ways: the monotonic [`Instant`]
/// is replaced by an absolute UNIX timestamp (Instants don't survive
/// serialization), and the `query` field carries free-text query strings for
/// session-mining consumers (the in-memory event type tracks file paths only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    /// UNIX seconds at the moment the event was recorded.
    pub timestamp_secs: u64,
    /// Event type — same enum as the in-memory variant.
    pub event_type: SharedEventType,
    /// File path the event refers to.
    pub file_path: String,
    /// Optional symbol name (for `SymbolLookup` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Recording agent identifier.
    pub agent_id: String,
}

/// A single event recorded in the shared session store.
#[derive(Debug, Clone)]
pub struct SharedSessionEvent {
    /// When the event occurred (monotonic clock for decay calculations).
    pub timestamp: Instant,
    /// What type of event this is.
    pub event_type: SharedEventType,
    /// The file path this event relates to.
    pub file_path: String,
    /// Optional symbol name (for symbol lookups).
    pub symbol: Option<String>,
    /// Which agent recorded this event.
    pub agent_id: String,
}

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_EVENTS: usize = 1000;
const DEFAULT_DECAY_MINUTES: f32 = 15.0;

/// Base boost scores by event type.
const SEARCH_BOOST: f32 = 0.08;
const FILE_READ_BOOST: f32 = 0.12;
const FILE_WRITE_BOOST: f32 = 0.20;
const SYMBOL_LOOKUP_BOOST: f32 = 0.10;
const NAVIGATION_BOOST: f32 = 0.06;

// ---------------------------------------------------------------------------
// SharedSession
// ---------------------------------------------------------------------------

/// Thread-safe shared session store for multi-agent context sharing.
///
/// Multiple MCP clients can record events concurrently, and all agents benefit
/// from the accumulated context. Uses a FIFO eviction policy to cap memory.
pub struct SharedSession {
    events: Arc<RwLock<VecDeque<SharedSessionEvent>>>,
    max_events: usize,
    decay_minutes: f32,
    /// Optional JSONL append log handle. When set, every `record()` writes
    /// a `PersistedSessionEvent` line. Wrapped in `Arc<Mutex<>>` so clones
    /// share the same file descriptor and writes are serialized.
    persistence: Option<Arc<Mutex<std::fs::File>>>,
}

impl SharedSession {
    /// Create a new shared session store with the given capacity and decay window.
    pub fn new(max_events: usize, decay_minutes: f32) -> Self {
        Self {
            events: Arc::new(RwLock::new(VecDeque::with_capacity(
                max_events.min(DEFAULT_MAX_EVENTS * 2),
            ))),
            max_events,
            decay_minutes,
            persistence: None,
        }
    }

    /// Create a new shared session store with default settings.
    pub fn default_new() -> Self {
        Self::new(DEFAULT_MAX_EVENTS, DEFAULT_DECAY_MINUTES)
    }

    /// Create a session store backed by a JSONL append log.
    ///
    /// On open, replays the existing log into the in-memory store, dropping
    /// events older than `3 × decay_minutes` (matches the in-memory boost
    /// window — anything older contributes 0 boost anyway). Capacity is
    /// enforced via FIFO eviction during the replay just like normal
    /// `record` calls.
    ///
    /// After replay, every subsequent [`Self::record`] additionally writes a
    /// JSON line to the same file. Write errors are logged at `debug!` and
    /// swallowed — observability persistence must never break the search
    /// boost path.
    ///
    /// Returns the underlying IO error if the parent directory cannot be
    /// created or the file cannot be opened. Replay errors during reading
    /// are silently skipped so a partially-corrupted log file degrades to a
    /// truncated session history rather than a hard failure.
    pub fn with_persistence(
        max_events: usize,
        decay_minutes: f32,
        path: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let mut session = Self::new(max_events, decay_minutes);
        if path.exists() {
            session.replay_persisted(path);
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        session.persistence = Some(Arc::new(Mutex::new(file)));
        Ok(session)
    }

    /// Open a default-sized session backed by `path`, falling back to an
    /// in-memory-only session if the file cannot be opened or its parent
    /// directory cannot be created.
    ///
    /// Use this from `Engine::init` / `Engine::open` where a persistence
    /// failure must not block engine startup. Logs a `warn!` on fallback so
    /// the operator sees something in the daemon log.
    pub fn with_persistence_or_default(path: impl AsRef<Path>) -> Self {
        match Self::with_persistence(DEFAULT_MAX_EVENTS, DEFAULT_DECAY_MINUTES, path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "shared_session persistence disabled, falling back to in-memory: {e}"
                );
                Self::default_new()
            }
        }
    }

    /// Read the JSONL append log without opening it for writes — for offline
    /// learners (e.g. `codixing sync --learn-reformulations`) that just want
    /// to consume the recorded history.
    ///
    /// Lines that fail to parse as `PersistedSessionEvent` are skipped, not
    /// fatal. Returns an empty vec if the file does not exist.
    pub fn read_persisted(path: impl AsRef<Path>) -> std::io::Result<Vec<PersistedSessionEvent>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(path)?;
        let mut out = Vec::new();
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            if let Ok(p) = serde_json::from_str::<PersistedSessionEvent>(line) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Replay events from a JSONL log into the in-memory store.
    ///
    /// Drops events older than `3 × decay_minutes` so a long-lived log
    /// doesn't bloat memory with events that contribute zero boost. Caps at
    /// `max_events` via FIFO eviction so the replay can't exceed the configured
    /// capacity even on large files.
    fn replay_persisted(&mut self, path: &Path) {
        let now_systime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let now_instant = Instant::now();
        let max_age_secs = (self.decay_minutes * 60.0 * 3.0) as u64;

        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };

        let mut events = self.events.write().unwrap_or_else(|e| e.into_inner());
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(persisted): Result<PersistedSessionEvent, _> = serde_json::from_str(line) else {
                continue;
            };
            let elapsed_secs = now_systime.saturating_sub(persisted.timestamp_secs);
            if elapsed_secs > max_age_secs {
                continue;
            }
            // Re-anchor the wall-clock elapsed back into the monotonic
            // Instant timeline so the in-memory decay math keeps working.
            let timestamp = now_instant
                .checked_sub(Duration::from_secs(elapsed_secs))
                .unwrap_or(now_instant);
            events.push_back(SharedSessionEvent {
                timestamp,
                event_type: persisted.event_type,
                file_path: persisted.file_path,
                symbol: persisted.symbol,
                agent_id: persisted.agent_id,
            });
            while events.len() > self.max_events {
                events.pop_front();
            }
        }
    }

    /// Record a session event. Evicts the oldest event if over capacity.
    /// If persistence is enabled, also appends a JSONL line to the log.
    pub fn record(&self, event: SharedSessionEvent) {
        // Snapshot wall-clock for persistence BEFORE we push so the
        // recorded timestamp matches "now" at the call site, not at file
        // lock acquisition.
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut events = self.events.write().unwrap_or_else(|e| e.into_inner());
        if events.len() >= self.max_events {
            events.pop_front();
        }
        events.push_back(event.clone());
        drop(events);

        if let Some(file) = &self.persistence {
            let persisted = PersistedSessionEvent {
                timestamp_secs,
                event_type: event.event_type,
                file_path: event.file_path,
                symbol: event.symbol,
                agent_id: event.agent_id,
            };
            if let Ok(line) = serde_json::to_string(&persisted)
                && let Ok(mut f) = file.lock()
                && let Err(e) = writeln!(f, "{line}")
            {
                tracing::debug!("shared_session persistence write failed: {e}");
            }
        }
    }

    /// Compute a time-decayed relevance boost for a file path across all agents.
    ///
    /// The boost is the sum of per-event base scores, each multiplied by an
    /// exponential decay factor: `base_score * exp(-elapsed_minutes / decay_minutes)`.
    pub fn get_file_boost(&self, file_path: &str) -> f32 {
        let events = self.events.read().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let mut boost: f32 = 0.0;

        for event in events.iter().rev() {
            if event.file_path != file_path {
                continue;
            }

            let elapsed_minutes = now.duration_since(event.timestamp).as_secs_f32() / 60.0;

            // Skip events well past the decay window (3x decay_minutes).
            if elapsed_minutes > self.decay_minutes * 3.0 {
                continue;
            }

            let base = match event.event_type {
                SharedEventType::Search => SEARCH_BOOST,
                SharedEventType::FileRead => FILE_READ_BOOST,
                SharedEventType::FileWrite => FILE_WRITE_BOOST,
                SharedEventType::SymbolLookup => SYMBOL_LOOKUP_BOOST,
                SharedEventType::Navigation => NAVIGATION_BOOST,
            };

            let decay = (-elapsed_minutes / self.decay_minutes).exp();
            boost += base * decay;
        }

        boost
    }

    /// Return the top files ranked by recent activity across all agents.
    pub fn get_hot_files(&self, limit: usize) -> Vec<(String, f32)> {
        let events = self.events.read().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        // Accumulate scores per file.
        let mut scores: HashMap<&str, f32> = HashMap::new();

        for event in events.iter() {
            let elapsed_minutes = now.duration_since(event.timestamp).as_secs_f32() / 60.0;

            if elapsed_minutes > self.decay_minutes * 3.0 {
                continue;
            }

            let base = match event.event_type {
                SharedEventType::Search => SEARCH_BOOST,
                SharedEventType::FileRead => FILE_READ_BOOST,
                SharedEventType::FileWrite => FILE_WRITE_BOOST,
                SharedEventType::SymbolLookup => SYMBOL_LOOKUP_BOOST,
                SharedEventType::Navigation => NAVIGATION_BOOST,
            };

            let decay = (-elapsed_minutes / self.decay_minutes).exp();
            *scores.entry(&event.file_path).or_insert(0.0) += base * decay;
        }

        let mut ranked: Vec<(String, f32)> = scores
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);
        ranked
    }

    /// Return events for a specific agent, most recent first.
    pub fn get_agent_context(&self, agent_id: &str) -> Vec<SharedSessionEvent> {
        let events = self.events.read().unwrap_or_else(|e| e.into_inner());
        events
            .iter()
            .rev()
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect()
    }

    /// List agents with recent activity (events within the decay window).
    pub fn active_agents(&self) -> Vec<String> {
        let events = self.events.read().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let cutoff_minutes = self.decay_minutes * 2.0;

        let mut seen: Vec<String> = Vec::new();
        for event in events.iter().rev() {
            let elapsed_minutes = now.duration_since(event.timestamp).as_secs_f32() / 60.0;
            if elapsed_minutes > cutoff_minutes {
                break;
            }
            if !seen.contains(&event.agent_id) {
                seen.push(event.agent_id.clone());
            }
        }
        seen
    }

    /// Total number of events currently stored.
    pub fn event_count(&self) -> usize {
        self.events.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Clone for SharedSession {
    fn clone(&self) -> Self {
        Self {
            events: Arc::clone(&self.events),
            max_events: self.max_events,
            decay_minutes: self.decay_minutes,
            persistence: self.persistence.as_ref().map(Arc::clone),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_event(
        event_type: SharedEventType,
        file_path: &str,
        agent_id: &str,
    ) -> SharedSessionEvent {
        SharedSessionEvent {
            timestamp: Instant::now(),
            event_type,
            file_path: file_path.to_string(),
            symbol: None,
            agent_id: agent_id.to_string(),
        }
    }

    #[test]
    fn record_and_get_file_boost() {
        let session = SharedSession::default_new();

        session.record(make_event(
            SharedEventType::FileRead,
            "src/main.rs",
            "agent-1",
        ));
        session.record(make_event(
            SharedEventType::FileWrite,
            "src/lib.rs",
            "agent-2",
        ));

        let main_boost = session.get_file_boost("src/main.rs");
        let lib_boost = session.get_file_boost("src/lib.rs");
        let other_boost = session.get_file_boost("src/other.rs");

        assert!(
            main_boost > 0.0,
            "main.rs boost should be positive, got {main_boost}"
        );
        assert!(
            lib_boost > 0.0,
            "lib.rs boost should be positive, got {lib_boost}"
        );
        assert_eq!(other_boost, 0.0, "unrelated file should have 0 boost");

        // Write boost should be higher than read boost for fresh events.
        assert!(
            lib_boost > main_boost,
            "write boost ({lib_boost}) should be > read boost ({main_boost})"
        );
    }

    #[test]
    fn fifo_eviction() {
        let session = SharedSession::new(5, 15.0);

        for i in 0..10 {
            session.record(make_event(
                SharedEventType::FileRead,
                &format!("file{i}.rs"),
                "agent-1",
            ));
        }

        assert_eq!(session.event_count(), 5);

        // The first 5 files should have been evicted.
        assert_eq!(session.get_file_boost("file0.rs"), 0.0);
        assert_eq!(session.get_file_boost("file4.rs"), 0.0);

        // The last 5 files should still be present.
        assert!(session.get_file_boost("file5.rs") > 0.0);
        assert!(session.get_file_boost("file9.rs") > 0.0);
    }

    #[test]
    fn hot_files_ranking() {
        let session = SharedSession::default_new();

        // Record multiple events for the same file to boost its score.
        for _ in 0..3 {
            session.record(make_event(
                SharedEventType::FileRead,
                "src/hot.rs",
                "agent-1",
            ));
        }
        session.record(make_event(
            SharedEventType::FileRead,
            "src/cold.rs",
            "agent-2",
        ));

        let hot = session.get_hot_files(10);
        assert!(!hot.is_empty());
        assert_eq!(hot[0].0, "src/hot.rs", "hottest file should be first");
        assert!(
            hot[0].1 > hot.last().unwrap().1,
            "first file should have higher score than last"
        );
    }

    #[test]
    fn multi_agent_isolation() {
        let session = SharedSession::default_new();

        session.record(make_event(
            SharedEventType::FileRead,
            "src/main.rs",
            "agent-1",
        ));
        session.record(make_event(
            SharedEventType::FileWrite,
            "src/lib.rs",
            "agent-2",
        ));
        session.record(make_event(
            SharedEventType::Search,
            "src/search.rs",
            "agent-1",
        ));

        let agent1_events = session.get_agent_context("agent-1");
        let agent2_events = session.get_agent_context("agent-2");

        assert_eq!(agent1_events.len(), 2, "agent-1 should have 2 events");
        assert_eq!(agent2_events.len(), 1, "agent-2 should have 1 event");

        // Verify all events belong to the correct agent.
        assert!(agent1_events.iter().all(|e| e.agent_id == "agent-1"));
        assert!(agent2_events.iter().all(|e| e.agent_id == "agent-2"));
    }

    #[test]
    fn active_agents() {
        let session = SharedSession::default_new();

        session.record(make_event(SharedEventType::FileRead, "a.rs", "agent-alpha"));
        session.record(make_event(SharedEventType::FileRead, "b.rs", "agent-beta"));
        session.record(make_event(SharedEventType::FileRead, "c.rs", "agent-alpha"));

        let agents = session.active_agents();
        assert_eq!(agents.len(), 2);
        // Most recently active first.
        assert!(agents.contains(&"agent-alpha".to_string()));
        assert!(agents.contains(&"agent-beta".to_string()));
    }

    #[test]
    fn cross_agent_boost() {
        let session = SharedSession::default_new();

        // Agent 1 reads a file.
        session.record(make_event(
            SharedEventType::FileRead,
            "src/shared.rs",
            "agent-1",
        ));

        // Agent 2 should also get a boost for this file.
        let boost = session.get_file_boost("src/shared.rs");
        assert!(
            boost > 0.0,
            "cross-agent file boost should be positive, got {boost}"
        );
    }

    #[test]
    fn thread_safety() {
        let session = SharedSession::default_new();
        let session_clone = session.clone();

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let s = session_clone.clone();
                thread::spawn(move || {
                    for j in 0..50 {
                        s.record(make_event(
                            SharedEventType::FileRead,
                            &format!("file{j}.rs"),
                            &format!("agent-{i}"),
                        ));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        // All 200 events should be recorded (cap is 1000).
        assert_eq!(session.event_count(), 200);
    }

    #[test]
    fn symbol_event() {
        let session = SharedSession::default_new();

        session.record(SharedSessionEvent {
            timestamp: Instant::now(),
            event_type: SharedEventType::SymbolLookup,
            file_path: "src/engine.rs".to_string(),
            symbol: Some("Engine".to_string()),
            agent_id: "agent-1".to_string(),
        });

        let boost = session.get_file_boost("src/engine.rs");
        assert!(
            boost > 0.0,
            "symbol lookup should create file boost, got {boost}"
        );

        let events = session.get_agent_context("agent-1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].symbol.as_deref(), Some("Engine"));
    }

    // -----------------------------------------------------------------------
    // Stress / edge-case tests (Task 2D)
    // -----------------------------------------------------------------------

    #[test]
    fn fifo_eviction_600_events_only_500_remain() {
        let session = SharedSession::new(500, 15.0);

        // Record 600 events — one per file — so each event is unique.
        for i in 0..600 {
            session.record(make_event(
                SharedEventType::FileRead,
                &format!("file_{i:04}.rs"),
                "agent-1",
            ));
        }

        // Only 500 events should remain (FIFO eviction).
        assert_eq!(session.event_count(), 500, "FIFO should cap at 500 events");

        // The first 100 files (0..100) should have been evicted.
        for i in 0..100 {
            let boost = session.get_file_boost(&format!("file_{i:04}.rs"));
            assert_eq!(
                boost, 0.0,
                "file_{i:04}.rs should have been evicted (boost should be 0)"
            );
        }

        // Files 100..600 should still be present.
        for i in [100, 250, 499, 599] {
            let boost = session.get_file_boost(&format!("file_{i:04}.rs"));
            assert!(
                boost > 0.0,
                "file_{i:04}.rs should still be present (boost > 0), got {boost}"
            );
        }
    }

    #[test]
    fn get_hot_files_descending_boost_order() {
        let session = SharedSession::default_new();

        // Record varying numbers of events per file to create different boost levels.
        // File "alpha.rs" gets 5 write events (highest boost).
        for _ in 0..5 {
            session.record(make_event(
                SharedEventType::FileWrite,
                "alpha.rs",
                "agent-1",
            ));
        }
        // File "beta.rs" gets 2 write events (medium boost).
        for _ in 0..2 {
            session.record(make_event(SharedEventType::FileWrite, "beta.rs", "agent-1"));
        }
        // File "gamma.rs" gets 1 read event (lowest boost).
        session.record(make_event(SharedEventType::FileRead, "gamma.rs", "agent-1"));

        let hot = session.get_hot_files(10);

        // Should have 3 files.
        assert_eq!(hot.len(), 3, "should have 3 hot files");

        // Must be sorted in descending boost order.
        assert_eq!(hot[0].0, "alpha.rs", "alpha.rs should be hottest");
        assert_eq!(hot[1].0, "beta.rs", "beta.rs should be second");
        assert_eq!(hot[2].0, "gamma.rs", "gamma.rs should be third");

        // Verify strict descending order of scores.
        for w in hot.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "hot files must be in descending boost order: {} ({}) should be >= {} ({})",
                w[0].0,
                w[0].1,
                w[1].0,
                w[1].1
            );
        }
    }

    #[test]
    fn empty_session() {
        let session = SharedSession::default_new();

        assert_eq!(session.event_count(), 0);
        assert_eq!(session.get_file_boost("anything.rs"), 0.0);
        assert!(session.get_hot_files(10).is_empty());
        assert!(session.active_agents().is_empty());
        assert!(session.get_agent_context("nobody").is_empty());
    }

    // -----------------------------------------------------------------------
    // Persistence (v0.42 Tier 1 #1)
    // -----------------------------------------------------------------------

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared_session.jsonl");

        // First session: record three events, drop it.
        {
            let session =
                SharedSession::with_persistence(100, 15.0, &path).expect("with_persistence");
            session.record(make_event(SharedEventType::FileRead, "src/a.rs", "agent-1"));
            session.record(make_event(
                SharedEventType::FileWrite,
                "src/b.rs",
                "agent-2",
            ));
            session.record(SharedSessionEvent {
                timestamp: Instant::now(),
                event_type: SharedEventType::SymbolLookup,
                file_path: "src/c.rs".to_string(),
                symbol: Some("Engine".to_string()),
                agent_id: "agent-1".to_string(),
            });
        }

        // Second session opens the same path, replay should restore events.
        let restored =
            SharedSession::with_persistence(100, 15.0, &path).expect("with_persistence reopen");
        assert_eq!(
            restored.event_count(),
            3,
            "all 3 persisted events should replay"
        );

        // Boost on the recorded files should be > 0 immediately after replay
        // (replay re-anchors the timestamp into the new monotonic timeline).
        assert!(restored.get_file_boost("src/a.rs") > 0.0);
        assert!(restored.get_file_boost("src/b.rs") > 0.0);
        assert!(restored.get_file_boost("src/c.rs") > 0.0);

        // Symbol field round-trips.
        let agent1 = restored.get_agent_context("agent-1");
        let symbol_event = agent1
            .iter()
            .find(|e| e.file_path == "src/c.rs")
            .expect("c.rs event present");
        assert_eq!(symbol_event.symbol.as_deref(), Some("Engine"));
    }

    #[test]
    fn read_persisted_skips_corrupt_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared_session.jsonl");

        // Write a mix of valid + corrupt lines directly.
        std::fs::write(
            &path,
            "{\"timestamp_secs\":100,\"event_type\":\"Search\",\"file_path\":\"a.rs\",\"agent_id\":\"x\"}\n\
             not json at all\n\
             {\"timestamp_secs\":200,\"event_type\":\"FileRead\",\"file_path\":\"b.rs\",\"agent_id\":\"y\"}\n\
             \n",
        )
        .unwrap();

        let events = SharedSession::read_persisted(&path).expect("read_persisted");
        assert_eq!(events.len(), 2, "corrupt + blank lines must be skipped");
        assert_eq!(events[0].file_path, "a.rs");
        assert_eq!(events[1].file_path, "b.rs");
    }

    #[test]
    fn read_persisted_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does_not_exist.jsonl");
        let events = SharedSession::read_persisted(&path).expect("missing file → empty");
        assert!(events.is_empty());
    }

    #[test]
    fn persistence_drops_events_older_than_3x_decay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared_session.jsonl");

        // Forge an old event directly to the log: timestamp 1000s before
        // the unix epoch baseline of the test (way older than 3 × 15min = 45min).
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale_ts = now.saturating_sub(60 * 60 * 24); // 24 h ago
        let fresh_ts = now.saturating_sub(60); // 1 min ago
        std::fs::write(
            &path,
            format!(
                "{{\"timestamp_secs\":{stale_ts},\"event_type\":\"FileRead\",\"file_path\":\"old.rs\",\"agent_id\":\"x\"}}\n\
                 {{\"timestamp_secs\":{fresh_ts},\"event_type\":\"FileRead\",\"file_path\":\"new.rs\",\"agent_id\":\"y\"}}\n"
            ),
        )
        .unwrap();

        let session = SharedSession::with_persistence(100, 15.0, &path).unwrap();
        assert_eq!(
            session.event_count(),
            1,
            "stale event must be dropped on replay"
        );
        assert_eq!(session.get_file_boost("old.rs"), 0.0);
        assert!(session.get_file_boost("new.rs") > 0.0);
    }

    #[test]
    fn record_appends_to_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared_session.jsonl");

        let session = SharedSession::with_persistence(100, 15.0, &path).unwrap();
        session.record(make_event(
            SharedEventType::FileWrite,
            "src/lib.rs",
            "agent-1",
        ));
        // Drop the session to flush + close the file before reading it back.
        drop(session);

        let events = SharedSession::read_persisted(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].file_path, "src/lib.rs");
        assert!(matches!(events[0].event_type, SharedEventType::FileWrite));
    }

    #[test]
    fn persistence_creates_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("nested")
            .join(".codixing")
            .join("shared_session.jsonl");
        // Parent dir doesn't exist yet — `with_persistence` must create it.
        assert!(!path.parent().unwrap().exists());

        let session = SharedSession::with_persistence(100, 15.0, &path).unwrap();
        session.record(make_event(SharedEventType::Search, "x.rs", "z"));
        drop(session);

        assert!(path.exists(), "log file must exist after first record");
    }

    #[test]
    fn cloned_session_shares_log_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared_session.jsonl");

        let session = SharedSession::with_persistence(100, 15.0, &path).unwrap();
        let cloned = session.clone();

        session.record(make_event(SharedEventType::Search, "a.rs", "agent-1"));
        cloned.record(make_event(SharedEventType::Search, "b.rs", "agent-2"));
        drop(session);
        drop(cloned);

        let events = SharedSession::read_persisted(&path).unwrap();
        assert_eq!(events.len(), 2, "both clones must write to the same log");
    }
}
