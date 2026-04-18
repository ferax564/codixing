//! Integration tests for the Codixing LSP — full JSON-RPC protocol roundtrips.
//!
//! These tests spawn the compiled `codixing-lsp` binary as a subprocess, speak
//! the LSP wire protocol (`Content-Length`-framed JSON-RPC over stdio), and
//! assert on the decoded responses.
//!
//! Strategy:
//! 1. Create a tempdir containing a tiny Rust source file with a known symbol
//!    (`hello_world`) and call it twice so references/rename have ≥1 hit.
//! 2. `Engine::init` builds the BM25-only index into `.codixing/` inside that
//!    tempdir (no ONNX, no model download).
//! 3. Drop the engine, spawn `codixing-lsp --root <tempdir>`.
//! 4. Send a scripted sequence of LSP requests, assert on responses.
//!
//! The binary is located via `env!("CARGO_BIN_EXE_codixing-lsp")`, which Cargo
//! auto-populates for integration tests and guarantees the binary is rebuilt
//! before the tests run. This keeps the tests CI-safe with no external setup.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

/// Convert an absolute filesystem path to a well-formed `file://` URI.
///
/// String concatenation (`"file://{}".format(path)`) produces invalid URIs on
/// Windows paths (drive letters, backslashes). `Url::from_file_path` handles
/// those edge cases correctly across platforms.
fn path_to_uri(p: &Path) -> String {
    Url::from_file_path(p)
        .expect("test fixture paths must be absolute")
        .to_string()
}

// Fixture source — one function, called twice, so `references` returns ≥ 1.
const FIXTURE_SRC: &str = r#"pub fn hello_world() -> &'static str {
    "hello"
}

pub fn main_entry() {
    let _a = hello_world();
    let _b = hello_world();
}
"#;

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

fn build_fixture() -> (TempDir, PathBuf) {
    use codixing_core::{EmbeddingConfig, Engine, IndexConfig};

    let dir = tempfile::tempdir().expect("tempdir");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let rs_path = src_dir.join("lib.rs");
    std::fs::write(&rs_path, FIXTURE_SRC).unwrap();

    // BM25-only engine: no embedding model download, fast init.
    let mut config = IndexConfig::new(dir.path());
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..EmbeddingConfig::default()
    };

    let engine = Engine::init(dir.path(), config).expect("engine init");
    // Persist + release locks before spawning the LSP subprocess, which will
    // call Engine::open on the same root.
    drop(engine);

    (dir, rs_path)
}

// ---------------------------------------------------------------------------
// LSP wire protocol helpers
// ---------------------------------------------------------------------------

struct LspHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicI64,
    // Keep the stderr-drain thread alive. Joined implicitly on Drop — the
    // drain exits when the child closes its stderr pipe.
    _stderr_drain: Option<JoinHandle<()>>,
}

