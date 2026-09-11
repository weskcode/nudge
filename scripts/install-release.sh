#!/bin/sh
# Build a release bundle of nudge and install it to /Applications (macOS).
# Usage: npm run install:app
set -euo pipefail

cd "$(dirname "$0")/.."

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

npm run tauri build -- --bundles app

APP=src-tauri/target/release/bundle/macos/Nudge.app
if [ ! -d "$APP" ]; then
  echo "expected bundle not found at $APP" >&2
  exit 1
fi

TARGET=/Applications/Nudge.app
pkill -f Nudge.app/Contents/MacOS/nudge || true
rm -rf "$TARGET"
cp -R "$APP" "$TARGET"
open "$TARGET"
echo "installed and launched $TARGET"
