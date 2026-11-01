#!/bin/bash
# Build a locally signed app. Set MACCER_SIGNING_IDENTITY for a distribution identity.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
    echo "Bundling maccer requires macOS." >&2
    exit 1
fi
profile="${1:-release}"
case "$profile" in
    release) cargo build -p launcher --release ;;
    debug) cargo build -p launcher ;;
    *) echo "Usage: $0 [release|debug]" >&2; exit 1 ;;
esac
target_dir="${CARGO_TARGET_DIR:-target}"
app="$target_dir/$profile/maccer.app"
mkdir -p "$app/Contents/MacOS"
cp "$target_dir/$profile/maccer" "$app/Contents/MacOS/maccer"
cp resources/Info.plist "$app/Contents/Info.plist"
/usr/bin/plutil -lint "$app/Contents/Info.plist"
/usr/bin/codesign --force --sign "${MACCER_SIGNING_IDENTITY:--}" "$app"
/usr/bin/codesign --verify --strict "$app"
echo "Built $app. Move it to ~/Applications or /Applications before enabling Launch at Login."
