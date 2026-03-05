#!/usr/bin/env bash
# Build Pianeer Android APK and install on device.
#
# MidiOpener.java is compiled to DEX by android/build.rs at cargo build time
# and embedded in the .so via include_bytes!.  No manual DEX injection needed.
#
# Prerequisites (adjust paths if needed):
#   ANDROID_SDK_ROOT  (or set SDK below)
#   ANDROID_NDK_ROOT  (or set NDK below)
#   CARGO_HOME        (large artifact cache on fast drive)
#   Keystore configured in android/Cargo.toml [package.metadata.android.signing.release]

set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

# ── Paths ────────────────────────────────────────────────────────────────────
SDK=${ANDROID_SDK_ROOT:-/run/media/forain/samsung970pro512/android-sdk}
NDK=${ANDROID_NDK_ROOT:-/run/media/forain/samsung970pro512/android-ndk-r29}
CARGO_HOME_DIR=${CARGO_HOME:-/run/media/forain/samsung970pro512/.cargo}

APK="target/release/apk/pianeer-android.apk"

NDK_SYSROOT="$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android"

# ── Build APK (build.rs compiles MidiOpener.java → DEX, embeds in .so) ──────
echo "==> Building APK with cargo apk..."
ANDROID_SDK_ROOT="$SDK" \
ANDROID_NDK_ROOT="$NDK" \
CARGO_HOME="$CARGO_HOME_DIR" \
PATH="/home/forain/.cargo/bin:$CARGO_HOME_DIR/bin:/usr/bin:/usr/local/bin:$PATH" \
RUSTFLAGS="-C link-arg=$NDK_SYSROOT/libc++_static.a -C link-arg=$NDK_SYSROOT/libc++abi.a" \
  cargo apk build --release -p pianeer-android

echo "==> APK ready: $APK"

# ── Install ───────────────────────────────────────────────────────────────────
echo "==> Installing on device..."
"$SDK/platform-tools/adb" devices
"$SDK/platform-tools/adb" install --no-incremental -r "$APK" || \
  "$SDK/platform-tools/adb" -s "$("$SDK/platform-tools/adb" devices | grep -v 'List\|^$' | head -1 | cut -f1)" install --no-incremental -r "$APK"
echo "==> Done."
