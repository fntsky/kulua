# Build kulua-server.jar (self-made Android server)
# Flow: protoc (generate protobuf Java) -> javac (classpath=android.jar + protobuf-javalite)
#       -> d8 (dex, bundling javalite runtime for a self-contained jar) -> package jar
#
# Prerequisites: Android SDK (d8.bat & android.jar), protoc (PATH or $Protoc),
#                JDK 11+ (javac; d8 uses JAVA_HOME)
# Usage: powershell -File build.ps1 [-SdkRoot D:\application\sdk] [-Api 35]
#
# NOTE: keep comments ASCII-only -- PowerShell 5.1 parses this file with the
# system ANSI codepage when there is no UTF-8 BOM, and non-ASCII comments can
# swallow following lines. Use English comments.

param(
    [string]$SdkRoot = "D:\application\sdk",
    [int]$Api = 35,
    # javac needs JDK 11+ (lambda / --release 11); PATH may point to JDK 8
    [string]$Javac = "C:\Program Files\Eclipse Adoptium\jdk-21.0.4.7-hotspot\bin\javac.exe",
    # d8 needs Java 11+ (d8.bat uses JAVA_HOME)
    [string]$JavaHome = "C:\Program Files\Eclipse Adoptium\jdk-21.0.4.7-hotspot",
    # protobuf compiler (proto/direct.proto -> Java)
    [string]$Protoc = "protoc",
    # protobuf-javalite runtime jar (phone-side lite runtime, dex-ified with our classes)
    [string]$JavaliteJar = ""
)

$ErrorActionPreference = "Stop"
$env:JAVA_HOME = $JavaHome
$root = Split-Path $PSScriptRoot -Parent
$project = Join-Path $root "kulua-server"
$out = Join-Path $project "build"
$classes = Join-Path $out "classes"
$genJava = Join-Path $out "java-generated"
$androidJar = Join-Path $SdkRoot "platforms\android-$Api\android.jar"
$d8 = Join-Path $SdkRoot "build-tools\35.0.0\d8.bat"
$proto = Join-Path $root "proto\direct.proto"
if ([string]::IsNullOrEmpty($JavaliteJar)) {
    $JavaliteJar = Join-Path $project "lib\protobuf-javalite-3.21.12.jar"
}

Write-Host "===== kulua-server build =====" -ForegroundColor Cyan

if (-not (Test-Path $androidJar)) {
    Write-Host "ERROR: android.jar not found: $androidJar" -ForegroundColor Red
    exit 1
}
if (-not (Test-Path $d8)) {
    Write-Host "ERROR: d8 not found: $d8" -ForegroundColor Red
    exit 1
}
if (-not (Test-Path $JavaliteJar)) {
    Write-Host "ERROR: protobuf-javalite jar not found: $JavaliteJar" -ForegroundColor Red
    Write-Host "  Download protobuf-javalite-3.21.12.jar into kulua-server/lib (or pass -JavaliteJar)" -ForegroundColor Red
    exit 1
}

# clean stale outputs (keep jar idempotent across source removals)
if (Test-Path $classes) { Remove-Item $classes -Recurse -Force }
if (Test-Path $genJava) { Remove-Item $genJava -Recurse -Force }
New-Item -ItemType Directory -Force -Path $classes | Out-Null
New-Item -ItemType Directory -Force -Path $genJava | Out-Null

# -- 0. protoc: generate protobuf Java (lite runtime, multi-file) --
Write-Host "[0/4] protoc..." -ForegroundColor Yellow
& $Protoc "--proto_path=$root\proto" "--java_out=lite:$genJava" $proto
if ($LASTEXITCODE -ne 0) {
    Write-Host "ERROR: protoc failed (is protoc on PATH? e.g. C:\Users\<user>\anaconda3\Library\bin\protoc.exe)" -ForegroundColor Red
    exit 1
}

# -- 1. javac --
Write-Host "[1/4] javac..." -ForegroundColor Yellow
$sources = Get-ChildItem -Recurse (Join-Path $project "src") -Filter "*.java" | ForEach-Object { $_.FullName }
$genSources = Get-ChildItem -Recurse $genJava -Filter "*.java" | ForEach-Object { $_.FullName }
# android.jar on classpath (Java 8+ syntax is desugared by d8, not bootclasspath)
$cp = "$androidJar;$JavaliteJar"
# stubs/ directory contains compile-time stand-ins for hidden classes (e.g.
# IContentProvider). If javac sees the .java sources directly it compiles them
# into classes/ (hence into dex, conflicting with the device's real classes).
# So compile stubs to build/stub-classes/ first, reference via classpath only.
$stubSources = Get-ChildItem -Recurse (Join-Path $project "stubs") -Filter "*.java" | ForEach-Object { $_.FullName }
if ($stubSources) {
    $stubClasses = Join-Path $out "stub-classes"
    New-Item -ItemType Directory -Force -Path $stubClasses | Out-Null
    & $Javac -encoding UTF-8 --release 11 -classpath $androidJar -d $stubClasses $stubSources
    if ($LASTEXITCODE -ne 0) { exit 1 }
    $cp = "$androidJar;$stubClasses;$JavaliteJar"
}
& $Javac -encoding UTF-8 --release 11 -classpath $cp -d $classes ($sources + $genSources)
if ($LASTEXITCODE -ne 0) { exit 1 }

# -- 2. d8 (dex-ify javalite runtime too, self-contained jar) --
Write-Host "[2/4] d8..." -ForegroundColor Yellow
# Bundle compiled classes into a jar first, then feed jars to d8: generated code
# (java_multiple_files + lite) yields many inner classes; passing them one by one
# on the command line exceeds Windows' command-line length limit.
$classesJar = Join-Path $out "classes.jar"
if (Test-Path $classesJar) { Remove-Item $classesJar }
Push-Location $classes
& jar cf $classesJar .
Pop-Location
& $d8 --lib $androidJar --release --output $out $classesJar $JavaliteJar
if ($LASTEXITCODE -ne 0) { exit 1 }

# -- 3. package jar (classes.dex at jar root, loaded by app_process) --
Write-Host "[3/4] jar..." -ForegroundColor Yellow
Push-Location $out
& jar cf (Join-Path $out "kulua-server.jar") classes.dex
Pop-Location

if (Test-Path (Join-Path $out "kulua-server.jar")) {
    Write-Host "`nOK: $out\kulua-server.jar" -ForegroundColor Green
} else {
    Write-Host "ERROR: jar packaging failed" -ForegroundColor Red
    exit 1
}
