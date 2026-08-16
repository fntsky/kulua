param(
    [switch]$Release,
    # FFmpeg 运行时目录（含 avcodec-62.dll / avutil-60.dll / swresample-6.dll，融合窗口用）。
    # 默认依次查找：./vendor/scrcpy-win64、$env:SCRCPY_HOME。
    [string]$FfmpegDir = ""
)

$config = if ($Release) { "release" } else { "debug" }
$targetFlag = if ($Release) { "--release" } else { "" }
$ErrorActionPreference = "Stop"

Write-Host "===== Kulua Build =====" -ForegroundColor Cyan

# ── 1. daemon ──
Write-Host "`n[1/2] Building daemon..." -ForegroundColor Yellow
cargo build --package daemon $targetFlag
if ($LASTEXITCODE -ne 0) { exit 1 }

# ── 2. UI ──
Write-Host "`n[2/2] Building UI (exe, no installer)..." -ForegroundColor Yellow
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

    New-Item -ItemType Directory -Force -Path $dist | Out-Null

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

    # ── scrcpy-server jar ──
    $jarFound = $false
    $jarCandidates = @("scrcpy-server", "./scrcpy-server")
    foreach ($j in $jarCandidates) {
        if (Test-Path $j) {
            Copy-Item $j "$dist/"
            Write-Host "  scrcpy-server bundled" -ForegroundColor Green
            $jarFound = $true
            break
        }
    }
    if (-not $jarFound) {
        Write-Host "  WARNING: scrcpy-server not found -- place it manually in $dist/" -ForegroundColor Yellow
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

    # ── fusion-viewer.exe + FFmpeg DLL（自研融合窗口客户端运行时） ──
    if ($FfmpegDir -eq "") {
        $FfmpegDir = if (Test-Path "./vendor/scrcpy-win64") {
            "./vendor/scrcpy-win64"
        } elseif ($env:SCRCPY_HOME -and (Test-Path "$env:SCRCPY_HOME/avcodec-62.dll")) {
            $env:SCRCPY_HOME
        } else {
            ""
        }
    }
    $viewerExe = "target/$config/fusion-viewer.exe"
    if (Test-Path $viewerExe) {
        Copy-Item $viewerExe "$dist/"
    } else {
        Write-Host "  WARNING: fusion-viewer.exe not found at $viewerExe" -ForegroundColor Red
    }
    $ffmpegFound = $false
    if ($FfmpegDir -ne "" -and (Test-Path "$FfmpegDir/avcodec-62.dll")) {
        foreach ($f in @("avcodec-62.dll", "avutil-60.dll", "swresample-6.dll")) {
            if (Test-Path "$FfmpegDir/$f") {
                Copy-Item "$FfmpegDir/$f" "$dist/"
            }
        }
        Write-Host "  FFmpeg 运行时 from $FfmpegDir" -ForegroundColor Green
        $ffmpegFound = $true
    }
    if (-not $ffmpegFound) {
        Write-Host "  WARNING: FFmpeg DLL 未找到 -- 融合模式（应用窗口）不可用；可传 -FfmpegDir <目录> 或设置 SCRCPY_HOME" -ForegroundColor Yellow
    }

    # ── 验证 ──
    Write-Host "`n$dist 内容：" -ForegroundColor Cyan
    Get-ChildItem $dist | Select-Object Name, Length | Format-Table -AutoSize

    Write-Host "`n===== Release package ready at $dist =====" -ForegroundColor Green
    Write-Host "  daemon.exe  — 后台服务（双击运行即可）"
    Write-Host "  sync-ui.exe — 桌面 GUI（可选）"
    if ($adbFound) { Write-Host "  adb.exe     — 已捆绑（含 DLL），无需预装 ADB" }
    if ($jarFound) { Write-Host "  scrcpy-server — scrcpy 服务端" }
    if ($ffmpegFound) { Write-Host "  fusion-viewer.exe — 自研融合窗口客户端（+ FFmpeg DLL）" }
    Write-Host "`n使用方法：直接双击 daemon.exe 即可启动全部功能" -ForegroundColor Green
} else {
    Write-Host "`n===== Debug build done =====" -ForegroundColor Green
    Write-Host "  daemon: target\$config\daemon.exe"
    Write-Host "  UI:     target\$config\sync-ui.exe"
}
