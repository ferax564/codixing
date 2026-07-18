#!/usr/bin/env bash
# Verify that every `codixing <subcommand>` in current user-facing examples is
# reported by the CLI's `--help` output. Catches docs and composite-action drift
# when subcommands are renamed or removed.
#
# Usage: scripts/check_readme_commands.sh [path/to/codixing-binary]
#
# Exits non-zero if a checked source mentions an unknown subcommand.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCES=(
  "$ROOT/README.md"
  "$ROOT/docs/index.html"
  "$ROOT/docs/docs.html"
  "$ROOT/npm/README.md"
  "$ROOT/claude-plugin/README.md"
  "$ROOT/claude-plugin/skills/codixing-setup/SKILL.md"
  "$ROOT/claude-plugin/skills/codixing-release/SKILL.md"
  "$ROOT/.github/actions/codixing/action.yml"
)

CODIXING="${1:-${CODIXING:-$ROOT/target/release/codixing}}"
if [ ! -x "$CODIXING" ]; then
  echo "error: codixing binary not found at $CODIXING" >&2
  echo "hint: build it first with: cargo build --release -p codixing" >&2
  exit 2
fi

# Allow-list: shell snippets sometimes show piped tools that aren't subcommands.
ALLOW_LIST=" help "

# Pull the unique set of `codixing <word>` mentions from checked sources. The pattern
# tolerates leading backticks, code-block indentation, and `./target/...`
# prefixes used in some examples.
mentions=$(
  grep -hoE '(\./target/release/)?codixing[[:space:]]+[a-z][a-z0-9-]*' "${SOURCES[@]}" \
    | sed -E 's|^\./target/release/||' \
    | awk '{print $2}' \
    | sort -u
)

# Subcommand list straight from the binary.
known=$(
  "$CODIXING" --help 2>&1 \
    | awk '/^Commands:$/{flag=1; next} /^Options:|^$/{flag=0} flag {print $1}' \
    | grep -E '^[a-z][a-z0-9-]*$' \
    | sort -u
)

missing=()
for cmd in $mentions; do
  if [[ "$ALLOW_LIST" == *" $cmd "* ]]; then
    continue
  fi
  if ! grep -qx "$cmd" <<<"$known"; then
    missing+=("$cmd")
  fi
done

if [ ${#missing[@]} -gt 0 ]; then
  echo "Current docs/actions reference unknown codixing subcommand(s):" >&2
  printf '  - %s\n' "${missing[@]}" >&2
  echo >&2
  echo "Known subcommands:" >&2
  echo "$known" | sed 's/^/  - /' >&2
  exit 1
fi

# Catch historically misleading examples that happen to use a real top-level
# command with a flag owned by a different command.
if grep -nHE 'codixing[[:space:]]+init[^[:cntrl:]]*--federation' "${SOURCES[@]}"; then
  echo "error: --federation is not an init flag; use 'codixing federation discover'" >&2
  exit 1
fi

require_flag() {
  local description="$1"
  local flag="$2"
  shift 2
  if ! "$CODIXING" "$@" --help 2>&1 | grep -Fq -- "$flag"; then
    echo "error: documented $description contract is missing '$flag' from '$* --help'" >&2
    exit 1
  fi
}

require_flag "embedded initialization" "--embed" init
require_flag "deferred embedding" "--defer-embeddings" init
require_flag "search strategy" "--strategy" search
require_flag "federation discovery output" "--output" federation discover

echo "Docs/actions ↔ CLI command check OK ($(echo "$mentions" | wc -w | tr -d ' ') mentions and 4 flag contracts verified)"
