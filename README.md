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

## Build & run

```sh
cargo run -p pb-app --release -- "C:\path\to\photos"        # a folder of JPEGs
cargo run -p pb-app --release -- "C:\path\to\photos" -r     # recurse subfolders
cargo run -p pb-app --release -- "C:\path\to\photos" -r --windowed
```

### Keys

| Key | Action |
|---|---|
| `space` / `→` | next photo |
| `backspace` / `←` | previous photo |
| `0` / `o` | toggle fit-to-screen ↔ original 1:1 (centered) |
| `i` | toggle info panel (path · resolution · codec) |
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
