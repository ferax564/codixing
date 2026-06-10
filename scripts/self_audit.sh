#!/usr/bin/env bash
# Self-audit: run Codixing against its own repository and fail on any
# integrity warning. Catches the class of bug that prints its evidence on
# every run but that nobody is watching for:
#   - index-persistence warnings during `init` (e.g. a codec that cannot
#     represent real chunk IDs)
#   - a first `sync` after `init` that is not a no-op (change baseline
#     missing files)
#   - a `doctor` report that says anything other than a healthy index
#
# Usage: scripts/self_audit.sh [path/to/codixing-binary]
#
# NOTE: rebuilds the `.codixing/` index of this repo from scratch (cheap,
# BM25-only). The index directory is gitignored.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CODIXING="${1:-${CODIXING:-$ROOT/target/release/codixing}}"

if [ ! -x "$CODIXING" ]; then
  echo "error: codixing binary not found at $CODIXING" >&2
  echo "hint: build it first with: cargo build --release -p codixing" >&2
  exit 2
fi

cd "$ROOT"
FAILURES=0

echo "== self-audit: init (fresh index, BM25-only) =="
rm -rf .codixing
INIT_OUT=$("$CODIXING" init . --model none 2>&1) || {
  echo "FAIL: init exited non-zero"
  echo "$INIT_OUT"
  exit 1
}
echo "$INIT_OUT" | tail -1
if echo "$INIT_OUT" | grep -qE ' (WARN|ERROR) '; then
  echo "FAIL: init emitted warnings/errors:"
  echo "$INIT_OUT" | grep -E ' (WARN|ERROR) '
  FAILURES=$((FAILURES + 1))
fi

echo "== self-audit: first sync after init must be a no-op =="
SYNC_OUT=$("$CODIXING" sync . 2>&1) || {
  echo "FAIL: sync exited non-zero"
  echo "$SYNC_OUT"
  exit 1
}
echo "$SYNC_OUT" | tail -1
if ! echo "$SYNC_OUT" | grep -q "0 added, 0 modified, 0 removed"; then
  echo "FAIL: first sync after init re-indexed files (baseline gap):"
  echo "$SYNC_OUT" | tail -3
  FAILURES=$((FAILURES + 1))
fi
if echo "$SYNC_OUT" | grep -qE ' (WARN|ERROR) '; then
  echo "FAIL: sync emitted warnings/errors:"
  echo "$SYNC_OUT" | grep -E ' (WARN|ERROR) '
  FAILURES=$((FAILURES + 1))
fi

echo "== self-audit: doctor must report a healthy index =="
DOCTOR_OUT=$("$CODIXING" doctor 2>&1) || {
  echo "FAIL: doctor exited non-zero"
  echo "$DOCTOR_OUT"
  exit 1
}
if ! echo "$DOCTOR_OUT" | grep -q "Index: ok"; then
  echo "FAIL: doctor did not report 'Index: ok':"
  echo "$DOCTOR_OUT"
  FAILURES=$((FAILURES + 1))
fi
if ! echo "$DOCTOR_OUT" | grep -q "Git staleness: current"; then
  # Informational only: CI checkouts are always current; local trees may
  # legitimately have uncommitted work.
  echo "note: doctor reports git staleness (not failing the audit)"
fi

echo "== self-audit: search smoke test =="
SEARCH_OUT=$("$CODIXING" search "trigram index" 2>&1) || {
  echo "FAIL: search exited non-zero"
  echo "$SEARCH_OUT"
  exit 1
}
if ! echo "$SEARCH_OUT" | grep -q "trigram"; then
  echo "FAIL: search for 'trigram index' returned nothing relevant:"
  echo "$SEARCH_OUT" | head -5
  FAILURES=$((FAILURES + 1))
fi

if [ "$FAILURES" -gt 0 ]; then
  echo "self-audit FAILED with $FAILURES finding(s)"
  exit 1
fi
echo "self-audit OK"
