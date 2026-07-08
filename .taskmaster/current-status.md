# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-08. Supersedes the prior macOS task #58/#59 handoff (recover via git if needed)._

## TL;DR

Chasing a **Linux/WSLg folder-tree panel "bleed"** (task #54 territory). The owner reports
tree rows spilling out the **top** (into the "Folders" header) and **bottom** (over the photo).

**Status: NOT fixed.** But it is now narrowed hard: the panel **layout/geometry is provably
correct** — the bug lives in the **live egui-overlay render/composite path**, which headless
`--egui-shot` does not exercise. A **self-serve X11 test harness** now exists so the next agent
can reproduce, screenshot, and iterate **without the owner** (previously I was debugging blind).

Several *adjacent* things WERE fixed and verified this session (menu redesign, menu-bar overlap,
graceful crash-exit) — see "Landed" below. The tree bleed is the one open item.

---

## The bug (what the owner sees)

Open a big folder (`/mnt/d/Media/Pictures`, ~16k images, tree = 47 rows), press **Shift+F**
(Folders tree). Rows appear to bleed past the panel: into the header at the top, over the photo
at the bottom. The panel background does **not** back the rows where they sit over the (bright)
image.

Reference images captured this session live in the scratchpad
(`.../scratchpad/x11_tree.png`, `v2_early.png`, `v2_late.png`).

---

## What is PROVEN (with live probe data)

I added temporary probes and drove the **live** app under X11 (see harness). At the settled
state, window 1280×800 @ ppp 1.0, 47-row tree:

```
[tree] screen=1280x800 ppp=1.000 max_h=722 body_max=687 rows=47 top_inset=30
[sdf pb_tree] response=(24,54 280x722) content=(24,54 280x722) final_h=722 max_h=722
[jobs] n=77 max_clip_y=800 n_clip_below_780=2 ppp=1.000 size=1280x800
```

Reading this:
- **Geometry is perfect.** `max_h=722` (= 800 − 2·EDGE(24) − top_inset(30) ✓). The panel window
  `response` **and** the real content `min_rect` are **both** `280×722`, sitting y=54→776, fully
  inside the 800px screen. The SDF background (`final_h=722`) encloses exactly that. So the
  `allocate_ui`/`scroll_body` sizing **and** the `sdf_panel` union-background fix all work.
- **egui's clip rects are correct.** Only **2** of 77 paint jobs have `clip_rect.max.y > 780`
  (the full-screen photo + one HUD element); everything else — including the tree rows — is
  clipped at ≤780 (the tree viewport is ~776). So egui **emits** the right scissor rects.
- **Headless clips; live does not.** `--egui-shot` renders the identical panel code through the
  identical `egui_wgpu::Renderer::render()` call and clips perfectly at every scale I tried. The
  live overlay does not. **The only difference is the render target/composite**, not the layout.

**Conclusion: this is a render/composite bug, not a panels_ui layout bug.** Stop trying to fix it
in `panels_ui.rs` — the numbers there are already right.

### Newest observation (reframes it)
In the settled screenshot (`v2_late.png`) the tree actually shows a **scrolled ~27-row window**
(`$RECYCLE.BIN`…`Contacts`), i.e. it *is* scrolling/clipping to the viewport. The visible
defect is that the **semi-transparent dark panel background does not darken the bright photo** —
rows over the black letterbox look correctly backed, rows over the bright "Full Spectrum" image
look like floating text with no panel behind them. That strongly implicates the **compositing of
the egui offscreen texture over the photo** (premultiplied-alpha handling), not the scissor.

---

## Two live hypotheses (next agent: decide between them with ONE test)

- **(A) Scissor not applied in the offscreen pass** — the ScrollArea mesh isn't cut at the
  viewport when rendered to the offscreen texture.
- **(B) Premultiplied-alpha composite mismatch** — the panel background *is* drawn correctly into
  the offscreen texture, but `fs_egui` composites it over the photo such that a semi-transparent
  dark fill fails to darken bright pixels. (My current lead — see "Newest observation".)

### THE decisive next step
**Dump the egui offscreen texture (`EguiOverlay::target`) to a PNG before compositing** and look
at it. Copy the readback logic from `egui_shot.rs` (it already maps an `Rgba` texture → PNG).
Add it in `crates/pb-app/src/egui_overlay.rs::run()` after `renderer.render(...)` into
`self.target`, gated on `PB_TREE_DEBUG`.
- If the panel background is **present and full-height** in that PNG → hypothesis **(B)**, fix in
  `crates/pb-render/src/gpu.rs` (`set_egui_overlay` @1680, `fs_egui` shader, the premultiplied
  path noted @1683 "store back to premultiplied linear; `fs_egui` composites straight" and the
  blend states @469/524/554).
- If the background is **short / rows unclipped** in that PNG → hypothesis **(A)**, the problem is
  in the egui→offscreen render pass in `egui_overlay.rs` (163–203) — check `ScreenDescriptor`
  vs the actual `self.target` size and the scissor.

Relevant compositing code already located:
- `crates/pb-app/src/egui_overlay.rs` — `run()` renders egui → `self.target` (offscreen,
  `LoadOp::Clear(TRANSPARENT)`), `ScreenDescriptor{ size_in_pixels: self.size, ppp }`.
- `crates/pb-render/src/gpu.rs:1680` `set_egui_overlay` — builds the bind group over the
  offscreen texture; comment says egui is premultiplied and `fs_egui` composites it straight.
- `crates/pb-render/src/gpu.rs` `fs_egui` shader + `BlendState`s (469/524/554) — the composite.

---

## THE self-serve X11 harness (biggest gift — use it, don't debug blind)

WSLg runs winit on **Wayland** by default, where the app can't be screenshotted or driven and
where it **intermittently segfaults** on a display hiccup. **Force winit to X11 (XWayland)** and
everything becomes automatable and stable:

```bash
export DISPLAY=:0
unset WAYLAND_DISPLAY          # <-- forces winit -> X11; app is now an X11 window
```

Tools installed this session (apt, via `wsl -d Ubuntu -u root`): **xdotool, imagemagick
(`import`), x11-utils**.

Recipe (a full script is in the scratchpad `x11v2.sh`, but it's easy to recreate):
```bash
PB_TREE_DEBUG=1 timeout 45 ~/photoblaze/target/debug/photoblaze /mnt/d/Media/Pictures >log 2>&1 &
sleep 9
WIN=$(xdotool search --class photoblaze | head -1)     # window title = "photo.jpg (n/total)"
xdotool windowactivate --sync "$WIN"; xdotool windowfocus "$WIN"
xdotool key --clearmodifiers shift+f                    # XTEST (NOT --window: winit ignores XSendEvent)
sleep 4
import -window "$WIN" out.png                           # screenshot the window
```
Notes: `--class photoblaze` finds it (title isn't "PhotoBlaze"). Key injection **must** be XTEST
(`xdotool key`, no `--window`) — winit ignores synthetic XSendEvent. Under X11 the app ran 35–45s
clean (no segfault). `PB_TREE_DEBUG=1` emits the `[tree]`/`[sdf pb_tree]`/`[jobs]` probe lines.

### WSL build gotcha that bit me hard
`rsync -a` from `/mnt/c/...` **content-syncs but preserves the old mtime**, so **cargo does not
recompile** (it "Finished in 0.18s" with stale binary). After rsync, **`touch` the changed
`.rs` files** before `cargo build`, or you'll test a stale binary and chase ghosts. (This is why
the owner "saw no change" twice.) See also [[linux-port]] memory for the rsync workflow.

**Strong recommendation for the next agent:** run Claude Code **natively inside Ubuntu** on a
Linux-filesystem git clone (not `/mnt/c`, not rsync) — edit/build/run/screenshot all local, no
mtime dance. Sync to Windows via git. The owner asked about this; it's the right call for
continued Linux UI work.

---

## Code changes this session (ALL UNCOMMITTED)

### Landed & verified — keep
- **Menu dropdown redesign** (`panels_ui.rs`, Linux-gated): `menu_item`/`menu_layout`/
  `menu_group`/`menu_separator` + `MENU_*` consts — narrow, aligned (uses `paint_vtext`
  optical centering), opaque, no card look. Owner approved earlier.
- **Menu-bar overlap fix**: `PanelFrame.top_inset` field + plumbing; top-anchored panels
  (`tree_panel`, `inspector_panel`, `scan_pill`) offset by `EDGE + top_inset`; `panel_max_height`
  subtracts `top_inset`. `main.rs render_overlay_frame` sets `top_inset = MENU_BAR_H` on Linux.
- **Graceful crash-exit** (`main.rs`, the `event_loop.run_app` call): on `Err` it prints
  "display connection lost" and `exit(1)` instead of `.expect()` panic → segfault. This tamed the
  WSLg Wayland "Connection reset by peer" crash into a clean exit.
- **`sdf_panel` union background** (`panels_ui.rs` ~1189–1198): background sized to
  `response.rect.union(content_rect)` where `content_rect = ui.min_rect()` captured in-closure.
  Correct improvement (bg can never be shorter than content). Keep regardless of the tree bug.
- **`--egui-shot` dev affordances** (`egui_shot.rs`): env toggles `PB_SHOT_PPP`,
  `PB_SHOT_TREE_ONLY`, `PB_SHOT_WARMUP`, `PB_SHOT_LAG` + `menu_dropdown_preview` + Linux
  `top_inset`. Useful; keep.

### Attempted for the tree bug — did NOT fix it (decide keep/revert)
- **`tree_panel` now uses `scroll_body`** (the shared helper the Inspector/Help use) instead of a
  raw `allocate_ui` + `ScrollArea`. Cleaner/DRY and geometry is correct, but it did **not** fix
  the live bleed (confirming the bug isn't here). Fine to keep; not a fix.

### Debug scaffolding — REMOVE before any commit
- `PB_TREE_DEBUG` probes in `panels_ui.rs` (`tree_panel` ~2088, `sdf_panel` ~1200) and
  `egui_overlay.rs` `run()` (~162, the `[jobs]` block).
- Startup **banner** `eprintln!("PhotoBlaze debug build … 2026-07-08a")` in `main.rs` (right after
  `velopack_startup`). It was a build-freshness check; drop it (or keep as a `debug_assertions`
  aid — owner's call).

---

## Environment quick-ref
- WSLg: winit defaults to **Wayland** (software lavapipe); RTX 5090 is CUDA-only for gfx Vulkan.
- Live window observed **1280×800 @ ppp 1.0**. Physical desktop differs (see
  [[windows-display-rdp-env]]).
- Wayland run is segfault-prone on display hiccups (handled gracefully now); **X11 run is stable**.
- HEIC needs `--features libheif` (system `libheif-dev`, already installed in this WSL).
- See [[linux-port]] for the menu-bar-is-egui / portability / WSL workflow context.

## Next tasks (priority order)
1. **Dump the egui offscreen texture to PNG** (above) → resolve hypothesis (A) vs (B). This is
   the single highest-value action; it turns a guessing game into a one-look answer.
2. Fix per the result: (B) → `pb-render` `fs_egui`/blend/premultiplied composite; (A) → the
   `egui_overlay` offscreen render pass / ScreenDescriptor-vs-target-size.
3. Verify with the X11 harness (screenshot the settled tree over a bright photo — rows must be
   backed by the panel, header clean, nothing over the image).
4. Strip all `PB_TREE_DEBUG` probes + the banner. Then have the owner confirm on a real Wayland
   run. Add a `CHANGELOG.md` line only if a user-facing Windows/mac fix falls out (Linux is
   experimental, usually no changelog).
