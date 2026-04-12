#!/usr/bin/env bash
# PreToolUse hook: intercept Grep calls and redirect to codixing CLI.
#
# DENY when the search targets file types Codixing indexes (code, docs, config)
# and the pattern looks like a concept/symbol search.
#
# PASSTHROUGH when:
#   - Pattern is a version/semver string (e.g., 0.29.1)
#   - Pattern is very short (<3 chars)
#   - Searching in non-indexed paths (target/, node_modules/, .git/)
#   - Searching a specific single file (not a directory)
#   - Output mode is "count" (just counting occurrences)
#   - No codixing index exists (.codixing/ directory missing)

set -euo pipefail

INPUT=$(cat)

# If no codixing index, passthrough — can't redirect to a tool that hasn't been set up.
if [ ! -d ".codixing" ]; then
  exit 0
fi

PATTERN=$(echo "$INPUT" | jq -r '.tool_input.pattern // ""')
GLOB=$(echo "$INPUT" | jq -r '.tool_input.glob // ""')
PATH_ARG=$(echo "$INPUT" | jq -r '.tool_input.path // ""')
TYPE_ARG=$(echo "$INPUT" | jq -r '.tool_input.type // ""')
OUTPUT_MODE=$(echo "$INPUT" | jq -r '.tool_input.output_mode // ""')

# --- PASSTHROUGH RULES ---

# 1. Empty pattern — nothing to search
if [ -z "$PATTERN" ]; then
  exit 0
fi

# 2. Very short patterns (1-2 chars) — too generic for semantic search
if [ ${#PATTERN} -lt 3 ]; then
  exit 0
fi

# 3. Version/semver patterns — infrastructure search, not code exploration
if echo "$PATTERN" | grep -qE '^v?[0-9]+\.[0-9]+(\.[0-9]+)?$'; then
  exit 0
fi

# 4. Count mode — just counting, not exploring
if [ "$OUTPUT_MODE" = "count" ]; then
  exit 0
fi

# 5. Searching in non-indexed directories
if echo "$PATH_ARG" | grep -qE '(target/|node_modules/|\.git/|\.codixing/|vendor/)'; then
  exit 0
fi

# 6. Searching a specific single file (has a file extension in path)
if echo "$PATH_ARG" | grep -qE '\.[a-zA-Z0-9]+$'; then
  exit 0
fi

# 7. Pattern is a file path or URL — not a code search
if echo "$PATTERN" | grep -qE '^(https?://|/|\./)'; then
  exit 0
fi

# 8. Conflict markers or git-specific patterns
if echo "$PATTERN" | grep -qE '^(<<<<|>>>>|====)'; then
  exit 0
fi

# --- CHECK IF TARGET FILES ARE INDEXED ---

# Determine if the glob/type targets files Codixing indexes.
INDEXED=false

# Code file extensions
CODE_EXTS='rs|py|ts|tsx|js|jsx|go|java|c|cpp|h|hpp|cs|rb|swift|kt|scala|php|zig|bash|sh'
# Doc file extensions
DOC_EXTS='md|html'
# Config file extensions
CONFIG_EXTS='json|toml|yaml|yml'

ALL_EXTS="($CODE_EXTS|$DOC_EXTS|$CONFIG_EXTS)"

# Check glob pattern
if [ -n "$GLOB" ]; then
  if echo "$GLOB" | grep -qEi "\\.($CODE_EXTS|$DOC_EXTS|$CONFIG_EXTS)"; then
    INDEXED=true
  fi
fi

# Check type argument (rg --type)
if [ -n "$TYPE_ARG" ]; then
  case "$TYPE_ARG" in
    rs|rust|py|python|ts|typescript|js|javascript|go|java|c|cpp|cs|csharp|rb|ruby|swift|kotlin|scala|php|zig|bash|sh|shell|md|markdown|json|toml|yaml|html)
      INDEXED=true
      ;;
  esac
fi

# If no glob and no type specified, check if pattern looks like code exploration
if [ -z "$GLOB" ] && [ -z "$TYPE_ARG" ]; then
  # Broad search (no file filter) — likely code exploration
  # Check if pattern looks like a symbol/concept search
  if echo "$PATTERN" | grep -qE '(fn |struct |class |def |impl |trait |enum |interface |type |pub |async |export |import |use |from |require|module|function|const |let |var )'; then
    INDEXED=true
  fi
  # Common code search patterns (function names, type names, etc.)
  if echo "$PATTERN" | grep -qE '^[A-Z][a-zA-Z]+$'; then
    # PascalCase — likely a type/struct/class name
    INDEXED=true
  fi
  if echo "$PATTERN" | grep -qE '^[a-z_][a-z_0-9]*$'; then
    # snake_case — likely a function/variable name
    INDEXED=true
  fi
  # Unfiltered grep with a natural language query
  if echo "$PATTERN" | grep -qE ' '; then
    # Multi-word pattern — likely a concept search
    INDEXED=true
  fi
fi

# --- DECISION ---

if [ "$INDEXED" = true ]; then
  # Build a helpful redirect message based on the pattern
  SUGGESTION="codixing search \"$PATTERN\""

  # If pattern looks like a symbol definition search, suggest symbols
  if echo "$PATTERN" | grep -qE '^(fn |struct |class |def |impl |trait |enum |type )'; then
    SYMBOL=$(echo "$PATTERN" | sed -E 's/^(fn |struct |class |def |impl |trait |enum |type )//')
    SUGGESTION="codixing symbols $SYMBOL"
  fi

  # If pattern looks like a specific identifier
  if echo "$PATTERN" | grep -qE '^[a-zA-Z_][a-zA-Z_0-9]*$'; then
    SUGGESTION="codixing usages $PATTERN  OR  codixing symbols $PATTERN"
  fi

  cat <<DENY_JSON
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "additionalContext": "CODIXING DOGFOODING: Use the codixing CLI instead of Grep for code/doc/config search.\n\nSuggested command:\n  $SUGGESTION\n\nAll available commands:\n  codixing search \"<query>\"      — semantic search (code, docs, config)\n  codixing symbols <name>        — find symbol definitions\n  codixing usages <symbol>       — find call sites and imports\n  codixing callers <file>        — who imports this file\n  codixing callees <file>        — what this file imports\n  codixing graph --map           — architecture overview\n\nRun via Bash from the repo root. Passthrough exceptions: version strings, single-file targets, count mode, very short patterns."
  }
}
DENY_JSON
  exit 0
fi

# Not an indexed file type — passthrough
exit 0
