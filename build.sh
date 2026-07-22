#!/usr/bin/env bash
set -euo pipefail

RELEASE="${1:-debug}"
TARGET_FLAG=""
TAURI_FLAGS="--debug --no-bundle"
[[ "$RELEASE" == "release" ]] && { TARGET_FLAG="--release"; TAURI_FLAGS=""; }

CONFIG_DIR="target/$([ "$RELEASE" == "release" ] && echo "release" || echo "debug")"

echo "===== Sync Workspace Build ====="

echo ""
echo "[1/2] Building daemon..."
cargo build --package daemon $TARGET_FLAG

echo ""
echo "[2/2] Building UI..."
cd ui
npm install --silent
# CI 环境变量干扰 tauri CLI，在子 shell 中清除
(unset CI; npm run tauri -- build $TAURI_FLAGS)
cd ..

echo ""
echo "===== All done ====="
echo "  daemon: $CONFIG_DIR/daemon"
echo "  UI:     $CONFIG_DIR/sync-ui"
