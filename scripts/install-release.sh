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

npm run tauri build -- --bundles app

APP=src-tauri/target/release/bundle/macos/Nudge.app
if [ ! -d "$APP" ]; then
  echo "expected bundle not found at $APP" >&2
  exit 1
fi

TARGET=/Applications/Nudge.app
# never replace a different app that happens to be called Nudge
if [ -d "$TARGET" ]; then
  EXISTING="$(defaults read "$TARGET/Contents/Info" CFBundleIdentifier 2>/dev/null || true)"
  if [ "$EXISTING" != "in.nudge.app" ]; then
    echo "$TARGET is a different app (${EXISTING:-no bundle ID}); not replacing it" >&2
    exit 1
  fi
fi
pkill -f "$TARGET/Contents/MacOS/nudge" || true
rm -rf "$TARGET"
cp -R "$APP" "$TARGET"
open "$TARGET"
echo "installed and launched $TARGET"
