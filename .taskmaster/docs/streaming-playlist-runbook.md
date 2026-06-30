# Streaming playlist — manual test runbook

_How to verify the "browse before the scan finishes" work by hand. Covers everything
shipped in the streaming track (commits `edd8f71` → `fcfa3f9`). Pair with
`streaming-playlist-plan.md` for the design._

The automated suite already covers the pure logic (`cargo test` — 135 pb-app + 76 pb-core,
incl. the order guarantee, the anti-thrash prefetch invariant, and the no-trace privacy
test). This runbook is for the things only a human at the window can confirm: that photos
appear before the scan finishes, the chip/dialog/cancel behave, and nothing regresses.

---

## 0. Build & test setup

```sh
# Fast feedback (logic):
cargo test -p pb-core -p pb-app

# Run the viewer (release = realistic decode/scan speed):
cargo run --release -p pb-app -- <path-to-folder>
cargo run --release -p pb-app                       # bare launch (Press O to open)
```

### Make test folders

You want three shapes. The streaming is only *visible* when the scan takes long enough to
notice — a few thousand images, or a deep tree.

```sh
# A) A normal flat folder (fast scan) — any folder of photos, e.g. ~/Pictures if shallow.

# B) A BIG flat folder (thousands of files) — to see the count climb on a flat scan.
mkdir -p /tmp/pb-big && cd /tmp/pb-big
# Copy one real photo many times (fast; same bytes decode fine):
for i in $(seq -w 1 5000); do cp ~/Pictures/some-photo.jpg "img_$i.jpg"; done

# C) A DEEP recursive tree (root has a few photos, lots more in subfolders) — the
#    headline case: first photo from the root appears instantly, the rest stream.
mkdir -p /tmp/pb-deep && cd /tmp/pb-deep
for d in $(seq 1 50); do mkdir -p "sub$d"; for i in $(seq -w 1 200); do cp ~/Pictures/some-photo.jpg "sub$d/p_$i.jpg"; done; done
cp ~/Pictures/some-photo.jpg ./root_aaa.jpg   # a couple of photos in the root itself
```

> A real, varied photo library (`~/Pictures`, a NAS mount, `~/Library` on macOS) is the best
> torture test — mixed sizes, deep nesting, HEIC/RAW. Use it for the "feels instant" checks.

---

## 1. The headline: browse before the scan finishes

