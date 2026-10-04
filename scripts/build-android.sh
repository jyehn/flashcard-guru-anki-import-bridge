#!/usr/bin/env bash
# Build the bridge as an Android JNI shared library (librust_import_core.so).
#
# Usage: scripts/build-android.sh [OUTPUT_JNILIBS_DIR]
#   Output: <OUTPUT_JNILIBS_DIR>/<abi>/librust_import_core.so
#           <OUTPUT_JNILIBS_DIR>/SHA256SUMS   (one line per ABI built in this run)
#           <OUTPUT_JNILIBS_DIR>/BUILD-INFO   (commit, toolchain, NDK, ABIs)
#   Default output: $REPO_ROOT/android/jniLibs
#
# Environment (all optional):
#   ANDROID_ABIS        space-separated ABIs, default "arm64-v8a x86_64 armeabi-v7a".
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
#   - 64-bit ABIs (arm64-v8a, x86_64) link with 16 KB ELF segment alignment: Play
#     requires 16 KB page support on 64-bit devices for apps targeting Android 15+,
#     and NDK r27 still defaults to 4 KB. armeabi-v7a keeps the linker's 4 KB
#     default, as NDK r28+ does: 16 KB pages exist only for 64-bit ABIs, and Android
#     has no plan to change the page size of 32-bit ones.
#   - DT_SONAME is set (bionic requires one for API >= 23; rustc sets none).
#   - --remap-path-prefix: panic locations name /cargo/..., /build/... instead of
#     the build machine's paths, so no home directory ends up in the binary, and the
#     bridge checkout's location, the target dir and CARGO_HOME no longer change the
#     bytes. The absolute path of ../anki-rs still does: Cargo hashes the path of a
#     path dependency outside the workspace into the crate metadata (symbol hashes,
#     section order), so byte-identical builds need anki-rs at the same absolute path.
#   - After each build it checks the ELF: machine, LOAD alignment, SONAME, no text
#     relocations, NEEDED only from {libc, libm, libdl, liblog, libandroid}, every
#     strong undefined symbol defined by those NDK stubs at $ANDROID_API, no setenv,
#     and Java_ exports present. Any failure stops the build.
#
# Layout requirement (same as iOS): ../anki-rs is the rslib checkout.

set -euo pipefail
export LC_ALL=C   # sort and comm must agree on the collation

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_ROOT="${1:-$REPO_ROOT/android/jniLibs}"
ABIS="${ANDROID_ABIS:-arm64-v8a x86_64 armeabi-v7a}"
NDK_VERSION="${ANDROID_NDK_VERSION:-27.1.12297006}"
API="${ANDROID_API:-26}"
TOOLCHAIN="${RUST_TOOLCHAIN:-1.92.0}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
LIB_NAME="librust_import_core.so"
ALLOWED_NEEDED="libc.so libm.so libdl.so liblog.so libandroid.so"

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
ANKI_DIR="$(cd "$REPO_ROOT/../anki-rs" && pwd -P)"

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
SYSROOT_LIB="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST_TAG/sysroot/usr/lib"
for tool in llvm-readelf llvm-nm llvm-strip llvm-strings llvm-ar llvm-ranlib; do
  [ -x "$LLVM_BIN/$tool" ] || die "$tool not found in $LLVM_BIN"
done

mkdir -p "$TARGET_DIR"
# Build-machine paths -> neutral ones, broad prefixes first (rustc applies the last
# matching remap). Cargo may see a path as written or resolved, so both forms are mapped.
# Without this, panic locations embed e.g. $HOME/.cargo/registry/src/....
REMAPS=()
MACHINE_PATHS=()
add_remap() {   # $1 = directory, $2 = replacement
  local logical physical form
  logical="$(cd "$1" && pwd -L)"
  physical="$(cd "$1" && pwd -P)"
  for form in "$logical" "$physical"; do
    case " ${MACHINE_PATHS[*]-} " in *" $form "*) continue ;; esac
    REMAPS+=("--remap-path-prefix=$form=$2")
    MACHINE_PATHS+=("$form")
  done
}
# The target dir comes last: it may sit inside the repository (the default).
add_remap "${CARGO_HOME:-$HOME/.cargo}" /cargo
add_remap "$REPO_ROOT/../anki-rs" /build/anki-rs
add_remap "$REPO_ROOT" /build/bridge
add_remap "$TARGET_DIR" /build/target
[ -n "${HOME:-}" ] && MACHINE_PATHS+=("$HOME")
BRIDGE_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
BRIDGE_DIRTY="$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no 2>/dev/null || true)"
RUSTC_VERSION="$(rustc +"$TOOLCHAIN" --version)"

