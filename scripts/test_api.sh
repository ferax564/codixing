#!/usr/bin/env bash
# =============================================================================
# Codixing REST API end-to-end test script
# Usage: ./scripts/test_api.sh [binary_dir]
# =============================================================================
set -euo pipefail

BIN_DIR="$(cd "${1:-./target/debug}" && pwd)"
CLI="$BIN_DIR/codixing"
PORT=13741
BASE="http://localhost:$PORT"
PASS=0; FAIL=0
SERVER_PID=""

GREEN="\033[32m"; RED="\033[31m"; CYAN="\033[36m"; BOLD="\033[1m"; RESET="\033[0m"
ok()   { PASS=$((PASS+1)); printf "${GREEN}PASS${RESET} %s\n" "$1"; }
fail() { FAIL=$((FAIL+1)); printf "${RED}FAIL${RESET} %s\n" "$1"; echo "  └─ $2"; }
info() { printf "${CYAN}────${RESET} %s\n" "$1"; }

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "${TMPDIR_ROOT:-}"
}
trap cleanup EXIT

assert_status() {
  local label="$1" expected="$2" actual="$3"
  if [ "$actual" = "$expected" ]; then ok "$label (HTTP $actual)"
  else fail "$label" "Expected HTTP $expected, got HTTP $actual"; fi
}

assert_body_contains() {
  local label="$1" needle="$2" body="$3"
  if echo "$body" | grep -qF "$needle"; then ok "$label"
  else fail "$label" "Expected body to contain: '$needle'"; echo "  Body: $(echo "$body" | head -c 200)"; fi
}

http_get() {
  curl -s -w "\n%{http_code}" "$BASE$1" 2>/dev/null
}

http_post() {
  curl -s -w "\n%{http_code}" -X POST "$BASE$1" \
    -H "Content-Type: application/json" -d "$2" 2>/dev/null
}

split_response() { BODY=$(echo "$1" | sed '$d'); STATUS=$(echo "$1" | tail -n 1); }

printf "\n${BOLD}Codixing REST API Tests${RESET}\n\n"

if [ ! -x "$CLI" ]; then
  echo "Binary not found at $CLI — run: cargo build --workspace"
  exit 1
fi

# ── Set up temp project ───────────────────────────────────────────────────────
TMPDIR_ROOT="$(mktemp -d)"
mkdir -p "$TMPDIR_ROOT/src"

cat > "$TMPDIR_ROOT/src/handler.rs" <<'EOF'
pub struct Handler { route: String }
impl Handler {
    pub fn new(route: &str) -> Self { Handler { route: route.to_string() } }
    pub fn handle(&self, method: &str) -> String { format!("{} {}", method, self.route) }
}
EOF

cat > "$TMPDIR_ROOT/src/router.rs" <<'EOF'
use crate::handler::Handler;
pub struct Router { handlers: Vec<Handler> }
impl Router {
    pub fn new() -> Self { Router { handlers: vec![] } }
    pub fn register(&mut self, route: &str) { self.handlers.push(Handler::new(route)); }
}
EOF

(cd "$TMPDIR_ROOT" && "$CLI" init . --no-embeddings >/dev/null 2>&1)

# ── Start server ──────────────────────────────────────────────────────────────
info "Starting server on port $PORT"
"$BIN_DIR/codixing-server" --port "$PORT" "$TMPDIR_ROOT" &
SERVER_PID=$!

for i in $(seq 1 30); do
  if curl -sf "$BASE/health" >/dev/null 2>&1; then break; fi
  sleep 0.5
done

if ! curl -sf "$BASE/health" >/dev/null 2>&1; then
  echo "Server failed to start within 15s"
  exit 1
fi
ok "server started"

# ── /health ───────────────────────────────────────────────────────────────────
info "1. GET /health"
RESP=$(http_get "/health"); split_response "$RESP"
assert_status        "/health 200"      "200" "$STATUS"
assert_body_contains "/health body ok"  "ok"  "$BODY"

