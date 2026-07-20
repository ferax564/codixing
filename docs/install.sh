#!/bin/sh
set -eu

REPO="ferax564/codixing"
VERSION="${CODIXING_VERSION-latest}"
BINARIES="codixing codixing-mcp codixing-lsp codixing-server"
MAX_MANIFEST_BYTES=1048576
MAX_BINARY_BYTES=268435456

fail() {
  echo "codixing installer: $*" >&2
  exit 1
}

valid_version_component() {
  case "$1" in
    0|[1-9]|[1-9][0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

valid_release_version() {
  candidate=$1
  case "$candidate" in
    ''|*[!0-9.]*) return 1 ;;
  esac

  major=${candidate%%.*}
  remainder=${candidate#*.}
  [ "$remainder" != "$candidate" ] || return 1
  minor=${remainder%%.*}
  patch=${remainder#*.}
  [ "$patch" != "$remainder" ] || return 1
  [ -n "$major" ] && [ -n "$minor" ] && [ -n "$patch" ] || return 1
  case "$patch" in
    *.*) return 1 ;;
  esac
  valid_version_component "$major" \
    && valid_version_component "$minor" \
    && valid_version_component "$patch"
}

binary_version() {
  output=$("$1" --version 2>&1) || return 1
  detected=${output##* }
  valid_release_version "$detected" || return 1
  printf '%s\n' "$detected"
}

verify_versioned_binaries() {
  directory=$1
  cli_version=$(binary_version "${directory%/}/codixing") \
    || fail "codixing did not report a valid version"
  mcp_version=$(binary_version "${directory%/}/codixing-mcp") \
    || fail "codixing-mcp did not report a valid version"
  [ "$cli_version" = "$mcp_version" ] \
    || fail "release suite version mismatch: codixing ${cli_version}, codixing-mcp ${mcp_version}"
  if [ "$VERSION" != latest ]; then
    [ "$cli_version" = "$VERSION" ] \
      || fail "release suite reports ${cli_version}, expected ${VERSION}"
  fi
}

download() {
  url=$1
  destination=$2
  max_bytes=$3

  case "$url" in
    https://*) ;;
    *) fail "refusing non-HTTPS download URL: $url" ;;
  esac
  command -v curl >/dev/null 2>&1 \
    || fail "curl is required (portable wget variants cannot reliably block HTTPS downgrade redirects)"

  rm -f "$destination"
  if ! curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' \
    --connect-timeout 10 --max-time 180 --retry 3 \
    --max-filesize "$max_bytes" \
    "$url" --output "$destination"; then
    rm -f "$destination"
    fail "download failed: $url"
  fi

  [ -f "$destination" ] || fail "download did not produce a regular file: $url"
  downloaded_bytes=$(wc -c < "$destination" | tr -d '[:space:]')
  case "$downloaded_bytes" in
    ''|*[!0-9]*) fail "could not determine download size: $url" ;;
  esac
  if [ "$downloaded_bytes" -gt "$max_bytes" ]; then
    rm -f "$destination"
    fail "download exceeds ${max_bytes}-byte limit: $url"
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required to verify downloads"
  fi
}

# The release workflow publishes exactly these three target families.
OS=$(uname -s)
ARCH=$(uname -m)
case "${OS}-${ARCH}" in
  Linux-x86_64)  SUFFIX="linux-x86_64" ;;
  Darwin-arm64)  SUFFIX="macos-aarch64" ;;
  Darwin-x86_64) fail "Intel macOS release binaries are not published; use Apple Silicon or build from source" ;;
  *)             fail "unsupported platform: ${OS}-${ARCH}" ;;
esac

case "$VERSION" in
  latest)
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
    VERSION_LABEL="latest release"
    ;;
  *)
    valid_release_version "$VERSION" \
      || fail "CODIXING_VERSION must be exactly X.Y.Z (for example, 0.46.0)"
    BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
    VERSION_LABEL="v${VERSION}"
    ;;
esac

if [ -n "${CODIXING_INSTALL_DIR:-}" ]; then
  INSTALL_DIR=$CODIXING_INSTALL_DIR
elif [ "$(id -u)" -eq 0 ] || [ -w /usr/local/bin ]; then
  INSTALL_DIR=/usr/local/bin
else
  [ -n "${HOME:-}" ] || fail "HOME is unset; set CODIXING_INSTALL_DIR explicitly"
  INSTALL_DIR="$HOME/.local/bin"
