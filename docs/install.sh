#!/bin/sh
set -e

VERSION="0.28.0"
REPO="ferax564/codixing"
INSTALL_DIR="/usr/local/bin"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
  Linux-x86_64)   SUFFIX="linux-x86_64" ;;
  Darwin-arm64)   SUFFIX="macos-aarch64" ;;
  Darwin-x86_64)  echo "Intel Mac not supported (ONNX Runtime limitation). Use Rosetta: arch -arm64 $0"; exit 1 ;;
  *) echo "Unsupported platform: ${OS}-${ARCH}"; exit 1 ;;
esac

BINARIES="codixing codixing-mcp codixing-lsp"
BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"

echo "Installing Codixing v${VERSION} for ${OS}/${ARCH}..."

for bin in $BINARIES; do
  URL="${BASE_URL}/${bin}-${SUFFIX}"
  echo "  Downloading ${bin}..."
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${URL}" -o "/tmp/${bin}"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "${URL}" -O "/tmp/${bin}"
  else
    echo "Error: curl or wget required"; exit 1
  fi
  chmod +x "/tmp/${bin}"

  if [ -w "${INSTALL_DIR}" ]; then
    mv "/tmp/${bin}" "${INSTALL_DIR}/${bin}"
  else
    sudo mv "/tmp/${bin}" "${INSTALL_DIR}/${bin}"
  fi
done

echo ""
echo "Codixing installed to ${INSTALL_DIR}/"
echo ""
echo "Quick start:"
echo "  codixing init .              # Index current directory"
echo "  codixing search 'query'      # Search your code"
echo ""
echo "MCP integration (Claude Code):"
echo "  claude mcp add codixing -- codixing-mcp --root ."
echo ""
echo "Or use npx (no install needed):"
echo "  npx -y codixing-mcp --root ."
