#!/usr/bin/env bash
# PostToolUse hook: re-index the edited file in the codixing index.
#
# Triggered after Edit, Write, NotebookEdit. Reads the Claude Code
# hook stdin JSON (.tool_input field), extracts the edited file path, and
# queues it for a single background `codixing update --file <path>` (or one
# `codixing sync .` when several files changed) to keep the index fresh.
#
# SILENT NO-OP when:
#   - No .codixing/ index in CWD (non-codixing projects)
#   - File extension is not indexed by codixing
#   - codixing binary not on PATH
#
# Must exit 0 always — a PostToolUse hook failure must not block the edit.

set -uo pipefail

# Require index.
[ -d ".codixing" ] || exit 0

# Require binary.
command -v codixing >/dev/null 2>&1 || exit 0

# Require jq (used to parse the hook stdin JSON).
command -v jq >/dev/null 2>&1 || exit 0

INPUT=$(cat)

# Extract the file path from the tool_input JSON.
# Edit tool: .tool_input.file_path
# Write tool: .tool_input.file_path
# NotebookEdit tool: .tool_input.notebook_path
FILE_PATH=$(printf '%s' "$INPUT" | jq -r '
  .tool_input.file_path //
  .tool_input.notebook_path //
  empty
' 2>/dev/null)

[ -z "$FILE_PATH" ] && exit 0

# Only re-index file types codixing understands. Skip docs, images, binaries.
case "$FILE_PATH" in
  *.rs|*.py|*.ts|*.tsx|*.js|*.jsx|*.go|*.java|*.c|*.cpp|*.h|*.hpp|\
  *.cs|*.rb|*.swift|*.kt|*.scala|*.php|*.zig|*.sh|*.toml|*.yaml|*.yml|\
  *.json|*.md|*.html)
    ;;
  *)
    exit 0
    ;;
esac

# Make path relative to CWD if absolute.
REL_PATH="$FILE_PATH"
if [[ "$FILE_PATH" = /* ]]; then
  REL_PATH="${FILE_PATH#"$(pwd)/"}"
  # If stripping CWD had no effect, file is outside the project — skip.
  if [[ "$REL_PATH" = /* ]]; then
    exit 0
  fi
fi

# Coalesce: a burst of edits must not start one indexing process per edit.
# On a large repository each update reopens the index (seconds, and over a
# gigabyte of memory), so N quick edits used to queue N such processes behind
# the writer lock. Instead every edit records its path and a single background
# runner drains the queue, looping while new paths keep arriving.
QUEUE=".codixing/hook-update.queue"
LOCK=".codixing/hook-update.lock"

printf '%s\n' "$REL_PATH" >>"$QUEUE" 2>/dev/null || exit 0

(
  # A runner that was killed leaves its lock behind; reclaim it after 10 min.
  find "$LOCK" -maxdepth 0 -mmin +10 -exec rmdir {} \; 2>/dev/null

  while :; do
    # `mkdir` is atomic: exactly one runner holds the lock.
    mkdir "$LOCK" 2>/dev/null || exit 0

    while [ -s "$QUEUE" ]; do
      BATCH="$QUEUE.$$"
      mv "$QUEUE" "$BATCH" 2>/dev/null || break
      if [ "$(sort -u "$BATCH" | wc -l)" -eq 1 ]; then
        codixing update --file "$(head -n 1 "$BATCH")"
      else
        # Several files changed: one sync covers them all.
        codixing sync .
      fi
      rm -f "$BATCH"
    done

    rmdir "$LOCK" 2>/dev/null
    # A path queued between the last check and the unlock gets another pass.
    [ -s "$QUEUE" ] || break
  done
) >/dev/null 2>&1 &
disown

exit 0
