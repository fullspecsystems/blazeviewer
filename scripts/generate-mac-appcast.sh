#!/usr/bin/env bash
# Generate the EdDSA-signed Sparkle appcast for the macOS auto-updater (task #65).
#
# Sparkle rides the existing notarized DMG on downloads.fullspec.ca via this appcast.xml,
# hosted next to the DMG (SUFeedURL in Info-swift-host.plist). release-macos.sh calls this
# after notarizing + stapling; release-mac-upload.ps1 uploads the appcast alongside the DMG.
#
# We emit a SINGLE-item appcast (the newest release). Sparkle offers the newest item that is
# newer than the running build, so one item updates any older version in a single hop — no
# need to accumulate history here. (If per-version release notes on the feed are ever wanted,
# switch to Sparkle's `generate_appcast` over a directory that retains old DMGs.)
#
# The enclosure signature is made with the PRIVATE EdDSA key in the release Mac's login
# keychain (generated once via Sparkle's generate_keys; the matching SUPublicEDKey ships in
# Info.plist). sign_update reads that key automatically.
#
# Usage: scripts/generate-mac-appcast.sh <dmg> <version> [out.xml]
#   <dmg>      path to the notarized PhotoBlaze-<version>.dmg
#   <version>  the release version (matches CFBundleVersion / CFBundleShortVersionString)
#   [out.xml]  output path (default: dist/appcast.xml)
set -euo pipefail

DMG="${1:?usage: generate-mac-appcast.sh <dmg> <version> [out.xml]}"
VERSION="${2:?missing <version>}"
OUT="${3:-dist/appcast.xml}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

[[ -f "$DMG" ]] || { echo "error: DMG not found: $DMG" >&2; exit 1; }

# Sparkle's sign_update lives in the SPM binary artifacts (present after a swift build, which
# release-macos.sh always runs first). It prints the enclosure attributes we splice in:
#   sparkle:edSignature="…" length="…"
SIGN="$(find mac/.build/artifacts -name sign_update -path '*bin*' 2>/dev/null | head -1)"
[[ -x "$SIGN" ]] || { echo "error: sign_update not found under mac/.build/artifacts (build the swift host first)" >&2; exit 1; }

SIG_ATTRS="$("$SIGN" "$DMG")"   # e.g. sparkle:edSignature="AbC…" length="10485760"
[[ "$SIG_ATTRS" == *edSignature* ]] || { echo "error: sign_update produced no signature (is the EdDSA private key in your keychain?)" >&2; exit 1; }

DMG_NAME="$(basename "$DMG")"
# The base the <enclosure> URL is built on. Defaults to the production feed; the local
# Sparkle staging test (scripts/test-sparkle-update.sh) overrides it to a localhost server.
BASE_URL="${PB_APPCAST_BASE_URL:-https://downloads.fullspec.ca/photoblaze/mac}"
ENCLOSURE_URL="$BASE_URL/$DMG_NAME"
PUB_DATE="$(date -u "+%a, %d %b %Y %H:%M:%S +0000")"

# Release notes: the CHANGELOG section for this version, wrapped as HTML (Sparkle renders the
# <description> in a WebView). A <pre> block keeps the Keep-a-Changelog layout readable without
# a markdown→HTML step; escape the three XML-significant chars first.
NOTES="$(bash scripts/changelog-section.sh "$VERSION" 2>/dev/null || true)"
NOTES_HTML="$(printf '%s' "$NOTES" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g')"

mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<XML
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>PhotoBlaze</title>
    <link>https://downloads.fullspec.ca/photoblaze/mac/appcast.xml</link>
    <description>PhotoBlaze updates for macOS.</description>
    <language>en</language>
    <item>
      <title>Version $VERSION</title>
      <pubDate>$PUB_DATE</pubDate>
      <sparkle:version>$VERSION</sparkle:version>
      <sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
      <description><![CDATA[<h2>Version $VERSION</h2><pre>$NOTES_HTML</pre>]]></description>
      <enclosure url="$ENCLOSURE_URL" type="application/octet-stream" $SIG_ATTRS />
    </item>
  </channel>
</rss>
XML

echo "==> Wrote $OUT"
echo "    enclosure: $ENCLOSURE_URL"
echo "    $SIG_ATTRS"
