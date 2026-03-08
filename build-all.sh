#!/usr/bin/env bash
# Build release binaries for all supported platforms.
#
# Tools required:
#   cross       — cargo install cross        (+ Docker for Linux arm64)
#   zig         — pacman -S zig  or  snap install zig --classic
#   cargo-zigbuild — cargo install cargo-zigbuild  (Windows targets)
#   cargo-apk   — cargo install cargo-apk    (Android)
#   trunk       — cargo install trunk        (WASM bundle)
#
# macOS binaries (x86_64, arm64, universal) can only be built on macOS or via CI.

set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
OUT="$REPO/dist-release"

NDK_ROOT="${ANDROID_NDK_ROOT:-/run/media/forain/samsung970pro512/android-ndk-r29}"
ANDROID_SDK="${ANDROID_SDK_ROOT:-/run/media/forain/samsung970pro512/android-sdk}"
CARGO_BIN="${CARGO_HOME:-/run/media/forain/samsung970pro512/.cargo}/bin"
KEYSTORE="${KEYSTORE:-/run/media/forain/samsung970pro512/pianeer-release.jks}"

mkdir -p "$OUT"
cd "$REPO"

step() { echo; echo "===> $*"; }

require() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: '$1' not found. $2"
        exit 1
    fi
}

# ── Tool checks ───────────────────────────────────────────────────────────────
require trunk       "Install with: cargo install trunk"
require cross       "Install with: cargo install cross  (also needs Docker)"
require zig         "Install with: pacman -S zig  or  snap install zig --classic"
require cargo-zigbuild "Install with: cargo install cargo-zigbuild"
"$CARGO_BIN/cargo-apk" --version &>/dev/null || { echo "ERROR: cargo-apk not found. Install with: cargo install cargo-apk"; exit 1; }

ndk_lib() { echo "$NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/$1"; }

# ── WASM bundle ───────────────────────────────────────────────────────────────
step "WASM bundle"
cd web-wasm && trunk build --release && cd "$REPO"

# ── Linux x86_64 (native) ────────────────────────────────────────────────────
step "Linux x86_64"
cargo build --release -p pianeer
cp target/release/pianeer "$OUT/pianeer-linux-amd64"

# ── Linux arm64 (cross via Docker) ───────────────────────────────────────────
step "Linux arm64"
cross build --release -p pianeer --target aarch64-unknown-linux-gnu
cp target/aarch64-unknown-linux-gnu/release/pianeer "$OUT/pianeer-linux-arm64"

# ── Windows x86_64 (cargo-zigbuild) ──────────────────────────────────────────
step "Windows x86_64"
cargo zigbuild --release -p pianeer --target x86_64-pc-windows-gnullvm
cp target/x86_64-pc-windows-gnullvm/release/pianeer.exe "$OUT/pianeer-windows-amd64.exe"

# ── Windows arm64 (cargo-zigbuild) ───────────────────────────────────────────
step "Windows arm64"
cargo zigbuild --release -p pianeer --target aarch64-pc-windows-gnullvm
cp target/aarch64-pc-windows-gnullvm/release/pianeer.exe "$OUT/pianeer-windows-arm64.exe"

# ── Android arm64 ────────────────────────────────────────────────────────────
step "Android arm64"
ANDROID_NDK_ROOT="$NDK_ROOT" \
ANDROID_SDK_ROOT="$ANDROID_SDK" \
JAVA_TOOL_OPTIONS="-Dkeystore.pkcs12.legacy" \
RUSTFLAGS="-C link-arg=$(ndk_lib aarch64-linux-android)/libc++_static.a -C link-arg=$(ndk_lib aarch64-linux-android)/libc++abi.a" \
"$CARGO_BIN/cargo-apk" apk build --release -p pianeer-android --target aarch64-linux-android
cp target/release/apk/pianeer-android.apk "$OUT/pianeer-android-arm64.apk"

# ── Android x86_64 ───────────────────────────────────────────────────────────
step "Android x86_64"
ANDROID_NDK_ROOT="$NDK_ROOT" \
ANDROID_SDK_ROOT="$ANDROID_SDK" \
JAVA_TOOL_OPTIONS="-Dkeystore.pkcs12.legacy" \
RUSTFLAGS="-C link-arg=$(ndk_lib x86_64-linux-android)/libc++_static.a -C link-arg=$(ndk_lib x86_64-linux-android)/libc++abi.a" \
"$CARGO_BIN/cargo-apk" apk build --release -p pianeer-android --target x86_64-linux-android
cp target/release/apk/pianeer-android.apk "$OUT/pianeer-android-x86_64.apk"

# ── macOS (cross-compile via cargo-zigbuild) ──────────────────────────────────
# Requires:
#   1. zig + cargo-zigbuild installed (see Windows steps above)
#   2. Rust macOS targets: rustup target add x86_64-apple-darwin aarch64-apple-darwin
#   3. Optionally set SDKROOT to a real macOS SDK if Zig's bundled headers are insufficient:
#        On your Mac: tar -czf MacOSX.sdk.tar.gz -C "$(dirname $(xcrun --show-sdk-path))" "$(basename $(xcrun --show-sdk-path))"
#        Transfer to Linux and set: export SDKROOT=/path/to/MacOSX.sdk
if command -v zig &>/dev/null && command -v cargo-zigbuild &>/dev/null; then
    step "macOS x86_64"
    cargo zigbuild --release -p pianeer --target x86_64-apple-darwin
    cp target/x86_64-apple-darwin/release/pianeer "$OUT/pianeer-macos-x86_64"

    step "macOS arm64"
    cargo zigbuild --release -p pianeer --target aarch64-apple-darwin
    cp target/aarch64-apple-darwin/release/pianeer "$OUT/pianeer-macos-arm64"

    step "macOS universal"
    lipo -create \
        -output "$OUT/pianeer-macos-universal" \
        "$OUT/pianeer-macos-x86_64" \
        "$OUT/pianeer-macos-arm64"
else
    echo "SKIP: macOS targets (zig/cargo-zigbuild not found)"
fi

# ── Summary ───────────────────────────────────────────────────────────────────
step "Done — artifacts in $OUT:"
ls -lh "$OUT"
