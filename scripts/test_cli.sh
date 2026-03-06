#!/usr/bin/env bash
# =============================================================================
# Codixing CLI end-to-end test script
# Usage: ./scripts/test_cli.sh [binary_dir]
# Default binary_dir: ./target/debug
# =============================================================================
set -euo pipefail

BIN_DIR="$(cd "${1:-./target/debug}" && pwd)"
CODIXING="$BIN_DIR/codixing"
PASS=0; FAIL=0

GREEN="\033[32m"; RED="\033[31m"; CYAN="\033[36m"; BOLD="\033[1m"; RESET="\033[0m"
ok()   { PASS=$((PASS+1)); printf "${GREEN}PASS${RESET} %s\n" "$1"; }
fail() { FAIL=$((FAIL+1)); printf "${RED}FAIL${RESET} %s\n" "$1"; echo "  └─ $2"; }
info() { printf "${CYAN}────${RESET} %s\n" "$1"; }

# Run a command inside the temp project dir
run() { (cd "$TMPDIR_ROOT" && "$CODIXING" "$@" 2>&1); }
run_ok() { (cd "$TMPDIR_ROOT" && "$CODIXING" "$@" >/dev/null 2>&1); }

assert_contains() {
  local label="$1" needle="$2" haystack="$3"
  if echo "$haystack" | grep -qF "$needle"; then
    ok "$label"
  else
    fail "$label" "Expected: '$needle'"
    echo "  Output: $(echo "$haystack" | head -3)"
  fi
}

assert_exit_ok() {
  local label="$1"; shift
  if (cd "$TMPDIR_ROOT" && "$@" >/dev/null 2>&1); then
    ok "$label"
  else
    fail "$label" "Command exited non-zero: $*"
  fi
}

printf "\n${BOLD}Codixing CLI Tests${RESET}\n\n"

if [ ! -x "$CODIXING" ]; then
  echo "Binary not found at $CODIXING — run: cargo build --workspace"
  exit 1
fi

# ── Set up temp project ───────────────────────────────────────────────────────
TMPDIR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

mkdir -p "$TMPDIR_ROOT/src"

cat > "$TMPDIR_ROOT/src/auth.rs" <<'EOF'
/// Handles authentication requests.
pub fn authenticate(user: &str, password: &str) -> Result<Token, AuthError> {
    if password.is_empty() {
        return Err(AuthError::EmptyPassword);
    }
    Ok(Token::new(user))
}

pub struct Token {
    pub value: String,
}

impl Token {
    pub fn new(user: &str) -> Self {
        Token { value: format!("tok_{}", user) }
    }
}

#[derive(Debug)]
pub enum AuthError {
    EmptyPassword,
    InvalidUser,
}
EOF

cat > "$TMPDIR_ROOT/src/db.rs" <<'EOF'
use crate::auth::Token;

pub struct DbPool {
    url: String,
}

impl DbPool {
    pub fn new(url: &str) -> Self {
        DbPool { url: url.to_string() }
    }

    pub fn verify_token(&self, token: &Token) -> bool {
        !token.value.is_empty()
    }
}
EOF

cat > "$TMPDIR_ROOT/src/main.rs" <<'EOF'
mod auth;
mod db;

fn main() {
    let pool = db::DbPool::new("postgres://localhost/app");
    match auth::authenticate("alice", "secret") {
        Ok(token) => {
            if pool.verify_token(&token) {
                println!("Authenticated: {}", token.value);
            }
        }
        Err(e) => eprintln!("Auth failed: {:?}", e),
    }
}
EOF

cat > "$TMPDIR_ROOT/src/utils.py" <<'EOF'
def parse_config(path: str) -> dict:
    """Parse a TOML config file and return a dict."""
    return {}

def format_token(token: str) -> str:
    """Format a token for display."""
    return f"[{token[:8]}...]"
EOF

# ── 1. Init ───────────────────────────────────────────────────────────────────
info "1. init"

OUT=$(run init . --no-embeddings)
assert_contains "init exits cleanly"    "Indexed"  "$OUT"
assert_contains "init reports files"    "files"    "$OUT"
assert_contains "init reports symbols"  "symbols"  "$OUT"

[ -d "$TMPDIR_ROOT/.codixing" ] && ok "init creates .codixing/" \
  || fail "init creates .codixing/" ".codixing/ not found"

# ── 2. Search ─────────────────────────────────────────────────────────────────
info "2. search"

OUT=$(run search "authentication" --strategy instant)
assert_contains "search finds auth module"   "auth"   "$OUT"

OUT=$(run search "DbPool verify token" --strategy instant)
assert_contains "search finds db module"     "db"     "$OUT"

OUT=$(run search "Token" --strategy instant)
assert_contains "search finds Token"         "Token"  "$OUT"

OUT=$(run search "parse config" --strategy instant)
assert_contains "search finds python file"   "utils"  "$OUT"

assert_exit_ok "search no results exits 0" \
  "$CODIXING" search "nonexistent_xyzzy_404" --strategy instant

# ── 3. Symbols ────────────────────────────────────────────────────────────────
info "3. symbols"

OUT=$(run symbols authenticate)
assert_contains "symbols finds authenticate"  "authenticate"  "$OUT"

OUT=$(run symbols Token)
assert_contains "symbols finds Token"         "Token"         "$OUT"

OUT=$(run symbols DbPool)
assert_contains "symbols finds DbPool"        "DbPool"        "$OUT"

OUT=$(run symbols parse_config)
assert_contains "symbols finds parse_config"  "parse_config"  "$OUT"

# ── 4. Graph ──────────────────────────────────────────────────────────────────
info "4. graph / callers / callees"

OUT=$(run graph)
assert_contains "graph shows stats"  "Nodes"  "$OUT"

assert_exit_ok "callers exits 0"  "$CODIXING" callers "src/db.rs"
assert_exit_ok "callees exits 0"  "$CODIXING" callees "src/main.rs"

# ── 5. Usages ─────────────────────────────────────────────────────────────────
info "5. usages"

assert_exit_ok "usages exits 0"  "$CODIXING" usages "Token"

# ── 6. Sync — picks up new file ───────────────────────────────────────────────
info "6. sync"

cat > "$TMPDIR_ROOT/src/cache.rs" <<'EOF'
pub struct Cache { capacity: usize }
impl Cache {
    pub fn new(n: usize) -> Self { Cache { capacity: n } }
    pub fn get(&self, _key: &str) -> Option<String> { None }
}
EOF

assert_exit_ok "sync exits 0"  "$CODIXING" sync .

OUT=$(run symbols Cache)
assert_contains "sync picks up new file"  "Cache"  "$OUT"

# ── 7. Strategy variants ──────────────────────────────────────────────────────
info "7. strategy variants"

for strategy in instant fast thorough explore; do
  assert_exit_ok "search --strategy $strategy" \
    "$CODIXING" search "authenticate" --strategy "$strategy"
done

# ── Summary ───────────────────────────────────────────────────────────────────
printf "\n${BOLD}Results: ${GREEN}$PASS passed${RESET}, "
if [ "$FAIL" -gt 0 ]; then
  printf "${RED}$FAIL failed${RESET}\n\n"
  exit 1
else
  printf "${GREEN}0 failed${RESET}\n\n"
fi
