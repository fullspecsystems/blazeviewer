#!/usr/bin/env bash
# Shared release gate — **sourced**, never executed. Used by release-macos.sh and
# release-linux.sh; release-windows.ps1 carries the same logic inline (PowerShell can't
# source this).
#
# WHY THIS EXISTS. `crates/pb-app/build.rs` stamps the build id `-dirty` on any
# `git status --porcelain` output and that ships in the About dialog. Everyone knows to
# check `git status` first — and that is **not enough**, which is how 0.2.1 shipped dirty
# on its first attempt despite a tree verified clean minutes earlier:
#
#   1. bump crates/pb-app/Cargo.toml 0.2.0 -> 0.2.1, commit
#   2. `git status` -> clean ✅
#   3. run the release script
#   4. the release build is the FIRST cargo command since the bump, so *it* rewrites
#      pb-app's version entry in Cargo.lock
#   5. the tree is now dirty **mid-build**, and build.rs — which runs after that — stamps
#      `0.2.1 (abc1234-dirty)` into the artifact
#
# A naive pre-flight `git status` sails straight past this: the tree really was clean when
# it looked. So the gate is two-sided.
#
#   `release_preflight`        — settle Cargo.lock FIRST (run cargo), THEN check. This turns
#                                the mid-build rewrite into an up-front, actionable failure.
#   `assert_build_id_clean`    — check what was actually STAMPED. The preflight is a
#                                precondition and preconditions can be defeated; this checks
#                                the outcome, so it catches causes we haven't thought of.
#                                Call it before signing/notarizing so a doomed build never
#                                costs an Apple round-trip.
#
# Both honour `--allow-dirty` for a deliberate throwaway build (never for a real release).

# release_preflight <allow_dirty:0|1>
release_preflight() {
	local allow_dirty="${1:-0}"

	# Settle Cargo.lock before looking. `cargo metadata` resolves the graph and rewrites the
	# lockfile if the manifests moved — cheap, and it means the `git status` below sees the
	# truth rather than a snapshot that the build is about to invalidate. --offline first so
	# a normal run never touches the network; fall back for a genuinely unresolved graph.
	if command -v cargo >/dev/null 2>&1; then
		cargo metadata --format-version 1 --offline >/dev/null 2>&1 ||
			cargo metadata --format-version 1 >/dev/null 2>&1 || true
	fi

	local dirty
	dirty="$(git status --porcelain 2>/dev/null || true)"
	[[ -z "$dirty" ]] && return 0

	if [[ "$allow_dirty" == "1" ]]; then
		echo "==> WARNING: --allow-dirty — this build WILL be stamped -dirty. Never ship it." >&2
		return 0
	fi

	{
		echo "error: refusing to release from a dirty working tree."
		echo
		echo "$dirty" | sed 's/^/       /'
		echo
		echo "       build.rs stamps the build id -dirty on ANY of the above (untracked"
		echo "       included) and it ships in the About dialog."
		echo
		echo "       If that is only Cargo.lock, this gate just did its job: bumping the"
		echo "       version changes pb-app's lockfile entry, and nothing rewrites it until"
		echo "       cargo runs. Commit it together with the bump."
		echo
		echo "       Commit or .gitignore, then re-run. --allow-dirty overrides (dev only)."
	} >&2
	exit 1
}

# assert_tree_clean_after_build <allow_dirty:0|1>
# The post-build half for targets where reading the artifact's stamped id isn't practical —
# the Linux binary isn't runnable until linuxdeploy bundles its libs, so `--version` is out.
# Catches the same class as assert_build_id_clean: the cargo build is itself what rewrites
# Cargo.lock, so if the tree went dirty during it, it is dirty now.
assert_tree_clean_after_build() {
	local allow_dirty="${1:-0}" dirty
	dirty="$(git status --porcelain 2>/dev/null || true)"
	if [[ -z "$dirty" ]]; then
		echo "==> tree still clean after the build (build id is not -dirty)"
		return 0
	fi
	if [[ "$allow_dirty" == "1" ]]; then
		echo "==> WARNING: tree went dirty during the build (--allow-dirty). Never ship it." >&2
		return 0
	fi
	{
		echo "error: the tree went dirty DURING the build, so the artifact is stamped -dirty."
		echo "       The preflight could not have caught this — it was clean when it looked."
		echo
		echo "$dirty" | sed 's/^/       /'
		echo
		echo "       Commit the above and re-run."
	} >&2
	exit 1
}

# assert_build_id_clean <build_id> <allow_dirty:0|1>
# The real gate: assert what the build ACTUALLY stamped, not what we predicted it would.
assert_build_id_clean() {
	local build_id="$1" allow_dirty="${2:-0}"
	case "$build_id" in
	*-dirty)
		if [[ "$allow_dirty" == "1" ]]; then
			echo "==> WARNING: artifact stamped '$build_id' (--allow-dirty). Never ship it." >&2
			return 0
		fi
		{
			echo "error: the built artifact is stamped '$build_id' — the tree went dirty DURING"
			echo "       the build, so the preflight could not have caught it."
			echo
			echo "       What changed under us:"
			git status --porcelain 2>/dev/null | sed 's/^/       /'
			echo
			echo "       Aborting before signing/notarizing — a dirty build is not worth an"
			echo "       Apple round-trip. Commit the above and re-run."
		} >&2
		exit 1
		;;
	esac
	echo "==> build id: ${build_id:-<no git>} (clean)"
}
