#!/usr/bin/env bash
# Reap orphaned `Runner.Worker` processes on a self-hosted macOS Actions runner.
#
# WHY THIS EXISTS. A cancelled job (timeout, or `cancel-in-progress` superseding a push) can
# leave behind a `Runner.Worker` process that spins one core at ~90% forever. Found 2026-08-11
# on the Mac runner: **nine** of them, 8–20 days old, ~5.9 of 12 cores gone, load average 89.
# Exactly one per `haven't exit within cancellation timout` event in the listener log — a 9/9
# hit rate, so this is systemic, not a fluke.
#
# What the orphan actually is: NOT the worker. It is the `fork()` half of .NET's
# `Process.Start` (fork + execve) that never completed its exec, frozen in the window where it
# still carries the parent's argv — which is why `ps` shows it as `Runner.Worker spawnclient
# <fd> <fd>` rather than the step's shell. Tells: PPID 1, a SINGLE thread, ~2 MB footprint (a
# real worker runs dozens of threads and 100+ MB), holds the real worker's `_diag` log FDs by
# inheritance without ever writing to them. A forked CoreCLR child cannot recover — only the
# forking thread exists, but locks held by the threads that didn't come across are still marked
# held — so it spins until killed.
#
# WHY IT NEEDS AN EXTERNAL REAPER. Three supervisors all miss it by construction:
#   • `timeout-minutes` is enforced server → listener → worker. The stub is off that chain.
#   • the listener's force-kill targets the *worker's* process tree. The stub isn't the worker
#     (it survives the listener's SIGKILL of the real worker, exit 137, every time).
#   • launchd adopts it as an orphan. Adopting is all launchd does; it doesn't police.
# So nothing in the runner will ever clean this up. Hence a 5-minute sweep from outside.
#
# The fork/exec race is an upstream runner bug, so fixing the *trigger* (see
# scripts/docker-preflight.sh, which stops the OrbStack hang that caused these nine) reduces
# the rate but cannot close the class. Keep both.
#
# Usage:
#   scripts/reap-orphan-runner-workers.sh              # sweep and kill (what launchd runs)
#   scripts/reap-orphan-runner-workers.sh --dry-run    # report only, kill nothing
#   scripts/reap-orphan-runner-workers.sh --install    # install+start the 5-min LaunchAgent
#   scripts/reap-orphan-runner-workers.sh --uninstall  # stop and remove the LaunchAgent
#   scripts/reap-orphan-runner-workers.sh --status     # is the agent loaded?
#
# Env: RUNNER_ROOT (default ~/actions-runner), MIN_AGE_SECONDS (default 120),
#      REAP_INTERVAL_SECONDS (default 300, --install only).
set -euo pipefail

RUNNER_ROOT="${RUNNER_ROOT:-$HOME/actions-runner}"
MIN_AGE_SECONDS="${MIN_AGE_SECONDS:-120}"
REAP_INTERVAL_SECONDS="${REAP_INTERVAL_SECONDS:-300}"

LABEL="ca.fullspec.BlazeViewer.runner-reaper"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG="$HOME/Library/Logs/blazeviewer-runner-reaper.log"

SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

# ── the sweep ────────────────────────────────────────────────────────────────────────────
# Emits "<pid> <age_seconds>" for each orphan. The safety predicate is all three of:
#   ppid == 1          the real worker is gone. A LIVE worker is a child of Runner.Listener,
#                      never of launchd, so this alone cannot match a working job. And if the
#                      listener really did die mid-job, that worker's job is unrecoverable
#                      anyway (the server reassigns or fails it) — killing it is correct.
#   path is a Runner.Worker under $RUNNER_ROOT    never some other stray `Runner.Worker`.
#   age >= MIN_AGE_SECONDS                       insurance against catching a legitimate
#                                                process mid-reparent during a runner restart.
# Note `$4` assumes the runner root holds no spaces (it doesn't: ~/actions-runner). If it ever
# did, the `index()` test fails and the row is SKIPPED — the safe direction.
find_orphans() {
  ps -Ao pid=,ppid=,etime=,comm= | awk -v root="$RUNNER_ROOT" -v minage="$MIN_AGE_SECONDS" '
    $2 == 1 && $4 ~ /\/Runner\.Worker$/ && index($4, root) == 1 {
      e = $3; d = 0
      if (index(e, "-")) { split(e, a, "-"); d = a[1]; e = a[2] }
      n = split(e, t, ":")
      s = (n == 3) ? t[1]*3600 + t[2]*60 + t[3] : (n == 2 ? t[1]*60 + t[2] : t[1] + 0)
      age = d*86400 + s
      if (age >= minage) print $1, age
    }'
}