# ── /status ───────────────────────────────────────────────────────────────────
info "2. GET /status"
RESP=$(http_get "/status"); split_response "$RESP"
assert_status        "/status 200"             "200"        "$STATUS"
assert_body_contains "/status has file_count"  "file_count" "$BODY"

# ── /search ───────────────────────────────────────────────────────────────────
info "3. POST /search"

RESP=$(http_post "/search" '{"query":"HTTP handler","strategy":"instant","limit":5}')
split_response "$RESP"
assert_status        "POST /search 200"           "200"     "$STATUS"
assert_body_contains "search returns handler"     "handler" "$BODY"

RESP=$(http_post "/search" '{"query":"register route","strategy":"instant"}')
split_response "$RESP"
assert_status        "POST /search router query"  "200"     "$STATUS"
assert_body_contains "search returns router"      "router"  "$BODY"

# ── /symbols ──────────────────────────────────────────────────────────────────
info "4. POST /symbols"

RESP=$(http_post "/symbols" '{"filter":"Handler"}'); split_response "$RESP"
assert_status        "POST /symbols Handler 200"   "200"     "$STATUS"
assert_body_contains "symbols finds Handler"       "Handler" "$BODY"

RESP=$(http_post "/symbols" '{"filter":"Router"}'); split_response "$RESP"
assert_body_contains "symbols finds Router"        "Router"  "$BODY"

# ── /graph/stats ──────────────────────────────────────────────────────────────
info "5. GET /graph/stats"

RESP=$(http_get "/graph/stats"); split_response "$RESP"
assert_status        "GET /graph/stats 200"    "200"  "$STATUS"
assert_body_contains "graph stats has nodes"   "node" "$BODY"

# ── /graph/repo-map ───────────────────────────────────────────────────────────
info "6. POST /graph/repo-map"

RESP=$(http_post "/graph/repo-map" '{"tokens":2000}')
split_response "$RESP"
assert_status "POST /graph/repo-map 200" "200" "$STATUS"

# ── /graph/callers + /graph/callees ───────────────────────────────────────────
info "7. GET /graph/callers + /graph/callees"

RESP=$(http_get "/graph/callers?file=src/handler.rs&depth=1"); split_response "$RESP"
assert_status "GET /graph/callers 200" "200" "$STATUS"

RESP=$(http_get "/graph/callees?file=src/router.rs&depth=1"); split_response "$RESP"
assert_status "GET /graph/callees 200" "200" "$STATUS"

# ── /index/reindex ────────────────────────────────────────────────────────────
info "8. POST /index/reindex"

cat > "$TMPDIR_ROOT/src/cache.rs" <<'EOF'
pub struct Cache { capacity: usize }
impl Cache {
    pub fn new(n: usize) -> Self { Cache { capacity: n } }
    pub fn get(&self, _k: &str) -> Option<String> { None }
}
EOF

RESP=$(http_post "/index/reindex" "{\"file_path\":\"$TMPDIR_ROOT/src/cache.rs\"}"); split_response "$RESP"
assert_status "POST /index/reindex 200" "200" "$STATUS"

sleep 1
RESP=$(http_post "/search" '{"query":"Cache capacity","strategy":"instant"}')
split_response "$RESP"
assert_body_contains "new file searchable after reindex" "Cache" "$BODY"

# ── DELETE /index/file ────────────────────────────────────────────────────────
info "9. DELETE /index/file"

RESP=$(curl -s -w "\n%{http_code}" -X DELETE "$BASE/index/file" \
  -H "Content-Type: application/json" \
  -d "{\"file_path\":\"$TMPDIR_ROOT/src/cache.rs\"}" 2>/dev/null)
split_response "$RESP"
assert_status "DELETE /index/file 200" "200" "$STATUS"

# ── Summary ───────────────────────────────────────────────────────────────────
printf "\n${BOLD}Results: ${GREEN}$PASS passed${RESET}, "
if [ "$FAIL" -gt 0 ]; then
  printf "${RED}$FAIL failed${RESET}\n\n"
  exit 1
else
  printf "${GREEN}0 failed${RESET}\n\n"
fi
