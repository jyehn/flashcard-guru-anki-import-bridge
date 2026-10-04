#!/usr/bin/env bash
# Build the bridge as an Android JNI shared library (librust_import_core.so).
#
# Usage: scripts/build-android.sh [OUTPUT_JNILIBS_DIR]
#   Output: <OUTPUT_JNILIBS_DIR>/<abi>/librust_import_core.so
#   Default output: $REPO_ROOT/android/jniLibs
#
# Environment (all optional):
#   ANDROID_ABIS        space-separated ABIs, default "arm64-v8a".
#                       Known: arm64-v8a, x86_64, armeabi-v7a (only arm64-v8a is
#                       verified so far).
#   ANDROID_NDK_HOME    NDK root. Default: $ANDROID_HOME/ndk/$ANDROID_NDK_VERSION.
#   ANDROID_NDK_VERSION default 27.1.12297006.
#   ANDROID_API         minimum API level for the NDK clang wrapper, default 26.
#   RUST_TOOLCHAIN      rustup toolchain, default 1.92.0 (anki-rs' rust-toolchain.toml).
#   CARGO_TARGET_DIR    cargo target dir, default $REPO_ROOT/target.
#   PROTOC              protoc for rslib's prost build; default: protoc on PATH.
#
# What it does differently from the iOS build (scripts/build-xcframework.sh):
#   - `--no-default-features --features android`: rslib with rustls instead of
#     native-tls (no OpenSSL), plus the JNI layer in src/android_jni.rs.
#   - `cargo rustc --crate-type cdylib`: Cargo.toml keeps crate-type = staticlib
#     for iOS; the Android shared library is requested on the command line.
#   - 16 KB ELF segment alignment (Play requirement for apps targeting Android 15+;
#     NDK r27 still defaults to 4 KB), verified with llvm-readelf after the build.
#
# Layout requirement (same as iOS): ../anki-rs is the rslib checkout.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_ROOT="${1:-$REPO_ROOT/android/jniLibs}"
ABIS="${ANDROID_ABIS:-arm64-v8a}"
NDK_VERSION="${ANDROID_NDK_VERSION:-27.1.12297006}"
API="${ANDROID_API:-26}"
TOOLCHAIN="${RUST_TOOLCHAIN:-1.92.0}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
LIB_NAME="librust_import_core.so"
PAGE_FLAGS="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384"

die() { echo "ERROR: $*" >&2; exit 1; }

for dir in "${HOME:-/}/.cargo/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
  if [ -d "$dir" ]; then
    case ":$PATH:" in *":$dir:"*) ;; *) export PATH="$dir:$PATH" ;; esac
  fi
done
command -v cargo >/dev/null 2>&1 || die "cargo not found (https://rustup.rs/)"
command -v rustup >/dev/null 2>&1 || die "rustup not found"

[ -f "$REPO_ROOT/../anki-rs/rslib/Cargo.toml" ] || die "anki-rs not found at $REPO_ROOT/../anki-rs/"
[ -d "$REPO_ROOT/../anki-rs/ftl/core-repo/core" ] || die "anki-rs submodules missing (git submodule update --init --recursive)"

if [ -z "${PROTOC:-}" ]; then
  command -v protoc >/dev/null 2>&1 || die "protoc not found (brew install protobuf)"
  export PROTOC="$(command -v protoc)"
fi

if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
  ANDROID_NDK_HOME="$SDK/ndk/$NDK_VERSION"
fi
[ -d "$ANDROID_NDK_HOME" ] || die "NDK not found at $ANDROID_NDK_HOME"

case "$(uname -s)" in
  Darwin) HOST_TAG="darwin-x86_64" ;;   # NDK ships universal binaries under this name
  Linux)  HOST_TAG="linux-x86_64" ;;
  *) die "unsupported host $(uname -s)" ;;
esac
LLVM_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST_TAG/bin"
[ -x "$LLVM_BIN/llvm-readelf" ] || die "llvm-readelf not found in $LLVM_BIN"

echo "==> build-android.sh"
echo "    repo       = $REPO_ROOT"
echo "    abis       = $ABIS"
echo "    toolchain  = $TOOLCHAIN ($(rustc +"$TOOLCHAIN" --version))"
echo "    ndk        = $ANDROID_NDK_HOME (API $API)"
echo "    target dir = $TARGET_DIR"
echo "    output     = $OUT_ROOT"
echo "    PROTOC     = $PROTOC"

