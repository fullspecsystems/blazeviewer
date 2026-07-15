#!/usr/bin/env bash
# Generate the EdDSA-signed Sparkle appcast for the macOS auto-updater (task #65).
#
# Sparkle rides the existing notarized DMG on downloads.blazeviewer.app via this appcast.xml,
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
#   <dmg>      path to the notarized BlazeViewer-<version>.dmg
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
BASE_URL="${PB_APPCAST_BASE_URL:-https://downloads.blazeviewer.app/mac}"
ENCLOSURE_URL="$BASE_URL/$DMG_NAME"
PUB_DATE="$(date -u "+%a, %d %b %Y %H:%M:%S +0000")"

# Release notes: the CHANGELOG section for this version, converted from Keep-a-Changelog
# Markdown to HTML — Sparkle renders <description> as HTML in a WebView, so raw Markdown shows
# its literal `###`/`**`/`-` syntax and a <pre> wrapper preserves every hard-wrapped line (a
# mile-long dialog). This does the small conversion the changelog actually uses (### headings,
# `-` bullets with wrapped continuation lines joined, **bold**, `code`) in pure perl — always
# present on macOS, so the release stays dependency-free (no pandoc/cmark needed).
md_to_html() {
	perl -e '
		my (@out, $in_list, $li, $p);
		sub esc { my $s = shift; $s =~ s/&/&amp;/g; $s =~ s/</&lt;/g; $s =~ s/>/&gt;/g; return $s; }
		# Inline markdown. Bold MUST run before italic: consuming ** first leaves only single
		# * pairs for the em rule, so **bold** never degrades into <em>*bold*</em>. Without the
		# italic rule, *alongside* reached the Sparkle dialog as literal asterisks (0.2.1).
		sub inl { my $s = shift; $s =~ s{\*\*(.+?)\*\*}{<strong>$1</strong>}g; $s =~ s{\*(.+?)\*}{<em>$1</em>}g; $s =~ s{`(.+?)`}{<code>$1</code>}g; return $s; }
		sub flush_li { if (defined $li && length $li) { push @out, "<li>".inl($li)."</li>"; $li=undef; } }
		sub close_list { if ($in_list) { flush_li(); push @out, "</ul>"; $in_list=0; } }
		# A paragraph accumulates its hard-wrapped continuation lines and flushes on a blank
		# line, a heading, or a list — the same continuation the list branch already does for
		# $li. Without this, every wrap in CHANGELOG.md became its own <p>, so Sparkle broke
		# the update dialog mid-sentence at each source line ending (hit on 0.2.1).
		sub flush_p { if (defined $p && length $p) { push @out, "<p>".inl($p)."</p>"; $p=undef; } }
		while (my $l = <STDIN>) {
			chomp $l; $l =~ s/\s+$//;
			if    ($l =~ /^#{1,6}\s+(.*)/) { close_list(); flush_p(); push @out, "<h3>".inl(esc($1))."</h3>"; }
			elsif ($l =~ /^[-*]\s+(.*)/)   { flush_p(); flush_li(); unless ($in_list) { push @out, "<ul>"; $in_list=1; } $li = esc($1); }
			elsif ($l =~ /^\s*$/)          { close_list(); flush_p(); }
			else  { my $t = $l; $t =~ s/^\s+//; if ($in_list) { $li .= " ".esc($t); } else { $p = defined $p ? $p." ".esc($t) : esc($t); } }
		}
		close_list();
		flush_p();
		print join("", @out);
	'
}
# Sparkle shows this in the update dialog, so it wants a SHORT "what's new" — regular users just
# want "oh cool, it does video now", not the full 34-bullet changelog. Prefer the version's
# `### Highlights` subsection (a ~7-line summary); fall back to the entire section for any version
# that predates the convention. The full detail always stays in CHANGELOG.md for the curious, and
# the (manual) GitHub release body still uses the whole section via changelog-section.sh.
SECTION="$(bash scripts/changelog-section.sh "$VERSION" 2>/dev/null)"
HIGHLIGHTS="$(printf '%s\n' "$SECTION" | awk '
	/^###[[:space:]]+[Hh]ighlights[[:space:]]*$/ { grab = 1; next }
	grab && /^###[[:space:]]/ { exit }
	grab { print }
')"
NOTES_HTML="$(printf '%s\n' "${HIGHLIGHTS:-$SECTION}" | md_to_html)"

mkdir -p "$(dirname "$OUT")"
cat > "$OUT" <<XML
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Blaze Viewer</title>
    <link>https://downloads.blazeviewer.app/mac/appcast.xml</link>
    <description>Blaze Viewer updates for macOS.</description>
    <language>en</language>
    <item>
      <title>Version $VERSION</title>
      <pubDate>$PUB_DATE</pubDate>
      <sparkle:version>$VERSION</sparkle:version>
      <sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
      <description><![CDATA[$NOTES_HTML]]></description>
      <enclosure url="$ENCLOSURE_URL" type="application/octet-stream" $SIG_ATTRS />
    </item>
  </channel>
</rss>
XML

echo "==> Wrote $OUT"
echo "    enclosure: $ENCLOSURE_URL"
echo "    $SIG_ATTRS"
