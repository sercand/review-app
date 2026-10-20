#!/usr/bin/env bash
# Render assets/app-icon/AppIcon.svg into assets/app-icon/AppIcon.icns.
#
# Needs rsvg-convert (brew install librsvg) or resvg (cargo install resvg),
# plus iconutil, which ships with macOS.
set -euo pipefail
source "$(dirname "$0")/common.sh"

if command -v rsvg-convert >/dev/null; then
    render() { rsvg-convert -w "$2" -h "$2" "$ICON_SVG" -o "$1"; }
elif command -v resvg >/dev/null; then
    render() { resvg -w "$2" -h "$2" "$ICON_SVG" "$1"; }
else
    die "install rsvg-convert (brew install librsvg) or resvg (cargo install resvg)"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
iconset="$work/AppIcon.iconset"
mkdir "$iconset"

for size in 16 32 128 256 512; do
    render "$iconset/icon_${size}x${size}.png" "$size"
    render "$iconset/icon_${size}x${size}@2x.png" "$((size * 2))"
done

iconutil --convert icns --output "$ICNS_PATH" "$iconset"
log "wrote ${ICNS_PATH#"$ROOT/"}"
