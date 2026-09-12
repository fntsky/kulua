#!/usr/bin/env bash
# Build kulua-server.jar (self-made Android server) -- Linux/macOS version.
# Same flow as build.ps1:
#   protoc (generate protobuf Java) -> javac (classpath=android.jar + protobuf-javalite)
#   -> d8 (dex, bundling javalite runtime for a self-contained jar) -> package jar
#
# Prerequisites: Android SDK (d8 & android.jar), protoc (PATH or $PROTOC),
#                JDK 11+ (javac; d8 uses JAVA_HOME)
# Usage: ./build.sh [SDK_ROOT] [API]
#   or:  ANDROID_HOME=/path/to/sdk API=35 ./build.sh
set -euo pipefail

SDK_ROOT="${SDK_ROOT:-${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}}"
API="${API:-35}"
JAVAC="${JAVAC:-javac}"
PROTOC="${PROTOC:-protoc}"
# protobuf-javalite runtime jar (phone-side lite runtime, dex-ified with our classes)
JAVALITE_JAR="${JAVALITE_JAR:-}"
if [[ -n "${JAVA_HOME:-}" ]]; then
    export PATH="$JAVA_HOME/bin:$PATH"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT="$ROOT/kulua-server"
OUT="$PROJECT/build"
CLASSES="$OUT/classes"
GEN_JAVA="$OUT/java-generated"
STUB_CLASSES="$OUT/stub-classes"
ANDROID_JAR="$SDK_ROOT/platforms/android-$API/android.jar"
D8="$SDK_ROOT/build-tools/35.0.0/d8"
PROTO_FILE="$ROOT/proto/direct.proto"
if [[ -z "$JAVALITE_JAR" ]]; then
    JAVALITE_JAR="$PROJECT/lib/protobuf-javalite-3.21.12.jar"
fi

echo "===== kulua-server build ====="

if [[ -z "$SDK_ROOT" ]]; then
    echo "ERROR: Android SDK not found. Set ANDROID_HOME (or pass SDK_ROOT)." >&2
    exit 1
fi
[[ -f "$ANDROID_JAR" ]] || { echo "ERROR: android.jar not found: $ANDROID_JAR" >&2; exit 1; }
[[ -f "$D8" ]] || { echo "ERROR: d8 not found: $D8" >&2; exit 1; }
[[ -f "$JAVALITE_JAR" ]] || {
    echo "ERROR: protobuf-javalite jar not found: $JAVALITE_JAR" >&2
    echo "  Download protobuf-javalite-3.21.12.jar into kulua-server/lib (or set JAVALITE_JAR)" >&2
    exit 1
}

# clean stale outputs (keep jar idempotent across source removals)
rm -rf "$CLASSES" "$GEN_JAVA" "$STUB_CLASSES"
mkdir -p "$CLASSES" "$GEN_JAVA"

# -- 0. protoc: generate protobuf Java (lite runtime, multi-file) --
echo "[0/4] protoc..."
"$PROTOC" "--proto_path=$ROOT/proto" "--java_out=lite:$GEN_JAVA" "$PROTO_FILE"

# -- 1. javac --
echo "[1/4] javac..."
# shellcheck disable=SC2046
mapfile -t SOURCES < <(find "$PROJECT/src" -name '*.java')
mapfile -t GEN_SOURCES < <(find "$GEN_JAVA" -name '*.java')
# android.jar on classpath (Java 8+ syntax is desugared by d8, not bootclasspath)
CP="$ANDROID_JAR:$JAVALITE_JAR"
# stubs/ directory contains compile-time stand-ins for hidden classes (e.g.
# IContentProvider). Compile stubs to build/stub-classes/ first, reference via
# classpath only -- never into classes/ (would conflict with device's real classes).
mapfile -t STUB_SOURCES < <(find "$PROJECT/stubs" -name '*.java' 2>/dev/null || true)
if ((${#STUB_SOURCES[@]} > 0)); then
    mkdir -p "$STUB_CLASSES"
    "$JAVAC" -encoding UTF-8 --release 11 -classpath "$ANDROID_JAR" -d "$STUB_CLASSES" "${STUB_SOURCES[@]}"
    CP="$ANDROID_JAR:$STUB_CLASSES:$JAVALITE_JAR"
fi
"$JAVAC" -encoding UTF-8 --release 11 -classpath "$CP" -d "$CLASSES" "${SOURCES[@]}" "${GEN_SOURCES[@]}"

# -- 2. d8 (dex-ify javalite runtime too, self-contained jar) --
echo "[2/4] d8..."
# Bundle compiled classes into a jar first, then feed jars to d8: generated code
# (java_multiple_files + lite) yields many inner classes; passing them one by one
# on the command line exceeds the command-line length limit (same as build.ps1).
CLASSES_JAR="$OUT/classes.jar"
rm -f "$CLASSES_JAR"
( cd "$CLASSES" && jar cf "$CLASSES_JAR" . )
"$D8" --lib "$ANDROID_JAR" --release --output "$OUT" "$CLASSES_JAR" "$JAVALITE_JAR"

# -- 3. package jar (classes.dex at jar root, loaded by app_process) --
echo "[3/4] jar..."
( cd "$OUT" && jar cf "$OUT/kulua-server.jar" classes.dex )

if [[ -f "$OUT/kulua-server.jar" ]]; then
    echo ""
    echo "OK: $OUT/kulua-server.jar"
else
    echo "ERROR: jar packaging failed" >&2
    exit 1
fi