//! Session state tracking for AI agent interactions.
//!
//! Records events (file reads, symbol lookups, searches, edits) with timestamps
//! and monotonic sequence numbers. Used to boost retrieval relevance based on
//! what the agent has been working on recently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Unique session identifier.
pub type SessionId = String;

/// Events that the session tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEventKind {
    FileRead(String),
    SymbolLookup { name: String, file: Option<String> },
    Search { query: String, result_count: usize },
    FileEdit(String),
    FileWrite(String),
}

/// A single recorded session event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic sequence number (0-indexed within session).
    pub seq: u64,
    /// Wall-clock timestamp.
    pub timestamp: SystemTime,
    /// Monotonic instant for duration calculations (not serialized).
    #[serde(skip)]
    pub instant: Option<Instant>,
    /// The event payload.
    pub kind: SessionEventKind,
}

/// Configuration constants for session behavior.
const MAX_EVENTS: usize = 500;
const SESSION_EXPIRE_HOURS: u64 = 4;
const RESTORE_MAX_AGE_HOURS: u64 = 2;

// Boost constants.
const FILE_READ_BOOST: f32 = 0.15;
const FILE_EDIT_BOOST: f32 = 0.25;
const SYMBOL_LOOKUP_BOOST: f32 = 0.10;
const DECAY_HALF_LIFE_SECS: f64 = 300.0; // 5 minutes

// Progressive focus constants.
const FOCUS_THRESHOLD: usize = 5;
const FOCUS_BOOST: f32 = 0.10;

// Graph propagation damping factors.
const HOP_1_DAMPING: f32 = 0.3;
const HOP_2_DAMPING: f32 = 0.1;

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// In-memory session state with concurrent access support.
pub struct SessionState {
    /// Current session ID.
    session_id: String,
    /// Event log for the current session.
    events: DashMap<String, Vec<SessionEvent>>,
    /// Monotonic sequence counter.
    seq_counter: AtomicU64,
    /// Session creation time.
    created_at: SystemTime,
    /// Interaction counts per top-level directory (for progressive focus).
    dir_counts: DashMap<String, usize>,
    /// Currently focused directory (set after FOCUS_THRESHOLD interactions).
    focus_directory: DashMap<String, String>, // session_id -> focus_dir
    /// Cached neighbor sets from the import graph (file -> neighbors).
    neighbor_cache: DashMap<String, Vec<String>>,
    /// Whether session tracking is enabled.
    enabled: bool,
    /// Root path for persistence.
    root: Option<PathBuf>,
}

impl SessionState {
    /// Create a new session state.
    pub fn new(enabled: bool) -> Self {
        let session_id = uuid_v4();
        Self {
            session_id,
            events: DashMap::new(),
            seq_counter: AtomicU64::new(0),
            created_at: SystemTime::now(),
            dir_counts: DashMap::new(),
            focus_directory: DashMap::new(),
            neighbor_cache: DashMap::new(),
            enabled,
            root: None,
        }
    }

    /// Create a new session state with persistence root.
    pub fn with_root(enabled: bool, root: &Path) -> Self {
        let mut state = Self::new(enabled);
        state.root = Some(root.to_path_buf());

        // Try to restore from persisted state.
        if enabled {
            state.try_restore();
        }
        state
    }