echo "==> build-android.sh"
echo "    repo       = $REPO_ROOT ($BRIDGE_COMMIT${BRIDGE_DIRTY:+, modified})"
echo "    abis       = $ABIS"
echo "    toolchain  = $TOOLCHAIN ($RUSTC_VERSION)"
echo "    ndk        = $ANDROID_NDK_HOME (API $API)"
echo "    target dir = $TARGET_DIR"
echo "    output     = $OUT_ROOT"
echo "    PROTOC     = $PROTOC"

# Prints the names of the dynamic symbols the allowed NDK stub libraries define.
ndk_provided_symbols() {
  local stub_dir="$1" lib
  for lib in $ALLOWED_NEEDED; do
    [ -f "$stub_dir/$lib" ] || die "NDK stub $stub_dir/$lib not found"
    "$LLVM_BIN/llvm-nm" -D --defined-only -j "$stub_dir/$lib"
  done | sed 's/@.*//' | sort -u
}

# Checks one stripped library. $1 = path, $2 = ABI, $3 = expected e_machine text,
# $4 = minimum LOAD alignment, $5 = NDK sysroot triple (stub libraries).
verify_library() {
  local so="$1" abi="$2" machine="$3" min_align="$4" stub_triple="$5"
  local header dynamic aligns align needed lib undefined provided missing weak jni

  header="$("$LLVM_BIN/llvm-readelf" -hW "$so")"
  echo "$header" | grep -q "Machine: *$machine" || die "[$abi] ELF machine is not $machine"

  echo "    LOAD segments (llvm-readelf -l):"
  "$LLVM_BIN/llvm-readelf" -lW "$so" | grep -E '^\s+(LOAD|GNU_RELRO)' | sed 's/^ */      /'
  aligns=$("$LLVM_BIN/llvm-readelf" -lW "$so" | awk '$1 == "LOAD" { print $NF }')
  [ -n "$aligns" ] || die "[$abi] no LOAD segments found"
  for align in $aligns; do
    [ $((align)) -ge "$min_align" ] || die "[$abi] LOAD segment alignment $align is below $min_align"
  done
  echo "    every LOAD segment is aligned to >= $min_align bytes"

  dynamic="$("$LLVM_BIN/llvm-readelf" -dW "$so")"
  echo "$dynamic" | grep -q "(SONAME) *Library soname: \[$LIB_NAME\]" \
    || die "[$abi] DT_SONAME is not $LIB_NAME"
  if echo "$dynamic" | grep -qE '\(TEXTREL\)|FLAGS\).*TEXTREL'; then
    die "[$abi] text relocations present (bionic refuses them for API >= 23)"
  fi
  needed=$(echo "$dynamic" | sed -n 's/.*(NEEDED) *Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')
  for lib in $needed; do
    case " $ALLOWED_NEEDED " in *" $lib "*) ;; *) die "[$abi] unexpected NEEDED library $lib" ;; esac
  done
  echo "    SONAME $LIB_NAME, no TEXTREL, NEEDED: ${needed% }"

  undefined="$("$LLVM_BIN/llvm-nm" -D --undefined-only "$so" | awk '{ print $1, $2 }' | sed 's/@.*//' | sort -u)"
  provided="$(ndk_provided_symbols "$SYSROOT_LIB/$stub_triple/$API")"
  missing="$(echo "$undefined" | awk '$1 == "U" { print $2 }' | comm -23 - <(echo "$provided"))"
  [ -z "$missing" ] || die "[$abi] undefined symbols not provided by $ALLOWED_NEEDED at API $API:
$missing"
  weak="$(echo "$undefined" | awk '$1 == "w" { print $2 }' | comm -23 - <(echo "$provided") | tr '\n' ' ')"
  echo "    undefined symbols: $(echo "$undefined" | grep -c .), all strong ones defined by the NDK stubs (API $API)"
  echo "    weak, unresolved at API $API (null at run time, by design): ${weak:-none}"
  if echo "$undefined" | awk '{ print $2 }' | grep -qx setenv; then
    die "[$abi] setenv is imported (the JNI layer must not change the environment)"
  fi

  jni=$("$LLVM_BIN/llvm-nm" -D --defined-only -j "$so" | grep -c '^Java_' || true)
  echo "    exported: $jni Java_ functions, $("$LLVM_BIN/llvm-nm" -D --defined-only -j "$so" | grep -c '^flashify_' || true) flashify_ functions"
  [ "$jni" -gt 0 ] || die "[$abi] no Java_ symbols exported"
}

mkdir -p "$OUT_ROOT"
SUMS="$(mktemp "${TMPDIR:-/tmp}/android-sums.XXXXXX")"
trap 'rm -f "$SUMS"' EXIT

