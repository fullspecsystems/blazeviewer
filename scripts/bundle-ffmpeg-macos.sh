#!/usr/bin/env bash
# Bundle FFmpeg dylibs into the .app so it runs with NO external FFmpeg
# (no Homebrew, no /usr/local) — task #84 plan §9-dist / §7 distribution.
#
# Pipeline:
#   1. Copy the FFmpeg dylibs (libav*, libsw*) from --libdir into Contents/Frameworks,
#      each under its soname (the LC_ID_DYLIB basename the app binary loads).
#   2. Rewrite install names to `@rpath`: each dylib's own id, its inter-lib
#      dependencies, and every FFmpeg reference in the app binary's load commands.
#   3. Ensure the binary carries an `@executable_path/../Frameworks` rpath
#      (Package.swift already adds one for Sparkle — this is a guard).
#   4. Ad-hoc re-sign the mutated Mach-O (install_name_tool invalidates the
#      signature; arm64 won't launch unsigned). release-macos.sh re-signs with the
#      Developer ID over top — bundling MUST run before that signing step, and the
#      FFmpeg dylibs must be signed inside-out (before the app binary).
#   5. AUDIT: fail if any load command in the binary or a bundled dylib resolves
#      OUTSIDE the bundle/system (the plan's "automated failure if any dep resolves
#      outside the bundle"). This is the real ship gate.
#
# Source the dylibs from scripts/build-ffmpeg-macos.sh's LGPL build for a SHIPPABLE
# bundle. Pointing --libdir at Homebrew's lib works for testing the MECHANICS but
# yields a NON-redistributable bundle (Homebrew FFmpeg is GPL) — the audit passes
# either way; licensing is on you.
#
# Usage:
#   scripts/bundle-ffmpeg-macos.sh <"Blaze Viewer.app"> --libdir <dir> [--no-sign] [--audit-only]
set -euo pipefail

APP=""
LIBDIR=""
SIGN=1
AUDIT_ONLY=0
while [[ $# -gt 0 ]]; do
	case "$1" in
		--libdir) LIBDIR="$2"; shift 2 ;;
		--no-sign) SIGN=0; shift ;;
		--audit-only) AUDIT_ONLY=1; shift ;;
		-*) echo "unknown flag: $1" >&2; exit 2 ;;
		*) APP="$1"; shift ;;
	esac
done

[[ -n "$APP" && -d "$APP" ]] || { echo "error: pass the path to the .app bundle" >&2; exit 2; }
# Read the executable's name out of the bundle rather than hardcoding it. It contains a
# space ("Blaze Viewer") and it has already been renamed once (task #101) — deriving it
# means a future rename can never silently break this audit.
BIN_NAME="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Contents/Info.plist" 2>/dev/null || true)"
[[ -n "$BIN_NAME" ]] || { echo "error: no CFBundleExecutable in $APP/Contents/Info.plist" >&2; exit 1; }
BIN="$APP/Contents/MacOS/$BIN_NAME"
FRAMEWORKS="$APP/Contents/Frameworks"
[[ -x "$BIN" ]] || { echo "error: $BIN not found" >&2; exit 1; }

# The FFmpeg libs the app links (soname majors — must match ffmpeg-sys-next 8.1).
FF_LIBS=(libavcodec libavformat libavutil libswscale libswresample)

