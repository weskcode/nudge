#!/bin/sh
# Check a built Nudge.app against the Mac App Store requirements (macOS).
# Usage: sh scripts/check-mas-bundle.sh [--local] path/to/Nudge.app [path/to/Nudge.pkg]
# --local is for the ad-hoc signed test build, which has no Apple Distribution
# identity, application identifier or provisioning profile; those checks are skipped.
# Read-only: it inspects the bundle and changes nothing. Exits 1 if a check fails.
set -eu

LOCAL=0
if [ "${1:-}" = "--local" ]; then
  LOCAL=1
  shift
fi
APP="${1:?usage: sh scripts/check-mas-bundle.sh [--local] path/to/Nudge.app [path/to/Nudge.pkg]}"
PKG="${2:-}"
[ -d "$APP" ] || { echo "no such bundle: $APP" >&2; exit 2; }

PLIST="$APP/Contents/Info.plist"
EXECUTABLE="$(plutil -extract CFBundleExecutable raw "$PLIST")"
BIN="$APP/Contents/MacOS/$EXECUTABLE"

WORK="$(mktemp -d /tmp/nudge-mas-check.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

codesign -d --entitlements - --xml "$APP" > "$WORK/entitlements.plist" 2>/dev/null || true
codesign -dvv "$APP" > "$WORK/signature.txt" 2>&1 || true
strings -a "$BIN" > "$WORK/strings.txt" && [ -s "$WORK/strings.txt" ] || {
  echo "could not read binary strings from $BIN" >&2
  exit 2
}
nm -u "$BIN" > "$WORK/imports.txt" || { echo "nm failed on $BIN" >&2; exit 2; }

FAILS=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILS=$((FAILS + 1)); }
holds() { # holds <description> <command...>
  description="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$description"; else fail "$description"; fi
}
# a check that only warns: Apple's page does not say the rule covers macOS
advises() {
  description="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$description"; else echo "WARN  $description"; fi
}
# a check a local test build cannot meet
signed() {
  if [ "$LOCAL" -eq 1 ]; then echo "SKIP  $1 (local build)"; else holds "$@"; fi
}
has() { grep -q -E "$1" "$2"; }
check_absent() {
  if [ ! -s "$3" ]; then
    echo "missing input for $1: $3" >&2
    exit 2
  fi
  if grep -E "$2" "$3" >/dev/null; then
    fail "$1"
  else
    status=$?
    [ "$status" -eq 1 ] || { echo "search failed for $1" >&2; exit 2; }
    pass "$1"
  fi
}

echo "checking $APP"

# signing and sandbox (guideline 2.4.5(i))
entitlement() { plutil -extract "$1" raw "$WORK/entitlements.plist"; }
sandboxed() { [ "$(entitlement 'com\.apple\.security\.app-sandbox')" = "true" ]; }
holds "App Sandbox entitlement is true" sandboxed
check_absent "no temporary-exception entitlements" 'temporary-exception' "$WORK/entitlements.plist"
check_absent "no get-task-allow entitlement (the store rejects it)" 'get-task-allow' "$WORK/entitlements.plist"
bundle_id="$(plutil -extract CFBundleIdentifier raw "$PLIST" 2>/dev/null || true)"
team_id() { entitlement 'com\.apple\.developer\.team-identifier'; }
identifiers_agree() { [ "$(entitlement 'com\.apple\.application-identifier')" = "$(team_id).$bundle_id" ]; }
signed_by_team() { grep -q "^TeamIdentifier=$(team_id)\$" "$WORK/signature.txt"; }
signed "application identifier is the team identifier plus $bundle_id" identifiers_agree
signed "team identifier matches the signing certificate's team" signed_by_team
signed "signed with an Apple Distribution identity" has 'Authority=(Apple Distribution|3rd Party Mac Developer Application)' "$WORK/signature.txt"
signed "provisioning profile is embedded" test -f "$APP/Contents/embedded.provisionprofile"

# Info.plist keys App Store Connect validates
holds "LSApplicationCategoryType is set" plutil -extract LSApplicationCategoryType raw "$PLIST"
holds "ITSAppUsesNonExemptEncryption is set" plutil -extract ITSAppUsesNonExemptEncryption raw "$PLIST"
valid_version() {
  plutil -extract CFBundleVersion raw -expect string -o "$WORK/version.txt" "$PLIST" || return 1
  LC_ALL=C awk 'NR == 1 && /^[0-9]+([.][0-9]+)?([.][0-9]+)?$/ { valid = 1 }
                END { exit !(NR == 1 && valid) }' "$WORK/version.txt"
}
holds "CFBundleVersion is one to three dotted integers" valid_version

# icon: the bundle's icns needs a 512x512@2x (1024 px) image
icon="$(plutil -extract CFBundleIconFile raw "$PLIST" 2>/dev/null || true)"
case "$icon" in *.icns) ;; *) icon="$icon.icns" ;; esac
iconutil -c iconset "$APP/Contents/Resources/$icon" -o "$WORK/icon.iconset" >/dev/null 2>&1 || true
holds "icns holds a 512x512@2x image" test -f "$WORK/icon.iconset/icon_512x512@2x.png"

# privacy manifest: Apple's required reason page names iOS, iPadOS, tvOS, visionOS
# and watchOS, not macOS, so a missing one only warns. a binary that calls stat()
# would declare the file timestamp category
manifest="$APP/Contents/Resources/PrivacyInfo.xcprivacy"
advises "PrivacyInfo.xcprivacy exists and is a valid plist" plutil -lint "$manifest"
if has '^_(stat|lstat|fstat)(64)?$' "$WORK/imports.txt"; then
  advises "manifest declares NSPrivacyAccessedAPICategoryFileTimestamp (binary imports stat)" \
    grep -q 'NSPrivacyAccessedAPICategoryFileTimestamp' "$manifest"
fi

# public APIs only (guideline 2.5.1) and a sandbox-safe login item (guideline 2.4.5(iii))
check_absent "no private WebKit keys (drawsBackground, fullScreenEnabled)" 'drawsBackground|fullScreenEnabled' "$WORK/strings.txt"
check_absent "no LaunchAgents login item code" 'LaunchAgents' "$WORK/strings.txt"

if [ -n "$PKG" ]; then
  # pkgutil exits non-zero for an unsigned package, and says when the certificate is not trusted
  package_signed() {
    pkgutil --check-signature "$PKG" > "$WORK/pkg.txt" 2>&1 \
      && grep -q -E 'Mac Installer Distribution|3rd Party Mac Developer Installer' "$WORK/pkg.txt" \
      && grep -q 'signed by a certificate trusted' "$WORK/pkg.txt"
  }
  holds "installer package has a trusted Mac installer signature" package_signed
fi

# Keep the scanner's findings and tool errors visible in the build log.
scan_status=0
sh "$(dirname "$0")/check-no-network.sh" --binary-only "$APP" || scan_status=$?
case "$scan_status" in
  0) ;;
  1) fail "network scan flagged a listed import or library (see output above)" ;;
  *) echo "network scan could not complete (exit $scan_status; see error above)" >&2
     exit "$scan_status" ;;
esac

echo
if [ "$FAILS" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$FAILS check(s) failed"
  exit 1
fi
