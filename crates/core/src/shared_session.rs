//! Multi-agent shared session store.
//!
//! Enables multiple MCP clients (AI agents) to share session context. When one
//! agent searches or edits code, other agents benefit from boosted relevance of
//! recently accessed files. All state is in-memory only (no persistence needed)
//! and thread-safe via `Arc<RwLock<>>`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The type of session event recorded by an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedEventType {
    Search,
    FileRead,
    FileWrite,
    SymbolLookup,
    Navigation,
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
        }
    }

    /// Create a new shared session store with default settings.
    pub fn default_new() -> Self {
        Self::new(DEFAULT_MAX_EVENTS, DEFAULT_DECAY_MINUTES)
    }

    /// Record a session event. Evicts the oldest event if over capacity.
    pub fn record(&self, event: SharedSessionEvent) {
        let mut events = self.events.write().expect("shared session lock poisoned");
        if events.len() >= self.max_events {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Compute a time-decayed relevance boost for a file path across all agents.
    ///
    /// The boost is the sum of per-event base scores, each multiplied by an
    /// exponential decay factor: `base_score * exp(-elapsed_minutes / decay_minutes)`.
    pub fn get_file_boost(&self, file_path: &str) -> f32 {
        let events = self.events.read().expect("shared session lock poisoned");
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
        let events = self.events.read().expect("shared session lock poisoned");
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
        let events = self.events.read().expect("shared session lock poisoned");
        events
            .iter()
            .rev()
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect()
    }

    /// List agents with recent activity (events within the decay window).
    pub fn active_agents(&self) -> Vec<String> {
        let events = self.events.read().expect("shared session lock poisoned");
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
        self.events
            .read()
            .expect("shared session lock poisoned")
            .len()
    }
}

impl Clone for SharedSession {
    fn clone(&self) -> Self {
        Self {
            events: Arc::clone(&self.events),
            max_events: self.max_events,
            decay_minutes: self.decay_minutes,
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

        session.record(make_event(
            SharedEventType::FileRead,
            "a.rs",
            "agent-alpha",
        ));
        session.record(make_event(
            SharedEventType::FileRead,
            "b.rs",
            "agent-beta",
        ));
        session.record(make_event(
            SharedEventType::FileRead,
            "c.rs",
            "agent-alpha",
        ));

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

    #[test]
    fn empty_session() {
        let session = SharedSession::default_new();

        assert_eq!(session.event_count(), 0);
        assert_eq!(session.get_file_boost("anything.rs"), 0.0);
        assert!(session.get_hot_files(10).is_empty());
        assert!(session.active_agents().is_empty());
        assert!(session.get_agent_context("nobody").is_empty());
    }
}