**Recursive tree (folder C), opened at runtime:**
1. Launch the viewer (`cargo run --release -p pb-app`), press **Shift+O**, pick `/tmp/pb-deep`.
   - ✅ A photo appears **almost immediately** (the root's first image) — you do NOT wait for
     all 50 subfolders to be walked.
   - ✅ The window title and the top-right **status card** show a count that **climbs** (`7`,
     `200`, `1,400`, … `10,002 images found`) as subfolders stream in.
2. While the count is still climbing, press **→ / Space** repeatedly.
   - ✅ You can browse the photos already loaded. Holding the key flies through them.
   - ✅ New photos keep appending at the end; the photo you're looking at never jumps or
     changes underneath you (indices are append-only).
3. Let it finish.
   - ✅ The card disappears when the scan completes; the title shows the final `X / 10002`.

**Flat folder (folder B):** open `/tmp/pb-big`.
   - ✅ Same behavior, single flat readdir — first photo fast, count climbs to `5000`.

**Normal folder (folder A):** open a small/typical folder.
   - ✅ Feels instant, no card flash, no dialog (the scan finishes before any of that shows).

---

## 2. Every entry point streams (not just the runtime picker)

1. **Startup launch (CLI / double-click):** `cargo run --release -p pb-app -- /tmp/pb-deep`.
   - ✅ The **window appears immediately** (it does not hang on a black/absent window while the
     tree is walked), then the first photo streams in. (Before this work, the window didn't
     show until the whole scan finished.)
   - ✅ Console prints `PhotoBlaze: scanning folder…` (not a final count up front).
2. **macOS double-click / file association:** in Finder, drag `/tmp/pb-deep` onto the built
   app (or set PhotoBlaze as a folder handler) — same instant-window behavior.
3. **`Ctrl+R` (recursive toggle):** open `/tmp/pb-deep` **non-recursively** first
   (`cargo run --release -p pb-app -- --no-recursive /tmp/pb-deep` → only `root_aaa.jpg`),
   then press **Ctrl+R**.
   - ✅ Toast "Recursive folders: on"; the subfolders **stream in** behind the current photo
     (the count climbs); the photo you were on stays put.
4. **Recursive-off escape hatch:** while a big recursive scan is still streaming (step 1),
   press **Ctrl+R** to turn recursion **off**.
   - ✅ Toast "Recursive folders: off"; it **immediately drops to just the root folder's
     photos** (fast) instead of waiting for the recursive walk — "stop, I only wanted this
     folder."

---

## 3. The "Scanning Folder" dialog (now first-image-gated)

The centered dialog now only appears if the **first** image is itself slow to find (rare —
only when even the root directory read is slow, e.g. a cold network drive).

1. Normal/fast folders: ✅ **no dialog** (the first photo shows before the 250 ms threshold).
2. To force it: open a folder on a slow network mount, or a directory whose first readdir is
   genuinely slow.
   - ✅ The "Scanning Folder" dialog appears with a spinner, "N images found", the current
     subfolder, and a **Cancel** button.
   - ✅ It closes when the scan finishes (or on Cancel).
3. **Archives still use the dialog fully** (they can't stream): open a big `.7z`.
   - ✅ The determinate progress-bar dialog behaves exactly as before — unchanged.

---

## 4. The scan status card + Cancel Scan button

While a scan streams in (after the first photo, step 1) and once it's lasted past ~250 ms:
1. ✅ A small **status card** sits in the **top-right, just below the loading pie**:
   ```
   Scanning "Iceland 2024"     (semibold heading — the folder you opened)
   8,230 images found          (dimmer count line)
   ( ■ Cancel Scan )           (subtle rounded-border button)
   ```
2. ✅ The count is the **browsable** total (what you can actually navigate to right now), and
   it climbs as batches land. The card hugs the right edge and doesn't jitter as the number
   widens.
3. **Click the Cancel Scan button** (only the button is the click target, not the whole card).
   - ✅ The scan **stops**, a "Scan stopped" toast flashes, and **whatever streamed in so far
     stays browsable** (it does not revert to a previous view or blank).
   - ✅ Clicking does **not** start a drag-to-pan (the click is consumed). Clicking elsewhere
     on the card (not the button) does nothing special.
4. ✅ A quick folder (finishes in <250 ms) never flashes the card.
5. In **borderless fullscreen** (press **F**), the title bar is hidden — ✅ the card is then
   the only place the folder + count are visible. Confirm it's legible over a bright photo.

---

## 5. Stop Scanning (menu + optional hotkey)

1. Start a big scan (folder C). Open the **File** menu (windowed mode).
   - ✅ **Stop Scanning** is **enabled** while the scan runs, **greyed out** otherwise.
2. Click **File ▸ Stop Scanning**.
   - ✅ Same as the card's Cancel Scan button: scan stops, "Scan stopped" toast, partial kept.
3. (Optional) Bind a key: **Settings ▸ Shortcuts**, find **Stop scanning**, assign a key, Save.
   - ✅ The key now cancels a running scan. (Esc is unchanged — it still Quits.)

---

## 6. Cancel-keeps-partial & supersession

1. **Keeps partial:** cancel a half-finished scan (Cancel Scan button, menu, or recursive-off).
   - ✅ You can still flick through everything that had streamed in before the cancel.
2. **Supersede:** while folder C is streaming, press **Shift+O** and open a *different* folder.
   - ✅ The old scan is abandoned; the new folder starts streaming from its first photo. No
     mixing of the two folders' photos.

---

## 7. Delete during a scan (tombstones — Codex P1)

1. Open folder C; while it's **still streaming**, delete the displayed photo (**Del** →
   confirm if prompted, or the Edit menu).
   - ✅ The photo is removed and you advance to the next one.
   - ✅ As more batches stream in, the deleted photo **does not reappear** (the count keeps
     climbing, but that file stays gone).
2. Delete a few more mid-scan, then let the scan finish.
   - ✅ None of the deleted photos are in the final playlist.
   - (Sanity: deletes still actually remove/Recycle the file on disk — that's the existing
     delete behavior, unchanged.)

---

## 8. Random (Enter) during a scan — no decode thrash

The risk this guards against: the shuffle deck regenerates each batch, so naively prefetching
the random look-ahead would decode-then-evict photos you never see.

1. Open folder C (big). While it's streaming, press **Enter** (random) a few times.
   - ✅ Random still works — it jumps to random loaded photos (the first jump may be a hair
     slower than when settled, since it's decode-on-demand during the scan; preview-first
     softens it).
   - ✅ The machine doesn't spin up / fans don't roar from runaway decoding while you sit on a
     streaming folder. (For a rigorous check, run with metrics — see below — and confirm the
     decode count stays bounded, not `deck-size × batches`.)
2. After the scan completes, press **Enter** repeatedly / hold it.
   - ✅ Random flies as fast as sequential (the deck is final; the look-ahead is prefetched
     again — pressing Enter is a rebind, not a decode).

> Optional instrumented check: run with the metrics flag (the one that enables
> `POOL_DECODE_MS` reporting — see `main.rs`), repeat step 1, and confirm the pool decode
> count is roughly `sequential window + your Enter presses`, not a multiple of the batch count.

---

## 9. Ordering is unchanged (no surprises)

The streaming walk uses `sort_by_file_name`, which is **identical** to the old
walk-then-`paths.sort()` order (verified by `streaming_walk_order_matches_paths_sort`).

- ✅ Open a folder you've used before — the photo order is the same as it always was.
- ✅ In a recursive tree, order is depth-first / "folder by folder, alphabetical within each."
- ✅ Numeric names stay byte-lexicographic (`img10` before `img2`) — natural sort was **not**
  introduced (it's a separate, deferred opt-in).

---

## 10. No-trace privacy still holds

The streaming scan is still read-only. The automated test
(`viewing_a_folder_writes_nothing_to_disk`) asserts this, but to eyeball it:
- ✅ Open and browse a folder; the card/count/folder name are all on-screen only. Nothing
  is written to disk by *viewing* (only the explicit Delete / Save-rotation commands write,
  unchanged).

---

## 11. Snapshot cost sanity (optional)

```sh
cargo run --release -p pb-source --example fssource_bench
```
- ✅ Prints clone + `FsSource::new` timings at 10k / 100k / 1M paths. Even 1M should be tens of
  ms (paid off the event loop, on the scan worker). Confirms the per-batch snapshot is cheap;
  if a row were hundreds of ms, that's the signal to switch to a segmented buffer (not needed
  on current numbers).

---

## Quick regression checklist

- [ ] Big recursive folder: first photo appears fast, count climbs, browse while it streams.
- [ ] Double-click / CLI launch a folder: window appears immediately.
- [ ] `Ctrl+R` on → subfolders stream; `Ctrl+R` off mid-scan → instantly just the root.
- [ ] Status card shows in the top-right; clicking Cancel Scan stops the scan and keeps the partial.
- [ ] File ▸ Stop Scanning enabled only during a scan; cancels.
- [ ] Delete during a scan → the photo never comes back.
- [ ] Random during a scan works and doesn't thrash; flies after completion.
- [ ] Normal small folder: instant, no card/dialog flash.
- [ ] Archives (.7z): progress-bar dialog unchanged.
- [ ] Order of a known folder is unchanged.
- [ ] `cargo test -p pb-core -p pb-app` green; `cargo clippy --all-targets -- -D warnings` clean.

---

## Known limitations / deferred polish (not bugs)

- **Sequential wrap during a scan:** if you race to the *last loaded* photo and press → while
  more are still streaming, it wraps to the first (rather than waiting for the next batch).
  Minor; the prefetch already avoids wasting work on this. Deferred.
- **Cancel button hover cursor:** the Cancel Scan button is clickable but the pointer doesn't
  yet change to a hand on hover. The bordered button + the menu item are the discoverable
  affordances. Deferred polish.
- **Provisional instant decode for single-file opens:** opening one file in a *huge* flat
  folder waits for that file to be reached in the (fast) readdir before showing it; we don't
  yet decode the known path before the scan. Fine in practice (flat readdir is quick).
