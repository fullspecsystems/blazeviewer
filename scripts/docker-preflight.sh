#!/usr/bin/env bash
# Shared container-engine gate — **sourced**, never executed. Used by check-linux.sh and
# release-linux-docker.sh.
#
# WHY THIS EXISTS. Both scripts used to gate on `command -v docker`, which asks the wrong
# question. OrbStack installs its `docker` CLI shim into /usr/local/bin permanently, so the
# CLI is on PATH whether or not the OrbStack VM is running. `command -v` sails straight
# through, and the *next* command — a `docker build` we've already committed to — is what
# discovers the daemon isn't there.
#
# Two failure shapes, and only one of them is survivable:
#
#   • socket file ABSENT  → the CLI fails fast with a clear error. Annoying, recoverable.
#   • socket file PRESENT but nothing serving it (OrbStack stopped uncleanly, or is still
#     booting) → the CLI blocks on connect. No output, no error, no end.
#
# The second one cost nine CPU cores on the Mac CI runner between 2026-07-22 and 08-03. The
# `linux-gate` job entered exactly that state, printed **zero bytes** for 44 minutes, hit its
# `timeout-minutes: 45`, and the runner's cancellation path then leaked a spinning orphan
# `Runner.Worker` fork-stub every single time (9 timeouts, 9 orphans — see
# .taskmaster/docs/self-hosted-runners.md → "Orphaned Runner.Worker processes"). Each stub
# pinned a core at ~90% until killed by hand. The oldest ran for 20 days.
#
# So this gate probes the DAEMON rather than the CLI, under a hard wall-clock cap. It turns a
# 45-minute silent hang into a ~10-second failure that says what to do.
#
# ⚠ Do NOT "simplify" the watchdog below to `timeout 10 docker info`. macOS ships no coreutils
# `timeout(1)` (verified absent on the runner: no `timeout`, no `gtimeout`), so that version
# works on Linux and silently degrades to nothing here — reintroducing the exact hang this
# file exists to prevent.

# _docker_preflight_capped <seconds> <cmd...>
# Run cmd with a hard wall-clock cap, discarding its output. Returns the command's own exit
# status, or 124 (timeout(1)'s convention) if the cap was reached and the command was killed.
_docker_preflight_capped() {
	local cap_tenths=$(( ${1:?_docker_preflight_capped: seconds required} * 10 ))
	shift
	local waited=0 pid rc=0 had_monitor=0

	# Launch under job control so the probe becomes its own process-group leader (pgid == pid).
	# That is what makes the kill below reach the WHOLE tree: signalling just the direct child
	# leaves its descendants running, which is how a `docker` CLI's helper processes would
	# survive the cap and keep the leak this file exists to prevent. Verified: without this,
	# killing a wrapper left its `sleep` child alive.
	[[ $- == *m* ]] && had_monitor=1
	set -m
	"$@" >/dev/null 2>&1 &
	pid=$!
	(( had_monitor )) || set +m

	# Poll rather than `wait` so we keep control of the clock. 0.1s granularity: fine for a
	# 10s cap, and cheap next to the multi-minute build this gates.
	while kill -0 "$pid" 2>/dev/null; do
		if (( waited >= cap_tenths )); then
			# Negative PID = "the process group". Fall back to the bare pid if the group is
			# already gone, so we never skip the kill entirely.
			kill -9 -- "-$pid" 2>/dev/null || kill -9 "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
			return 124
		fi
		sleep 0.1
		waited=$(( waited + 1 ))
	done

	# `|| rc=$?` rather than a bare `wait`: this function is safe to call from a `set -e`
	# script in any context, not only inside a condition.
	wait "$pid" || rc=$?
	return "$rc"
}

# docker_preflight <engine> [cap_seconds]
# Assert <engine>'s daemon is up AND answering. Returns 0 when usable, 1 otherwise (with an
# actionable message on stderr). Callers decide what to do: `docker_preflight "$ENGINE" || exit 1`.
docker_preflight() {
	local engine="${1:?docker_preflight: engine required}"
	local cap="${2:-10}"
	local rc=0

	if ! command -v "$engine" >/dev/null 2>&1; then
		echo "!! '$engine' not found. Start OrbStack (or set DOCKER=<engine>)." >&2
		return 1
	fi

	# `info` over `version`: both podman and docker implement it, and it round-trips to the
	# daemon rather than answering from the client alone.
	_docker_preflight_capped "$cap" "$engine" info || rc=$?

	case "$rc" in
		0) return 0 ;;
		124)
			echo "!! '$engine' did not answer within ${cap}s." >&2
			echo "   The daemon socket exists but nothing is serving it — OrbStack stopped" >&2
			echo "   uncleanly, or is still booting. Restart OrbStack and re-run." >&2
			echo "   (Failing now on purpose: this is the state that hangs a CI job silently" >&2
			echo "    for its full timeout and leaks an orphan runner worker.)" >&2
			;;
		*)
			echo "!! '$engine info' failed (exit $rc) — the daemon is not running." >&2
			echo "   Start OrbStack (or set DOCKER=<engine>), then re-run." >&2
			;;
	esac

	return 1
}
