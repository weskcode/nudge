#!/bin/sh
# Build the Mac App Store Nudge.app from a clean clone of the current commit (macOS).
# Usage: sh scripts/build-mas.sh [--local]
#
# Without --local it needs, and checks for, three things from your Apple developer
# account: an Apple Distribution identity and a Mac installer identity in the
# keychain, and a Mac App Store provisioning profile for in.nudge.app saved as
# src-tauri/embedded.provisionprofile (git ignores it). It signs the app, then builds
# a signed Nudge.pkg with productbuild. Uploading is a separate step.
#
# --local builds an ad-hoc signed copy in the App Sandbox under another bundle ID,
# so it can run on this Mac beside the installed Nudge. It is for testing only.
#
# Prints the path of the finished bundle (and package).
set -euo pipefail

# tauri bundle notarizes, or imports a certificate, when these are set; neither belongs in this build
unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH \
  APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

LOCAL=0
if [ "${1:-}" = "--local" ]; then
  LOCAL=1
elif [ -n "${1:-}" ]; then
  echo "usage: sh scripts/build-mas.sh [--local]" >&2
  exit 2
fi

# the build is made from HEAD, so uncommitted changes would not be in it
if [ -n "$(git status --porcelain)" ]; then
  echo "commit or stash your changes first; the build is made from HEAD" >&2
  exit 1
fi
COMMIT="$(git rev-parse HEAD)"

SIGN_ID=""
INSTALLER_ID=""
if [ "$LOCAL" -eq 0 ]; then
  # the identities must belong to the team named in the entitlements
  TEAM="$(/usr/libexec/PlistBuddy -c 'Print :com.apple.developer.team-identifier' src-tauri/entitlements.mas.plist)"
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n -E 's/.*"((Apple Distribution|3rd Party Mac Developer Application): [^"]*\('"$TEAM"'\))".*/\1/p' | head -n 1)"
  INSTALLER_ID="$(security find-identity -v 2>/dev/null \
    | sed -n -E 's/.*"((Mac Installer Distribution|3rd Party Mac Developer Installer): [^"]*\('"$TEAM"'\))".*/\1/p' | head -n 1)"
  [ -n "$SIGN_ID" ] || { echo "no Apple Distribution identity for team $TEAM in the keychain" >&2; exit 1; }
  [ -n "$INSTALLER_ID" ] || { echo "no Mac installer identity (Mac Installer Distribution) for team $TEAM in the keychain" >&2; exit 1; }
  [ -f src-tauri/embedded.provisionprofile ] || {
    echo "src-tauri/embedded.provisionprofile is missing: download the Mac App Store profile for in.nudge.app" >&2
    exit 1
  }
fi

# build outside the home directory, so the project path Tauri embeds in the
# binary doesn't contain a user name
BUILD="$(mktemp -d /tmp/nudge-mas.XXXXXX)"
git clone --quiet --no-hardlinks "$ROOT" "$BUILD/src"
git -C "$BUILD/src" checkout --quiet --detach "$COMMIT"
cd "$BUILD/src"
if [ "$LOCAL" -eq 0 ]; then
  cp "$ROOT/src-tauri/embedded.provisionprofile" src-tauri/
fi

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

# tauri-build compares the tauri crate's feature list in Cargo.toml with the
# config, so the private API cannot be switched off from a cargo flag. this copy
# of Cargo.toml loses the feature, and the overlay config turns the flag off too
sed -i '' 's/features = \["tray-icon", "macos-private-api"\]/features = ["tray-icon"]/' src-tauri/Cargo.toml
if grep -q '"macos-private-api"' src-tauri/Cargo.toml; then
  echo "could not remove the macos-private-api feature from Cargo.toml" >&2
  exit 1
fi

CONFIGS="--config $BUILD/src/src-tauri/tauri.mas.conf.json"
if [ "$LOCAL" -eq 1 ]; then
  # without the profile the system refuses an app that claims an application
  # identifier, so the test build drops it, and takes another bundle ID
  cp src-tauri/entitlements.mas.plist "$BUILD/local.entitlements.plist"
  /usr/libexec/PlistBuddy -c 'Delete :com.apple.application-identifier' "$BUILD/local.entitlements.plist"
  /usr/libexec/PlistBuddy -c 'Delete :com.apple.developer.team-identifier' "$BUILD/local.entitlements.plist"
  cat > "$BUILD/local.conf.json" <<EOF
{
  "identifier": "in.nudge.maslocal",
  "bundle": {
    "macOS": {
      "entitlements": "$BUILD/local.entitlements.plist",
      "files": { "embedded.provisionprofile": null }
    }
  }
}
EOF
  CONFIGS="$CONFIGS --config $BUILD/local.conf.json"
else
  export APPLE_SIGNING_IDENTITY="$SIGN_ID"
  echo "signing with: $SIGN_ID"
fi

# panic messages embed the source path of every crate, and crates live under
# ~/.cargo, so rewrite the home directory out of those paths (the encoded form
# keeps a home path with spaces in one piece)
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=~"

# reuse the checkout's node_modules instead of downloading them again
if [ -d "$ROOT/node_modules" ]; then
  ln -s "$ROOT/node_modules" node_modules
else
  npm ci
fi

# shellcheck disable=SC2086
npm run tauri build -- --no-bundle $CONFIGS -- --locked --no-default-features --features mas

# strip = true in Cargo.toml runs rust-objcopy, which can fail without failing
# the build, so strip here before the bundle is signed
BIN=src-tauri/target/release/nudge
xcrun strip "$BIN"

# shellcheck disable=SC2086
npm run tauri bundle -- --bundles app $CONFIGS

APP="$BUILD/src/src-tauri/target/release/bundle/macos/Nudge.app"
if [ ! -d "$APP" ]; then
  echo "expected bundle not found at $APP" >&2
  exit 1
fi
if [ "$LOCAL" -eq 1 ]; then
  codesign --force --sign - --entitlements "$BUILD/local.entitlements.plist" "$APP"
fi
# not grep -q: it exits at the first match, strings then dies of SIGPIPE, and
# pipefail turns that into a failed pipeline, which would skip this check
if strings -a "$APP/Contents/MacOS/nudge" | grep '/Users/' >/dev/null; then
  echo "the binary still contains a /Users/ path" >&2
  exit 1
fi
codesign --verify --strict "$APP"

if [ "$LOCAL" -eq 1 ]; then
  sh "$ROOT/scripts/check-mas-bundle.sh" --local "$APP"
  echo "built $COMMIT (local test build, bundle ID in.nudge.maslocal)"
  echo "$APP"
  echo "delete $BUILD when you are finished with it"
else
  PKG="$BUILD/Nudge.pkg"
  xcrun productbuild --component "$APP" /Applications --sign "$INSTALLER_ID" "$PKG"
  sh "$ROOT/scripts/check-mas-bundle.sh" "$APP" "$PKG"
  echo "built $COMMIT"
  echo "$APP"
  echo "$PKG"
  echo "delete $BUILD when you are finished with it; it holds a copy of the provisioning profile"
fi
