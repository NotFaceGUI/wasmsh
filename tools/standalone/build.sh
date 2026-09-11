#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="${1:-$REPO_ROOT/dist/standalone-pkg}"
WASM_PACK_BIN="${WASM_PACK_BIN:-wasm-pack}"
WASM_BINDGEN_BIN="${WASM_BINDGEN_BIN:-wasm-bindgen}"
WASM_OPT_BIN="${WASM_OPT_BIN:-wasm-opt}"
source "$REPO_ROOT/tools/standalone/versions.env"

case "$OUT_DIR" in
  /*) ;;
  *) OUT_DIR="$REPO_ROOT/$OUT_DIR" ;;
esac
CRATE_DIR="$REPO_ROOT/crates/wasmsh-browser"

if ! command -v "$WASM_PACK_BIN" >/dev/null 2>&1; then
  echo "wasm-pack is required; install the pinned version from tools/standalone/versions.env" >&2
  exit 1
fi
if ! command -v "$WASM_BINDGEN_BIN" >/dev/null 2>&1; then
  echo "wasm-bindgen is required; install the pinned CLI version from tools/standalone/versions.env" >&2
  exit 1
fi
if ! command -v "$WASM_OPT_BIN" >/dev/null 2>&1; then
  echo "wasm-opt is required; run tools/standalone/install-binaryen.sh first" >&2
  exit 1
fi

wasm_pack_version="$($WASM_PACK_BIN --version)"
case "$wasm_pack_version" in
  "wasm-pack $WASM_PACK_VERSION") ;;
  *)
    echo "expected wasm-pack $WASM_PACK_VERSION, got: $wasm_pack_version" >&2
    exit 1
    ;;
esac

wasm_bindgen_version="$($WASM_BINDGEN_BIN --version)"
case "$wasm_bindgen_version" in
  "wasm-bindgen $WASM_BINDGEN_CLI_VERSION") ;;
  *)
    echo "expected wasm-bindgen $WASM_BINDGEN_CLI_VERSION, got: $wasm_bindgen_version" >&2
    exit 1
    ;;
esac

wasm_opt_version="$($WASM_OPT_BIN --version)"
case "$wasm_opt_version" in
  "wasm-opt version $BINARYEN_VERSION ("*) ;;
  *)
    echo "expected wasm-opt Binaryen $BINARYEN_VERSION, got: $wasm_opt_version" >&2
    exit 1
    ;;
esac

mkdir -p "$OUT_DIR"

for target in bundler web nodejs; do
  target_dir="$OUT_DIR/$target"
  rm -rf "$target_dir"
  mkdir -p "$target_dir"
  out_arg="$(realpath --relative-to="$CRATE_DIR" "$target_dir")"
  echo "Building wasmsh-browser target=$target"
  "$WASM_PACK_BIN" build \
    --target "$target" \
    --release \
    --mode no-install \
    --out-dir "$out_arg" \
    "$CRATE_DIR" \
    --locked
done

node "$REPO_ROOT/tools/standalone/verify-package.mjs" "$OUT_DIR"
echo "Standalone package targets verified in $OUT_DIR"