fi

umask 022
mkdir -p "$INSTALL_DIR" || fail "cannot create install directory: $INSTALL_DIR"
[ -d "$INSTALL_DIR" ] || fail "install path is not a directory: $INSTALL_DIR"
[ -w "$INSTALL_DIR" ] || fail "install directory is not writable: $INSTALL_DIR (set CODIXING_INSTALL_DIR or run with suitable permissions)"

TMP_ROOT=${TMPDIR:-/tmp}
WORK_DIR=""
STAGE_DIR=""
BACKUP_DIR=""
TRANSACTION_ACTIVE=0
ROLLBACK_FAILED=0
LOCK_DIR="${INSTALL_DIR%/}/.codixing-install.lock"
LOCK_HELD=0

path_exists() {
  [ -e "$1" ] || [ -L "$1" ]
}

rollback() {
  [ "$TRANSACTION_ACTIVE" -eq 1 ] || return 0
  echo "codixing installer: restoring previous installation" >&2
  ROLLBACK_FAILED=0
  for bin in $BINARIES; do
    destination="${INSTALL_DIR%/}/${bin}"
    backup="${BACKUP_DIR%/}/${bin}"
    marker="${BACKUP_DIR%/}/.published-${bin}"
    if path_exists "$backup"; then
      if ! rm -f "$destination"; then
        echo "codixing installer: warning: could not remove failed ${bin}" >&2
        ROLLBACK_FAILED=1
        continue
      fi
      if ! mv -f "$backup" "$destination"; then
        echo "codixing installer: warning: could not restore ${bin} from ${backup}" >&2
        ROLLBACK_FAILED=1
      fi
    elif [ -f "$marker" ]; then
      if ! rm -f "$destination"; then
        echo "codixing installer: warning: could not remove newly installed ${bin}" >&2
        ROLLBACK_FAILED=1
      fi
    fi
  done
  TRANSACTION_ACTIVE=0
}

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ "$TRANSACTION_ACTIVE" -eq 1 ]; then
    rollback
  fi
  if [ -n "$WORK_DIR" ]; then
    if ! rm -rf "$WORK_DIR"; then
      echo "codixing installer: warning: could not remove temporary directory ${WORK_DIR}" >&2
      if [ "$status" -eq 0 ]; then
        status=1
      fi
    fi
  fi
  if [ -n "$STAGE_DIR" ]; then
    if ! rm -rf "$STAGE_DIR"; then
      echo "codixing installer: warning: could not remove staging directory ${STAGE_DIR}" >&2
      if [ "$status" -eq 0 ]; then
        status=1
      fi
    fi
  fi
  if [ -n "$BACKUP_DIR" ]; then
    if [ "$ROLLBACK_FAILED" -eq 0 ]; then
      if ! rm -rf "$BACKUP_DIR"; then
        echo "codixing installer: warning: could not remove rollback directory ${BACKUP_DIR}" >&2
        if [ "$status" -eq 0 ]; then
          status=1
        fi
      fi
    else
      echo "codixing installer: rollback files retained at ${BACKUP_DIR}" >&2
    fi
  fi
  if [ "$LOCK_HELD" -eq 1 ]; then
    LOCK_HELD=0
    if ! rmdir "$LOCK_DIR"; then
      echo "codixing installer: warning: could not remove installation lock ${LOCK_DIR}" >&2
      if [ "$status" -eq 0 ]; then
        status=1
      fi
    fi
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# Publishing four executables requires a process-wide transaction, not just
# per-file atomic renames. Hold an empty same-directory lock for the complete
# download, verification, publication, and rollback lifecycle. Temporarily
# ignore termination signals across mkdir + ownership bookkeeping so cleanup
# can never remove another installer's lock or strand one we just acquired.
trap '' HUP INT TERM
if mkdir "$LOCK_DIR" 2>/dev/null; then
  LOCK_HELD=1
  lock_acquired=true
else
  lock_acquired=false
fi
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
[ "$lock_acquired" = true ] \
  || fail "another installer is active (remove stale lock only after checking): ${LOCK_DIR}"

WORK_DIR=$(mktemp -d "${TMP_ROOT%/}/codixing-install.XXXXXX") \
  || fail "could not create a temporary directory"

echo "Installing Codixing ${VERSION_LABEL} for ${OS}/${ARCH}..."
download "${BASE_URL}/SHA256SUMS" "$WORK_DIR/SHA256SUMS" "$MAX_MANIFEST_BYTES"

