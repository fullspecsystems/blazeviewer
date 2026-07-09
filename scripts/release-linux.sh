#!/usr/bin/env bash
#
# Build a self-contained PhotoBlaze **AppImage** for Linux.
#
# Philosophy (matches the AppImage excludelist): bundle the *specialized* libraries a
# desktop won't already have — libheif (+ its dlopen'd HEVC/AV1 plugins), FFmpeg, and the
# AV1/HEVC codecs — while leaving the ~universal system stack (glibc, GTK, Mesa/GL, X11,
# Wayland) to the host. The result is one executable file the user downloads, marks
# executable, and runs: no `apt install`, no dependency hunt.
#
# Full-feature build: `libheif` (HEIC/AVIF stills) + `livephoto` (Live Photo motion/audio
# and animated AVIF/HEIC, via system FFmpeg). Live Photo *audio* additionally needs
# `pw-cat` (PipeWire) on the user's PATH at runtime — present on any modern desktop, and
# it degrades to silent motion if absent, so it isn't bundled.
#
# Usage:  ./scripts/release-linux.sh
# Output: dist/PhotoBlaze-<version>-<arch>.AppImage
#
# NOTE: the AppImage is built for the *host* architecture. Run it on an x86_64 box to
# produce the x86_64 artifact (and on aarch64 for the arm64 one); there is no cross-build.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
VERSION="$(grep -m1 '^version' crates/pb-app/Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
ARCH="$(uname -m)"
DIST="$ROOT/dist"
APPDIR="$DIST/PhotoBlaze.AppDir"
TOOLS="$DIST/appimage-tools"
OUTPUT="PhotoBlaze-$VERSION-$ARCH.AppImage"

echo ">> PhotoBlaze $VERSION  ($ARCH)  ->  $OUTPUT"

# Running an AppImage-packaged tool *inside* this build (linuxdeploy, appimagetool) can nest
# FUSE mounts awkwardly (especially in a VM), so extract-and-run sidesteps FUSE entirely.
export APPIMAGE_EXTRACT_AND_RUN=1

# ── 1. Build the full-feature release binary ─────────────────────────────────────────
cargo build --release -p pb-app --features livephoto,pb-decode/libheif

# ── 2. Assemble the AppDir skeleton ──────────────────────────────────────────────────
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps"
cp target/release/photoblaze "$APPDIR/usr/bin/photoblaze"

# Icon (AppImage wants a top-level <name>.png plus the hicolor path).
cp icons/photoblaze-icon-v3-windows.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/photoblaze.png"
cp icons/photoblaze-icon-v3-windows.png "$APPDIR/photoblaze.png"

# Desktop entry (the app's identity + the image types it opens).
DESKTOP="$APPDIR/usr/share/applications/photoblaze.desktop"
cat > "$DESKTOP" <<'EOF'
[Desktop Entry]
Type=Application
Name=PhotoBlaze
GenericName=Photo Viewer
Comment=A blazing-fast, keyboard-driven photo viewer
Exec=photoblaze %F
Icon=photoblaze
Categories=Graphics;Viewer;Photography;
Terminal=false
MimeType=image/jpeg;image/png;image/gif;image/webp;image/avif;image/heic;image/heif;image/tiff;image/bmp;image/x-adobe-dng;image/jxl;image/svg+xml;
EOF
cp "$DESKTOP" "$APPDIR/photoblaze.desktop"

# Custom launcher: point libheif at the bundled plugins and our lib dir at load time.
# Written *outside* the AppDir — linuxdeploy copies it in as AppRun (copying it onto itself
# would fail), so it must not already sit at the destination path.
APPRUN_TMPL="$DIST/AppRun.template"
cat > "$APPRUN_TMPL" <<'EOF'
#!/bin/bash
HERE="$(dirname "$(readlink -f "${0}")")"
export LD_LIBRARY_PATH="$HERE/usr/lib:${LD_LIBRARY_PATH:-}"
# libheif dlopen's its HEVC/AV1 decoders (libde265, …) at runtime — tell it where they are.
export LIBHEIF_PLUGIN_PATH="$HERE/usr/lib/libheif/plugins"
exec "$HERE/usr/bin/photoblaze" "$@"
EOF
chmod +x "$APPRUN_TMPL"

# ── 3. Bundle the deps linuxdeploy CAN see (walks the binary's NEEDED libs) ───────────
LD="$TOOLS/linuxdeploy.AppImage"
if [ ! -x "$LD" ]; then
  echo ">> fetching linuxdeploy"
  mkdir -p "$TOOLS"
  curl -fL --retry 3 -o "$LD" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
  chmod +x "$LD"
fi
"$LD" --appdir "$APPDIR" --custom-apprun "$APPRUN_TMPL"

# ── 4. Bundle libheif's dlopen'd plugins (invisible to ldd/linuxdeploy) + their deps ──
# Find the system libheif plugin dir and copy every plugin; then pull each plugin's own
# NEEDED libs (e.g. libde265, libaom) into usr/lib so the dlopen resolves inside the bundle.
HEIF_SO="$(ldconfig -p | awk '/libheif\.so/{print $NF; exit}')"
PLUGIN_DIR="$(dirname "$HEIF_SO")/libheif/plugins"
if [ -d "$PLUGIN_DIR" ]; then
  echo ">> bundling libheif plugins from $PLUGIN_DIR"
  mkdir -p "$APPDIR/usr/lib/libheif/plugins"
  cp -v "$PLUGIN_DIR"/*.so "$APPDIR/usr/lib/libheif/plugins/"
  # Copy each plugin's NEEDED shared libs (resolve symlinks to the real .so file).
  for plug in "$APPDIR/usr/lib/libheif/plugins/"*.so; do
    ldd "$plug" 2>/dev/null | awk '/=>/{print $3}' | while read -r dep; do
      case "$dep" in
        */libde265*|*/libaom*|*/libx265*|*/libdav1d*|*/libheif*)
          real="$(readlink -f "$dep")"
          cp -n "$real" "$APPDIR/usr/lib/$(basename "$real")" 2>/dev/null || true
          # Preserve the SONAME symlink the loader looks for.
          ln -sf "$(basename "$real")" "$APPDIR/usr/lib/$(basename "$dep")" 2>/dev/null || true
          ;;
      esac
    done
  done
else
  echo "!! WARNING: no libheif plugin dir found ($PLUGIN_DIR) — HEIC/HEVC decode may fail in the bundle"
fi

# ── 5. Package the AppDir into a single .AppImage ─────────────────────────────────────
PKG="$TOOLS/linuxdeploy-plugin-appimage.AppImage"
if [ ! -x "$PKG" ]; then
  echo ">> fetching linuxdeploy-plugin-appimage"
  curl -fL --retry 3 -o "$PKG" \
    "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-$ARCH.AppImage"
  chmod +x "$PKG"
fi
( cd "$DIST" && OUTPUT="$OUTPUT" VERSION="$VERSION" "$PKG" --appdir "$APPDIR" )

echo ""
echo ">> built  dist/$OUTPUT"
ls -lh "$DIST/$OUTPUT"
