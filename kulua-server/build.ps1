# 构建 kulua-server.jar（自研 Android server）
# 流程：javac（bootclasspath=android.jar）→ d8（dex）→ jar 打包
#
# 前置：Android SDK（d8.bat 与 android.jar）
# 用法：powershell -File build.ps1 [-SdkRoot D:\application\sdk] [-Api 35]

param(
    [string]$SdkRoot = "D:\application\sdk",
    [int]$Api = 35,
    # javac 需要 JDK 11+（lambda/--release 11）；PowerShell PATH 里可能是 JDK 8
    [string]$Javac = "C:\Program Files\Eclipse Adoptium\jdk-21.0.4.7-hotspot\bin\javac.exe",
    # d8 需要 Java 11+（JAVA_HOME 控制 d8.bat 使用的 java）
    [string]$JavaHome = "C:\Program Files\Eclipse Adoptium\jdk-21.0.4.7-hotspot"
)

$ErrorActionPreference = "Stop"
$env:JAVA_HOME = $JavaHome
$root = Split-Path $PSScriptRoot -Parent
$project = Join-Path $root "kulua-server"
$out = Join-Path $project "build"
$classes = Join-Path $out "classes"
$androidJar = Join-Path $SdkRoot "platforms\android-$Api\android.jar"
$d8 = Join-Path $SdkRoot "build-tools\35.0.0\d8.bat"

Write-Host "===== kulua-server build =====" -ForegroundColor Cyan

if (-not (Test-Path $androidJar)) {
    Write-Host "ERROR: android.jar not found: $androidJar" -ForegroundColor Red
    exit 1
}
if (-not (Test-Path $d8)) {
    Write-Host "ERROR: d8 not found: $d8" -ForegroundColor Red
    exit 1
}

New-Item -ItemType Directory -Force -Path $classes | Out-Null

# ── 1. javac ──
Write-Host "[1/3] javac..." -ForegroundColor Yellow
$sources = Get-ChildItem -Recurse (Join-Path $project "src") -Filter "*.java" | ForEach-Object { $_.FullName }
# android.jar 放 classpath（lambda 等 Java 8+ 语法由 d8 desugar，不用作 bootclasspath）
& $Javac -encoding UTF-8 --release 11 -classpath $androidJar -d $classes $sources
if ($LASTEXITCODE -ne 0) { exit 1 }

# ── 2. d8 ──
Write-Host "[2/3] d8..." -ForegroundColor Yellow
& $d8 --lib $androidJar --release --output $out (Get-ChildItem -Recurse $classes -Filter "*.class" | ForEach-Object { $_.FullName })
if ($LASTEXITCODE -ne 0) { exit 1 }

# ── 3. jar 打包（classes.dex 放 jar 根，app_process 直接加载）──
Write-Host "[3/3] jar..." -ForegroundColor Yellow
Push-Location $out
& jar cf (Join-Path $out "kulua-server.jar") classes.dex
Pop-Location

if (Test-Path (Join-Path $out "kulua-server.jar")) {
    Write-Host "`nOK: $out\kulua-server.jar" -ForegroundColor Green
} else {
    Write-Host "ERROR: jar 打包失败" -ForegroundColor Red
    exit 1
}