# ---- allowed load-command prefixes for the audit -------------------------------
is_allowed() {
	case "$1" in
		@rpath/*|@executable_path/*|@loader_path/*) return 0 ;;
		/usr/lib/*|/System/Library/*) return 0 ;;
		*) return 1 ;;
	esac
}

audit_macho() {
	# $1 = Mach-O path, $2 = human label. Prints offenders, returns 1 if any.
	local file="$1" label="$2" bad=0 dep
	while IFS= read -r dep; do
		[[ -z "$dep" ]] && continue
		if ! is_allowed "$dep"; then
			echo "  ✗ $label -> $dep" >&2
			bad=1
		elif [[ "$dep" == @rpath/* ]]; then
			# An @rpath dep must actually be present in Frameworks.
			local base="${dep#@rpath/}"
			if [[ "$base" == libav* || "$base" == libsw* ]] && [[ ! -f "$FRAMEWORKS/$base" ]]; then
				echo "  ✗ $label -> $dep (not in Frameworks/)" >&2
				bad=1
			fi
		fi
	done < <(otool -L "$file" | tail -n +2 | awk '{print $1}')
	return $bad
}

run_audit() {
	echo "==> Auditing dependency closure (no external FFmpeg allowed)"
	local fail=0
	audit_macho "$BIN" "$BIN_NAME" || fail=1
	for f in "$FRAMEWORKS"/libav*.dylib "$FRAMEWORKS"/libsw*.dylib; do
		[[ -f "$f" ]] || continue
		audit_macho "$f" "$(basename "$f")" || fail=1
	done
	if [[ $fail == 1 ]]; then
		echo "==> AUDIT FAILED: the app depends on FFmpeg (or other libs) outside the bundle." >&2
		return 1
	fi
	echo "==> Audit OK: every FFmpeg dependency resolves inside the bundle or the system."
}

if [[ $AUDIT_ONLY == 1 ]]; then
	run_audit
	exit $?
fi

[[ -n "$LIBDIR" && -d "$LIBDIR" ]] || { echo "error: --libdir <dir with libav*.dylib> required" >&2; exit 2; }
mkdir -p "$FRAMEWORKS"

# 1) Copy each FFmpeg dylib under its soname. `cp -L` follows the bare-name symlink
# (libavcodec.dylib -> libavcodec.NN.MM.PP.dylib) to the real file; the soname is the
# dylib's own id basename (e.g. libavcodec.62.dylib) — the name the binary/inter-deps load.
echo "==> Copying FFmpeg dylibs from $LIBDIR"
declare -a BUNDLED=()
for name in "${FF_LIBS[@]}"; do
	src="$LIBDIR/$name.dylib"
	[[ -e "$src" ]] || { echo "error: $src not found (build with scripts/build-ffmpeg-macos.sh)" >&2; exit 1; }
	soname="$(basename "$(otool -D "$src" | tail -1)")"
	[[ -n "$soname" ]] || soname="$name.dylib"
	cp -Lf "$src" "$FRAMEWORKS/$soname"
	chmod u+w "$FRAMEWORKS/$soname"
	BUNDLED+=("$soname")
	echo "  $name -> Frameworks/$soname"
done

# 2) Rewrite install names to @rpath.
echo "==> Rewriting install names to @rpath"
for soname in "${BUNDLED[@]}"; do
	f="$FRAMEWORKS/$soname"
	install_name_tool -id "@rpath/$soname" "$f"
	# Point any inter-lib dependency at its bundled soname, whatever the source
	# path was (@rpath already, Homebrew absolute, or the build prefix).
	while IFS= read -r dep; do
		depbase="$(basename "$dep")"
		for other in "${BUNDLED[@]}"; do
			if [[ "$depbase" == "$other" && "$dep" != "@rpath/$other" ]]; then
				install_name_tool -change "$dep" "@rpath/$other" "$f"
			fi
		done
	done < <(otool -L "$f" | tail -n +2 | awk '{print $1}')
done

# 3) Point the app binary's FFmpeg load commands at @rpath, and ensure the rpath.
echo "==> Repointing the app binary + verifying rpath"
while IFS= read -r dep; do
	depbase="$(basename "$dep")"
	for soname in "${BUNDLED[@]}"; do
		if [[ "$depbase" == "$soname" && "$dep" != "@rpath/$soname" ]]; then
			install_name_tool -change "$dep" "@rpath/$soname" "$BIN"
		fi
	done
done < <(otool -L "$BIN" | tail -n +2 | awk '{print $1}')
if ! otool -l "$BIN" | grep -A2 LC_RPATH | grep -q "@executable_path/../Frameworks"; then
	echo "  adding missing rpath @executable_path/../Frameworks"
	install_name_tool -add_rpath "@executable_path/../Frameworks" "$BIN"
fi

# 4) Ad-hoc re-sign the mutated Mach-O (release re-signs with the Developer ID).
if [[ $SIGN == 1 ]]; then
	echo "==> Ad-hoc signing mutated Mach-O (release re-signs inside-out with Developer ID)"
	for soname in "${BUNDLED[@]}"; do
		codesign --force --sign - "$FRAMEWORKS/$soname"
	done
	codesign --force --sign - "$BIN"
fi

# 5) Audit.
echo
run_audit
