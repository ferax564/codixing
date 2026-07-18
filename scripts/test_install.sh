#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TMP=$(mktemp -d)
INSTALLER_PID=""
BLOCK_RELEASE=""
cleanup() {
  if [[ -n "$INSTALLER_PID" ]]; then
    [[ -n "$BLOCK_RELEASE" ]] && : > "$BLOCK_RELEASE"
    kill "$INSTALLER_PID" 2>/dev/null || true
    wait "$INSTALLER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

FIXTURES="$TMP/fixtures"
MOCK_BIN="$TMP/mock-bin"
INSTALL_DIR="$TMP/install dir"
mkdir -p "$FIXTURES" "$MOCK_BIN" "$INSTALL_DIR"

SUFFIX=linux-x86_64
BINARIES=(codixing codixing-mcp codixing-lsp codixing-server)

rebuild_manifest() {
  : > "$FIXTURES/SHA256SUMS"
  for bin in "${BINARIES[@]}"; do
    asset="${bin}-${SUFFIX}"
    printf '%s  %s\n' "$(sha256sum "$FIXTURES/$asset" | awk '{print $1}')" "$asset" \
      >> "$FIXTURES/SHA256SUMS"
  done
}

make_assets() {
  mode=$1
  label=$2
  for bin in "${BINARIES[@]}"; do
    asset="${bin}-${SUFFIX}"
    if [[ "$mode" == smoke-fail && "$bin" == codixing ]]; then
      printf '%s\n' \
        '#!/bin/sh' \
        'case "$0" in' \
        '  *"/.codixing-stage."*) printf "%s\\n" "codixing 9.9.9"; exit 0 ;;' \
        'esac' \
        'exit 42' > "$FIXTURES/$asset"
    elif [[ "$bin" == codixing-lsp || "$bin" == codixing-server ]]; then
      # Protocol/service executables deliberately fail if the installer tries
      # to execute them as a generic --version probe.
      printf '%s\n' \
        '#!/bin/sh' \
        'exit 97' > "$FIXTURES/$asset"
    else
      printf '#!/bin/sh\n# asset: %s\nprintf "%%s\\n" "%s 9.9.9"\n' "$label" "$bin" \
        > "$FIXTURES/$asset"
    fi
    chmod +x "$FIXTURES/$asset"
  done
  rebuild_manifest
}

declare -A before
record_installed_hashes() {
  for bin in "${BINARIES[@]}"; do
    before[$bin]=$(sha256sum "$INSTALL_DIR/$bin" | awk '{print $1}')
  done
}

assert_install_unchanged() {
  for bin in "${BINARIES[@]}"; do
    after=$(sha256sum "$INSTALL_DIR/$bin" | awk '{print $1}')
    [[ "$after" == "${before[$bin]}" ]]
  done
  for entry in "$INSTALL_DIR"/.codixing-stage.* "$INSTALL_DIR"/.codixing-backup.*; do
    [[ ! -e "$entry" ]]
  done
  [[ ! -e "$INSTALL_DIR/.codixing-install.lock" ]]
}

make_assets normal 9.9.9

cat > "$MOCK_BIN/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
destination=
url=
proto=
proto_redir=
max_filesize=
while (($#)); do
  case "$1" in
    --output|-o)
      shift
      destination=${1:-}
      ;;
    --output=*) destination=${1#--output=} ;;
    --proto)
      shift
      proto=${1:-}
      ;;
    --proto-redir)
      shift
      proto_redir=${1:-}
      ;;
    --max-filesize)
      shift
      max_filesize=${1:-}
      ;;
    https://*) url=$1 ;;
  esac
  shift || true
done
[[ -n "$destination" && -n "$url" ]]
[[ "$proto" == "=https" && "$proto_redir" == "=https" ]]
[[ "$max_filesize" =~ ^[0-9]+$ ]]
if [[ -n "${CODIXING_TEST_BLOCK_DOWNLOAD:-}" && "${url##*/}" == SHA256SUMS ]]; then
  : > "${CODIXING_TEST_BLOCK_READY:?}"
  while [[ ! -e "${CODIXING_TEST_BLOCK_RELEASE:?}" ]]; do
    sleep 0.02
  done
