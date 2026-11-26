#!/bin/sh
# macOS. With no arguments, scan both source configurations on Intel and Apple silicon.
# An app argument also scans its binary. --binary-only APP skips the source checks.
# --mas-source checks the native Store configuration in the checkout used for packaging.
# Exit 0 for a completed clean scan, 1 for listed matches, 2 for incomplete input.
set -eu

incomplete() { printf 'Scan incomplete: %s\n' "$*" >&2; exit 2; }

mode=all
case "${1:-}" in
  --binary-only) mode=binary; shift ;;
  --mas-source) mode=mas; shift ;;
esac
APP="${1:-}"
[ "$#" -le 1 ] || incomplete "unexpected arguments"
case "$mode" in
  binary) [ -n "$APP" ] || incomplete "--binary-only needs an app" ;;
  mas) [ "$#" -eq 0 ] || incomplete "--mas-source takes no app" ;;
esac
case "$APP" in
  ""|/*) ;;
  *) APP="$PWD/$APP" ;;
esac
if [ -n "$APP" ] && [ ! -d "$APP" ]; then incomplete "no such bundle: $APP"; fi

WORK="$(mktemp -d /tmp/nudge-network-check.XXXXXX)" || incomplete "could not create working directory"
trap 'rm -rf "$WORK"' EXIT
findings=0

# grep returns 1 for no match and >1 for a read or pattern error.
scan_list() {
  if grep -E "$2" "$3"; then
    printf 'FAIL  Listed %s found (see matches above)\n' "$1"
    findings=1
  else
    status=$?
    [ "$status" -eq 1 ] || incomplete "could not scan $1"
    printf 'PASS  No listed %s found\n' "$1"
  fi
}

scan_tree() {
  label="$1"
  shift
  cargo tree --locked --offline --manifest-path src-tauri/Cargo.toml --edges normal \
    --prefix none --format '{p}' "$@" > "$WORK/tree.txt" || incomplete "could not read the offline graph for $label"
  # With no tree prefixes, every line starts with a package name and version.
  # Require the Nudge root and its Tauri dependency before accepting the graph.
  awk '
    NR == 1 && $1 == "nudge" { root = 1 }
    $1 == "tauri" { tauri = 1 }
    $1 !~ /^[A-Za-z0-9_-]+$/ || $2 !~ /^v[0-9]/ { malformed = 1 }
    END { exit !(root && tauri && !malformed) }
  ' "$WORK/tree.txt" || incomplete "unexpected dependency graph for $label"
  scan_list "networking crates ($label)" \
    '^(reqwest|hyper|hyper-util|h2|ureq|attohttpc|isahc|surf|curl|curl-sys|rustls|openssl|openssl-sys|native-tls|tungstenite|socket2|mio)[[:space:]]' "$WORK/tree.txt"
}

if [ "$mode" != binary ]; then
  cd "$(dirname "$0")/.." || incomplete "could not enter the source checkout"
  if ! command -v cargo >/dev/null 2>&1; then
    case "$(uname -m)" in
      arm64) arch=aarch64 ;;
      x86_64) arch=x86_64 ;;
      *) incomplete "cargo is unavailable" ;;
    esac
    PATH="$HOME/.rustup/toolchains/stable-$arch-apple-darwin/bin:$PATH"
    export PATH
  fi
  if [ "$mode" = mas ]; then
    scan_tree "Store build checkout" --no-default-features --features mas
  else
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
      scan_tree "$target, default" --target "$target"
      scan_tree "$target, Store" --target "$target" --no-default-features --features mas
    done
  fi
fi

if [ -n "$APP" ]; then
  executable="$(plutil -extract CFBundleExecutable raw "$APP/Contents/Info.plist")" || incomplete "missing executable name"
  BIN="$APP/Contents/MacOS/$executable"
  nm -u "$BIN" > "$WORK/imports.txt" || incomplete "nm failed on $BIN"
  grep -Eq '^[[:space:]]*_[^[:space:]]+' "$WORK/imports.txt" || incomplete "no recognizable imports in $BIN"
  scan_list "networking imports" '^[[:space:]]*_(connect|getaddrinfo|socket)$|NSURLSession|NSURLConnection|CFSocket|CFStream|CFHTTP|nw_connection' "$WORK/imports.txt"

  otool -L "$BIN" > "$WORK/libs.txt" || incomplete "otool failed on $BIN"
  grep -Eq '^[[:space:]]+.*\(compatibility version ' "$WORK/libs.txt" || incomplete "no recognizable linked libraries in $BIN"
  scan_list "linked networking libraries" '^[[:space:]]+.*(CFNetwork|Network\.framework|libcurl|libssl|libcrypto|libnghttp)' "$WORK/libs.txt"
fi

exit "$findings"