impl LspHarness {
    fn spawn(root: &Path) -> Self {
        let bin = env!("CARGO_BIN_EXE_codixing-lsp");
        let mut child = Command::new(bin)
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn codixing-lsp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        // Drain stderr continuously so the child never blocks writing logs.
        // Without this, a chatty LSP run can fill the 64KiB pipe buffer and
        // deadlock the server mid-response.
        let stderr_drain = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buf)
                    && n > 0
                {}
            })
        });

        Self {
            child,
            stdin,
            stdout,
            next_id: AtomicI64::new(1),
            _stderr_drain: stderr_drain,
        }
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_vec(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.stdin.write_all(&body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read one framed LSP message, returning the decoded JSON value.
    ///
    /// Fails the test if no message arrives within `Self::READ_TIMEOUT`.
    /// (We don't enforce a hard wall timeout because Rust's std BufReader has
    /// no native read_timeout; instead the overall test uses a budget via the
    /// outer `run_with_timeout` wrapper.)
    fn recv(&mut self) -> Value {
        // Read header lines until blank line.
        let mut content_length: Option<usize> = None;
        loop {
            let mut header = String::new();
            let n = self
                .stdout
                .read_line(&mut header)
                .expect("read header line");
            if n == 0 {
                panic!("LSP server closed stdout before completing response");
            }
            let trimmed = header.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(rest.trim().parse::<usize>().unwrap());
            }
        }
        let len = content_length.expect("LSP response missing Content-Length");
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf).expect("read body");
        serde_json::from_slice(&buf).expect("valid JSON response")
    }

    /// Send a request and read messages until we get a response matching `id`.
    /// Notifications (no `id`) are discarded.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let msg = self.recv();
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
            // else: notification (e.g. window/logMessage, textDocument/publishDiagnostics) — skip
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn shutdown(&mut self) {
        // Best-effort: send shutdown request + exit notification, then kill as
        // a belt-and-suspenders to avoid leaking subprocesses on test failure.
        let _ = self.send_and_ignore(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "shutdown",
            "params": null,
        }));
        let _ = self.send_and_ignore(&json!({
            "jsonrpc": "2.0",
            "method": "exit",
        }));
        std::thread::sleep(Duration::from_millis(50));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn send_and_ignore(&mut self, msg: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for LspHarness {
    fn drop(&mut self) {
        // Ensure we don't leak subprocesses even if a test panics.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Run a test body with a 10-second wall clock deadline. If it exceeds, panic.
// The LSP subprocess is cheap (BM25-only, single file), so any test taking
// >10s indicates a hang.
//
// Panics inside `f` are re-raised on the calling thread via JoinHandle::join
// so assertion failures inside the spawned thread are reported correctly
// instead of being misattributed to a timeout.
fn with_test_budget<F: FnOnce() + Send + 'static>(f: F) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    match rx.recv_timeout(deadline.duration_since(Instant::now())) {
        Ok(()) => {
            // Thread finished cleanly — join to reap it.
            handle.join().expect("test body thread join");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // Sender dropped without sending → `f` panicked. Join to surface
            // the real panic payload on this thread.
            if let Err(panic) = handle.join() {
                std::panic::resume_unwind(panic);
            }
            panic!("LSP test thread exited without reporting completion");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("LSP test exceeded 10-second budget (hang?)");
        }
    }
}

fn initialize(harness: &mut LspHarness, root: &Path) -> Value {
    let root_uri = path_to_uri(root);
    let params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {},
    });
    let resp = harness.request("initialize", params);
    harness.notify("initialized", json!({}));
    resp
}

fn did_open(harness: &mut LspHarness, path: &Path, text: &str) {
    let uri = path_to_uri(path);
    harness.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rust",
                "version": 1,
                "text": text,
            }
        }),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn initialize_handshake_returns_capabilities() {
    with_test_budget(|| {
        let (dir, _rs) = build_fixture();
        let mut harness = LspHarness::spawn(dir.path());

        let resp = initialize(&mut harness, dir.path());

        let caps = resp
            .get("result")
            .and_then(|r| r.get("capabilities"))
            .unwrap_or_else(|| panic!("missing capabilities in {resp}"));

        // Presence of each capability key is sufficient — their specific shapes
        // (bool vs object) are covered in the LSP main.rs unit tests.
        for key in [
            "textDocumentSync",
            "hoverProvider",
            "definitionProvider",
            "referencesProvider",
            "renameProvider",
        ] {
            assert!(
                caps.get(key).is_some(),
                "capability {key} missing from initialize response: {caps}"
            );
        }

        harness.shutdown();
        drop(dir);
    });
}

#[test]
fn hover_returns_markdown_on_rust_symbol() {
    with_test_budget(|| {
        let (dir, rs) = build_fixture();
        let mut harness = LspHarness::spawn(dir.path());

        let _init = initialize(&mut harness, dir.path());
        did_open(&mut harness, &rs, FIXTURE_SRC);

        let uri = path_to_uri(&rs);
        // Line 0, col 9 — lands on the `hello_world` identifier.
        let resp = harness.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 9 },
            }),
        );

        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("hover response missing result: {resp}"));
        assert!(
            !result.is_null(),
            "hover result should not be null for known symbol: {resp}"
        );

        let contents = result
            .get("contents")
            .unwrap_or_else(|| panic!("hover missing contents: {result}"));
        // tower-lsp serializes MarkupContent as `{"kind": "markdown", "value": "..."}`.
        let value = contents
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("hover contents.value should be a string: {contents}"));
        assert!(
            !value.is_empty(),
            "hover contents.value should be non-empty"
        );
        assert!(
            value.contains("hello_world"),
            "hover markdown should mention the symbol: {value}"
        );

        harness.shutdown();
        drop(dir);
    });
}