fi
cp "$CODIXING_TEST_FIXTURES/${url##*/}" "$destination"
MOCK
chmod +x "$MOCK_BIN/curl"

REAL_MV=$(command -v mv)
export CODIXING_TEST_REAL_MV="$REAL_MV"
cat > "$MOCK_BIN/mv" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
source_path=${@: -2:1}
destination=${@: -1}
if [[ -n "${CODIXING_TEST_FAIL_PUBLISH:-}" \
      && "$source_path" == *"/.codixing-stage."* \
      && "$destination" == "$CODIXING_INSTALL_DIR/$CODIXING_TEST_FAIL_PUBLISH" ]]; then
  exit 73
fi
exec "$CODIXING_TEST_REAL_MV" "$@"
MOCK
chmod +x "$MOCK_BIN/mv"

REAL_RM=$(command -v rm)
export CODIXING_TEST_REAL_RM="$REAL_RM"
cat > "$MOCK_BIN/rm" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
target=${@: -1}
if [[ -n "${CODIXING_TEST_FAIL_WORK_CLEANUP:-}" \
      && "$target" == *"/codixing-install."* ]]; then
  exit 74
fi
exec "$CODIXING_TEST_REAL_RM" "$@"
MOCK
chmod +x "$MOCK_BIN/rm"

export CODIXING_TEST_FIXTURES="$FIXTURES"
export CODIXING_INSTALL_DIR="$INSTALL_DIR"
export CODIXING_VERSION=9.9.9
export PATH="$MOCK_BIN:$PATH"

sh "$ROOT/docs/install.sh" >/dev/null
for bin in "${BINARIES[@]}"; do
  [[ -x "$INSTALL_DIR/$bin" ]]
done
"$INSTALL_DIR/codixing" --version | grep -q 'codixing 9.9.9'
"$INSTALL_DIR/codixing-mcp" --version | grep -q 'codixing-mcp 9.9.9'

# Hold the first installer deterministically inside its checksum download. A
# concurrent installer must fail before downloading or touching the published
# suite, then the lock must disappear when the owner completes normally.
BLOCK_READY="$TMP/contention-ready"
BLOCK_RELEASE="$TMP/contention-release"
FIRST_LOG="$TMP/contention-first.log"
SECOND_LOG="$TMP/contention-second.log"
CODIXING_TEST_BLOCK_DOWNLOAD=1 \
CODIXING_TEST_BLOCK_READY="$BLOCK_READY" \
CODIXING_TEST_BLOCK_RELEASE="$BLOCK_RELEASE" \
  sh "$ROOT/docs/install.sh" >"$FIRST_LOG" 2>&1 &
INSTALLER_PID=$!
for _ in {1..250}; do
  [[ -e "$BLOCK_READY" ]] && break
  kill -0 "$INSTALLER_PID" 2>/dev/null || break
  sleep 0.02
done
[[ -e "$BLOCK_READY" ]] || {
  echo "first installer did not reach the deterministic contention point" >&2
  exit 1
}
if sh "$ROOT/docs/install.sh" >"$SECOND_LOG" 2>&1; then
  echo "expected concurrent installer to fail on the suite lock" >&2
  exit 1
fi
grep -q 'another installer is active' "$SECOND_LOG"
: > "$BLOCK_RELEASE"
wait "$INSTALLER_PID"
INSTALLER_PID=""
[[ ! -e "$INSTALL_DIR/.codixing-install.lock" ]]

# A termination signal received while a child download is active is handled
# after that child returns. Cleanup must still release the suite lock and leave
# the previously installed files untouched.
record_installed_hashes
BLOCK_READY="$TMP/signal-ready"
BLOCK_RELEASE="$TMP/signal-release"
rm -f "$BLOCK_READY" "$BLOCK_RELEASE"
CODIXING_TEST_BLOCK_DOWNLOAD=1 \
CODIXING_TEST_BLOCK_READY="$BLOCK_READY" \
CODIXING_TEST_BLOCK_RELEASE="$BLOCK_RELEASE" \
  sh "$ROOT/docs/install.sh" >/dev/null 2>&1 &
