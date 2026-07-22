param(
    [switch]$Release
)

$config = if ($Release) { "release" } else { "debug" }
$targetFlag = if ($Release) { "--release" } else { "" }
$ErrorActionPreference = "Stop"

Write-Host "===== Sync Workspace Build =====" -ForegroundColor Cyan

# ── 1. daemon ──
Write-Host "`n[1/2] Building daemon..." -ForegroundColor Yellow
cargo build --package daemon $targetFlag
if ($LASTEXITCODE -ne 0) { exit 1 }

# ── 2. UI ──
Write-Host "`n[2/2] Building UI (exe, no installer)..." -ForegroundColor Yellow
Push-Location ui
npm install --silent
if ($Release) {
    npm run tauri build
} else {
    # CI 环境变量会干扰 tauri CLI，在子进程中清除
    powershell -Command "Remove-Item env:CI -ErrorAction Ignore; npm run tauri -- build --debug --no-bundle"
}
if ($LASTEXITCODE -ne 0) { Pop-Location; exit 1 }
Pop-Location

Write-Host "`n===== All done =====" -ForegroundColor Green
Write-Host "  daemon: target\$config\daemon.exe"
Write-Host "  UI:     target\$config\sync-ui.exe"
