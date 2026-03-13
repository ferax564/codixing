# Sprint Plan: Phase 12 + 13a

**Duration:** 4 weeks (2026-03-17 → 2026-04-11)
**Goal:** Ship distribution (Phase 12) in parallel with the first session-aware retrieval features (Phase 13a). Distribution is non-engineering work that shouldn't block engine development.

---

## Week 1: Foundation + Distribution Kick-off

### Engineering: Session event tracking (13a backbone)

**Ticket 1.1 — Session state module** `crates/core/src/session.rs`
- New module: `SessionState` struct holding an in-memory event log
- Events: `FileRead(path)`, `SymbolLookup(name, path)`, `Search(query, results)`, `FileEdit(path)`, `FileWrite(path)`
- Each event gets a timestamp and monotonic sequence number
- Storage: `DashMap<SessionId, Vec<SessionEvent>>` for concurrent access
- SQLite persistence: write events to `session_events` table in `.codixing/session.db` on flush (every 30s or on shutdown)
- Restore: on daemon startup, load last session if < 2 hours old
- **Files to modify:** new `crates/core/src/session.rs`, add `mod session` to `crates/core/src/lib.rs`
- **AC:** `SessionState::record()`, `SessionState::recent_files()`, `SessionState::recent_symbols()` pass unit tests

**Ticket 1.2 — Wire session events into Engine**
- Add `session: Arc<SessionState>` field to `Engine` struct in `crates/core/src/engine.rs`
- Emit `FileRead` events from `read_file`, `SymbolLookup` from `find_symbol`/`explain`, `Search` from `search`, `FileEdit`/`FileWrite` from `write_file`/`edit_file`
- No behavior change yet — just recording
- **Files to modify:** `crates/core/src/engine.rs`
- **AC:** After calling engine methods, `session.recent_files()` returns the correct paths

**Ticket 1.3 — Wire session into MCP server**
- Pass `Arc<SessionState>` from daemon into MCP tool handlers in `crates/mcp/src/tools.rs`
- Each tool call emits the appropriate session event via the engine
- Add `session_id` field to daemon state (UUID, generated on daemon start, rotated on explicit reset)
- **Files to modify:** `crates/mcp/src/tools.rs`, `crates/mcp/src/main.rs` (or daemon entry point)
- **AC:** Running MCP tools in daemon mode populates the session log; `index_status` response includes `session_event_count`

### Distribution (parallel, non-engineering)

**Ticket 1.4 — README rewrite**
- Lead with "The code context engine" positioning
- Move the b2Vec2 benchmark into the first 20 lines
- Add a "Quick demo" section with terminal commands showing before/after (grep vs codixing)
- Trim the feature-list sections below the fold
- **AC:** README opens with value prop, not install instructions

**Ticket 1.5 — Install script validation**
- Test `curl -fsSL https://codixing.com/install.sh | sh` on: macOS ARM (M1+), macOS x86, Ubuntu 22.04, Ubuntu 24.04, Fedora 40
- Fix any failures
- **AC:** Clean install succeeds on all 5 targets, produces working `codixing --version`

---

## Week 2: Session-Boosted Search + Listings

### Engineering: Session boost in retrieval

**Ticket 2.1 — Session boost scoring**
- Add `session_boost: f32` field to `SearchResult` in `crates/core/src/retriever/mod.rs`
- New function `compute_session_boost(path, session: &SessionState) -> f32`:
  - File read in last 5 min → +0.15
  - File edited in last 5 min → +0.25
  - Symbol looked up in last 10 min → +0.10
  - Decay: linear decay from event time to now (5 min half-life)
- Apply boost in the retriever pipeline after BM25/hybrid scoring, before final sort
- **Files to modify:** `crates/core/src/retriever/mod.rs`, `crates/core/src/retriever/hybrid.rs`, `crates/core/src/retriever/bm25.rs`
- **AC:** Unit test: after recording a `FileEdit("auth.rs")` event, search for "handler" ranks `auth.rs` results higher than without the session

