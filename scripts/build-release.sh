#!/bin/sh
# Build the signed release Nudge.app from a clean clone of the current commit (macOS).
# Usage: sh scripts/build-release.sh
# Prints the path of the finished bundle. Notarizing and publishing are separate steps.
set -euo pipefail

cd "$(dirname "$0")/.."

# the release is built from HEAD, so uncommitted changes would not be in it
if [ -n "$(git status --porcelain)" ]; then
  echo "commit or stash your changes first; the release is built from HEAD" >&2
  exit 1
fi
COMMIT="$(git rev-parse HEAD)"

# build outside the home directory, so the project path Tauri embeds in the
# binary doesn't contain a user name
BUILD=/tmp/nudge-release-build
rm -rf "$BUILD"
git clone --quiet --no-hardlinks "$(pwd)" "$BUILD"
git -C "$BUILD" checkout --quiet --detach "$COMMIT"
cd "$BUILD"

# cargo is not always on PATH in this environment; fall back to rustup's stable toolchain
if ! command -v cargo >/dev/null 2>&1; then
  TOOLCHAIN="${HOME}/.rustup/toolchains/stable-$(uname -m)-apple-darwin/bin"
  if [ -d "$TOOLCHAIN" ]; then
    export PATH="$TOOLCHAIN:$PATH"
  else
    echo "cargo not found: install the Rust toolchain first" >&2
    exit 1
  fi
fi

# sign with a Developer ID certificate when one is installed, so macOS keeps
# permissions (Input Monitoring, login item) across updates; otherwise Tauri
# falls back to an ad-hoc signature
if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  APPLE_SIGNING_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n 1)"
fi
if [ -n "$APPLE_SIGNING_IDENTITY" ]; then
  export APPLE_SIGNING_IDENTITY
  echo "signing with: $APPLE_SIGNING_IDENTITY"
fi

# panic messages embed the source path of every crate, and crates live under
# ~/.cargo, so rewrite the home directory out of those paths
export RUSTFLAGS="--remap-path-prefix=$HOME=~"

npm ci
npm run tauri build -- --no-bundle

# strip = true in Cargo.toml runs rust-objcopy, which can fail without failing
# the build, so strip here before the bundle is signed
BIN=src-tauri/target/release/nudge
xcrun strip "$BIN"

npm run tauri bundle -- --bundles app

APP="$BUILD/src-tauri/target/release/bundle/macos/Nudge.app"
if [ ! -d "$APP" ]; then
  echo "expected bundle not found at $APP" >&2
  exit 1
fi
if strings -a "$APP/Contents/MacOS/nudge" | grep -q '/Users/'; then
  echo "the binary still contains a /Users/ path" >&2
  exit 1
fi
codesign --verify --strict "$APP"
echo "built $COMMIT"
echo "$APP"
