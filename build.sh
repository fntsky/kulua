#!/usr/bin/env bash
set -euo pipefail

RELEASE="${1:-debug}"
TARGET_FLAG=""
TAURI_FLAGS="--debug --no-bundle"
[[ "$RELEASE" == "release" ]] && { TARGET_FLAG="--release"; TAURI_FLAGS=""; }

CONFIG_DIR="target/$([ "$RELEASE" == "release" ] && echo "release" || echo "debug")"

echo "===== Sync Workspace Build ====="

echo ""
echo "[1/3] Building daemon..."
cargo build --package daemon $TARGET_FLAG

echo ""
echo "[2/3] Building fusion-viewer (self-developed viewer)..."
# msys2/mingw64 in PATH breaks ffmpeg-sys-next C header probing on Windows;
# on Linux a clean build works normally.
if [ -f "scripts/build-viewer.cmd" ] && command -v cmd >/dev/null 2>&1; then
    cmd /c "scripts\build-viewer.cmd build -p fusion-viewer $TARGET_FLAG"
else
    cargo build --package fusion-viewer $TARGET_FLAG
fi

echo ""
echo "[3/3] Building UI..."
cd ui
npm install --silent
# CI 环境变量干扰 tauri CLI，在子 shell 中清除
(unset CI; npm run tauri -- build $TAURI_FLAGS)
cd ..

echo ""
echo "===== All done ====="
echo "  daemon: $CONFIG_DIR/daemon"
echo "  UI:     $CONFIG_DIR/sync-ui"
