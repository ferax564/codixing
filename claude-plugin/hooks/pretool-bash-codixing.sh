#!/usr/bin/env bash
# PreToolUse hook: intercept Bash calls that shell out to grep/rg/find/cat
# against code/doc/config files and redirect to the codixing CLI.
#
# Mirrors the intent of pretool-codixing.sh (which targets the Grep tool) but
# covers the bypass where agents call `Bash("grep -r foo crates/")` directly.
#
# PASSTHROUGH when:
#   - No codixing index exists (.codixing/ directory missing)
#   - Command does not start with grep/rg/find/cat (or their absolute paths)
#   - Command targets non-indexed paths (target/, node_modules/, .git/, /tmp/)
#   - Command is piped with | wc -l (count mode)
#   - Command is a version check (grep 'version')
#   - Command is `cat` on a single specific file (Read-replacement use)
#   - Command uses find with -name/-iname only (file-finding, not content search)

set -euo pipefail

INPUT=$(cat)

# If no codixing index, passthrough.
if [ ! -d ".codixing" ]; then
  exit 0
fi

COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

# Empty command — nothing to check.
if [ -z "$COMMAND" ]; then
  exit 0
fi

# Strip leading whitespace.
TRIMMED=$(echo "$COMMAND" | sed -E 's/^[[:space:]]+//')

# Extract the first binary invoked. Handles:
#   grep ...             -> grep
#   /usr/bin/grep ...    -> grep
#   rg ...               -> rg
#   find . -name ...     -> find
#   cat path             -> cat
FIRST_BIN=$(echo "$TRIMMED" | awk '{print $1}' | awk -F/ '{print $NF}')

# --- ADVISORY: codixing search|symbols|usages ... | wc -l → suggest --count ---
# Only fires for subcommands that actually support --count. Fires before any
# blocking logic so it's always seen.
if echo "$TRIMMED" | grep -qE '^codixing (search|symbols|usages) .* \| *wc +-l'; then
  echo "Hint: use --count flag instead of piping to wc -l (e.g., codixing search ... --count)" >&2
  exit 0
fi

# --- PASSTHROUGH: commands that aren't grep-family ---
case "$FIRST_BIN" in
  grep|egrep|fgrep|rgrep|rg|ag|ack|ripgrep)
    TOOL_TYPE="content-search"
    ;;
  find)
    TOOL_TYPE="find"
    ;;
  cat|bat|less|more|head|tail)
    TOOL_TYPE="read"
    ;;
  *)
    exit 0
    ;;
esac

# --- PASSTHROUGH for `find` with only file-finding flags ---
# If `find` has no -exec, no grep, no content-search modes, let it pass.
if [ "$TOOL_TYPE" = "find" ]; then
  if ! echo "$TRIMMED" | grep -qE '(-exec|\\| *grep|\\| *rg|-print0)'; then
    # Pure file-finding — allow.
    exit 0
  fi
fi

# --- PASSTHROUGH for `cat`/read commands targeting a single file ---
# Agents use `cat file.txt` legitimately when Read isn't suitable.
# Only deny when piping the output into grep/rg (content search).
if [ "$TOOL_TYPE" = "read" ]; then
  if ! echo "$TRIMMED" | grep -qE '\\| *(grep|rg|ag|ack|egrep|fgrep)'; then
    exit 0
  fi
fi

# --- PASSTHROUGH for count mode (| wc -l) ---
if echo "$TRIMMED" | grep -qE '\\| *wc +-l'; then
  exit 0
fi

# --- PASSTHROUGH for version/semver searches ---
if echo "$TRIMMED" | grep -qE "['\"]v?[0-9]+\.[0-9]+(\.[0-9]+)?['\"]"; then
  exit 0
fi

# --- PASSTHROUGH for non-indexed target directories ---
if echo "$TRIMMED" | grep -qE '(target/|node_modules/|\.git/|\.codixing/|vendor/|/tmp/|/private/tmp/)'; then
  exit 0
fi

# --- PASSTHROUGH: command targets a single file with an extension (not a dir walk) ---
# e.g. `grep foo src/main.rs` — the agent already knows the file, let them search it.
# Heuristic: if any argument ends in a known extension AND no -r/-R/--recursive flag is present.
if ! echo "$TRIMMED" | grep -qE '( -r| -R| --recursive| -rn| -rH)'; then
  if echo "$TRIMMED" | grep -qE '\.(rs|py|ts|tsx|js|jsx|go|java|c|cpp|h|hpp|cs|rb|swift|kt|scala|php|zig|sh|md|html|json|toml|yaml|yml)( |$)'; then
    exit 0
  fi
fi

# --- DENY: this is a code-exploration content search ---

# Try to extract the search pattern for a helpful suggestion.
PATTERN=$(echo "$TRIMMED" | sed -nE "s/^[^ ]+ +(-[a-zA-Z]+ +)*['\"]?([^'\" ]+)['\"]?.*/\2/p" | head -1)
if [ -z "$PATTERN" ]; then
  PATTERN="<your-query>"
fi

cat <<DENY_JSON
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "additionalContext": "CODIXING DOGFOODING: Your Bash command shelled out to '${FIRST_BIN}' against indexed code. Use the codixing CLI instead.\n\nSuggested commands:\n  codixing search \"${PATTERN}\"       — semantic search\n  codixing symbols ${PATTERN}         — find symbol definitions\n  codixing usages ${PATTERN}          — find call sites and imports\n\nAll available commands:\n  codixing search \"<query>\"           — semantic search (code, docs, config)\n  codixing symbols <name>             — find symbol definitions\n  codixing usages <symbol>            — find call sites and imports\n  codixing callers <file>             — who imports this file\n  codixing callees <file>             — what this file imports\n  codixing impact <file>              — blast radius analysis\n  codixing graph --map                — architecture overview\n\nPassthrough exceptions: single-file targets, count mode (| wc -l), version searches, non-indexed paths (target/, node_modules/, .git/), find without -exec."
  }
}
DENY_JSON
exit 0
