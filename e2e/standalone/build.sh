#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$REPO_ROOT/e2e/standalone/fixture/pkg"

bash "$REPO_ROOT/tools/standalone/build.sh" "$OUT_DIR"

echo "Built standalone browser fixture to $OUT_DIR"
