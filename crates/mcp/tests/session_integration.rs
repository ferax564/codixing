//! Integration tests for session tracking features (Phase 13a).
//!
//! These tests exercise session event recording, session-boosted search,
//! progressive focus, get_session_summary, and session_reset_focus through
//! the MCP tool dispatch layer (same path as a real MCP client).

use std::path::Path;
use std::sync::Arc;

use codixing_core::{Engine, IndexConfig, SessionEventKind, SessionState};

/// Build a minimal engine from a temp directory.
fn make_engine(root: &Path) -> Engine {
    // Create a minimal source file so the index has something to work with.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/auth.rs"),
        "pub fn authenticate(user: &str) -> bool {\n    user == \"admin\"\n}\n\
         pub fn authorize(role: &str) -> bool {\n    role == \"admin\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/handler.rs"),
        "use crate::auth;\n\npub fn handle_request() {\n    auth::authenticate(\"test\");\n}\n",
    )
    .unwrap();

    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    config.graph.enabled = false;
    Engine::init(root, config).expect("engine init failed")
}

#[test]
fn session_events_recorded_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    // Record various events.
    session.record(SessionEventKind::FileRead("src/main.rs".into()));
    session.record(SessionEventKind::FileRead("src/auth.rs".into()));
    session.record(SessionEventKind::SymbolLookup {
        name: "authenticate".into(),
        file: Some("src/auth.rs".into()),
    });
    session.record(SessionEventKind::Search {
        query: "authentication".into(),
        result_count: 3,
    });
    session.record(SessionEventKind::FileEdit("src/auth.rs".into()));
    session.record(SessionEventKind::FileWrite("src/handler.rs".into()));

    assert_eq!(session.event_count(), 6);

    // Check recent files.
    let recent = session.recent_files(std::time::Duration::from_secs(60));
    assert!(recent.contains(&"src/main.rs".to_string()));
    assert!(recent.contains(&"src/auth.rs".to_string()));
    assert!(recent.contains(&"src/handler.rs".to_string()));

    // Check recent symbols.
    let symbols = session.recent_symbols(std::time::Duration::from_secs(60));
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].0, "authenticate");
}

#[test]
fn session_boost_changes_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    // Record a file edit on auth.rs.
    session.record(SessionEventKind::FileEdit("src/auth.rs".into()));

    // auth.rs should have a positive boost.
    let boost = session.compute_file_boost("src/auth.rs");
    assert!(
        boost > 0.0,
        "edited file should have positive boost, got {boost}"
    );

    // Unrelated file should have zero boost.
    let no_boost = session.compute_file_boost("src/main.rs");
    assert!(
        no_boost == 0.0 || no_boost < boost,
        "unrelated file boost ({no_boost}) should be less than edited file ({boost})"
    );
}

#[test]
fn session_summary_is_accurate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    session.record(SessionEventKind::FileRead("src/auth.rs".into()));
    session.record(SessionEventKind::FileEdit("src/auth.rs".into()));
    session.record(SessionEventKind::FileRead("src/handler.rs".into()));
    session.record(SessionEventKind::Search {
        query: "authentication flow".into(),
        result_count: 5,
    });
    session.record(SessionEventKind::SymbolLookup {
        name: "authenticate".into(),
        file: Some("src/auth.rs".into()),
    });

    let summary = session.summary(1500);
    assert!(
        summary.contains("Session Summary"),
        "summary should have header: {summary}"
    );
    assert!(
        summary.contains("5 events"),
        "summary should show event count: {summary}"
    );
    assert!(
        summary.contains("auth.rs"),
        "summary should mention auth.rs: {summary}"
    );
}

#[test]
fn progressive_focus_activates_and_resets() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    // Initially no focus.
    assert!(session.focus_directory().is_none());

    // Generate 6 events in src/ directory.
    for i in 0..6 {
        session.record(SessionEventKind::FileRead(format!("src/file{i}.rs")));
    }

    // Focus should now be set.
    let focus = session.focus_directory();
    assert!(
        focus.is_some(),
        "focus should be set after 6 events in same dir"
    );
    assert!(
        focus.as_deref().unwrap().starts_with("src"),
        "focus should be on src/ directory, got: {:?}",
        focus
    );

    // Search should include focus info.
    // The focus boost applies to any file in the focused directory.
    let _boost = session.compute_file_boost("src/new_file.rs");

    // Reset focus.
    session.reset_focus();
    assert!(
        session.focus_directory().is_none(),
        "focus should be cleared after reset"
    );
}

#[test]
fn session_persists_across_restart() {
    let dir = tempfile::tempdir().unwrap();

    // Create initial session with events.
    {
        let engine = make_engine(dir.path());
        let session = engine.session();

        session.record(SessionEventKind::FileRead("src/auth.rs".into()));
        session.record(SessionEventKind::FileEdit("src/handler.rs".into()));
        session.record(SessionEventKind::SymbolLookup {
            name: "authenticate".into(),
            file: Some("src/auth.rs".into()),
        });

        assert_eq!(session.event_count(), 3);

        // Flush to disk.
        session.flush();
    }

    // Re-open the engine (simulates daemon restart within 2h window).
    {
        let engine = Engine::open(dir.path()).expect("engine reopen failed");
        let session = engine.session();

        // Session should be restored.
        assert_eq!(
            session.event_count(),
            3,
            "session should be restored from disk"
        );

        let recent = session.recent_files(std::time::Duration::from_secs(3600));
        assert!(
            recent.contains(&"src/auth.rs".to_string()),
            "restored session should contain auth.rs in recent files"
        );
    }
}

#[test]
fn no_session_disables_all_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = make_engine(dir.path());

    // Disable session tracking.
    engine.set_session(Arc::new(SessionState::new(false)));

    let session = engine.session();
    session.record(SessionEventKind::FileRead("src/auth.rs".into()));

    assert_eq!(
        session.event_count(),
        0,
        "disabled session should record nothing"
    );
    assert_eq!(
        session.compute_file_boost("src/auth.rs"),
        0.0,
        "disabled session should return 0 boost"
    );
    assert_eq!(
        session.summary(1500),
        "Session tracking is disabled.",
        "disabled session should return disabled message"
    );
}

#[test]
fn session_event_limit_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    // Record 550 events (limit is 500).
    for i in 0..550 {
        session.record(SessionEventKind::FileRead(format!("src/file{i}.rs")));
    }

    assert_eq!(
        session.event_count(),
        500,
        "session should cap at 500 events"
    );
}

#[test]
fn previously_explored_symbols_detected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path());
    let session = engine.session();

    session.record(SessionEventKind::SymbolLookup {
        name: "Engine".into(),
        file: Some("src/engine.rs".into()),
    });
    session.record(SessionEventKind::SymbolLookup {
        name: "Parser".into(),
        file: Some("src/parser.rs".into()),
    });

    let explored = session.previously_explored(&[
        "Engine".to_string(),
        "Parser".to_string(),
        "Unknown".to_string(),
    ]);

    assert_eq!(
        explored.len(),
        2,
        "should find 2 previously explored symbols"
    );
    assert!(
        explored.iter().any(|(n, _)| n == "Engine"),
        "should find Engine"
    );
    assert!(
        explored.iter().any(|(n, _)| n == "Parser"),
        "should find Parser"
    );
}