INSTALLER_PID=$!
for _ in {1..250}; do
  [[ -e "$BLOCK_READY" ]] && break
  kill -0 "$INSTALLER_PID" 2>/dev/null || break
  sleep 0.02
done
[[ -e "$BLOCK_READY" ]] || {
  echo "installer did not reach the deterministic signal point" >&2
  exit 1
}
kill -TERM "$INSTALLER_PID"
: > "$BLOCK_RELEASE"
if wait "$INSTALLER_PID"; then
  echo "expected signalled installer to exit non-zero" >&2
  exit 1
fi
INSTALLER_PID=""
assert_install_unchanged

# Cleanup failures must affect the exit status without short-circuiting the
# EXIT trap before it releases the independently owned installation lock.
CLEANUP_LOG="$TMP/cleanup-failure.log"
if CODIXING_TEST_FAIL_WORK_CLEANUP=1 \
    sh "$ROOT/docs/install.sh" >"$CLEANUP_LOG" 2>&1; then
  echo "expected temporary-directory cleanup failure to fail" >&2
  exit 1
fi
grep -q 'could not remove temporary directory' "$CLEANUP_LOG"
[[ ! -e "$INSTALL_DIR/.codixing-install.lock" ]]

# Version overrides are deliberately strict so malformed release paths never
# reach the network layer.
for invalid in v1.2.3 1 1.2 1.2.3.4 1.2.x 1..3 01.2.3 1.02.3 1.2.03 ''; do
  if CODIXING_VERSION="$invalid" sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
    echo "expected malformed version '$invalid' to fail" >&2
    exit 1
  fi
done

# Corrupt a release asset after the checksum set was generated. The complete
# download phase must fail before replacing any installed executable.
record_installed_hashes
printf 'corrupt\n' >> "$FIXTURES/codixing-mcp-$SUFFIX"

if sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
  echo "expected checksum mismatch to fail" >&2
  exit 1
fi
assert_install_unchanged

# The manifest and each release asset have hard size ceilings even when a curl
# implementation ignores --max-filesize.
dd if=/dev/zero of="$FIXTURES/SHA256SUMS" bs=1048577 count=1 2>/dev/null
if sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
  echo "expected oversized checksum manifest to fail" >&2
  exit 1
fi
assert_install_unchanged

# A failure partway through the four renames restores every previous binary,
# including those already published earlier in the transaction.
make_assets normal publication-candidate
if CODIXING_TEST_FAIL_PUBLISH=codixing-lsp \
    sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
  echo "expected publication failure to fail" >&2
  exit 1
fi
assert_install_unchanged

# The same rollback also removes binaries that had no predecessor in a partial
# or first-time installation.
EMPTY_INSTALL_DIR="$TMP/empty-install"
mkdir -p "$EMPTY_INSTALL_DIR"
if CODIXING_INSTALL_DIR="$EMPTY_INSTALL_DIR" CODIXING_TEST_FAIL_PUBLISH=codixing-lsp \
    sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
  echo "expected first-install publication failure to fail" >&2
  exit 1
fi
for bin in "${BINARIES[@]}"; do
  [[ ! -e "$EMPTY_INSTALL_DIR/$bin" ]]
done
for entry in "$EMPTY_INSTALL_DIR"/.codixing-stage.* "$EMPTY_INSTALL_DIR"/.codixing-backup.*; do
  [[ ! -e "$entry" ]]
done

# Version-reporting binaries are preflighted, then checked again after publish.
# Simulate a CLI that succeeds only in staging to exercise smoke rollback.
make_assets smoke-fail smoke-candidate
if sh "$ROOT/docs/install.sh" >/dev/null 2>&1; then
  echo "expected installed-suite smoke failure to fail" >&2
  exit 1
fi
assert_install_unchanged

echo "shell installer tests passed"
