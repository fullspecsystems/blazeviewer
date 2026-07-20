#!/usr/bin/env bash
# Publish the Blaze Viewer Linux AppImage(s) to the downloads.blazeviewer.app feed — the Linux
# equivalent of scripts/release-mac-upload.sh. Uploads straight from the build machine (the
# Mac that ran scripts/release-linux-docker.sh), one hop.
#
# For each arch present in dist/ (x86_64 and/or aarch64) it uploads:
#   • BlazeViewer-<version>-<arch>.AppImage      the bundle
#   • BlazeViewer-<version>-<arch>.AppImage.sha256   integrity sidecar (generated if absent)
# then writes a shared latest.json manifest (version + per-arch url/sha256/size) and repoints
# the permanent symlinks BlazeViewer-latest-<arch>.AppImage → the versioned file, so
# /latest/linux (x86_64) and /latest/linux-arm64 (aarch64) serve the newest.
#
# Idempotent: re-running with the same build re-uploads and re-points to the same targets.
#
# Usage:
#   scripts/release-linux-upload.sh                 # every BlazeViewer-<ver>-*.AppImage in dist/
#   scripts/release-linux-upload.sh --source <dir>  # from another folder
set -euo pipefail

SOURCE="dist"
UPLOAD_HOST="jdlien.com"
REMOTE_DIR="/var/www/downloads.blazeviewer.app/linux"
BASE_URL="https://downloads.blazeviewer.app/linux"
while [[ $# -gt 0 ]]; do
	case "$1" in
		--source) SOURCE="$2"; shift 2 ;;
		--host) UPLOAD_HOST="$2"; shift 2 ;;
		--remote-dir) REMOTE_DIR="$2"; shift 2 ;;
		-*) echo "unknown flag: $1" >&2; exit 2 ;;
		*) echo "unexpected arg: $1" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
VERSION="$(grep -m1 '^version' crates/pb-app/Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
SRC_DIR="$(cd "$SOURCE" && pwd)"

# 1) Locate the versioned AppImage(s) for this version — never the 'latest' aliases.
declare -a ARCHES FILES
for arch in x86_64 aarch64; do
	f="BlazeViewer-$VERSION-$arch.AppImage"
	if [[ -f "$SRC_DIR/$f" ]]; then ARCHES+=("$arch"); fi
done
[[ ${#ARCHES[@]} -gt 0 ]] || { echo "error: no BlazeViewer-$VERSION-{x86_64,aarch64}.AppImage in $SRC_DIR (build first, or pass --source)" >&2; exit 1; }
echo "==> version $VERSION  arch(es): ${ARCHES[*]}"

# 2) Per-arch sha256 sidecar (generate if missing) + collect the upload set.
sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }
UPLOAD=()
declare -A SHA SIZE
for arch in "${ARCHES[@]}"; do
	f="BlazeViewer-$VERSION-$arch.AppImage"
	if [[ ! -f "$SRC_DIR/$f.sha256" ]]; then
		( cd "$SRC_DIR" && shasum -a 256 "$f" > "$f.sha256" )
	fi
	SHA[$arch]="$(sha_of "$SRC_DIR/$f")"
	SIZE[$arch]="$(wc -c < "$SRC_DIR/$f" | tr -d ' ')"
	UPLOAD+=("$f" "$f.sha256")
	echo "    $f  (${SIZE[$arch]} bytes, sha256 ${SHA[$arch]:0:12}…)"
done

# 3) latest.json — a shared manifest an in-app updater (or a website button) can read to find
#    the newest build per arch. Written into dist/ then uploaded with the bundles.
{
	echo "{"
	echo "  \"product\": \"Blaze Viewer\","
	echo "  \"version\": \"$VERSION\","
	echo "  \"platform\": \"linux\","
	echo "  \"assets\": {"
	for i in "${!ARCHES[@]}"; do
		arch="${ARCHES[$i]}"; f="BlazeViewer-$VERSION-$arch.AppImage"
		comma=","; [[ $i -eq $((${#ARCHES[@]} - 1)) ]] && comma=""
		echo "    \"$arch\": {"
		echo "      \"file\": \"$f\","
		echo "      \"url\": \"$BASE_URL/$f\","
		echo "      \"sha256\": \"${SHA[$arch]}\","
		echo "      \"size\": ${SIZE[$arch]}"
		echo "    }$comma"
	done
	echo "  }"
	echo "}"
} > "$SRC_DIR/latest.json"
UPLOAD+=("latest.json")

# 4) scp the file set (bare names from the source dir).
echo "==> scp ${#UPLOAD[@]} file(s) -> ${UPLOAD_HOST}:${REMOTE_DIR}/"
( cd "$SRC_DIR" && scp "${UPLOAD[@]}" "${UPLOAD_HOST}:${REMOTE_DIR}/" )

# 5) Repoint the permanent per-arch symlinks (relative targets, valid within the dir).
for arch in "${ARCHES[@]}"; do
	f="BlazeViewer-$VERSION-$arch.AppImage"
	echo "==> repoint BlazeViewer-latest-$arch.AppImage -> $f"
	ssh -o BatchMode=yes "$UPLOAD_HOST" "cd '$REMOTE_DIR' && ln -sfn '$f' 'BlazeViewer-latest-$arch.AppImage'"
done

# 6) Verify the permanent URLs serve this build. latest/linux* are 302 → the AppImage; follow.
echo "==> Live:"
for pair in "x86_64:latest/linux" "aarch64:latest/linux-arm64"; do
	arch="${pair%%:*}"; path="${pair#*:}"
	[[ " ${ARCHES[*]} " == *" $arch "* ]] || continue
	code="$(curl -sL -o /dev/null -w '%{http_code}' "https://downloads.blazeviewer.app/$path" || true)"
	[[ "$code" == "200" ]] || { echo "error: /$path returned HTTP $code" >&2; exit 1; }
	echo "    https://downloads.blazeviewer.app/$path   (HTTP 200, $arch)"
done
mcode="$(curl -sL -o /dev/null -w '%{http_code}' "$BASE_URL/latest.json" || true)"
[[ "$mcode" == "200" ]] || { echo "error: latest.json returned HTTP $mcode" >&2; exit 1; }
echo "    $BASE_URL/latest.json   (HTTP 200, manifest)"

# 7) Sync the version printed on blazeviewer.app/download. Idempotent, so if the mac
#    upload already ran this is a no-op; it is here so a Linux-only release still
#    updates the page. Never fatal — see the same call in release-mac-upload.sh.
if ! "$REPO_ROOT/scripts/update-site-version.sh"; then
	echo "warning: download page version NOT updated — bump it by hand:" >&2
	echo "         scripts/update-site-version.sh" >&2
fi