**Ticket 2.2 — Graph-propagated session context**
- When computing session boost for a file, also apply a dampened boost to its 1-hop neighbors in the import graph:
  - Direct file: boost × 1.0
  - Caller/callee (1-hop): boost × 0.3
  - 2-hop: boost × 0.1 (optional, skip if perf impact > 1ms)
- Use existing `get_references()` from `crates/core/src/engine.rs` to find neighbors
- Cache neighbor sets for the session (invalidate on index change)
- **Files to modify:** `crates/core/src/retriever/mod.rs`, `crates/core/src/session.rs` (add neighbor cache)
- **AC:** Test: editing `auth.rs` causes its callers to rank higher for related queries

**Ticket 2.3 — `get_session_summary` MCP tool**
- New tool in `crates/mcp/src/tools.rs`
- Groups session events by file/module subsystem (use directory structure as proxy for modules)
- Output format:
  ```
  ## Session Summary (47 events, 12 min)
  ### crates/core/src/retriever/ (most active)
  - Edited: mod.rs, hybrid.rs
  - Read: bm25.rs, vector.rs
  - Searched: "session boost", "ranking pipeline"
  ### crates/mcp/src/
  - Read: tools.rs
  ```
- Token-budgeted (default 1500 tokens, configurable)
- **AC:** After a multi-tool session, `get_session_summary` returns a structured, accurate summary

### Distribution (parallel)

**Ticket 2.4 — MCP directory submissions**
- Submit to: awesome-mcp-servers (GitHub PR), mcp.so (submission form), glama.ai (submission form)
- Include: description, install command, tool count, benchmark highlight
- **AC:** PRs/submissions filed for 3+ directories

**Ticket 2.5 — Continue.dev integration guide**
- Write `docs/guides/continue-dev.md` with step-by-step MCP setup for Continue.dev
- Submit as PR to Continue.dev docs repo
- **AC:** Guide written, PR submitted

---

## Week 3: Progressive Focus + Cursor Guide

### Engineering: Progressive focus and explain enhancement

**Ticket 3.1 — Progressive focus**
- Track interaction density per directory in `SessionState`:
  - Count events per top-level module directory
  - After 5+ events in the same directory, set `focus_directory` on the session
- When `focus_directory` is set:
  - Results from that directory get an additional +0.10 boost
  - Results from *outside* that directory are not penalized (avoid hiding things)
  - Search response includes a `focus: "crates/core/src/retriever/"` field so the agent knows narrowing is active
- `session_reset_focus` MCP tool: clears the focus, resets interaction counts
- **Files to modify:** `crates/core/src/session.rs`, `crates/core/src/retriever/mod.rs`, `crates/mcp/src/tools.rs`
- **AC:** After 6 searches/reads in `retriever/`, subsequent searches show `focus` field; `session_reset_focus` clears it

**Ticket 3.2 — Session-aware explain**
- Enhance the `explain` MCP tool:
  - After assembling definition + callers + callees, check which of those symbols the agent has already seen this session
  - Add a `session_context` section to the output: "Previously explored: `compute_session_boost` (2 min ago), `SearchResult` (5 min ago)"
  - If a callee was already explained, show a one-line summary instead of full context (save tokens)
- **Files to modify:** `crates/mcp/src/tools.rs` (explain handler)
- **AC:** Calling `explain` on symbol B after already exploring symbol A (which calls B) shows "Previously explored: A" in the output

**Ticket 3.3 — Integration test suite for session features**
- End-to-end test: start daemon → call 10+ MCP tools in sequence → verify:
  - Session events recorded correctly
  - Search ranking changes based on session context
  - `get_session_summary` output is accurate
  - Session persists across simulated compaction (daemon restart within 2h window)
  - `session_reset_focus` works