for ABI in $ABIS; do
  case "$ABI" in
    arm64-v8a)
      TRIPLE="aarch64-linux-android"; CLANG_PREFIX="aarch64-linux-android"
      STUB_TRIPLE="aarch64-linux-android"; MACHINE="AArch64"; PAGE=16384 ;;
    x86_64)
      TRIPLE="x86_64-linux-android"; CLANG_PREFIX="x86_64-linux-android"
      STUB_TRIPLE="x86_64-linux-android"; MACHINE="Advanced Micro Devices X86-64"; PAGE=16384 ;;
    armeabi-v7a)
      TRIPLE="armv7-linux-androideabi"; CLANG_PREFIX="armv7a-linux-androideabi"
      STUB_TRIPLE="arm-linux-androideabi"; MACHINE="ARM"; PAGE=4096 ;;
    *) die "unknown ABI $ABI" ;;
  esac

  if ! rustup target list --installed --toolchain "$TOOLCHAIN" | grep -qx "$TRIPLE"; then
    die "Rust target $TRIPLE missing: rustup target add $TRIPLE --toolchain $TOOLCHAIN"
  fi

  CLANG="$LLVM_BIN/${CLANG_PREFIX}${API}-clang"
  [ -x "$CLANG" ] || die "NDK clang wrapper not found: $CLANG"
  TRIPLE_UPPER="$(echo "$TRIPLE" | tr 'a-z-' 'A-Z_')"
  TRIPLE_UNDERSCORE="$(echo "$TRIPLE" | tr '-' '_')"

  # Flags for every crate of the target (CARGO_ENCODED_RUSTFLAGS: separated by 0x1f,
  # so paths may contain spaces). Rustc applies the last matching remap, so the
  # broad prefixes come first.
  FLAGS=("${REMAPS[@]}")
  if [ "$PAGE" -eq 16384 ]; then
    FLAGS+=("-Clink-arg=-Wl,-z,max-page-size=16384" "-Clink-arg=-Wl,-z,common-page-size=16384")
  fi
  ENCODED="$(printf '%s\x1f' "${FLAGS[@]}")"
  ENCODED="${ENCODED%$'\x1f'}"

  echo "==> [$ABI] cargo rustc --target $TRIPLE --crate-type cdylib (LOAD alignment $PAGE)"
  (
    cd "$REPO_ROOT"
    export CARGO_TARGET_DIR="$TARGET_DIR"
    export CARGO_ENCODED_RUSTFLAGS="$ENCODED"
    unset RUSTFLAGS
    export "CARGO_TARGET_${TRIPLE_UPPER}_LINKER=$CLANG"
    export "CC_${TRIPLE_UNDERSCORE}=$CLANG"
    export "CXX_${TRIPLE_UNDERSCORE}=${CLANG}++"
    export "AR_${TRIPLE_UNDERSCORE}=$LLVM_BIN/llvm-ar"
    export "RANLIB_${TRIPLE_UNDERSCORE}=$LLVM_BIN/llvm-ranlib"
    # The SONAME only matters for the final link, so it goes to this crate alone and
    # changing it never rebuilds the dependencies.
    cargo +"$TOOLCHAIN" rustc --release --lib --locked \
      --target "$TRIPLE" \
      --no-default-features --features android \
      --crate-type cdylib \
      -- -C "link-arg=-Wl,-soname,$LIB_NAME"
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
  SHA=$(shasum -a 256 "$OUT_DIR/$LIB_NAME" | cut -d' ' -f1)
  echo "    size: $BYTES bytes ($(awk "BEGIN { printf \"%.1f\", $BYTES / 1048576 }") MiB)"
  echo "    sha256: $SHA"
  verify_library "$OUT_DIR/$LIB_NAME" "$ABI" "$MACHINE" "$PAGE" "$STUB_TRIPLE"
  LEAKED=0
  for path in "${MACHINE_PATHS[@]}"; do
    n=$("$LLVM_BIN/llvm-strings" -n 8 "$OUT_DIR/$LIB_NAME" | grep -cF "$path/" || true)
    [ "$n" -eq 0 ] || echo "    WARNING: $n strings in the library contain $path/" >&2
    LEAKED=$((LEAKED + n))
  done
  [ "$LEAKED" -gt 0 ] || echo "    no build-machine paths in the library (checked: ${MACHINE_PATHS[*]})"
  echo "$SHA  $ABI/$LIB_NAME" >> "$SUMS"
done

cp "$SUMS" "$OUT_ROOT/SHA256SUMS"
{
  echo "bridge_commit=$BRIDGE_COMMIT"
  echo "bridge_modified=$([ -n "$BRIDGE_DIRTY" ] && echo yes || echo no)"
  echo "anki_rev=$(git -C "$ANKI_DIR" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "rustc=$RUSTC_VERSION"
  echo "ndk=$(sed -n 's/^Pkg.Revision *= *//p' "$ANDROID_NDK_HOME/source.properties" 2>/dev/null || echo unknown)"
  echo "android_api=$API"
  echo "abis=$ABIS"
} > "$OUT_ROOT/BUILD-INFO"
echo "==> $OUT_ROOT/SHA256SUMS"
sed 's/^/    /' "$OUT_ROOT/SHA256SUMS"
echo "==> Done"