human_age() {
  local s="$1"
  if (( s >= 86400 )); then echo "$(( s / 86400 ))d$(( (s % 86400) / 3600 ))h"
  elif (( s >= 3600 )); then echo "$(( s / 3600 ))h$(( (s % 3600) / 60 ))m"
  else echo "$(( s / 60 ))m"; fi
}

sweep() {
  local dry="${1:-0}" found=0 killed=0 pid age

  while read -r pid age; do
    [[ -n "${pid:-}" ]] || continue
    found=$(( found + 1 ))
    if (( dry )); then
      echo "would reap pid $pid (orphan, age $(human_age "$age"))"
      continue
    fi
    # SIGKILL, not SIGTERM. These ignore cancel and terminate by construction — the runner's
    # own listener already proved that ("both cancel and terminate signal has been ignored by
    # the target process") before it resorted to a tree kill. A polite signal is wasted here.
    if kill -9 "$pid" 2>/dev/null; then
      echo "$(date '+%Y-%m-%d %H:%M:%S') reaped orphan Runner.Worker pid $pid (age $(human_age "$age"))"
      killed=$(( killed + 1 ))
    else
      echo "$(date '+%Y-%m-%d %H:%M:%S') could not kill pid $pid (gone already?)"
    fi
  done < <(find_orphans)

  if (( dry )); then
    (( found )) || echo "no orphans (checked: ppid 1, under $RUNNER_ROOT, age >= ${MIN_AGE_SECONDS}s)"
  fi
  # Deliberately silent on a clean non-dry sweep: this runs every 5 minutes, and a "nothing to
  # do" line each time would bury the entries that matter under ~288 lines a day. The log is a
  # record of incidents, not a heartbeat.
  return 0
}

# ── LaunchAgent management ───────────────────────────────────────────────────────────────
install_agent() {
  mkdir -p "$(dirname "$PLIST")" "$(dirname "$LOG")"
  cat >"$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/bash</string>
		<string>$SELF</string>
	</array>
	<key>StartInterval</key>
	<integer>$REAP_INTERVAL_SECONDS</integer>
	<key>RunAtLoad</key>
	<true/>
	<key>StandardOutPath</key>
	<string>$LOG</string>
	<key>StandardErrorPath</key>
	<string>$LOG</string>
	<key>ProcessType</key>
	<string>Background</string>
	<key>LowPriorityIO</key>
	<true/>
</dict>
</plist>
PLIST_EOF

  # bootout first so --install is idempotent and picks up an edited plist.
  launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$UID" "$PLIST"
  echo ">> installed $LABEL (every ${REAP_INTERVAL_SECONDS}s)"
  echo "   plist: $PLIST"
  echo "   log:   $LOG"
}

uninstall_agent() {
  launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo ">> removed $LABEL (log left in place: $LOG)"
}

status_agent() {
  if launchctl print "gui/$UID/$LABEL" >/dev/null 2>&1; then
    echo ">> $LABEL is loaded"
  else
    echo ">> $LABEL is NOT loaded  (install with: $0 --install)"
    return 1
  fi
}

case "${1:-}" in
  "") sweep 0 ;;
  --dry-run|-n) sweep 1 ;;
  --install) install_agent ;;
  --uninstall) uninstall_agent ;;
  --status) status_agent ;;
  -h|--help) sed -n '/^# Usage:/,/^#      REAP_INTERVAL/p' "$0" | sed 's/^# \{0,1\}//' ;;
  *) echo "usage: $0 [--dry-run|--install|--uninstall|--status]" >&2; exit 1 ;;
esac
