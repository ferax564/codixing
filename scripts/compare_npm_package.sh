#!/usr/bin/env bash
# Compare the exact files npm would publish locally with an existing package.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: compare_npm_package.sh LOCAL_PACKAGE_DIR PACKAGE_SPEC" >&2
  exit 2
fi

LOCAL_PACKAGE_DIR=$(cd "$1" && pwd -P)
PACKAGE_SPEC=$2
WORK_DIR=$(mktemp -d)
export NPM_CONFIG_CACHE="$WORK_DIR/npm-cache"
cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p \
  "$WORK_DIR/local-pack" "$WORK_DIR/registry-pack" \
  "$WORK_DIR/local" "$WORK_DIR/registry"

npm pack "$LOCAL_PACKAGE_DIR" \
  --ignore-scripts --loglevel=error \
  --pack-destination "$WORK_DIR/local-pack" >/dev/null
npm pack "$PACKAGE_SPEC" \
  --ignore-scripts --loglevel=error \
  --pack-destination "$WORK_DIR/registry-pack" >/dev/null

shopt -s nullglob
local_tarballs=("$WORK_DIR/local-pack"/*.tgz)
registry_tarballs=("$WORK_DIR/registry-pack"/*.tgz)
if [[ ${#local_tarballs[@]} -ne 1 || ${#registry_tarballs[@]} -ne 1 ]]; then
  echo "ERROR: npm pack did not produce exactly one local and one registry tarball" >&2
  exit 1
fi

tar -xzf "${local_tarballs[0]}" --no-same-owner --no-same-permissions \
  -C "$WORK_DIR/local"
tar -xzf "${registry_tarballs[0]}" --no-same-owner --no-same-permissions \
  -C "$WORK_DIR/registry"

if ! diff -ru --no-dereference \
    "$WORK_DIR/local/package" "$WORK_DIR/registry/package"; then
  echo "ERROR: $PACKAGE_SPEC exists but differs from the exact local npm package" >&2
  exit 1
fi

echo "$PACKAGE_SPEC matches the exact local npm package."
