#!/usr/bin/env bash
# Regenerate the committed macOS icon artifacts from the sources in icons/. Run this on
# your Mac whenever the icon design changes (rare — an OS cycle, or a redesign), then
# commit the results. They're consumed directly by scripts/bundle-macos.sh and the CI
# release job, so **CI needs neither Xcode 26 (actool) nor ImageMagick** — the icons are
# baked here, where you have both.
#
# Outputs (commit these):
#   packaging/macos/Assets.car      — Liquid Glass AppIcon for macOS 26+ (from AppIcon.icon)
#   packaging/macos/PhotoBlaze.icns — flat fallback for pre-26 (legacy PNG, outline only)
#
# Requires: Xcode 26+ (`xcrun actool`) and ImageMagick (`magick`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

ICON_BUNDLE="icons/AppIcon.icon"
LEGACY_PNG="icons/photoblaze-icon-macos-legacy.png"
OUT="packaging/macos"
mkdir -p "$OUT"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v magick >/dev/null || { echo "error: ImageMagick (magick) required" >&2; exit 1; }
xcrun --find actool >/dev/null || { echo "error: Xcode 'actool' required (need Xcode 26+)" >&2; exit 1; }

echo "==> Liquid Glass: $ICON_BUNDLE → $OUT/Assets.car (actool)"
xcrun actool "$ICON_BUNDLE" \
	--compile "$TMP" \
	--app-icon AppIcon \
	--output-partial-info-plist "$TMP/partial.plist" \
	--platform macosx \
	--minimum-deployment-target 26.0 \
	--errors --warnings \
	--output-format human-readable-text >/dev/null
[[ -f "$TMP/Assets.car" ]] || { echo "error: actool produced no Assets.car (Xcode 26+?)" >&2; exit 1; }
cp "$TMP/Assets.car" "$OUT/Assets.car"

echo "==> Flat legacy: $LEGACY_PNG → $OUT/PhotoBlaze.icns"
ICONSET="$TMP/PhotoBlaze.iconset"
mkdir -p "$ICONSET"
for s in 16 32 64 128 256 512; do
	sips -z "$s" "$s"             "$LEGACY_PNG" --out "$ICONSET/icon_${s}x${s}.png"    >/dev/null
	sips -z "$((s*2))" "$((s*2))" "$LEGACY_PNG" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$OUT/PhotoBlaze.icns"

echo "==> Done. Commit:"
echo "    $OUT/Assets.car  $OUT/PhotoBlaze.icns"
ls -la "$OUT/Assets.car" "$OUT/PhotoBlaze.icns"
