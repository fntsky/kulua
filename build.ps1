param(
    [switch]$Release
)

$config = if ($Release) { "release" } else { "debug" }
$targetFlag = if ($Release) { "--release" } else { "" }
$ErrorActionPreference = "Stop"

Write-Host "===== Kulua Build =====" -ForegroundColor Cyan

# ── 1. daemon ──
Write-Host "`n[1/3] Building daemon..." -ForegroundColor Yellow
cargo build --package daemon $targetFlag
if ($LASTEXITCODE -ne 0) { exit 1 }

# ── 2. UI ──
Write-Host "`n[2/3] Building UI (exe, no installer)..." -ForegroundColor Yellow
Push-Location ui
npm install --silent
if ($Release) {
    powershell -Command "Remove-Item env:CI -ErrorAction Ignore; npm run tauri build"
} else {
    # CI 环境变量会干扰 tauri CLI，在子进程中清除
    powershell -Command "Remove-Item env:CI -ErrorAction Ignore; npm run tauri -- build --debug --no-bundle"
}
if ($LASTEXITCODE -ne 0) { Pop-Location; exit 1 }
Pop-Location

# ── 3. 打包发布文件夹 ──
if ($Release) {
    $dist = "dist/kulua"
    Write-Host "`n[3/3] Packaging release to $dist ..." -ForegroundColor Yellow

    # 清理旧打包残留（如早期版本的官方 scrcpy-server jar），避免混淆
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Get-ChildItem $dist -File -ErrorAction SilentlyContinue | Remove-Item -Force

    # ── daemon ──
    Copy-Item "target/release/daemon.exe" "$dist/" -ErrorAction SilentlyContinue
    if (-not (Test-Path "$dist/daemon.exe")) {
        Write-Host "  WARNING: daemon.exe not found at target/release/daemon.exe" -ForegroundColor Red
    }

    # ── UI ──
    $tauriExe = "target/release/sync-ui.exe"
    if (Test-Path $tauriExe) {
        Copy-Item $tauriExe "$dist/"
    } else {
        Write-Host "  WARNING: sync-ui.exe not found at $tauriExe" -ForegroundColor Red
    }

    # ── 自研 kulua-server jar（替代官方 scrcpy-server；未构建时自动构建） ──
    $jarFound = $false
    $jarCandidates = @("kulua-server/build/kulua-server.jar", "./kulua-server.jar", "kulua-server.jar")
    foreach ($j in $jarCandidates) {
        if (Test-Path $j) {
            Copy-Item $j "$dist/kulua-server.jar"
            Write-Host "  kulua-server.jar bundled ($j)" -ForegroundColor Green
            $jarFound = $true
            break
        }
    }
    if (-not $jarFound) {
        Write-Host "  kulua-server.jar 未找到，尝试自动构建..." -ForegroundColor Yellow
        if (Test-Path "./kulua-server/build.ps1") {
            & powershell -NoProfile -ExecutionPolicy Bypass -File "./kulua-server/build.ps1"
            if (Test-Path "./kulua-server/build/kulua-server.jar") {
                Copy-Item "./kulua-server/build/kulua-server.jar" "$dist/kulua-server.jar"
                Write-Host "  kulua-server.jar 构建并打包" -ForegroundColor Green
                $jarFound = $true
            }
        }
    }
    if (-not $jarFound) {
        Write-Host "  WARNING: kulua-server.jar not found -- place it manually in $dist/" -ForegroundColor Yellow
    }

    # ── adb.exe + 依赖 DLL ──
    $adbFound = $false
    $adbPaths = @(
        "./adb.exe"
        "${env:ANDROID_HOME}/platform-tools/adb.exe"
        "${env:ANDROID_SDK_ROOT}/platform-tools/adb.exe"
    )
    foreach ($p in $adbPaths) {
        $expanded = [System.Environment]::ExpandEnvironmentVariables($p)
        if (Test-Path $expanded) {
            $dir = Split-Path $expanded -Parent
            Copy-Item $expanded "$dist/adb.exe"
            Get-ChildItem "$dir/*.dll" -ErrorAction SilentlyContinue | ForEach-Object {
                Copy-Item $_.FullName "$dist/"
                Write-Host "    DLL: $($_.Name)" -ForegroundColor DarkGray
            }
            Write-Host "  adb.exe + DLLs from $dir" -ForegroundColor Green
            $adbFound = $true
            break
        }
    }
    if (-not $adbFound) {
        $pathAdb = Get-Command "adb.exe" -ErrorAction SilentlyContinue
        if ($pathAdb) {
            $dir = Split-Path $pathAdb.Source -Parent
            Copy-Item $pathAdb.Source "$dist/adb.exe"
            Get-ChildItem "$dir/*.dll" -ErrorAction SilentlyContinue | ForEach-Object {
                Copy-Item $_.FullName "$dist/"
                Write-Host "    DLL: $($_.Name)" -ForegroundColor DarkGray
            }
            Write-Host "  adb.exe + DLLs from PATH ($dir)" -ForegroundColor Green
            $adbFound = $true
        }
    }
    if (-not $adbFound) {
        Write-Host "  WARNING: adb.exe not found -- place it manually in $dist/ so daemon can find it" -ForegroundColor Red
    }

    # ── 验证 ──
    Write-Host "`n$dist 内容：" -ForegroundColor Cyan
    Get-ChildItem $dist | Select-Object Name, Length | Format-Table -AutoSize

    Write-Host "`n===== Release package ready at $dist =====" -ForegroundColor Green
    Write-Host "  daemon.exe  — 后台服务（双击运行即可）"
    Write-Host "  sync-ui.exe — 桌面 GUI（可选）"
    if ($adbFound) { Write-Host "  adb.exe     — 已捆绑（含 DLL），无需预装 ADB" }
    if ($jarFound) { Write-Host "  kulua-server.jar — 自研设备端服务（剪贴板/多窗口视频/音频）" }
    Write-Host "`n使用方法：直接双击 daemon.exe 即可启动全部功能" -ForegroundColor Green
} else {
    Write-Host "`n===== Debug build done =====" -ForegroundColor Green
    Write-Host "  daemon: target\$config\daemon.exe"
    Write-Host "  UI:     target\$config\sync-ui.exe"
}
