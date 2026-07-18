#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
WORK_DIR=$(mktemp -d)
cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

make_fixture() {
  local directory=$1 content=$2
  mkdir -p "$directory"
  printf '%s\n' \
    '{' \
    '  "name": "codixing-package-compare-fixture",' \
    '  "version": "1.2.3",' \
    '  "files": ["index.js"]' \
    '}' > "$directory/package.json"
  printf '%s\n' "$content" > "$directory/index.js"
}

make_fixture "$WORK_DIR/local" 'module.exports = "same";'
make_fixture "$WORK_DIR/same" 'module.exports = "same";'
make_fixture "$WORK_DIR/different" 'module.exports = "different";'

bash "$ROOT/scripts/compare_npm_package.sh" \
  "$WORK_DIR/local" "$WORK_DIR/same" >/dev/null

if bash "$ROOT/scripts/compare_npm_package.sh" \
    "$WORK_DIR/local" "$WORK_DIR/different" >/dev/null 2>&1; then
  echo "expected npm package comparison to reject different contents" >&2
  exit 1
fi

echo "npm package comparison tests passed"
