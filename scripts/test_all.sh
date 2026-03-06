#!/usr/bin/env bash
# =============================================================================
# Run all Codixing test suites
# Usage: ./scripts/test_all.sh [binary_dir]
# =============================================================================
set -euo pipefail

BIN_DIR="${1:-./target/debug}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BOLD="\033[1m"; GREEN="\033[32m"; RED="\033[31m"; CYAN="\033[36m"; RESET="\033[0m"

ALL_PASS=0; ALL_FAIL=0

run_suite() {
  local name="$1" script="$2"
  printf "\n${CYAN}══════════════════════════════════════${RESET}\n"
  printf "${BOLD} $name${RESET}\n"
  printf "${CYAN}══════════════════════════════════════${RESET}\n"

  if bash "$script" "$BIN_DIR"; then
    ALL_PASS=$((ALL_PASS+1))
    printf "${GREEN}Suite passed: $name${RESET}\n"
  else
    ALL_FAIL=$((ALL_FAIL+1))
    printf "${RED}Suite FAILED: $name${RESET}\n"
  fi
}

printf "\n${BOLD}Running Codixing test suites (binary: $BIN_DIR)${RESET}\n"

# Rust unit + integration tests
printf "\n${CYAN}══════════════════════════════════════${RESET}\n"
printf "${BOLD} Rust unit + integration tests${RESET}\n"
printf "${CYAN}══════════════════════════════════════${RESET}\n"
if cargo test --workspace --quiet 2>&1; then
  ALL_PASS=$((ALL_PASS+1))
  printf "${GREEN}Suite passed: Rust tests${RESET}\n"
else
  ALL_FAIL=$((ALL_FAIL+1))
  printf "${RED}Suite FAILED: Rust tests${RESET}\n"
fi

run_suite "CLI tests"         "$SCRIPT_DIR/test_cli.sh"
run_suite "MCP server tests"  "$SCRIPT_DIR/test_mcp.sh"
run_suite "REST API tests"    "$SCRIPT_DIR/test_api.sh"

printf "\n${CYAN}══════════════════════════════════════${RESET}\n"
printf "${BOLD} Final summary${RESET}\n"
printf "${CYAN}══════════════════════════════════════${RESET}\n"
printf "${GREEN}$ALL_PASS suites passed${RESET}"
if [ "$ALL_FAIL" -gt 0 ]; then
  printf ", ${RED}$ALL_FAIL suites FAILED${RESET}\n\n"
  exit 1
else
  printf ", ${GREEN}0 failed${RESET}\n\n"
fi
