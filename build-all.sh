#!/usr/bin/env bash
# Build release binaries for all supported platforms. No Docker required.
#
# Tools required:
#   zig            — pacman -S zig
#   cargo-zigbuild — cargo install cargo-zigbuild   (all cross-targets)
#   cargo-apk      — cargo install cargo-apk         (Android)
#   trunk          — cargo install trunk             (WASM bundle)
#
# Linux arm64 also needs an arm64 sysroot with JACK+ALSA libs:
#   export ARM64_SYSROOT=/path/to/aarch64-sysroot
#   (bootstrap one with: mkarchroot or download Arch ARM packages)
#
# macOS targets need the macOS SDK:
#   On your Mac: tar -czf MacOSX.sdk.tar.gz \
#     -C "$(dirname $(xcrun --show-sdk-path))" \
#     "$(basename $(xcrun --show-sdk-path))"
#   Transfer here and: export SDKROOT=/path/to/MacOSX.sdk

set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
OUT="$REPO/dist-release"

NDK_ROOT="${ANDROID_NDK_ROOT:-/run/media/forain/samsung970pro512/android-ndk-r29}"
ANDROID_SDK="${ANDROID_SDK_ROOT:-/run/media/forain/samsung970pro512/android-sdk}"
CARGO_BIN="${CARGO_HOME:-/run/media/forain/samsung970pro512/.cargo}/bin"

mkdir -p "$OUT"
cd "$REPO"

step()  { echo; echo "===> $*"; }
skip()  { echo "SKIP: $*"; }
ndk_lib() { echo "$NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/$1"; }

require() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: '$1' not found. $2"; exit 1
    fi
}

# ── Tool checks ───────────────────────────────────────────────────────────────
require trunk          "cargo install trunk"
require zig            "pacman -S zig"
require cargo-zigbuild "cargo install cargo-zigbuild"
"$CARGO_BIN/cargo-apk" --version &>/dev/null \
    || { echo "ERROR: cargo-apk not found — cargo install cargo-apk"; exit 1; }

# ── WASM bundle ───────────────────────────────────────────────────────────────
step "WASM bundle"
cd web-wasm && trunk build --release && cd "$REPO"

# ── Linux x86_64 (native) ────────────────────────────────────────────────────
step "Linux x86_64"
cargo build --release -p pianeer
cp target/release/pianeer "$OUT/pianeer-linux-amd64"

# ── Linux arm64 (cargo-zigbuild + arm64 sysroot) ─────────────────────────────
step "Linux arm64"
if [ -n "${ARM64_SYSROOT:-}" ]; then
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_SYSROOT_DIR="$ARM64_SYSROOT" \
    PKG_CONFIG_PATH="$ARM64_SYSROOT/usr/lib/pkgconfig:$ARM64_SYSROOT/usr/share/pkgconfig" \
    cargo zigbuild --release -p pianeer --target aarch64-unknown-linux-gnu
    cp target/aarch64-unknown-linux-gnu/release/pianeer "$OUT/pianeer-linux-arm64"
else
    skip "Linux arm64 — set ARM64_SYSROOT=/path/to/aarch64-sysroot"
    echo "       The sysroot needs arm64 JACK and ALSA dev libraries."
    echo "       Quick setup: pacman -S qemu-user-static aarch64-linux-gnu-gcc"
    echo "       then bootstrap: mkarchroot \$ARM64_SYSROOT base libjack libasound2"
fi

# ── Windows x86_64 ───────────────────────────────────────────────────────────
step "Windows x86_64"
cargo zigbuild --release -p pianeer --target x86_64-pc-windows-gnullvm
cp target/x86_64-pc-windows-gnullvm/release/pianeer.exe "$OUT/pianeer-windows-amd64.exe"

# ── Windows arm64 ────────────────────────────────────────────────────────────
step "Windows arm64"
cargo zigbuild --release -p pianeer --target aarch64-pc-windows-gnullvm
cp target/aarch64-pc-windows-gnullvm/release/pianeer.exe "$OUT/pianeer-windows-arm64.exe"

# ── macOS x86_64 + arm64 + universal ─────────────────────────────────────────
step "macOS"
if [ -n "${SDKROOT:-}" ]; then
    cargo zigbuild --release -p pianeer --target x86_64-apple-darwin
    cp target/x86_64-apple-darwin/release/pianeer "$OUT/pianeer-macos-x86_64"

    cargo zigbuild --release -p pianeer --target aarch64-apple-darwin
    cp target/aarch64-apple-darwin/release/pianeer "$OUT/pianeer-macos-arm64"

    lipo -create \
        -output "$OUT/pianeer-macos-universal" \
        "$OUT/pianeer-macos-x86_64" \
        "$OUT/pianeer-macos-arm64"
else
    skip "macOS — set SDKROOT=/path/to/MacOSX.sdk"
    echo "       Extract on your Mac: tar -czf MacOSX.sdk.tar.gz \\"
    echo "         -C \"\$(dirname \$(xcrun --show-sdk-path))\" \\"
    echo "         \"\$(basename \$(xcrun --show-sdk-path))\""
fi

# ── Android arm64 ────────────────────────────────────────────────────────────
step "Android arm64"
ANDROID_NDK_ROOT="$NDK_ROOT" \
ANDROID_SDK_ROOT="$ANDROID_SDK" \
JAVA_TOOL_OPTIONS="-Dkeystore.pkcs12.legacy" \
RUSTFLAGS="-C link-arg=$(ndk_lib aarch64-linux-android)/libc++_static.a \
           -C link-arg=$(ndk_lib aarch64-linux-android)/libc++abi.a" \
"$CARGO_BIN/cargo-apk" apk build --release -p pianeer-android --target aarch64-linux-android
cp target/release/apk/pianeer-android.apk "$OUT/pianeer-android-arm64.apk"

# ── Android x86_64 ───────────────────────────────────────────────────────────
step "Android x86_64"
ANDROID_NDK_ROOT="$NDK_ROOT" \
ANDROID_SDK_ROOT="$ANDROID_SDK" \
JAVA_TOOL_OPTIONS="-Dkeystore.pkcs12.legacy" \
RUSTFLAGS="-C link-arg=$(ndk_lib x86_64-linux-android)/libc++_static.a \
           -C link-arg=$(ndk_lib x86_64-linux-android)/libc++abi.a" \
"$CARGO_BIN/cargo-apk" apk build --release -p pianeer-android --target x86_64-linux-android
cp target/release/apk/pianeer-android.apk "$OUT/pianeer-android-x86_64.apk"

# ── Summary ───────────────────────────────────────────────────────────────────
step "Done — artifacts in $OUT:"
ls -lh "$OUT"
