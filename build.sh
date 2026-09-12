#!/usr/bin/env bash
# Linux 构建脚本（与 build.ps1 对应；Windows 用 PowerShell 版）
#
# 用法:
#   ./build.sh            # debug 构建（daemon + sync-ui，不打包）
#   ./build.sh release    # release 构建 + 打包 dist/kulua/
#   TAURI_BUNDLE=1 ./build.sh release   # 额外生成原生安装包（deb/rpm/AppImage）
#
# 依赖: rust (cargo), node/npm, tauri Linux 系统库（webkit2gtk-4.1 等），
#       打包阶段可选 adb（PATH 或 ./adb）与 kulua-server.jar（缺时自动构建）。
set -euo pipefail

RELEASE="${1:-debug}"
TARGET_FLAG=""
TAURI_FLAGS="--debug --no-bundle"
if [[ "$RELEASE" == "release" ]]; then
    TARGET_FLAG="--release"
    # 默认不生成原生安装包（AppImage 打包需联网下载 linuxdeploy 等工具），
    # 与 Windows 版 dist/kulua 文件夹形态一致；需要安装包时 TAURI_BUNDLE=1 ./build.sh release
    TAURI_FLAGS="${TAURI_BUNDLE:+}"
    [[ -n "$TAURI_BUNDLE" ]] || TAURI_FLAGS="--no-bundle"
fi

CONFIG_DIR="target/$([ "$RELEASE" == "release" ] && echo "release" || echo "debug")"
DIST="dist/kulua"

echo "===== Sync Workspace Build (Linux) ====="

echo ""
echo "[1/3] Building daemon..."
cargo build --package daemon $TARGET_FLAG

echo ""
echo "[2/3] Building UI (Tauri)..."
(
    cd ui
    npm install --silent
    # CI 环境变量干扰 tauri CLI，在子 shell 中清除
    unset CI
    npm run tauri -- build $TAURI_FLAGS
)

if [[ "$RELEASE" != "release" ]]; then
    echo ""
    echo "===== Debug build done ====="
    echo "  daemon: $CONFIG_DIR/daemon"
    echo "  UI:     $CONFIG_DIR/sync-ui"
    exit 0
fi

# ── 3. 打包发布文件夹 ──
echo ""
echo "[3/3] Packaging release to $DIST ..."
rm -rf "$DIST"
mkdir -p "$DIST"

# ── daemon ──
if [[ -f "target/release/daemon" ]]; then
    cp "target/release/daemon" "$DIST/" && chmod +x "$DIST/daemon"
else
    echo "  WARNING: daemon not found at target/release/daemon" >&2
fi

# ── UI ──
if [[ -f "target/release/sync-ui" ]]; then
    cp "target/release/sync-ui" "$DIST/" && chmod +x "$DIST/sync-ui"
else
    echo "  WARNING: sync-ui not found at target/release/sync-ui" >&2
fi

# ── 自研 kulua-server jar（替代官方 scrcpy-server；未构建时自动构建） ──
jar_found=false
for j in "kulua-server/build/kulua-server.jar" "./kulua-server.jar" "kulua-server.jar"; do
    if [[ -f "$j" ]]; then
        cp "$j" "$DIST/kulua-server.jar"
        echo "  kulua-server.jar bundled ($j)"
        jar_found=true
        break
    fi
done
if [[ "$jar_found" != true && -f "kulua-server/build.sh" ]]; then
    echo "  kulua-server.jar 未找到，尝试自动构建..."
    if bash "kulua-server/build.sh"; then
        cp "kulua-server/build/kulua-server.jar" "$DIST/kulua-server.jar"
        echo "  kulua-server.jar 构建并打包"
        jar_found=true
    fi
fi
if [[ "$jar_found" != true ]]; then
    echo "  WARNING: kulua-server.jar not found -- place it manually in $DIST/" >&2
fi

# ── adb（Linux 无捆绑 DLL；./adb 优先，PATH 兜底） ──
adb_found=false
if [[ -f "./adb" ]]; then
    cp "./adb" "$DIST/adb" && chmod +x "$DIST/adb"
    echo "  adb bundled (./adb)"
    adb_found=true
elif command -v adb >/dev/null 2>&1; then
    cp "$(command -v adb)" "$DIST/adb" && chmod +x "$DIST/adb"
    echo "  adb bundled from PATH ($(command -v adb))"
    adb_found=true
else
    echo "  WARNING: adb not found（daemon 必需）-- 装系统包后重跑，或下载放 ./adb：" >&2
    echo "    sudo pacman -S android-tools" >&2
    echo "    # 或: 手动下载 https://dl.google.com/android/repository/platform-tools-latest-linux.zip 解压到项目根 ./adb" >&2
fi

# ── 验证 ──
echo ""
echo "$DIST 内容："
ls -l "$DIST"

echo ""
echo "===== Release package ready at $DIST ====="
echo "  daemon       — 后台服务（直接运行即可）"
echo "  sync-ui      — 桌面 GUI（可选，daemon 会自动拉起）"
if [[ "$adb_found" == true ]]; then echo "  adb          — 已捆绑" ; fi
if [[ "$jar_found" == true ]]; then echo "  kulua-server.jar — 自研设备端服务（剪贴板/多窗口视频/音频）"; fi
echo ""
echo "使用方法：直接运行 ./daemon 即可启动全部功能（或放入 ~/.config/autostart/ 开机自启）"