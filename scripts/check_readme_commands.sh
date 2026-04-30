#!/usr/bin/env bash
# Verify that every `codixing <subcommand>` referenced in README.md is a real
# subcommand reported by the CLI's `--help` output. Catches doc drift when
# subcommands are renamed or removed without a README update.
#
# Usage: scripts/check_readme_commands.sh [path/to/codixing-binary]
#
# Exits non-zero if README mentions an unknown subcommand.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
README="$ROOT/README.md"

CODIXING="${1:-${CODIXING:-$ROOT/target/release/codixing}}"
if [ ! -x "$CODIXING" ]; then
  echo "error: codixing binary not found at $CODIXING" >&2
  echo "hint: build it first with: cargo build --release -p codixing-cli" >&2
  exit 2
fi

# Allow-list: shell snippets sometimes show piped tools that aren't subcommands.
ALLOW_LIST=" help "

# Pull the unique set of `codixing <word>` mentions from README. The pattern
# tolerates leading backticks, code-block indentation, and `./target/...`
# prefixes used in some examples.
mentions=$(
  grep -oE '(\./target/release/)?codixing[[:space:]]+[a-z][a-z0-9-]*' "$README" \
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
  echo "README references unknown codixing subcommand(s):" >&2
  printf '  - %s\n' "${missing[@]}" >&2
  echo >&2
  echo "Known subcommands:" >&2
  echo "$known" | sed 's/^/  - /' >&2
  exit 1
fi

echo "README ↔ CLI subcommand check OK ($(echo "$mentions" | wc -w | tr -d ' ') mentions verified)"
