#!/usr/bin/env bash
# PostToolUse hook: re-index the edited file in the codixing index.
#
# Triggered after Edit, Write, NotebookEdit. Reads the Claude Code
# hook stdin JSON (.tool_input field), extracts the edited file path, and
# runs `codixing update --file <path>` to keep the symbol index fresh.
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
  *.json|*.md|*.html|*.css)
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

# Run async in background so the next tool call is not blocked.
codixing update --file "$REL_PATH" >/dev/null 2>&1 &
disown

exit 0