for ABI in $ABIS; do
  case "$ABI" in
    arm64-v8a)   TRIPLE="aarch64-linux-android";    CLANG_PREFIX="aarch64-linux-android" ;;
    x86_64)      TRIPLE="x86_64-linux-android";     CLANG_PREFIX="x86_64-linux-android" ;;
    armeabi-v7a) TRIPLE="armv7-linux-androideabi";  CLANG_PREFIX="armv7a-linux-androideabi" ;;
    *) die "unknown ABI $ABI" ;;
  esac

  if ! rustup target list --installed --toolchain "$TOOLCHAIN" | grep -qx "$TRIPLE"; then
    die "Rust target $TRIPLE missing: rustup target add $TRIPLE --toolchain $TOOLCHAIN"
  fi

  CLANG="$LLVM_BIN/${CLANG_PREFIX}${API}-clang"
  [ -x "$CLANG" ] || die "NDK clang wrapper not found: $CLANG"
  TRIPLE_UPPER="$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')"
  TRIPLE_UNDERSCORE="$(echo "$TRIPLE" | tr '-' '_')"

  echo "==> [$ABI] cargo rustc --target $TRIPLE --crate-type cdylib"
  (
    cd "$REPO_ROOT"
    export CARGO_TARGET_DIR="$TARGET_DIR"
    export "CARGO_TARGET_${TRIPLE_UPPER}_LINKER=$CLANG"
    export "CARGO_TARGET_${TRIPLE_UPPER}_RUSTFLAGS=$PAGE_FLAGS"
    export "CC_${TRIPLE_UNDERSCORE}=$CLANG"
    export "CXX_${TRIPLE_UNDERSCORE}=${CLANG}++"
    export "AR_${TRIPLE_UNDERSCORE}=$LLVM_BIN/llvm-ar"
    export "RANLIB_${TRIPLE_UNDERSCORE}=$LLVM_BIN/llvm-ranlib"
    cargo +"$TOOLCHAIN" rustc --release --lib --locked \
      --target "$TRIPLE" \
      --no-default-features --features android \
      --crate-type cdylib
  )

  BUILT="$TARGET_DIR/$TRIPLE/release/$LIB_NAME"
  [ -f "$BUILT" ] || die "build produced no $BUILT"

  OUT_DIR="$OUT_ROOT/$ABI"
  mkdir -p "$OUT_DIR"
  # The release profile already strips symbols; strip-unneeded is a no-op safety net
  # that keeps the dynamic symbol table (the JNI entry points) intact.
  "$LLVM_BIN/llvm-strip" --strip-unneeded -o "$OUT_DIR/$LIB_NAME" "$BUILT"

  echo "==> [$ABI] $OUT_DIR/$LIB_NAME"
  BYTES=$(wc -c < "$OUT_DIR/$LIB_NAME" | tr -d ' ')
  echo "    size: $BYTES bytes ($(awk "BEGIN { printf \"%.1f\", $BYTES / 1048576 }") MiB)"
  echo "    sha256: $(shasum -a 256 "$OUT_DIR/$LIB_NAME" | cut -d' ' -f1)"
  echo "    llvm-readelf -l (LOAD segments):"
  "$LLVM_BIN/llvm-readelf" -lW "$OUT_DIR/$LIB_NAME" | grep -E '^\s+LOAD' | sed 's/^/      /'
  LOAD_ALIGNS=$("$LLVM_BIN/llvm-readelf" -lW "$OUT_DIR/$LIB_NAME" | awk '$1 == "LOAD" { print $NF }')
  [ -n "$LOAD_ALIGNS" ] || die "[$ABI] no LOAD segments found"
  for ALIGN in $LOAD_ALIGNS; do
    [ $((ALIGN)) -ge 16384 ] || die "[$ABI] LOAD segment alignment $ALIGN is below 16 KB"
  done
  echo "    every LOAD segment is aligned to >= 0x4000 (16 KB)"
  JNI_COUNT=$("$LLVM_BIN/llvm-nm" -D --defined-only "$OUT_DIR/$LIB_NAME" | grep -c ' Java_' || true)
  echo "    exported JNI functions: $JNI_COUNT"
  [ "$JNI_COUNT" -gt 0 ] || die "[$ABI] no Java_ symbols exported"
done

echo "==> Done"
