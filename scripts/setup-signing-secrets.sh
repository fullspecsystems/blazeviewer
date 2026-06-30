#!/usr/bin/env bash
# Verify a Developer ID Application .p12 and set this repo's macOS signing/notarization
# GitHub secrets. Reads the .p12 password and the app-specific password securely with
# `read -rs` — they're never echoed and never land in argv or shell history.
#
# Run it yourself in a terminal (it needs to prompt):
#   ./scripts/setup-signing-secrets.sh ~/Downloads/certs/devid-fresh.p12
#
# Requires: openssl, gh (authenticated for this repo). Sets: CSC_LINK, CSC_KEY_PASSWORD,
# APPLE_TEAM_ID (auto-detected from the cert), APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD.
set -euo pipefail

P12="${1:-}"
[[ -n "$P12" && -f "$P12" ]] || { echo "usage: $0 <path-to-developer-id-application.p12>" >&2; exit 1; }
command -v gh   >/dev/null || { echo "error: gh CLI not found" >&2; exit 1; }
command -v openssl >/dev/null || { echo "error: openssl not found" >&2; exit 1; }
REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"

# OpenSSL 3 needs -legacy to read Keychain-exported .p12s (old RC2/3DES); LibreSSL doesn't
# know the flag — detect which we have.
LEGACY=""; openssl pkcs12 -help 2>&1 | grep -q -- -legacy && LEGACY="-legacy"

read -rsp "Password for $(basename "$P12"): " P12PW; echo
CERT="$(openssl pkcs12 -in "$P12" -nokeys -clcerts $LEGACY -passin "pass:$P12PW" 2>/dev/null || true)"
[[ -n "$CERT" ]] || { echo "✗ Could not decrypt the .p12 — wrong password, or not a PKCS#12 file." >&2; exit 1; }

SUBJECT="$(printf '%s' "$CERT" | openssl x509 -noout -subject -nameopt RFC2253 2>/dev/null)"
ENDDATE="$(printf '%s' "$CERT" | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)"
echo "  $SUBJECT"
echo "  expires: $ENDDATE"

printf '%s' "$SUBJECT" | grep -q "Developer ID Application" || {
	echo "✗ Not a 'Developer ID Application' certificate — that's the one notarized DMG" >&2
	echo "  distribution needs (not 'Developer ID Installer' or 'Apple Distribution')." >&2
	echo "  Try another .p12, or create one at developer.apple.com → Certificates." >&2
	exit 1
}
# Reject an expired cert early (compare its notAfter to now).
if ! printf '%s' "$CERT" | openssl x509 -noout -checkend 0 >/dev/null 2>&1; then
	echo "✗ This certificate has EXPIRED ($ENDDATE). Use a current one." >&2; exit 1
fi
# Team ID = the OU in the subject (also the (XXXXXXXXXX) suffix on the common name).
TEAM="$(printf '%s' "$SUBJECT" | grep -oE 'OU=[A-Z0-9]{10}' | head -1 | cut -d= -f2)"
echo "✓ Valid Developer ID Application cert. Team ID: ${TEAM:-<not found>}"
echo ""
read -rp "Set the 5 macOS secrets on $REPO from this cert? [y/N] " ok
[[ "$ok" == [yY] ]] || { echo "Aborted — nothing changed."; exit 0; }

# CSC_LINK = single-line base64 of the .p12; CSC_KEY_PASSWORD = its password.
base64 < "$P12" | tr -d '\n' | gh secret set CSC_LINK --repo "$REPO"
printf '%s' "$P12PW" | gh secret set CSC_KEY_PASSWORD --repo "$REPO"
[[ -n "$TEAM" ]] && printf '%s' "$TEAM" | gh secret set APPLE_TEAM_ID --repo "$REPO"

read -rp "Apple ID email [jd@jdlien.com]: " AID; AID="${AID:-jd@jdlien.com}"
printf '%s' "$AID" | gh secret set APPLE_ID --repo "$REPO"
echo "App-specific password — make a FRESH one at appleid.apple.com → Sign-In & Security"
echo "→ App-Specific Passwords (and rotate any you've committed elsewhere)."
read -rsp "App-specific password: " ASP; echo
[[ -n "$ASP" ]] && printf '%s' "$ASP" | gh secret set APPLE_APP_SPECIFIC_PASSWORD --repo "$REPO"

echo ""
echo "✓ Secrets set on $REPO:"
gh secret list --repo "$REPO" | grep -E 'CSC_LINK|CSC_KEY_PASSWORD|APPLE_' || true
