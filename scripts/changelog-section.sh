#!/usr/bin/env bash
# Print the CHANGELOG.md section for one version — the GitHub release body for that version.
# Emits the content between `## [<version>]` and the next `## [` heading, with leading and
# trailing blank lines trimmed. Exits non-zero if the section is empty/missing, so a release
# fails loudly rather than shipping empty notes (roll [Unreleased] into the version first).
#
# Usage: scripts/changelog-section.sh 0.1.0-beta.4
set -euo pipefail

VERSION="${1:?usage: changelog-section.sh <version, e.g. 0.1.0-beta.4>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANGELOG="$REPO_ROOT/CHANGELOG.md"

section="$(awk -v h="## [$VERSION]" '
  index($0, h) == 1 { grab = 1; next }   # start at the version heading (skip it)
  grab && /^## \[/  { exit }              # stop at the next version heading
  grab              { print }
' "$CHANGELOG")"

# Trim leading then trailing blank lines (portable — no tac, runs on macOS + Linux).
section="$(printf '%s\n' "$section" | awk 'NF { p = 1 } p' \
  | awk '{ a[NR] = $0 } END { n = NR; while (n > 0 && a[n] ~ /^[[:space:]]*$/) n--; for (i = 1; i <= n; i++) print a[i] }')"

[[ -n "$section" ]] || { echo "error: no CHANGELOG section for [$VERSION]" >&2; exit 1; }
printf '%s\n' "$section"