    /// Whether session tracking is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Current session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Total number of events in the current session.
    pub fn event_count(&self) -> usize {
        self.events
            .get(&self.session_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    // ----- Recording -----

    /// Record a session event.
    pub fn record(&self, kind: SessionEventKind) {
        if !self.enabled {
            return;
        }

        // Track directory interactions for progressive focus.
        match &kind {
            SessionEventKind::FileRead(path)
            | SessionEventKind::FileEdit(path)
            | SessionEventKind::FileWrite(path) => {
                self.track_directory(path);
            }
            SessionEventKind::SymbolLookup { file: Some(f), .. } => {
                self.track_directory(f);
            }
            SessionEventKind::Search { .. } | SessionEventKind::SymbolLookup { file: None, .. } => {
            }
        }

        let seq = self.seq_counter.fetch_add(1, Ordering::Relaxed);
        let event = SessionEvent {
            seq,
            timestamp: SystemTime::now(),
            instant: Some(Instant::now()),
            kind,
        };

        let mut entry = self.events.entry(self.session_id.clone()).or_default();
        let events = entry.value_mut();

        // FIFO eviction if at capacity.
        if events.len() >= MAX_EVENTS {
            events.remove(0);
        }

        events.push(event);
    }

    /// Track directory interaction for progressive focus.
    fn track_directory(&self, path: &str) {
        let top_dir = top_level_directory(path);
        let mut count = self.dir_counts.entry(top_dir.clone()).or_insert(0);
        *count += 1;

        // Check if we should set focus.
        if *count >= FOCUS_THRESHOLD {
            let should_focus = match self.focus_directory.get(&self.session_id) {
                None => true,
                Some(current) => {
                    if *current == top_dir {
                        false // already focused here
                    } else {
                        // Switch focus if new dir has 2x more interactions.
                        let current_count = self
                            .dir_counts
                            .get(current.as_str())
                            .map(|c| *c)
                            .unwrap_or(0);
                        *count > current_count * 2
                    }
                }
            };

            if should_focus {
                debug!(dir = %top_dir, count = *count, "setting progressive focus");
                self.focus_directory
                    .insert(self.session_id.clone(), top_dir);
            }
        }
    }

    // ----- Querying -----

    /// Get files read/edited recently, ordered by most recent first.
    pub fn recent_files(&self, max_age: Duration) -> Vec<String> {
        if !self.enabled {
            return vec![];
        }

        let cutoff = SystemTime::now() - max_age;
        let mut files: Vec<(u64, String)> = Vec::new();

        if let Some(events) = self.events.get(&self.session_id) {
            for event in events.iter().rev() {
                if event.timestamp < cutoff {
                    break;
                }
                let path = match &event.kind {
                    SessionEventKind::FileRead(p)
                    | SessionEventKind::FileEdit(p)
                    | SessionEventKind::FileWrite(p) => Some(p.clone()),
                    _ => None,
                };
                if let Some(p) = path {
                    if !files.iter().any(|(_, f)| f == &p) {
                        files.push((event.seq, p));
                    }
                }
            }
        }

        files.into_iter().map(|(_, f)| f).collect()
    }

    /// Get symbols looked up recently, ordered by most recent first.
    pub fn recent_symbols(&self, max_age: Duration) -> Vec<(String, Option<String>)> {
        if !self.enabled {
            return vec![];
        }

        let cutoff = SystemTime::now() - max_age;
        let mut symbols: Vec<(u64, String, Option<String>)> = Vec::new();

        if let Some(events) = self.events.get(&self.session_id) {
            for event in events.iter().rev() {
                if event.timestamp < cutoff {
                    break;
                }
                if let SessionEventKind::SymbolLookup { name, file } = &event.kind {
                    if !symbols.iter().any(|(_, n, _)| n == name) {
                        symbols.push((event.seq, name.clone(), file.clone()));
                    }
                }
            }
        }

        symbols.into_iter().map(|(_, n, f)| (n, f)).collect()
    }

    /// Compute session boost for a file path.
    ///
    /// Returns a boost value based on recent interactions with this file:
    /// - File read in last 5 min: +0.15 (with linear decay)
    /// - File edited in last 5 min: +0.25 (with linear decay)
    /// - Symbol lookup in last 10 min: +0.10 (with linear decay)
    pub fn compute_file_boost(&self, file_path: &str) -> f32 {
        if !self.enabled {
            return 0.0;
        }

        let now = SystemTime::now();
        let mut boost: f32 = 0.0;

        if let Some(events) = self.events.get(&self.session_id) {
            for event in events.iter().rev() {
                let age_secs = now
                    .duration_since(event.timestamp)
                    .unwrap_or_default()
                    .as_secs_f64();

                // Skip events older than 10 minutes.
                if age_secs > 600.0 {
                    break;
                }

                let decay = linear_decay(age_secs);

                match &event.kind {
                    SessionEventKind::FileRead(p) if p == file_path && age_secs <= 300.0 => {
                        boost += FILE_READ_BOOST * decay;
                    }
                    SessionEventKind::FileEdit(p) | SessionEventKind::FileWrite(p)
                        if p == file_path && age_secs <= 300.0 =>
                    {
                        boost += FILE_EDIT_BOOST * decay;
                    }
                    SessionEventKind::SymbolLookup { file: Some(f), .. } if f == file_path => {
                        boost += SYMBOL_LOOKUP_BOOST * decay;
                    }
                    _ => {}
                }
            }
        }

        // Apply progressive focus boost.
        if let Some(focus) = self.focus_directory.get(&self.session_id) {
            if file_path.starts_with(focus.as_str()) {
                boost += FOCUS_BOOST;
            }
        }

        boost
    }

    /// Compute session boost with graph propagation.
    ///
    /// Direct file: boost x 1.0
    /// 1-hop neighbors: boost x 0.3
    /// 2-hop neighbors: boost x 0.1
    pub fn compute_file_boost_with_graph(
        &self,
        file_path: &str,
        get_neighbors: &dyn Fn(&str) -> Vec<String>,
    ) -> f32 {
        if !self.enabled {
            return 0.0;
        }

        // Direct boost.
        let direct = self.compute_file_boost(file_path);
        if direct > 0.0 {
            return direct;
        }

        // Check if this file is a neighbor of any recently-interacted file.
        let recent = self.recent_files(Duration::from_secs(600));
        let mut propagated_boost: f32 = 0.0;

        for recent_file in &recent {
            let neighbors = self.get_or_cache_neighbors(recent_file, get_neighbors);
            let recent_boost = self.compute_file_boost(recent_file);

            if recent_boost > 0.0 {
                // 1-hop: file_path is a direct neighbor of recent_file.
                if neighbors.contains(&file_path.to_string()) {
                    propagated_boost += recent_boost * HOP_1_DAMPING;
                    continue;
                }

                // 2-hop: check neighbors of neighbors.
                for neighbor in &neighbors {
                    let n2 = self.get_or_cache_neighbors(neighbor, get_neighbors);
                    if n2.contains(&file_path.to_string()) {
                        propagated_boost += recent_boost * HOP_2_DAMPING;
                        break;
                    }
                }
            }
        }

        propagated_boost
    }

    /// Get or cache the import graph neighbors of a file.
    fn get_or_cache_neighbors(
        &self,
        file: &str,
        get_neighbors: &dyn Fn(&str) -> Vec<String>,
    ) -> Vec<String> {
        if let Some(cached) = self.neighbor_cache.get(file) {
            return cached.clone();
        }

        let neighbors = get_neighbors(file);
        self.neighbor_cache
            .insert(file.to_string(), neighbors.clone());
        neighbors
    }

    /// Invalidate the neighbor cache (call after index changes).
    pub fn invalidate_neighbor_cache(&self) {
        self.neighbor_cache.clear();
    }

    /// Get the current focus directory, if any.
    pub fn focus_directory(&self) -> Option<String> {
        self.focus_directory
            .get(&self.session_id)
            .map(|v| v.clone())
    }

    /// Reset progressive focus and interaction counts.
    pub fn reset_focus(&self) {
        self.focus_directory.remove(&self.session_id);
        self.dir_counts.clear();
        debug!("progressive focus reset");
    }

    /// Generate a structured session summary.
    pub fn summary(&self, token_budget: usize) -> String {
        if !self.enabled {
            return "Session tracking is disabled.".to_string();
        }

        let events = match self.events.get(&self.session_id) {
            Some(e) => e.clone(),
            None => return "No session events recorded.".to_string(),
        };

        if events.is_empty() {
            return "No session events recorded.".to_string();
        }

        let total = events.len();
        let elapsed = events
            .first()
            .and_then(|first| {
                events.last().and_then(|last| {
                    last.timestamp
                        .duration_since(first.timestamp)
                        .ok()
                        .map(|d| d.as_secs() / 60)
                })
            })
            .unwrap_or(0);

        // Group by directory.
        let mut dir_events: HashMap<String, DirSummary> = HashMap::new();

        for event in &events {
            match &event.kind {
                SessionEventKind::FileRead(path) => {
                    let dir = top_level_directory(path);
                    let entry = dir_events.entry(dir).or_default();
                    let fname = file_name(path);
                    if !entry.read.contains(&fname) {
                        entry.read.push(fname);
                    }
                    entry.event_count += 1;
                }
                SessionEventKind::FileEdit(path) | SessionEventKind::FileWrite(path) => {
                    let dir = top_level_directory(path);
                    let entry = dir_events.entry(dir).or_default();
                    let fname = file_name(path);
                    if !entry.edited.contains(&fname) {
                        entry.edited.push(fname);
                    }
                    entry.event_count += 1;
                }
                SessionEventKind::SymbolLookup { name, file } => {
                    let dir = file
                        .as_deref()
                        .map(top_level_directory)
                        .unwrap_or_else(|| "(global)".to_string());
                    let entry = dir_events.entry(dir).or_default();
                    if !entry.symbols.contains(name) {
                        entry.symbols.push(name.clone());
                    }
                    entry.event_count += 1;
                }
                SessionEventKind::Search { query, .. } => {
                    let entry = dir_events.entry("(searches)".to_string()).or_default();
                    if !entry.searches.contains(query) {
                        entry.searches.push(query.clone());
                    }
                    entry.event_count += 1;
                }
            }
        }

        // Sort directories by event count (most active first).
        let mut dirs: Vec<(String, DirSummary)> = dir_events.into_iter().collect();
        dirs.sort_by_key(|b| std::cmp::Reverse(b.1.event_count));

        // Build output.
        let mut out = format!("## Session Summary ({total} events, {elapsed} min)\n\n");
        let mut char_budget = token_budget * 4;

        if let Some(focus) = self.focus_directory() {
            out.push_str(&format!("**Active focus:** `{focus}`\n\n"));
        }

        for (dir, summary) in &dirs {
            if char_budget < 50 {
                out.push_str("\n*(truncated — increase token_budget for full summary)*\n");
                break;
            }

            let label = if dirs.first().map(|(d, _)| d) == Some(dir) {
                format!("### {dir} (most active)\n")
            } else {
                format!("### {dir}\n")
            };
            out.push_str(&label);
            char_budget = char_budget.saturating_sub(label.len());

            if !summary.edited.is_empty() {
                let line = format!("- Edited: {}\n", summary.edited.join(", "));
                out.push_str(&line);
                char_budget = char_budget.saturating_sub(line.len());
            }
            if !summary.read.is_empty() {
                let line = format!("- Read: {}\n", summary.read.join(", "));
                out.push_str(&line);
                char_budget = char_budget.saturating_sub(line.len());
            }
            if !summary.symbols.is_empty() {
                let line = format!("- Symbols: {}\n", summary.symbols.join(", "));
                out.push_str(&line);
                char_budget = char_budget.saturating_sub(line.len());
            }
            if !summary.searches.is_empty() {
                let items: Vec<String> = summary
                    .searches
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect();
                let line = format!("- Searched: {}\n", items.join(", "));
                out.push_str(&line);
                char_budget = char_budget.saturating_sub(line.len());
            }
        }

        out
    }

    /// Check which of the given symbols have been seen in this session.
    ///
    /// Returns (symbol_name, minutes_ago) for each previously explored symbol.
    pub fn previously_explored(&self, symbols: &[String]) -> Vec<(String, u64)> {
        if !self.enabled {
            return vec![];
        }

        let now = SystemTime::now();
        let mut result = Vec::new();

        if let Some(events) = self.events.get(&self.session_id) {
            for sym in symbols {
                for event in events.iter().rev() {
                    if let SessionEventKind::SymbolLookup { name, .. } = &event.kind {
                        if name == sym {
                            let mins = now
                                .duration_since(event.timestamp)
                                .unwrap_or_default()
                                .as_secs()
                                / 60;
                            result.push((sym.clone(), mins));
                            break;
                        }
                    }
                }
            }
        }

        result
    }

    // ----- Persistence -----

    /// Flush session state to disk.
    pub fn flush(&self) {
        if !self.enabled {
            return;
        }
        let Some(root) = &self.root else { return };
        let session_file = root.join(".codixing/session.json");

        let data = SessionPersist {
            session_id: self.session_id.clone(),
            created_at: self.created_at,
            events: self
                .events
                .get(&self.session_id)
                .map(|e| e.clone())
                .unwrap_or_default(),
        };

        match serde_json::to_string_pretty(&data) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&session_file, json) {
                    warn!(error = %e, "failed to flush session state");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize session state"),
        }
    }

    /// Try to restore session from persisted state.
    fn try_restore(&mut self) {
        let Some(root) = &self.root else { return };
        let session_file = root.join(".codixing/session.json");

        if !session_file.exists() {
            return;
        }

        let data = match std::fs::read_to_string(&session_file) {
            Ok(s) => s,
            Err(e) => {
                debug!(error = %e, "no session file to restore");
                return;
            }
        };

        let persist: SessionPersist = match serde_json::from_str(&data) {
            Ok(p) => p,
            Err(e) => {
                debug!(error = %e, "corrupt session file, starting fresh");
                return;
            }
        };

        // Only restore if the session is less than RESTORE_MAX_AGE_HOURS old.
        let age = SystemTime::now()
            .duration_since(persist.created_at)
            .unwrap_or_default();
        if age > Duration::from_secs(RESTORE_MAX_AGE_HOURS * 3600) {
            info!(
                age_hours = age.as_secs() / 3600,
                "session too old to restore, starting fresh"
            );
            std::fs::remove_file(&session_file).ok();
            return;
        }

        info!(
            session_id = %persist.session_id,
            events = persist.events.len(),
            "restored session from disk"
        );

        self.session_id = persist.session_id;
        self.created_at = persist.created_at;
        let seq = persist.events.last().map(|e| e.seq + 1).unwrap_or(0);
        self.seq_counter.store(seq, Ordering::Relaxed);
        self.events.insert(self.session_id.clone(), persist.events);
    }

    /// Clean up expired sessions (older than 24 hours).
    pub fn cleanup_old_sessions(&self) {
        let Some(root) = &self.root else { return };
        let session_file = root.join(".codixing/session.json");

        if !session_file.exists() {
            return;
        }

        let data = match std::fs::read_to_string(&session_file) {
            Ok(s) => s,
            Err(_) => return,
        };

        let persist: SessionPersist = match serde_json::from_str(&data) {
            Ok(p) => p,
            Err(_) => return,
        };

        let age = SystemTime::now()
            .duration_since(persist.created_at)
            .unwrap_or_default();
        if age > Duration::from_secs(24 * 3600) {
            info!("cleaning up session older than 24 hours");
            std::fs::remove_file(&session_file).ok();
        }
    }

    /// Check if a session is expired (older than SESSION_EXPIRE_HOURS).
    pub fn is_expired(&self) -> bool {
        let age = SystemTime::now()
            .duration_since(self.created_at)
            .unwrap_or_default();
        age > Duration::from_secs(SESSION_EXPIRE_HOURS * 3600)
    }
}

// ---------------------------------------------------------------------------
// Persistence data
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct SessionPersist {
    session_id: String,
    created_at: SystemTime,
    events: Vec<SessionEvent>,
}

#[derive(Default)]
struct DirSummary {
    event_count: usize,
    edited: Vec<String>,
    read: Vec<String>,
    symbols: Vec<String>,
    searches: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Linear decay from 1.0 at age=0 to 0.0 at age=2*half_life.
fn linear_decay(age_secs: f64) -> f32 {
    let full_life = DECAY_HALF_LIFE_SECS * 2.0;
    if age_secs >= full_life {
        return 0.0;
    }
    (1.0 - age_secs / full_life) as f32
}

/// Extract the top-level directory from a path.
/// e.g. "crates/core/src/retriever/mod.rs" -> "crates/core/src/retriever/"
fn top_level_directory(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return "(root)".to_string();
    }
    let dir_parts = &parts[..parts.len() - 1];
    format!("{}/", dir_parts.join("/"))
}

/// Extract the file name from a path.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Generate a simple UUID v4 (no external dependency).
fn uuid_v4() -> String {
    use std::time::UNIX_EPOCH;

    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id() as u128;
    let seed = t ^ (pid << 64);

    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (seed >> 96) as u32,
        (seed >> 80) as u16,
        (seed >> 64) as u16 & 0x0FFF,
        ((seed >> 48) as u16 & 0x3FFF) | 0x8000,
        seed as u64 & 0xFFFFFFFFFFFF
    )
}

