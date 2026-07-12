# PhotoBlaze

A photo viewer with one obsession: **how fast you can flick through thousands of
images.** No chrome, fit-to-screen, keyboard-driven, with photos held in GPU
memory so the next frame is already there when you press a key.

> Prime directive: *will this make it faster, or have basically zero performance
> impact?* If it's neither, it doesn't ship. See [`CLAUDE.md`](./CLAUDE.md) and
> [`.taskmaster/docs/`](./.taskmaster/docs/) for the full design, research, and
> decision log.

## Status

Early but real. The decode/upload performance spikes are done (they drove the
architecture), and the viewer paages through folders today:

- **Phase 1 (done):** chrome-less fullscreen window, wgpu (DX12) renderer,
  fit-to-screen letterboxing, headless golden-image test harness.
- **Phase 2 (done):** directory scan (+ recursive), `zune-jpeg` decode with EXIF
  orientation, sequential navigation, self-paced advance, linear filtering,
  HiDPI-aware, GPU-adaptive texture limits.
- **Next — Phase 3:** the decode pool + prefetch ring + self-paced advance that
  make holding a key *fly* on full-resolution photos.

The GPU-stack architecture was chosen from measurement (CPU decode ≈ 2.5× and a
staging-ring upload ≈ 3.4× the 120 Hz budget on the test machine), reviewed, and
recorded in [`.taskmaster/docs/decisions.md`](./.taskmaster/docs/decisions.md).

## Requirements

- Rust (stable; see [`rust-toolchain.toml`](./rust-toolchain.toml))
- A GPU with Vulkan or D3D12 (wgpu)

## Install (Windows)

Download the signed `PhotoBlaze-x.y.z-x86_64.msi` from the **Releases** page and
run it. The installer adds PhotoBlaze to the **Open with** list for common image
types and an **Open with PhotoBlaze** entry to the folder right-click menu.
Windows won't let any installer silently take over the default viewer — to make
it the default, open a photo's *Open with → Choose another app*, pick PhotoBlaze,
and tick *Always*. Building the MSI yourself: see
[`.taskmaster/docs/packaging.md`](./.taskmaster/docs/packaging.md).

## Build & run

```sh
cargo run -p pb-app --release -- "C:\path\to\photos"          # a folder (recursive by default)
cargo run -p pb-app --release -- "C:\path\to\photo.jpg"       # one photo (opens its folder, flat)
cargo run -p pb-app --release -- "C:\path\to\photos" --no-recursive
cargo run -p pb-app --release -- --windowed                   # dev window; then drop a photo or press O
```

At runtime you can also **double-click** an image (once installed), **drag-and-drop**
photos or a folder onto the window, or press **`O`** for the native open dialog.

### Command-line options

Run `photoblaze --help` for the full list. Every option shapes a **single launch**
and never changes your saved settings.

| Option | Effect |
|---|---|
| `PATH...` | Files, a folder, or an archive (`.zip` / `.7z`) to open |
| `-w, --windowed` / `-f, --fullscreen` | Start windowed or borderless-fullscreen |
| `-r, --recursive` / `--no-recursive` | Include subfolders, or open the folder flat |
| `--info` / `--no-info` | Show or hide the info line on launch |
| `--details` / `--folders` | Open the image-details (Inspector) / folder-tree panel |
| `--slideshow[=SECS]` | Start a slideshow, optionally at SECS per slide (`5`, `3s`, `0.5m`; clamped to 0.1–60 s) |
| `--shuffle` | Navigate in random (shuffle) order |
| `--reverse` | Play backward — with `--shuffle`, a reverse shuffle |
| `--scale fit\|fill\|original` | Initial scale mode |
| `--theme light\|dark\|system` | Light / dark theme for this launch |
| `--mute` | Mute Live Photo audio |
| `--start-at N\|NAME` | Open at photo N (1-based) or the first name match |
| `-h, --help` / `-V, --version` | Print help / version and exit |

Encrypted archives are opened by entering the password in the viewer, not on the
command line.

#### macOS

The same options work on macOS. Install the `photoblaze` command once via
**PhotoBlaze ▸ Install Command-Line Tool…** (creates
`/usr/local/bin/photoblaze`; the same menu item removes or repairs it), then:

```sh
photoblaze ~/Photos --slideshow=3s --shuffle
```

- Bare paths work (`photoblaze ~/Photos`); the older `--pb-open <path>` form
  remains as a compatibility alias.
- `--help` / `--version` / errors print to the terminal — colored on a TTY,
  plain when piped or redirected. A Finder/Dock launch with a bad option shows
  a dialog instead of failing silently.
- `open -a PhotoBlaze --args --theme dark ~/Photos` also works, but `open`
  detaches the terminal — use the installed `photoblaze` command when you want
  `--help`/`--version` output.

### Keys

| Key | Action |
|---|---|
| `space` | next photo |
| `backspace` | previous photo |
| `enter` | random photo (precomputed shuffle) |
| `← ↑ ↓ →` | pan around the photo (hold to accelerate) |
| `=` / `-` | zoom in / out (hold; numpad `+`/`-` too) |
| `8` / `9` / `0` | fit / fill / toggle original 1:1 ↔ fit |
| `r` / `Shift+R` | rotate 90° cw / ccw (per-image, RAM-only) |
| `Ctrl+R` | toggle recursive subfolder browsing |
| `o` / `Shift+O` | open file(s) / open a folder |
| `i` / `Shift+I` | info panel / full-EXIF panel |
| `/` or `?` | keyboard-shortcut help |
| `esc` | quit |

Hold a nav key to page through every photo (advance is self-paced and capped at
the display refresh; nothing is skipped).

## Workspace

```
crates/
  pb-core    pure nav / precomputed-random / prefetch / cache logic (no I/O, no GPU)
  pb-decode  decode abstraction (decode-to-fit + preview-first) + zune-jpeg backend
  pb-render  wgpu presenter + fit-to-screen geometry
  pb-app     the binary: winit event loop wiring it together
spikes/      throwaway measurement spikes (decode + upload throughput)
.taskmaster/ design docs, research, decisions, roadmap, and the task backlog
```

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Tests follow TDD: the pure logic in `pb-core` is fully unit-tested, and the GPU
path is covered by headless golden-image tests.

## License

Dual-licensed under MIT or Apache-2.0.
