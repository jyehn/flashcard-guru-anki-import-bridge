#!/usr/bin/env bash
# Build flashcard-guru-anki-import-bridge as a multi-platform xcframework.
#
# Targets (current):
#   aarch64-apple-ios       — iPhone / iPad device
#   aarch64-apple-ios-sim   — Apple Silicon simulator
#
# Layout requirements (works in CI and in a standalone clone):
#
#   $REPO_ROOT/Cargo.toml              # this crate (the bridge)
#   $REPO_ROOT/../anki-rs/rslib/...    # Anki rslib at sibling path
#
# Usage: scripts/build-xcframework.sh [debug|release]
# Default profile is `release`.
#
# Output: $REPO_ROOT/RustImportCore.xcframework

set -euo pipefail

PROFILE="${1:-release}"
case "$PROFILE" in
  debug|Debug)     CARGO_FLAGS="";          BUILD_DIR_NAME="debug"   ;;
  release|Release) CARGO_FLAGS="--release"; BUILD_DIR_NAME="release" ;;
  *) echo "Unknown profile: $PROFILE (use debug|release)"; exit 1 ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE_DIR="$REPO_ROOT"
TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim)
LIB_NAME="librust_import_core.a"
XCFRAMEWORK_OUT="$REPO_ROOT/RustImportCore.xcframework"

echo "==> build-xcframework.sh starting"
echo "    profile  = $PROFILE"
echo "    repo     = $REPO_ROOT"
echo "    targets  = ${TARGETS[*]}"

# ---------------------------------------------------------------------------
# Sanity checks
# ---------------------------------------------------------------------------

if [ ! -f "$REPO_ROOT/../anki-rs/rslib/Cargo.toml" ]; then
  echo "ERROR: anki-rs not found at $REPO_ROOT/../anki-rs/"
  echo "Clone it as a sibling: cd .. && git clone https://github.com/ankitects/anki anki-rs"
  echo "Then check out the same revision flashifyIOS pins."
  exit 1
fi

if [ ! -d "$REPO_ROOT/../anki-rs/ftl/core-repo/core" ]; then
  echo "ERROR: anki-rs nested submodules not initialised."
  echo "Run inside ../anki-rs:  git submodule update --init --recursive"
  exit 1
fi

# Make sure cargo is on PATH (rustup typically lives in ~/.cargo/bin).
for dir in "${HOME:-/}/.cargo/bin" "/opt/homebrew/bin" "/usr/local/bin"; do
  if [ -d "$dir" ]; then
    case ":$PATH:" in
      *":$dir:"*) ;;
      *) export PATH="$dir:$PATH" ;;
    esac
  fi
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: cargo not found. Install via https://rustup.rs/"
  exit 1
fi
echo "==> Using cargo at $(command -v cargo)"

# rslib's prost-build needs protoc. Anki bundles a copy under
# out/extracted/protoc/, but we don't run that bootstrap; use the system one.
if [ -z "${PROTOC:-}" ]; then
  if command -v protoc >/dev/null 2>&1; then
    export PROTOC="$(command -v protoc)"
  else
    echo "ERROR: protoc not found. Install via 'brew install protobuf'."
    exit 1
  fi
fi
echo "==> Using PROTOC=$PROTOC"

# ---------------------------------------------------------------------------
# Targets + build
# ---------------------------------------------------------------------------

echo "==> Verifying Rust targets"
for target in "${TARGETS[@]}"; do
  if ! rustup target list --installed | grep -q "^$target$"; then
    echo "Installing $target ..."
    rustup target add "$target"
  fi
done

echo "==> Building $PROFILE for: ${TARGETS[*]}"
cd "$CRATE_DIR"
for target in "${TARGETS[@]}"; do
  echo "    cargo build --target $target"
  cargo build $CARGO_FLAGS --target "$target"
done

# ---------------------------------------------------------------------------
# Locate the cbindgen-generated header
# ---------------------------------------------------------------------------

echo "==> Locating cbindgen header"
TARGET_ROOT=$(cargo metadata --format-version 1 --no-deps | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
HEADER_SOURCE=$(find "$TARGET_ROOT/${TARGETS[0]}/$BUILD_DIR_NAME/build" \
  -name rust_import_core.h -path "*/out/include/*" 2>/dev/null \
  | xargs -I {} stat -f "%m %N" {} \
  | sort -rn | head -1 | cut -d' ' -f2-)
if [ -z "$HEADER_SOURCE" ] || [ ! -f "$HEADER_SOURCE" ]; then
  echo "ERROR: cbindgen header not produced. Did cargo run build.rs?"
  exit 1
fi
echo "    found: $HEADER_SOURCE"

# ---------------------------------------------------------------------------
# Stage + xcodebuild -create-xcframework
# ---------------------------------------------------------------------------

STAGE_ROOT="$REPO_ROOT/.rust-stage"
rm -rf "$STAGE_ROOT" "$XCFRAMEWORK_OUT"
mkdir -p "$STAGE_ROOT"

stage_platform() {
  local target="$1"
  local platform_dir="$STAGE_ROOT/$target"
  mkdir -p "$platform_dir/include"
  cp "$HEADER_SOURCE" "$platform_dir/include/rust_import_core.h"
  cp "$CRATE_DIR/target/$target/$BUILD_DIR_NAME/$LIB_NAME" "$platform_dir/$LIB_NAME"
  cat > "$platform_dir/include/module.modulemap" <<EOF
module RustImportCore {
    header "rust_import_core.h"
    export *
}
EOF
}

for target in "${TARGETS[@]}"; do
  stage_platform "$target"
done

echo "==> Assembling xcframework"
xcodebuild -create-xcframework \
  -library "$STAGE_ROOT/aarch64-apple-ios/$LIB_NAME" \
    -headers "$STAGE_ROOT/aarch64-apple-ios/include" \
  -library "$STAGE_ROOT/aarch64-apple-ios-sim/$LIB_NAME" \
    -headers "$STAGE_ROOT/aarch64-apple-ios-sim/include" \
  -output "$XCFRAMEWORK_OUT"

rm -rf "$STAGE_ROOT"

echo ""
echo "==> Done"
echo "    xcframework: $XCFRAMEWORK_OUT"
