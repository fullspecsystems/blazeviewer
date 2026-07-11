# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-11. Supersedes the 07-08 Linux-bleed/menu-nav handoff (all landed)._

## State: main @ `26ce921`, pushed, fully green

`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, the
featured clippy (`pb-decode/pb-app/pb-cli` with `libheif,dav1d`), and `fmt --check` all pass
on the merged tree. `feat/cli-clap` is merged into main (worktree `photoblaze-wt1` + branch
can be cleaned up).

## What landed today

- **#76 animated AVIF on Windows — DONE, owner-verified.** vcpkg-pinned dav1d behind a C
  accessor shim (`pb-decode/csrc/dav1d_shim.c` — the pattern for any future vcpkg C lib),
  `probe_avis` demuxer (avis-only, HDR→still, no dead hints), YUV→RGB, cancellable
  `decode_animation_cancellable`, fuzz harness (`fuzz/`), release/CI wiring. Ship features
  are now **`libheif,dav1d`**. Corpus perf: 26×1280×531 ≈ 139 ms release decode.
- **#78 CLI (merged from the other agent, status `in-progress`)**: new `pb-cli` crate
  (clap 4, FFI-ready, no process::exit), `LaunchOverrides` in pb-app-core (session-only,
  never persisted), `win_console.rs` AttachConsole shim, `--help/--version` + override
  flags. **Awaiting owner smoke** → then mark done.
- **#79 video playback (tier 2) — fully planned, rev2.** Plan:
  `.taskmaster/plans/79-video-playback-tier2.md` (authoritative; Codex review verified +
  incorporated; all owner decisions locked, incl. MKV-visible-with-error and Shift=±15 s).
- New tasks: **#77** LGPL static-link compliance before commercial release (libheif/libde265).

## Next task: #79 phase 0 (contracts + platform spikes)

Read the plan doc first — it is the spec. Phase 0 in brief: define `LibraryItemKind`,
`VideoMetadata`, `VideoFrame`/`VideoColorInfo`, session states, `AudioClockSample`,
byte-budget invariants; **spike MF** fitted-output scaling, stream deselection, PTS,
seek-then-decode-forward, cancellation cost, and the container probe sweep (does Win10/11's
native MKV handler open MKVs?); prototype incremental Linux audio; lock the SDR guarantee on
real phone footage. Spike results replace the schedule estimate (2-3 wk = Windows milestone
only). Windows-first; subtasks in tasks.json mirror plan phases 0-7.

Key architecture (details in plan): forward-only `VideoSession` (separate from `Playback`),
byte-budgeted 2-3-frame queue, credit-driven producer selecting over {capacity, commands}
(never a blocking send), session_id + seek_generation on frames, rebuffer-don't-drift clock
policy, audio-position master clock via new `AudioClockSample` core⇄shell events. Guardrails:
path-only video items (typed, dispatched **before** `source.bytes()` — engine.rs:223 calls it
unconditionally today), split extension predicates (pb-source shares
`is_supported_extension` via callback — don't broaden it), duration-independent audio.

## Loose ends (small)

1. **#78 owner smoke** → flip to done (+ CHANGELOG entry if missing).
2. **#76 ARM64 mirror**: on the ARM64 box run `setup-libheif.ps1 -Triplet
   arm64-windows-static-md`, `cargo test -p pb-decode --features libheif,dav1d`; check the
   vcpkg log that dav1d's ARM asm wasn't silently disabled. CI lane wired, behind
   `WIN_ARM64_RUNNER`.
3. **#77**: append the patent-exposure note (OS codecs shift H.264/HEVC patent liability to
   the OS vendor; bundling FFmpeg would make us the distributor — owner discussed 07-11,
   agreed FFmpeg stays rejected on Windows, deferred/demand-gated dylib option on macOS).
4. CLAUDE.md "Minimal UI" keymap list is stale v0 (arrows are actually PanLeft/PanRight,
   keymap.rs:502) — fixed as part of #79 phase 7 docs, or earlier if touching CLAUDE.md.

## Environment / conventions quick-ref

- vcpkg pinned via `setup-libheif.ps1 -VcpkgRef a0400024` (libheif 1.23.0, libde265 1.1.1,
  dav1d 1.5.3), trees at `C:\vcpkg-pb` (CI) + `~\vcpkg` (dev), both provisioned.
- Corpus: `D:\Media\test-images\animated\` (3.avif = the 26-frame avis). avis fixtures +
  regen commands: `crates/pb-decode/tests/fixtures/avis/README.md` (gotchas: CSS `green` is
  half-bright — use `lime`; `-color_primaries` dies through `-filter_complex` — use
  `setparams`).
- tasks.json edits: PowerShell `ConvertFrom-Json` → mutate → `ConvertTo-Json -Depth 10`
  round-trip produces minimal diffs; IDs must stay numeric.
- Commits: no AI-attribution trailers; dev builds `pwsh scripts/build-windows.ps1`
  (`-NoNative` skips vcpkg libs); release uploads need native PowerShell (see memory).
