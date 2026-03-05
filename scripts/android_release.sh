#!/usr/bin/env bash
# Build Pianeer Android APK with injected Java MIDI helper classes.
#
# Steps:
#   1. Compile MidiHelper.java + MidiQueue.java → .class files
#   2. Convert to DEX with d8
#   3. cargo apk build --release
#   4. Inject classes2.dex into the APK
#   5. Zipalign + re-sign with apksigner
#   6. Install on device via adb
#
# Prerequisites (adjust paths if needed):
#   ANDROID_SDK_ROOT  (or set SDK below)
#   ANDROID_NDK_ROOT  (or set NDK below)
#   CARGO_HOME        (large artifact cache on fast drive)
#   Keystore at KEYSTORE_PATH with alias "pianeer" pass "pianeer123"

set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

# ── Paths ────────────────────────────────────────────────────────────────────
SDK=${ANDROID_SDK_ROOT:-/run/media/forain/samsung970pro512/android-sdk}
NDK=${ANDROID_NDK_ROOT:-/run/media/forain/samsung970pro512/android-ndk-r29}
CARGO_HOME_DIR=${CARGO_HOME:-/run/media/forain/samsung970pro512/.cargo}
KEYSTORE=${KEYSTORE_PATH:-/run/media/forain/samsung970pro512/pianeer-release.jks}

BT="$SDK/build-tools/35.0.1"
ANDROID_JAR="$SDK/platforms/android-35/android.jar"
APK="target/release/apk/pianeer-android.apk"

JAVA_SRC="android/java"
WORK_DIR="/tmp/pianeer_build_$$"
mkdir -p "$WORK_DIR/classes" "$WORK_DIR/dex"

# ── 1. Compile Java ───────────────────────────────────────────────────────────
echo "==> Compiling Java helpers..."
javac \
  -source 8 -target 8 \
  -cp "$ANDROID_JAR" \
  -d "$WORK_DIR/classes" \
  "$JAVA_SRC/com/pianeer/app/MidiHelper.java" \
  "$JAVA_SRC/com/pianeer/app/MidiQueue.java"

# ── 2. Convert to DEX ────────────────────────────────────────────────────────
echo "==> Converting to DEX..."
"$BT/d8" \
  --lib "$ANDROID_JAR" \
  --output "$WORK_DIR/dex" \
  "$WORK_DIR/classes/com/pianeer/app/MidiHelper.class" \
  "$WORK_DIR/classes/com/pianeer/app/MidiQueue.class"

# d8 outputs classes.dex; keep that name — cargo-apk produces no classes.dex
# (pure native activity), so there is no primary DEX to collide with, and
# Android requires classes.dex to exist before it will load classes2.dex.

# ── 3. cargo apk build ───────────────────────────────────────────────────────
echo "==> Building APK with cargo apk..."

NDK_SYSROOT="$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android"

ANDROID_SDK_ROOT="$SDK" \
ANDROID_NDK_ROOT="$NDK" \
CARGO_HOME="$CARGO_HOME_DIR" \
PATH="/home/forain/.cargo/bin:$CARGO_HOME_DIR/bin:/usr/bin:/usr/local/bin:$PATH" \
RUSTFLAGS="-C link-arg=$NDK_SYSROOT/libc++_static.a -C link-arg=$NDK_SYSROOT/libc++abi.a" \
  cargo apk build --release -p pianeer-android

# ── 4. Inject classes2.dex ───────────────────────────────────────────────────
echo "==> Injecting classes.dex..."
cp "$APK" "$WORK_DIR/pianeer.apk"
(cd "$WORK_DIR/dex" && zip "$WORK_DIR/pianeer.apk" classes.dex)

# ── 5. Zipalign + re-sign ────────────────────────────────────────────────────
echo "==> Zipaligning..."
"$BT/zipalign" -f 4 "$WORK_DIR/pianeer.apk" "$WORK_DIR/pianeer-aligned.apk"

echo "==> Signing..."
"$BT/apksigner" sign \
  --ks "$KEYSTORE" \
  --ks-key-alias pianeer \
  --ks-pass pass:pianeer123 \
  --out "$WORK_DIR/pianeer-signed.apk" \
  "$WORK_DIR/pianeer-aligned.apk"

cp "$WORK_DIR/pianeer-signed.apk" "$APK"
rm -rf "$WORK_DIR"

echo "==> APK ready: $APK"

# ── 6. Install ───────────────────────────────────────────────────────────────
echo "==> Installing on device..."
"$SDK/platform-tools/adb" devices
"$SDK/platform-tools/adb" install --no-incremental -r "$APK" || \
  "$SDK/platform-tools/adb" -s "$("$SDK/platform-tools/adb" devices | grep -v 'List\|^$' | head -1 | cut -f1)" install --no-incremental -r "$APK"
echo "==> Done."