# Download and authenticate the complete suite before replacing any installed
# executable. A truncated or mismatched asset leaves the existing install alone.
for bin in $BINARIES; do
  asset="${bin}-${SUFFIX}"
  echo "  Downloading ${bin}..."
  download "${BASE_URL}/${asset}" "$WORK_DIR/${asset}" "$MAX_BINARY_BYTES"

  expected=$(awk -v name="$asset" '$2 == name { print $1; exit }' "$WORK_DIR/SHA256SUMS")
  [ -n "$expected" ] || fail "SHA256SUMS does not contain ${asset}"
  [ "${#expected}" -eq 64 ] || fail "invalid SHA-256 entry for ${asset}"
  case "$expected" in
    *[!0-9A-Fa-f]*) fail "invalid SHA-256 entry for ${asset}" ;;
  esac

  actual=$(sha256_file "$WORK_DIR/${asset}")
  expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
  actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
  [ "$actual" = "$expected" ] || fail "checksum mismatch for ${asset}"
done

# Stage inside the destination directory so every publication rename stays on
# one filesystem. Preflight the complete suite before touching an installation.
STAGE_DIR=$(mktemp -d "${INSTALL_DIR%/}/.codixing-stage.XXXXXX") \
  || fail "cannot create a staging directory in ${INSTALL_DIR}"
for bin in $BINARIES; do
  asset="${bin}-${SUFFIX}"
  cp "$WORK_DIR/${asset}" "$STAGE_DIR/$bin" || fail "could not stage ${bin}"
  chmod 755 "$STAGE_DIR/$bin" || fail "could not make staged ${bin} executable"
  [ -s "$STAGE_DIR/$bin" ] && [ -x "$STAGE_DIR/$bin" ] \
    || fail "staged ${bin} is not a non-empty executable"

  destination="${INSTALL_DIR%/}/${bin}"
  if path_exists "$destination" && [ -d "$destination" ] && [ ! -L "$destination" ]; then
    fail "refusing to replace directory at ${destination}"
  fi
done

# Only the CLI and MCP executables expose a side-effect-free --version command.
# The LSP binary speaks its protocol on stdin and the HTTP server starts a
# service, so executing either as an installer probe would be unsafe.
verify_versioned_binaries "$STAGE_DIR"

# Publish all four binaries as one transaction. Existing files remain in the
# same-directory backup until the installed suite passes its smoke checks.
BACKUP_DIR=$(mktemp -d "${INSTALL_DIR%/}/.codixing-backup.XXXXXX") \
  || fail "cannot create a rollback directory in ${INSTALL_DIR}"
TRANSACTION_ACTIVE=1
for bin in $BINARIES; do
  destination="${INSTALL_DIR%/}/${bin}"
  backup="${BACKUP_DIR%/}/${bin}"
  marker="${BACKUP_DIR%/}/.published-${bin}"
  if path_exists "$destination"; then
    mv -f "$destination" "$backup" || fail "could not back up installed ${bin}"
  fi
  : > "$marker" || fail "could not record publication state for ${bin}"
  mv -f "$STAGE_DIR/$bin" "$destination" || fail "could not publish ${bin}"
done

for bin in $BINARIES; do
  [ -s "${INSTALL_DIR%/}/${bin}" ] && [ -x "${INSTALL_DIR%/}/${bin}" ] \
    || fail "installed ${bin} is not a non-empty executable"
done
verify_versioned_binaries "$INSTALL_DIR"

TRANSACTION_ACTIVE=0
rm -rf "$BACKUP_DIR"
BACKUP_DIR=""
rmdir "$STAGE_DIR" || fail "could not remove installation staging directory"
STAGE_DIR=""

echo
echo "Codixing installed to ${INSTALL_DIR}/"
case ":${PATH:-}:" in
  *:"${INSTALL_DIR}":*) ;;
  *)
    echo "Add ${INSTALL_DIR} to PATH, for example:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
echo
echo "Quick start:"
echo "  codixing init .              # Index current directory"
echo "  codixing search 'query'      # Search your code"
echo
echo "MCP integration (minimal context profile):"
echo "  claude mcp add codixing -- codixing-mcp --root . --profile minimal --no-daemon-fork"
echo
echo "Or use npx without a global install:"
echo "  npx -y codixing-mcp --root . --profile minimal --no-daemon-fork"
