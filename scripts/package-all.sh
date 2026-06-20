#!/usr/bin/env bash
# Build platform-specific VSIXes; each bundles the matching native engine binary.
#
#   scripts/package-all.sh [target ...]   # default: every target
#
# Cross-OS builds need the right toolchain on the host; targets that can't build
# locally are skipped with a warning (CI builds the full matrix on real runners).
#
# macOS binaries are codesigned (+ notarized) when these are set:
#   SIGN_IDENTITY="Developer ID Application: NAME (TEAMID)"
#   APPLE_ID, APPLE_APP_PASSWORD, APPLE_TEAM_ID
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/dist-vsix"
mkdir -p "$OUT"

declare -A TRIPLE=(
    [darwin-arm64]=aarch64-apple-darwin
    [darwin-x64]=x86_64-apple-darwin
    [win32-x64]=x86_64-pc-windows-msvc
    [linux-x64]=x86_64-unknown-linux-gnu
    [linux-arm64]=aarch64-unknown-linux-gnu
)

VERSION="$(node -p "require('./package.json').version")"
TARGETS=("$@")
[ ${#TARGETS[@]} -eq 0 ] && TARGETS=("${!TRIPLE[@]}")

sign_macos() {
    local bin="$1"
    [ -n "${SIGN_IDENTITY:-}" ] || { echo "  (skip signing: SIGN_IDENTITY unset)"; return 0; }
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$bin"
    if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
        ditto -c -k "$bin" "$bin.zip"
        xcrun notarytool submit "$bin.zip" --apple-id "$APPLE_ID" \
            --password "$APPLE_APP_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait
        rm -f "$bin.zip"
    fi
}

for t in "${TARGETS[@]}"; do
    triple="${TRIPLE[$t]:-}"
    [ -n "$triple" ] || { echo "!! unknown target: $t"; continue; }
    echo "== $t ($triple) =="
    rustup target add "$triple" >/dev/null 2>&1 || true
    if ! cargo build --release --manifest-path engine/Cargo.toml --target "$triple"; then
        echo "  !! engine build failed for $triple (missing cross toolchain?) — skipping"
        continue
    fi
    ext=""; case "$t" in win32-*) ext=".exe" ;; esac
    rm -rf bin && mkdir -p bin
    cp "engine/target/$triple/release/getaibd-agent$ext" "bin/getaibd-agent$ext"
    case "$t" in darwin-*) sign_macos "bin/getaibd-agent" ;; esac
    npx --yes @vscode/vsce package --target "$t" -o "$OUT/getaibd-$t-$VERSION.vsix"
done

echo "Done -> $OUT"
ls -lh "$OUT"
