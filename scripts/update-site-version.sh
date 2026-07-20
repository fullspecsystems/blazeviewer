#!/usr/bin/env bash
# Sync the version shown on https://blazeviewer.app/download with this repo's version.
#
# The download page names a version in prose ("Downloads · version 0.3.0"). Nothing
# structural ties that string to a release, so it drifts: on 2026-07-20 the page still
# read 0.2.1 a full day after 0.3.0 shipped, with every download link already serving
# 0.3.0. This script closes that gap by making the bump part of publishing.
#
# The page itself lives in a DIFFERENT repo (blazeviewer.app, deployed to Cloudflare
# Workers on push to main). Pushing here therefore deploys the marketing page.
#
# Design notes, all of which are load-bearing:
#
#   • Idempotent. Already at the target version -> no commit, exit 0. That is what makes
#     it safe to call from every platform's upload script; whichever runs first does the
#     work and the rest are free no-ops. Do not add "only run on macOS" logic.
#
#   • Stages ONE file. `public/index.html` in the site repo is a long-lived Tailwind
#     Play-CDN draft that must not ship. `git commit -a` there would deploy it. This
#     script only ever adds public/download/index.html, by explicit path.
#
#   • Runs AFTER the upload in each caller, never before. The page should not announce a
#     version until its binaries are actually downloadable.
#
#   • A missing site repo is a skip, not a failure — the release is already published and
#     the marketing page is not worth aborting over. But a page that no longer matches the
#     expected pattern is a hard error: silent no-match is the exact drift this exists to
#     prevent, so it must be loud.
#
# Usage:
#   scripts/update-site-version.sh                  # version from crates/pb-app/Cargo.toml
#   scripts/update-site-version.sh --version 0.3.1  # explicit
#   scripts/update-site-version.sh --no-push        # commit locally, don't deploy
#   scripts/update-site-version.sh --dry-run        # report what would change
#
#   BLAZEVIEWER_SITE_DIR=/path/to/blazeviewer-site  # default: ../blazeviewer-site
set -euo pipefail

VERSION=""
DO_PUSH=1
DRY_RUN=0
while [[ $# -gt 0 ]]; do
	case "$1" in
		--version) VERSION="$2"; shift 2 ;;
		--no-push) DO_PUSH=0; shift ;;
		--dry-run) DRY_RUN=1; shift ;;
		-*) echo "unknown flag: $1" >&2; exit 2 ;;
		*) echo "unexpected arg: $1" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 1) Version to publish.
if [[ -z "$VERSION" ]]; then
	VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/crates/pb-app/Cargo.toml" | head -1)"
	[[ -n "$VERSION" ]] || { echo "error: no version in crates/pb-app/Cargo.toml (pass --version)" >&2; exit 1; }
fi
# Guard against a tag-shaped value ("v0.3.0") reaching the page, which reads "version v0.3.0".
VERSION="${VERSION#v}"

# 2) Locate the site repo. Absent is a skip: the release itself is fine.
SITE_DIR="${BLAZEVIEWER_SITE_DIR:-$REPO_ROOT/../blazeviewer-site}"
if [[ ! -d "$SITE_DIR/.git" ]]; then
	echo "==> site repo not found at $SITE_DIR — skipping the version bump on the download page."
	echo "    Set BLAZEVIEWER_SITE_DIR, or update it by hand:"
	echo "    https://github.com/fullspecsystems/blazeviewer.app -> public/download/index.html"
	exit 0
fi
SITE_DIR="$(cd "$SITE_DIR" && pwd)"
PAGE="$SITE_DIR/public/download/index.html"
PAGE_REL="public/download/index.html"
[[ -f "$PAGE" ]] || { echo "error: $PAGE not found — has the page moved?" >&2; exit 1; }

# 3) Read what the page currently claims. No match is a hard error, not a silent skip.
CURRENT="$(sed -n 's/.*Downloads &middot; version \([^<]*\)<.*/\1/p' "$PAGE" | head -1)"
if [[ -z "$CURRENT" ]]; then
	{
		echo "error: could not find the version string in $PAGE_REL."
		echo "       Expected a line matching: Downloads &middot; version <x.y.z>"
		echo
		echo "       The page was probably redesigned. Fix the pattern in this script —"
		echo "       do NOT leave it unmatched, or the page silently stops tracking releases,"
		echo "       which is the failure this script exists to prevent."
	} >&2
	exit 1
fi

if [[ "$CURRENT" == "$VERSION" ]]; then
	echo "==> download page already at $VERSION — nothing to do."
	exit 0
fi

echo "==> download page: $CURRENT -> $VERSION"
if [[ "$DRY_RUN" == "1" ]]; then
	echo "    (--dry-run; no changes written)"
	exit 0
fi

# 4) Rewrite. Via a temp file rather than `sed -i`, whose syntax differs between BSD
#    (the Mac this usually runs on) and GNU.
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
sed "s/\(Downloads &middot; version \)[^<]*</\1$VERSION</" "$PAGE" >"$TMP"
cat "$TMP" >"$PAGE"

# Confirm the write actually took, rather than trusting sed's exit status.
WROTE="$(sed -n 's/.*Downloads &middot; version \([^<]*\)<.*/\1/p' "$PAGE" | head -1)"
[[ "$WROTE" == "$VERSION" ]] || { echo "error: page still reads '$WROTE' after rewrite" >&2; exit 1; }

# 5) Commit — this file and nothing else. See the header: the site repo carries an
#    undeployable draft index.html, so a broad `git add` would ship it.
cd "$SITE_DIR"
git add -- "$PAGE_REL"
if git diff --cached --quiet -- "$PAGE_REL"; then
	echo "==> no staged change (already committed?) — nothing to do."
	exit 0
fi
git commit -q -m "Download page: version $VERSION

Released $VERSION. Bumped by scripts/update-site-version.sh in the blazeviewer
repo, so the page tracks the release instead of being hand-edited after it."
echo "==> committed in $SITE_DIR"

# 6) Push = deploy (Workers Builds on main). Never fatal: the binaries are already live,
#    and a failed push leaves a correct local commit to send by hand.
if [[ "$DO_PUSH" == "0" ]]; then
	echo "==> --no-push; commit is local. \`git -C $SITE_DIR push\` to deploy."
	exit 0
fi
BRANCH="$(git branch --show-current)"
if [[ "$BRANCH" != "main" ]]; then
	echo "==> site repo is on '$BRANCH', not main — not pushing (main is what deploys)." >&2
	exit 0
fi
if git push -q origin main; then
	echo "==> pushed; Cloudflare will deploy https://blazeviewer.app/download shortly."
else
	echo "warning: push failed. The commit is local and correct — push it by hand:" >&2
	echo "         git -C $SITE_DIR push origin main" >&2
fi
