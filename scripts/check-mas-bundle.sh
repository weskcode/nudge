#!/bin/sh
# Check a built Nudge.app against the Mac App Store requirements (macOS).
# Usage: sh scripts/check-mas-bundle.sh path/to/Nudge.app [path/to/Nudge.pkg]
# Read-only: it inspects the bundle and changes nothing. Exits 1 if a check fails.
set -eu

APP="${1:?usage: sh scripts/check-mas-bundle.sh path/to/Nudge.app [path/to/Nudge.pkg]}"
PKG="${2:-}"
[ -d "$APP" ] || { echo "no such bundle: $APP" >&2; exit 2; }

PLIST="$APP/Contents/Info.plist"
EXECUTABLE="$(plutil -extract CFBundleExecutable raw "$PLIST")"
BIN="$APP/Contents/MacOS/$EXECUTABLE"

WORK="$(mktemp -d /tmp/nudge-mas-check.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

codesign -d --entitlements - "$APP" > "$WORK/entitlements.txt" 2>/dev/null || true
codesign -dvv "$APP" > "$WORK/signature.txt" 2>&1 || true
strings -a "$BIN" > "$WORK/strings.txt"
nm -u "$BIN" > "$WORK/imports.txt" 2>/dev/null || true

FAILS=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILS=$((FAILS + 1)); }
holds() { # holds <description> <command...>
  description="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$description"; else fail "$description"; fi
}
has() { grep -q -E "$1" "$2"; }
lacks() { ! grep -q -E "$1" "$2"; }

echo "checking $APP"

# signing and sandbox (guideline 2.4.5(i))
holds "App Sandbox entitlement is set" has 'com.apple.security.app-sandbox' "$WORK/entitlements.txt"
holds "no temporary-exception entitlements" lacks 'temporary-exception' "$WORK/entitlements.txt"
holds "application identifier entitlement is set" has 'com.apple.application-identifier' "$WORK/entitlements.txt"
holds "team identifier entitlement is set" has 'com.apple.developer.team-identifier' "$WORK/entitlements.txt"
holds "signed with an Apple Distribution identity" has 'Authority=(Apple Distribution|3rd Party Mac Developer Application)' "$WORK/signature.txt"
holds "provisioning profile is embedded" test -f "$APP/Contents/embedded.provisionprofile"

# Info.plist keys App Store Connect validates
holds "LSApplicationCategoryType is set" plutil -extract LSApplicationCategoryType raw "$PLIST"
holds "ITSAppUsesNonExemptEncryption is set" plutil -extract ITSAppUsesNonExemptEncryption raw "$PLIST"
version="$(plutil -extract CFBundleVersion raw "$PLIST" 2>/dev/null || true)"
holds "CFBundleVersion is one to three dotted integers (got '$version')" \
  sh -c "printf '%s' '$version' | grep -q -E '^[0-9]+(\.[0-9]+){0,2}\$'"

# icon: the bundle's icns needs a 512x512@2x (1024 px) image
icon="$(plutil -extract CFBundleIconFile raw "$PLIST" 2>/dev/null || true)"
case "$icon" in *.icns) ;; *) icon="$icon.icns" ;; esac
iconutil -c iconset "$APP/Contents/Resources/$icon" -o "$WORK/icon.iconset" >/dev/null 2>&1 || true
holds "icns holds a 512x512@2x image" test -f "$WORK/icon.iconset/icon_512x512@2x.png"

# privacy manifest: a binary that calls stat() must declare the file timestamp category
manifest="$APP/Contents/Resources/PrivacyInfo.xcprivacy"
holds "PrivacyInfo.xcprivacy exists and is a valid plist" plutil -lint "$manifest"
if has '^_(stat|lstat|fstat)(64)?$' "$WORK/imports.txt"; then
  holds "manifest declares NSPrivacyAccessedAPICategoryFileTimestamp (binary imports stat)" \
    grep -q 'NSPrivacyAccessedAPICategoryFileTimestamp' "$manifest"
fi

# public APIs only (guideline 2.5.1) and a sandbox-safe login item (guideline 2.4.5(iii))
holds "no private WebKit keys (drawsBackground, fullScreenEnabled)" lacks 'drawsBackground|fullScreenEnabled' "$WORK/strings.txt"
holds "no LaunchAgents login item code" lacks 'LaunchAgents' "$WORK/strings.txt"

if [ -n "$PKG" ]; then
  pkgutil --check-signature "$PKG" > "$WORK/pkg.txt" 2>&1 || true
  holds "installer package is signed with a Mac installer identity" \
    has 'Mac Installer Distribution|3rd Party Mac Developer Installer' "$WORK/pkg.txt"
fi

echo
if [ "$FAILS" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$FAILS check(s) failed"
  exit 1
fi
