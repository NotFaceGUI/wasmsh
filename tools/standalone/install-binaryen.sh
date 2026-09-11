#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
source "$REPO_ROOT/tools/standalone/versions.env"

DEST_DIR="${1:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/wasmsh-binaryen-${BINARYEN_VERSION}}"
ARCHIVE_NAME="binaryen-version_${BINARYEN_VERSION}-x86_64-linux.tar.gz"
ARCHIVE_URL="https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/${ARCHIVE_NAME}"

case "$(uname -s):$(uname -m)" in
  Linux:x86_64) ;;
  *)
    echo "standalone Binaryen installer supports only Linux x86_64 runners" >&2
    exit 1
    ;;
esac

WASM_OPT="$DEST_DIR/bin/wasm-opt"
if [ ! -x "$WASM_OPT" ]; then
  download_dir="$(mktemp -d)"
  trap 'rm -rf "$download_dir"' EXIT
  archive="$download_dir/$ARCHIVE_NAME"

  curl --fail --location --retry 3 --retry-delay 2 --silent --show-error \
    --output "$archive" "$ARCHIVE_URL"
  printf '%s  %s\n' "$BINARYEN_ARCHIVE_SHA256" "$archive" | sha256sum --check --status

  mkdir -p "$DEST_DIR"
  tar -xzf "$archive" -C "$download_dir"
  extracted="$download_dir/binaryen-version_${BINARYEN_VERSION}"
  test -x "$extracted/bin/wasm-opt"
  cp -R "$extracted/." "$DEST_DIR/"
fi

version="$($WASM_OPT --version)"
case "$version" in
  "wasm-opt version $BINARYEN_VERSION ("*) ;;
  *)
    echo "expected wasm-opt Binaryen $BINARYEN_VERSION, got: $version" >&2
    exit 1
    ;;
esac

printf '%s\n' "$DEST_DIR"