// ---------------------------------------------------------------------------
// Public constants (for use by retriever)
// ---------------------------------------------------------------------------

/// Damping factor for 1-hop graph neighbors.
pub const GRAPH_HOP_1_DAMPING: f32 = HOP_1_DAMPING;

/// Damping factor for 2-hop graph neighbors.
pub const GRAPH_HOP_2_DAMPING: f32 = HOP_2_DAMPING;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_recent_files() {
        let session = SessionState::new(true);

        session.record(SessionEventKind::FileRead("src/main.rs".into()));
        session.record(SessionEventKind::FileRead("src/lib.rs".into()));
        session.record(SessionEventKind::FileEdit("src/main.rs".into()));

        let recent = session.recent_files(Duration::from_secs(60));
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0], "src/main.rs");
        assert_eq!(recent[1], "src/lib.rs");
    }

    #[test]
    fn test_record_and_recent_symbols() {
        let session = SessionState::new(true);

        session.record(SessionEventKind::SymbolLookup {
            name: "Engine".into(),
            file: Some("src/engine.rs".into()),
        });
        session.record(SessionEventKind::SymbolLookup {
            name: "Parser".into(),
            file: Some("src/parser.rs".into()),
        });

        let recent = session.recent_symbols(Duration::from_secs(60));
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].0, "Parser");
        assert_eq!(recent[1].0, "Engine");
    }

    #[test]
    fn test_session_boost() {
        let session = SessionState::new(true);

        session.record(SessionEventKind::FileEdit("auth.rs".into()));
        session.record(SessionEventKind::FileRead("handler.rs".into()));

        let edit_boost = session.compute_file_boost("auth.rs");
        let read_boost = session.compute_file_boost("handler.rs");
        let no_boost = session.compute_file_boost("unrelated.rs");

        assert!(
            edit_boost > read_boost,
            "edit boost ({edit_boost}) should be > read boost ({read_boost})"
        );
        assert!(read_boost > 0.0, "read boost should be > 0");
        assert_eq!(no_boost, 0.0, "unrelated file should have 0 boost");
    }

    #[test]
    fn test_progressive_focus() {
        let session = SessionState::new(true);

        assert!(session.focus_directory().is_none());

        for i in 0..6 {
            session.record(SessionEventKind::FileRead(format!(
                "crates/core/src/retriever/file{i}.rs"
            )));
        }

        let focus = session.focus_directory();
        assert!(focus.is_some(), "focus should be set after 5+ events");
        assert!(
            focus.as_deref().unwrap().contains("retriever"),
            "focus should be on retriever directory"
        );

        session.reset_focus();
        assert!(session.focus_directory().is_none());
    }

    #[test]
    fn test_event_limit() {
        let session = SessionState::new(true);

        for i in 0..MAX_EVENTS + 50 {
            session.record(SessionEventKind::FileRead(format!("file{i}.rs")));
        }

        assert_eq!(session.event_count(), MAX_EVENTS);
    }

    #[test]
    fn test_disabled_session() {
        let session = SessionState::new(false);

        session.record(SessionEventKind::FileRead("src/main.rs".into()));

        assert_eq!(session.event_count(), 0);
        assert!(session.recent_files(Duration::from_secs(60)).is_empty());
        assert_eq!(session.compute_file_boost("src/main.rs"), 0.0);
    }

    #[test]
    fn test_summary() {
        let session = SessionState::new(true);

        session.record(SessionEventKind::FileRead(
            "crates/core/src/engine.rs".into(),
        ));
        session.record(SessionEventKind::FileEdit(
            "crates/core/src/retriever/mod.rs".into(),
        ));
        session.record(SessionEventKind::Search {
            query: "session boost".into(),
            result_count: 5,
        });

        let summary = session.summary(1500);
        assert!(summary.contains("Session Summary"));
        assert!(summary.contains("engine.rs") || summary.contains("mod.rs"));
    }

    #[test]
    fn test_previously_explored() {
        let session = SessionState::new(true);

        session.record(SessionEventKind::SymbolLookup {
            name: "Engine".into(),
            file: Some("src/engine.rs".into()),
        });
        session.record(SessionEventKind::SymbolLookup {
            name: "Parser".into(),
            file: Some("src/parser.rs".into()),
        });

        let explored =
            session.previously_explored(&["Engine".into(), "Parser".into(), "Unknown".into()]);

        assert_eq!(explored.len(), 2);
        assert!(explored.iter().any(|(n, _)| n == "Engine"));
        assert!(explored.iter().any(|(n, _)| n == "Parser"));
    }

    #[test]
    fn test_linear_decay() {
        assert!((linear_decay(0.0) - 1.0).abs() < 0.001);
        assert!((linear_decay(DECAY_HALF_LIFE_SECS) - 0.5).abs() < 0.001);
        assert_eq!(linear_decay(DECAY_HALF_LIFE_SECS * 2.0), 0.0);
        assert_eq!(linear_decay(DECAY_HALF_LIFE_SECS * 3.0), 0.0);
    }
}