#[test]
fn goto_definition_finds_local_symbol() {
    with_test_budget(|| {
        let (dir, rs) = build_fixture();
        let mut harness = LspHarness::spawn(dir.path());

        let _init = initialize(&mut harness, dir.path());
        did_open(&mut harness, &rs, FIXTURE_SRC);

        let uri = path_to_uri(&rs);
        // Line 5, col 13 — lands inside the first `hello_world()` call.
        let resp = harness.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 5, "character": 13 },
            }),
        );

        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("definition response missing result: {resp}"));
        assert!(
            !result.is_null(),
            "definition result should not be null: {resp}"
        );

        // Response is either a single Location or a Vec<Location>; we accept either.
        let has_uri = match result {
            Value::Object(obj) => obj.get("uri").and_then(|u| u.as_str()).is_some(),
            Value::Array(arr) => arr
                .iter()
                .any(|l| l.get("uri").and_then(|u| u.as_str()).is_some()),
            _ => false,
        };
        assert!(
            has_uri,
            "definition result should contain a URI-bearing Location: {result}"
        );

        harness.shutdown();
        drop(dir);
    });
}

#[test]
fn references_finds_at_least_one_usage() {
    with_test_budget(|| {
        let (dir, rs) = build_fixture();
        let mut harness = LspHarness::spawn(dir.path());

        let _init = initialize(&mut harness, dir.path());
        did_open(&mut harness, &rs, FIXTURE_SRC);

        let uri = path_to_uri(&rs);
        // Position on the `hello_world` definition — references should include call sites.
        let resp = harness.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 9 },
                "context": { "includeDeclaration": true },
            }),
        );

        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("references response missing result: {resp}"));

        let arr = result
            .as_array()
            .unwrap_or_else(|| panic!("references result should be an array, got: {result}"));
        assert!(
            !arr.is_empty(),
            "references should return at least one Location: {result}"
        );

        for loc in arr {
            assert!(
                loc.get("uri").and_then(|u| u.as_str()).is_some(),
                "each reference Location should have a uri: {loc}"
            );
        }

        harness.shutdown();
        drop(dir);
    });
}

#[test]
fn rename_produces_workspace_edit() {
    with_test_budget(|| {
        let (dir, rs) = build_fixture();
        let mut harness = LspHarness::spawn(dir.path());

        let _init = initialize(&mut harness, dir.path());
        did_open(&mut harness, &rs, FIXTURE_SRC);

        let uri = path_to_uri(&rs);
        // Position on the `hello_world` definition.
        let resp = harness.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 9 },
                "newName": "greet",
            }),
        );

        // Rename might legitimately be `null` if the engine flags a conflict,
        // but for our fresh fixture with one symbol it should succeed with an
        // edit map. An `error` is a failure — fail loudly with context.
        if let Some(err) = resp.get("error") {
            panic!("rename returned error: {err}");
        }

        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("rename response missing result: {resp}"));
        assert!(
            !result.is_null(),
            "rename result should not be null: {resp}"
        );

        // WorkspaceEdit serializes with a `changes` map keyed by URI.
        let changes = result
            .get("changes")
            .unwrap_or_else(|| panic!("rename WorkspaceEdit missing changes: {result}"));
        let obj = changes
            .as_object()
            .unwrap_or_else(|| panic!("changes should be an object: {changes}"));
        assert!(
            !obj.is_empty(),
            "rename changes map should contain at least one file: {changes}"
        );

        harness.shutdown();
        drop(dir);
    });
}
