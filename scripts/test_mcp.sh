#!/usr/bin/env bash
# =============================================================================
# Codixing MCP server end-to-end test script
# Usage: ./scripts/test_mcp.sh [binary_dir]
# =============================================================================
set -euo pipefail

BIN_DIR="$(cd "${1:-./target/debug}" && pwd)"
MCP="$BIN_DIR/codixing-mcp"
CLI="$BIN_DIR/codixing"
PASS=0; FAIL=0

GREEN="\033[32m"; RED="\033[31m"; CYAN="\033[36m"; BOLD="\033[1m"; RESET="\033[0m"
ok()   { PASS=$((PASS+1)); printf "${GREEN}PASS${RESET} %s\n" "$1"; }
fail() { FAIL=$((FAIL+1)); printf "${RED}FAIL${RESET} %s\n" "$1"; echo "  └─ $2"; }
info() { printf "${CYAN}────${RESET} %s\n" "$1"; }

assert_contains() {
  local label="$1" needle="$2" body="$3"
  if echo "$body" | grep -qF "$needle"; then ok "$label"
  else fail "$label" "Expected: '$needle'"; echo "  Got: $(echo "$body" | head -c 200)"; fi
}

assert_no_error() {
  local label="$1" body="$2"
  if echo "$body" | grep -qF '"error"'; then
    fail "$label" "Got JSON-RPC error: $(echo "$body" | grep -o '"message":"[^"]*"')"
  else ok "$label"; fi
}

# Send JSON-RPC payload to MCP, capturing stdout only (stderr suppressed)
# Uses perl alarm as portable timeout (works on macOS and Linux)
mcp_call() {
  local payload="$1"
  echo "$payload" | perl -e 'alarm(15); exec @ARGV' \
    "$MCP" --root "$TMPDIR_ROOT" 2>/dev/null || true
}

printf "\n${BOLD}Codixing MCP Tests${RESET}\n\n"

if [ ! -x "$MCP" ]; then
  echo "Binary not found at $MCP — run: cargo build --workspace"
  exit 1
fi

TMPDIR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT
mkdir -p "$TMPDIR_ROOT/src"

cat > "$TMPDIR_ROOT/src/engine.rs" <<'EOF'
/// Core search engine.
pub struct Engine {
    index_path: std::path::PathBuf,
}

impl Engine {
    pub fn new(path: &str) -> Self {
        Engine { index_path: path.into() }
    }

    pub fn search(&self, query: &str) -> Vec<String> {
        vec![format!("result for: {}", query)]
    }

    pub fn index_file(&self, path: &str) -> bool { true }
}
EOF

cat > "$TMPDIR_ROOT/src/retriever.rs" <<'EOF'
use crate::engine::Engine;

pub struct HybridRetriever {
    engine: Engine,
}

impl HybridRetriever {
    pub fn new(engine: Engine) -> Self {
        HybridRetriever { engine }
    }

    pub fn retrieve(&self, query: &str, limit: usize) -> Vec<String> {
        self.engine.search(query).into_iter().take(limit).collect()
    }
}
EOF

(cd "$TMPDIR_ROOT" && "$CLI" init . --no-embeddings >/dev/null 2>&1)

# ── 1. tools/list ─────────────────────────────────────────────────────────────
info "1. tools/list"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}')
assert_no_error     "tools/list no error"                  "$RESP"
assert_contains     "tools/list has code_search"      "code_search"      "$RESP"
assert_contains     "tools/list has find_symbol"      "find_symbol"      "$RESP"
assert_contains     "tools/list has get_repo_map"     "get_repo_map"     "$RESP"
assert_contains     "tools/list has grep_code"        "grep_code"        "$RESP"
assert_contains     "tools/list has read_file"        "read_file"        "$RESP"
assert_contains     "tools/list has index_status"     "index_status"     "$RESP"

# ── 2. code_search ────────────────────────────────────────────────────────────
info "2. code_search"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"code_search","arguments":{"query":"search engine","strategy":"instant","limit":5}}}')
assert_no_error     "code_search no error"              "$RESP"
assert_contains     "code_search returns content"  "result"       "$RESP"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"code_search","arguments":{"query":"HybridRetriever","strategy":"instant"}}}')
assert_contains     "code_search finds HybridRetriever" "retriever" "$RESP"

# ── 3. find_symbol ────────────────────────────────────────────────────────────
info "3. find_symbol"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"find_symbol","arguments":{"name":"Engine"}}}')
assert_no_error     "find_symbol no error"           "$RESP"
assert_contains     "find_symbol finds Engine"  "Engine"      "$RESP"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"find_symbol","arguments":{"name":"HybridRetriever"}}}')
assert_contains     "find_symbol finds HybridRetriever" "HybridRetriever" "$RESP"

# ── 4. index_status ───────────────────────────────────────────────────────────
info "4. index_status"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"index_status","arguments":{}}}')
assert_no_error     "index_status no error"            "$RESP"
assert_contains     "index_status has file count" "Files indexed" "$RESP"

# ── 5. read_file ──────────────────────────────────────────────────────────────
info "5. read_file"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_file","arguments":{"file":"src/engine.rs"}}}')
assert_no_error     "read_file no error"             "$RESP"
assert_contains     "read_file returns source"  "Engine"      "$RESP"

# ── 6. grep_code ──────────────────────────────────────────────────────────────
info "6. grep_code"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"grep_code","arguments":{"pattern":"pub fn","glob":"**/*.rs","context":1}}}')
assert_no_error     "grep_code no error"          "$RESP"
assert_contains     "grep_code finds pub fn" "pub fn"     "$RESP"

# ── 7. get_repo_map ───────────────────────────────────────────────────────────
info "7. get_repo_map"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"get_repo_map","arguments":{"tokens":2000}}}')
assert_no_error     "get_repo_map no error"    "$RESP"
assert_contains     "get_repo_map has content" "result" "$RESP"

# ── 8. Error handling ─────────────────────────────────────────────────────────
info "8. error handling"

RESP=$(mcp_call '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}')
assert_contains "unknown tool returns isError" "isError" "$RESP"

# Must not crash on missing required param
RESP=$(mcp_call '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"code_search","arguments":{}}}' || true)
ok "missing required param doesn't crash"

# ── Summary ───────────────────────────────────────────────────────────────────
printf "\n${BOLD}Results: ${GREEN}$PASS passed${RESET}, "
if [ "$FAIL" -gt 0 ]; then
  printf "${RED}$FAIL failed${RESET}\n\n"
  exit 1
else
  printf "${GREEN}0 failed${RESET}\n\n"
fi
