#!/usr/bin/env sh
set -e

REPO="ferax564/codixing"
INSTALL_DIR="/usr/local/bin"
BINARIES="codixing codixing-mcp codixing-server"

# ── Colours ──────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
  BOLD="\033[1m"; CYAN="\033[36m"; GREEN="\033[32m"; RED="\033[31m"; RESET="\033[0m"
else
  BOLD=""; CYAN=""; GREEN=""; RED=""; RESET=""
fi

info()  { printf "${CYAN}→${RESET} %s\n" "$1"; }
ok()    { printf "${GREEN}✓${RESET} %s\n" "$1"; }
err()   { printf "${RED}✗${RESET} %s\n" "$1" >&2; exit 1; }
bold()  { printf "${BOLD}%s${RESET}\n" "$1"; }

# ── Detect OS ────────────────────────────────────────────────────────────────
OS="$(uname -s)"
case "$OS" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *)      err "Unsupported OS: $OS. Please build from source: https://github.com/$REPO" ;;
esac

# ── Detect arch ──────────────────────────────────────────────────────────────
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64 | amd64) ARCH="x86_64" ;;
  arm64 | aarch64) ARCH="aarch64" ;;
  *) err "Unsupported architecture: $ARCH" ;;
esac

SUFFIX="${OS}-${ARCH}"

# ── Resolve latest release tag ───────────────────────────────────────────────
info "Fetching latest release..."
if command -v curl >/dev/null 2>&1; then
  FETCH="curl -fsSL"
elif command -v wget >/dev/null 2>&1; then
  FETCH="wget -qO-"
else
  err "curl or wget is required"
fi

TAG=$($FETCH "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name"' \
  | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

[ -z "$TAG" ] && err "Could not determine latest release tag"
info "Installing Codixing $TAG for $OS/$ARCH"

# ── Download ──────────────────────────────────────────────────────────────────
BASE_URL="https://github.com/$REPO/releases/download/$TAG"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

for BIN in $BINARIES; do
  URL="$BASE_URL/${BIN}-${SUFFIX}"
  DEST="$TMP/$BIN"
  info "Downloading $BIN..."
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$URL" -o "$DEST"
  else
    wget -qO "$DEST" "$URL"
  fi
  chmod +x "$DEST"
done

# ── Install ───────────────────────────────────────────────────────────────────
if [ -w "$INSTALL_DIR" ]; then
  SUDO=""
else
  SUDO="sudo"
  info "sudo required to write to $INSTALL_DIR"
fi

for BIN in $BINARIES; do
  $SUDO mv "$TMP/$BIN" "$INSTALL_DIR/$BIN"
done

# ── Verify ────────────────────────────────────────────────────────────────────
bold ""
bold "  Codixing installed successfully!"
bold ""
ok "codixing        → $(command -v codixing)"
ok "codixing-mcp    → $(command -v codixing-mcp)"
ok "codixing-server → $(command -v codixing-server)"
bold ""
printf "  ${CYAN}Next steps:${RESET}\n"
printf "  1. Index your project:  ${BOLD}codixing init .${RESET}\n"
printf "  2. Search:              ${BOLD}codixing search \"your query\"${RESET}\n"
printf "  3. Connect to Claude:   ${BOLD}codixing-mcp --root . --daemon${RESET}\n"
bold ""
printf "  Docs: https://codixing.com/docs\n"
bold ""