- **Files:** new `crates/mcp/tests/session_integration.rs`
- **AC:** All integration tests pass in CI

### Distribution (parallel)

**Ticket 3.4 — Cursor MCP guide**
- Write `docs/guides/cursor.md` with `.cursor/mcp.json` setup
- Include screenshot of Codixing tools appearing in Cursor's MCP panel
- **AC:** Guide written and verified on Cursor latest

---

## Week 4: Polish, VS Code Publish, Ship

### Engineering: Polish and edge cases

**Ticket 4.1 — Session event limits and cleanup**
- Cap session events at 500 (FIFO eviction of oldest)
- Auto-expire sessions older than 4 hours
- SQLite cleanup: delete sessions older than 24 hours on daemon start
- Memory budget: ensure `SessionState` stays under 2MB for a 500-event session
- **Files to modify:** `crates/core/src/session.rs`
- **AC:** Memory stays bounded; old sessions cleaned up; no performance regression in retriever benchmarks

**Ticket 4.2 — Session opt-out flag**
- Add `--no-session` flag to daemon / MCP server
- When set, session tracking is disabled entirely (no events recorded, no boost applied)
- For privacy-conscious users or benchmarking without session influence
- **Files to modify:** `crates/mcp/src/main.rs`, `crates/core/src/engine.rs`
- **AC:** `--no-session` flag disables all session behavior; search results are identical to pre-session behavior

**Ticket 4.3 — Update tool count and docs**
- Update README: tool count (now 27: +`get_session_summary`, +`session_reset_focus`, plus existing 25)
- Update CLAUDE.md if needed
- Add session features to the MCP tools table in README
- **AC:** Docs reflect new tools accurately

### Distribution: Ship

**Ticket 4.4 — VS Code extension publish**
- Bump version to 0.2.0 in `editors/vscode/package.json`
- Add marketplace metadata: icon, description, categories, keywords
- `vsce package && vsce publish`
- **AC:** Extension live on VS Code Marketplace, installable via Extensions panel

**Ticket 4.5 — Launch post**
- Write "Show HN" post: "Codixing — code context engine with session-aware retrieval for AI agents"
- Lead with the problem (agents lose context after compaction), the solution (graph-propagated session intelligence), and the benchmark (b2Vec2 case)
- Post to HN, r/ClaudeAI, r/cursor, r/neovim
- **AC:** Posts published

---

## Sprint Exit Criteria

| Criteria | Measurement |
|---|---|
| Session event tracking works in daemon mode | Integration tests pass |
| Session-boosted search measurably changes ranking | Unit test: session-boosted file ranks higher |
| Graph propagation works (1-hop neighbors boosted) | Unit test with known graph topology |
| `get_session_summary` returns accurate structured output | Integration test |
| Progressive focus activates after 5+ interactions | Integration test |
| `session_reset_focus` clears focus | Integration test |
| `explain` shows session context | Integration test |
| Session persists across daemon restart (< 2h) | Integration test |
| `--no-session` disables all session behavior | Unit test |
| No performance regression | Retriever benchmark < 5% slower with session enabled |
| All existing 334 tests still pass | `cargo test --workspace` |
| `cargo clippy --workspace -- -D warnings` passes | CI |
| VS Code extension published | Live on Marketplace |
| MCP directory submissions filed | 3+ directories |
| Continue.dev + Cursor guides written | Docs exist |
| README rewritten with new positioning | Committed |

---

## What's Next (Phase 13b preview)

After this sprint ships, Phase 13b (Temporal Code Context) builds on the same `SessionState` infrastructure:
- `get_hotspots` uses git log + the same scoring pipeline
- `search_changes` reuses the session summary grouping logic for git diff results
- Blame-aware `explain` extends the same `session_context` output section
- The session SQLite database gets a `git_events` table alongside `session_events`

The session infrastructure from 13a is the foundation for everything in 13b. That's why we build it first.
