# Task #78 — macOS CLI parity (rev 2, post-Codex review)

**Goal:** bring the Windows/winit CLI (task #78, `feat/cli-clap`) to the macOS SwiftUI
host so `PhotoBlaze.app`'s embedded binary honors the same flag surface. The parser
(`pb-cli`) and the override application (`AppCore::apply_launch_overrides`) already exist
and are cross-platform; the Mac shell just never wires them. This is the wiring +
Mac-specific launch plumbing.

**Scope (owner call): full parity** — every task-#78 flag *including the shell-owned
behaviors* (path validation, recursive override, `--metrics`), the effective-value wiring
for `--theme`/`--mute`/window-mode, `--help`/`--version` with a correct output policy,
forcing the window on a bare-path launch (§6), and tests.

**Rev 2 (2026-07-11):** incorporates the Codex review — argv[0] contract, launch-preflight
lifecycle, output policy (isatty was wrong), shell-owned parity gaps (recursive/paths/
metrics), the paths+version FFI contract, and double-delivery arbitration for bare paths.

## Current state (2026-07-11)

| Piece | Windows/winit (`pb-app`) | macOS (`pb-mac-ffi` + Swift host) |
|---|---|---|
| Parser | `pb_cli::parse_from(args_os, ver)` → `Cli` | **none** — `pb-mac-ffi` has no `pb-cli` dep |
| Path opening | clap positional `paths` → `open::plan` | `--pb-open <path>` only (manual Swift scan) |
| Overrides | `cli.to_overrides()` → `apply_launch_overrides` | **never applied** |
| Path validation | nonexistent path → exit 2 (main.rs:3699) | none |
| `--recursive` | mutates `Source::Scan.recursive` (main.rs:3717) | `open_paths` uses `open::plan`'s default (always recursive) |
| `--metrics` | `METRICS_ON_FLAG` + `StageTimes::enabled()` + exit report | none (but `StageTimes` lives on `AppCore` — portable) |
| `--help`/`--version` | AttachConsole + stdout / dialog | none |
| Errors | stderr (console) / `rfd` dialog (no console) | none |
| `--theme`/`--mute` | live via `effective_*` reads | Swift reads **raw** `settings_form()`; menu state reads raw `settings.mute_live_audio` (lib.rs:1536) |
| `--windowed`/`-f` | resolved pre-window from override | `startup_fullscreen()` reads settings only |

## Implementation sequence (dependency order)

argv/output contract (§1) → preflight lifecycle (§2) → bare-path window spike +
dedup (§6) → FFI paths/version contract (§1b) → recursive/path-validation/metrics
parity (§3) → effective runtime reads (§4) → end-to-end smoke matrix (§7).

---

### 1. `pb-mac-ffi` links `pb-cli`; the parse FFI entry — exact contracts

Add `pb-cli` to `pb-mac-ffi/Cargo.toml` (macOS target dep; pure Rust, no build risk).

**argv[0] contract (P0).** `pb_cli::parse_from` → clap `try_get_matches_from` treats the
FIRST element as the executable name. Swift must pass **all of
`ProcessInfo.processInfo.arguments`** (including argv[0]) — never `.dropFirst()`, which
would eat the first real flag/path. Name the FFI entry to make misuse hard:
`parse_launch_args(argv: Vec<String>, version: String)` documented as "full argv,
argv[0] included". Regression test: `["photoblaze", "--help"]` yields the help error kind;
`["photoblaze", "/tmp/x"]` yields one positional path.

**Outcome type (P0 — `{ShowText, Exit}` was ambiguous).** Return a plain FFI struct:

```
LaunchParseFfi {
    proceed: bool,       // true → run the app; false → render text + exit(exit_code)
    text: String,        // rendered clap output ("" when proceed)
    use_stderr: bool,    // clap's use_stderr(): errors → stderr, help/version → stdout
    exit_code: i32,      // clap's exit_code(): 0 help/version, 2 usage error
}
```

Rust renders `clap::Error` to `text` via `.render().to_string()`; **no `process::exit`
inside the lib** (same rule as the winit shell). On `proceed`, Rust stashes
`cli.to_overrides()` + `cli.paths` on the handle and calls
`core.apply_launch_overrides(&overrides)` immediately (this is safe pre-window: it only
mutates core state that later reads consume).

**1b. Parsed-paths + version contract (P1).**
- Paths: `take_launch_paths() -> Vec<String>` — consumed exactly once (the stash-pull
  pattern this FFI already uses for clipboard/dialogs). Swift calls it after the canvas
  exists and routes through the existing `open_paths` (which mirrors `classify_inputs`).
  A second call returns empty — that's the idempotence guard.
- Version: **Swift passes the version string in** (from the bundle's
  `CFBundleShortVersionString` + build id, i.e. what About shows). `pb-mac-ffi`'s own
  `CARGO_PKG_VERSION` is `0.1.0` and must not be used. Alternative (rejected): stamping
  `PB_BUILD_ID` into pb-mac-ffi's build — the bundle is already the single source of the
  packaged version on this platform.
- `--pb-open <path>`: keep working. Preferred: a hidden clap arg on the shared `Cli`
  (`--pb-open <PATH>`, hide = true, appends to `paths`) so it routes through the same
  parser; it remains the safety net while §6 is proven.

### 2. Launch preflight — parse BEFORE the window/Sparkle, not in `onAppear` (P0)

Parsing in `ContentView.onAppear` is too late: SwiftUI has already materialized a window
and `applicationDidFinishLaunching` has started Sparkle, so a terminal `--help` would
flash a GUI and do unrelated startup work before exiting.

- Run the preflight at **`CoreModel` construction / app bootstrap** (before the `body`
  scene is evaluated, or at latest in `applicationWillFinishLaunching`): build the FFI
  handle, call `parse_launch_args(ProcessInfo.arguments, bundleVersion)`.
- Store a typed `LaunchDisposition` on the model: `.proceed(paths: [String])` /
  `.emit(text, useStderr, exitCode)`.
- `.emit` + output available → write text to the right stream and `exit(exit_code)`
  **before** any window exists. `.emit` + GUI launch → §5's alert policy.
- Only `.proceed` starts Sparkle (`Updater.startIfEnabled` moves behind the disposition)
  and normal window work.
- Path opening stays deferred until the canvas exists (the current
  `openLaunchPathIfAny` slot) — but now it pulls `take_launch_paths()` instead of
  scanning argv itself.
- Initial appearance (`applyAppearance`) and `startup_fullscreen()` run AFTER
  `apply_launch_overrides` has been called (it happens inside the parse FFI, so ordering
  is structural, not conventional). First frame must already be in the overridden
  theme/window mode — no visible correction after launch (verify in §7).

### 3. Shell-owned parity the core doesn't cover (P1)

`apply_launch_overrides` handles core state only. The winit shell also does three things
the Mac must mirror:

- **Path existence validation** (Mixed strictness): before opening, every positional
  path must exist; a missing one is a usage error — text
  `photoblaze: no such file or folder: <path>`, exit 2, reported per §5's output policy.
  Do this in Rust (a `validate_launch_paths()` FFI or fold into the parse entry —
  but note the winit shell validates *after* parse succeed, before window; match that)
  so the message text stays identical across shells.
- **Recursive override:** the Mac's `open_paths` builds `open::plan` unchanged, so a
  directory always scans recursive=true regardless of the CLI or the saved setting. Fix
  in `pb-mac-ffi::open_paths` (or the launch-path variant): after `open::plan`, mutate
  `Source::Scan { recursive, .. }` to `launch.recursive.unwrap_or(settings.recursive)`
  — the exact winit logic (main.rs:3717). This also fixes a latent pre-existing gap:
  the saved recursive preference is ignored on Mac launch opens today.
- **`--metrics`:** implement, don't stub (owner scope = full parity). `StageTimes` lives
  on `AppCore` (`core.metrics`, recorded core-side in decode/upload/present paths), so:
  on `proceed` with `metrics`, set `core.metrics = StageTimes::enabled()`; on quit,
  emit the same report the winit shell prints (stdout when launched from a terminal).
  The winit `METRICS_ON_FLAG` static is decode-closure plumbing local to pb-app — check
  whether pb-app-core's host decode path needs an equivalent knob; if the decode stage
  can't be timed on Mac v1, say so in the report rather than printing a misleading zero.

### 4. Effective-value wiring (so `--theme`/`--mute`/window-mode actually show)

Rule: **`settings_form()` stays raw** (it edits persisted preferences); every *runtime*
read goes through the effective helpers.

- `applyAppearance` (CoreModel.swift ~1203): read `core.effective_appearance()` (add the
  FFI accessor) instead of `settings_form().appearance_mode`.
- Playback mute: audio paths read `effective_mute()` (core-side reads already do —
  verify the Mac host has no raw read on its audio path).
- **Menu state (named seam):** `pb-mac-ffi::apply_menu_state` (lib.rs:1536) passes
  `self.core.settings.mute_live_audio` → the Mute checkmark would sit stale under
  `--mute`. Switch to `self.core.effective_mute()`. Audit the other menu-state inputs
  against overrides while there (scale mode + info line are live core state, already
  correct; fullscreen comes from `core.windowed`).
- `startup_fullscreen()` (lib.rs ~824): fold `launch.windowed` — a `--windowed`/`-f`
  override wins over the saved `start_fullscreen()`.

### 5. Output policy for help/version/errors (P0 — isatty-on-stdout was wrong)

Pipes and redirects are non-TTY but MUST receive output (`PhotoBlaze --help | less`,
`--version > version.txt`). The policy:

- **Always** write help/version to stdout and usage/path errors to stderr when the
  process HAS those streams — i.e. whenever it was exec'd from a shell, TTY or not.
  Preserve clap's exit codes (0 help/version, 2 errors).
- The `NSAlert` is only for a **real error** on a **GUI launch** (Finder / `open` /
  Dock: no meaningful output sink). Never alert for help/version (silent exit 0), never
  alert when output went to a pipe/redirect.
- GUI-launch detection: don't test stdout's TTY-ness alone. A Finder/`open`-launched app
  has stdout attached to the null device or the unified log, not a pipe the user made.
  Practical detect: `isatty(STDOUT) || isatty(STDERR)` → terminal; else
  `fstat` stdout — a pipe/regular file means a deliberate redirect (emit, no alert);
  a character device that isn't a TTY (`/dev/null` route) on a launchd-parented process
  means GUI (alert on error). Spike this detection early and encode it in ONE Swift
  helper with the truth table in a comment; it's the Mac analog of
  `win_console::attach_parent_console`'s `have_output`.
- Belt-and-braces: write the text to the streams in every case (it's harmless when
  nobody reads them), the detection only decides the *alert*.

### 6. Force the window on a bare-path launch (decided: option B) — spike first

AppKit treats a bare `argv[1]` path as a document-open launch and suppresses the initial
`WindowGroup` window (the "great windowless-app hunt"). clap positionals reintroduce this
for `photoblaze ~/Photos`. **Owner decision: engineer around it** so bare paths work like
Windows. Highest-risk item — spike before building on it.

- Detect the windowless case post-launch (bare path in argv AND no visible host window)
  and materialize the window. Candidate mechanisms, cheapest first:
  `@Environment(\.openWindow)` with an explicit `WindowGroup(id:)`; fallback: check
  `NSApp.windows` for the host window in `applicationDidFinishLaunching` and
  force-create/`makeKeyAndOrderFront`.
- **Success criterion is NOT "a window appears".** The forced window must: host the same
  `CoreModel` instance, run `ContentView.onAppear` wiring exactly once (menu bar, FFI
  bridge, open handlers, `openSettingsAction`), attach the Metal canvas, and leave the
  Settings scene reachable. Add a smoke assertion for each.
- **Double-delivery arbitration (P1).** The same bare path ALSO arrives as an Apple
  Event: `application(_:open:)` fires (or buffers into `pendingURLs`,
  PhotoBlazeMacApp.swift:56) for the document-open launch. After this change the path
  would arrive twice — once from parsed argv, once from the open-URLs handler — and the
  second `open_paths` would supersede the first scan. Rule:
  - Record the set of launch paths that came from argv (standardized file URLs —
    resolve symlinks/`..`, compare `URL.standardizedFileURL`).
  - On the FIRST open-URLs batch delivered during launch, drop any URL that matches the
    argv set; deliver the remainder (if any) normally.
  - Later Finder open events (post-launch) are never filtered.
  - Prove exactly one `open_paths` call / one scan generation for
    `photoblaze ~/Photos` (assert on `scan_gen`).
- Keep `--pb-open` working throughout as the safety net / smoke-test path while B is
  proven. If the spike busts its timebox (say, a day), fall back to shipping everything
  else with `--pb-open` + leading-flag forms documented, and keep B as its own task.

### 7. Verification matrix (beyond Rust unit tests)

Rust unit tests (pb-cli is already covered; pb-mac-ffi as rlib on macOS):
- argv[0] regression (first flag + first positional survive).
- `parse_launch_args` outcome mapping: help / version / bad flag / proceed.
- `take_launch_paths` consumed-once semantics.
- Overrides fold into `effective_appearance` / `effective_mute` / `startup_fullscreen` /
  menu-state mute.
- Recursive: dir plan honors `launch.recursive.unwrap_or(settings.recursive)` (all four
  combinations).
- No-trace: overrides never touch `settings.toml` (reuse the existing invariant).

End-to-end smoke (scripted where possible, otherwise a manual checklist on the Mac):
- Direct embedded binary (`…/PhotoBlaze.app/Contents/MacOS/…`): `--help`, `--version`,
  bad flag, bad path — correct stream + exit code; piped (`--help | cat`) and
  redirected (`--version > f`) both receive output; no alert, no window flash.
- `open -a PhotoBlaze --args --slideshow=5 --shuffle ~/Photos` — flags land; paths with
  spaces + Unicode survive the `open --args` route.
- Bare positional folder / image / archive: ONE window, ONE open, ONE scan generation.
- `--pb-open <path>`: retained compatibility.
- Saved fullscreen/recursive/theme/mute crossed with each CLI override (override wins,
  session-only).
- First-frame theme + window mode: no visible correction after launch.
- `settings.toml` byte-identical after an override-laden session.
- `--metrics`: report emitted on quit.

### 8. `--slideshow` duration units (shared `pb-cli` change — benefits Windows too)

Owner call (2026-07-11): accept unit suffixes, e.g. `--slideshow=3s`, `--slideshow=0.5m`.
- Change the arg from a raw `f64` to a custom clap `value_parser` (string → seconds):
  bare number = seconds (back-compat, `--slideshow=5` unchanged), `Ns` = seconds,
  `Nm` = minutes. Case-insensitive; reject anything else with a clear clap value error
  (`3h`, `3 s` with a space, garbage).
- Clamp interplay: the existing `[MIN_INTERVAL, MAX_INTERVAL]` = [0.1 s, 60 s] clamp
  applies AFTER unit conversion — so `--slideshow=5m` clamps to 60 s (matches the
  runtime slider's own ceiling; Mixed strictness says clamp, don't error). Document in
  help text ("SECS or 3s / 0.5m; clamped to 0.1–60 s").
- Unit tests in pb-cli: `5`, `3s`, `0.5m`, `1m` (=60 s exactly), `5m` (clamps),
  `3h`/`abc` (error). Update the help `value_name` + README table + EXAMPLES block.

### 9. The `photoblaze` PATH shim (owner call: in scope — agent-first rationale)

The literal `photoblaze ~/Photos …` command needs the binary on PATH. This is a
first-class goal: beyond humans, AI agents drive the machine via CLI, and a discoverable
`photoblaze` command + `--help` + stable exit codes is what makes "show me a slideshow
of my 2015 photos" a one-command action for them.

- **Opt-in, never automatic** (the VS Code / iTerm pattern): a Settings row / menu item
  "Install command-line tool…" that creates the symlink. A silent PATH write on launch
  is bad citizenship and may need admin rights. This is an explicit user action — same
  ADR-018 category as saving a rotation (footprint fine, user-initiated write fine).
- Mechanism: symlink `photoblaze` → the embedded binary at its stable
  `/Applications/PhotoBlaze.app/Contents/MacOS/…` path. Sparkle replaces bundle
  *contents*, so the link survives updates. Target dir: `/usr/local/bin` when writable,
  else offer `~/.local/bin` (or prompt-with-privilege as a later nicety); detect + offer
  to repair a stale link (app moved) from the same Settings action.
- Verify `Bundle.main` resolution through the symlink (CFBundle resolves the real
  executable path — expected fine; smoke-test resources + Sparkle feed still resolve).
- Uninstall consideration: note the link in the README; removing the app orphans a tiny
  dead symlink (harmless), the Settings action can also remove it.
- Windows note (parity, later): the Velopack install already puts the app in a stable
  per-user dir; a PATH entry could ride the installer hook — separate task, not this one.

### 10. Agent-friendliness notes (design intent, mostly already true)

- Multiple positional paths are accepted → "photos from 2015" = a year folder, or an
  `mdfind`/`find` result list passed as args.
- Deterministic exit codes (0/2), stable stderr message shape, `--help` as agent
  self-discovery, session-only overrides (an agent's weird flag combo can never corrupt
  saved preferences or leave a trace).
- Deferred (only if a real need appears): a `--files-from <file|->` list input to lift
  the shell `ARG_MAX` (~1 MB argv) cap for huge agent-generated file lists.

### 11. Docs
- README: macOS invocation section — the `photoblaze` shim install, direct-binary form,
  `open -a PhotoBlaze --args …` (note `open` detaches stdout: help/version there go to
  the log, use the direct binary for CLI output), bare-path support, `--pb-open`
  back-compat, slideshow unit suffixes. CHANGELOG under Added.

## Out of scope / deferred
- `--new-window` stays a parse-accepted no-op (single-instance is future task #1).
- `--files-from` stdin/file list input (agent mega-lists) — only on demonstrated need.
- A Windows PATH entry via the Velopack hook — separate parity task.
- Windows `AttachConsole` shim — not needed on macOS.
- The winit-only render-and-exit dev flags (`--hud-gallery`/`--egui-shot`/
  `--settings-shot`) stay out of the shared `Cli` and off the Mac.
