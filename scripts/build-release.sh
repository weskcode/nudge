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
BUILD="$(mktemp -d /tmp/nudge-release.XXXXXX)"
git clone --quiet --no-hardlinks "$(pwd)" "$BUILD"
git -C "$BUILD" checkout --quiet --detach "$COMMIT"
cd "$BUILD"

# cargo is not always on PATH in this environment; fall back to rustup's stable toolchain
if ! command -v cargo >/dev/null 2>&1; then
  # uname says arm64, rustup says aarch64
  TOOLCHAIN="${HOME}/.rustup/toolchains/stable-$(uname -m | sed 's/arm64/aarch64/')-apple-darwin/bin"
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
# ~/.cargo, so rewrite the home directory out of those paths (the encoded form
# keeps a home path with spaces in one piece)
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=~"

npm ci
# --locked: build exactly the crate versions in Cargo.lock
npm run tauri build -- --no-bundle -- --locked

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
# not grep -q: it exits at the first match, strings then dies of SIGPIPE, and
# pipefail turns that into a failed pipeline, which would skip this check
if strings -a "$APP/Contents/MacOS/nudge" | grep '/Users/' >/dev/null; then
  echo "the binary still contains a /Users/ path" >&2
  exit 1
fi
codesign --verify --strict "$APP"
echo "built $COMMIT"
echo "$APP"
