# 3D models — STL / OBJ / 3MF and friends (tasks #151–#154)

Status: **plan.** Written 2026-11-23 from an evaluation session (codebase survey + library
research). Owner decisions D1–D5 are settled (see *Owner decisions*, near the end).

| Task | What | Phase |
|---|---|---|
| **#151** | Still renders of STL / OBJ / 3MF: parse, render to a picture, browse like photos | 1 |
| **#152** | Slicer-project 3MF fidelity (Bambu / Orca / Prusa): plates, filament colours, painted multi-material, `.gcode.3mf`, print facts | 1.5 |
| **#153** | Live 3D view: orbit, zoom, pan on the GPU | 2 |
| **#154** | More formats: glTF/GLB, PLY, AMF, OFF, FBX, G-code thumbnails; research notes for STEP, USDZ | later |
| **#155** | Dependency advisories found during this research: `zip` ≥ 9 and `quick-xml` ≥ 0.41 across the tree | adjacent |

## The feature in one paragraph

A folder (or a ZIP — Thingiverse and Printables downloads are ZIPs of STLs) of 3D-print
files browses exactly like a folder of photos: every model shows as a shaded picture, blaze
mode flicks through them, the Info panel says what the model *is* (triangles, size in mm,
objects, who made it), and Copy / Export / Describe work on the picture. That is phase 1, and
it deliberately reuses the whole image path — a model is an **image item whose decoder
happens to rasterize a mesh**, the same trick SVG already plays. Phase 2 lets the user grab
the parked model and turn it: orbit, zoom, pan, reset, at refresh rate.

## The Prime-Directive read

- **Blazing:** the next likely action is "the next model". Each one needs a display-fit
  picture *before* the keypress, which means parse + render on the decode pool, inside the
  prefetch ring, cancellable. The wall is parse time on big files (a 100 MB+ 3MF is mostly
  XML text), not drawing. Slicer 3MFs carry an embedded preview PNG — the preview rung for
  free (see *Details that bite: the thumbnail jump*).
- **Parked:** the next likely action on a model is **rotate it**. Phase 2's anticipation
  rule: when the user parks on a model, keep the parsed mesh (RAM, budgeted) and upload it to
  the GPU *then*, so the first drag is a camera update, never a parse or an upload. Never
  repeat likely work: the parsed mesh is retained, so leaving 3D and coming back is a rebind.
- **Measure:** new `PB_PERF` episodes — open → first model on screen; park → first-drag-to-
  photon; orbit frame time p95 on the largest corpus mesh.

## Complexity evaluation

| Piece | Complexity | Why |
|---|---|---|
| STL parse | **Low** | Binary is an 80-byte header + count + 50 bytes/triangle; ASCII is line text. A few hundred lines hand-rolled, or a crate. Main care: size pre-flight from the count, ASCII files mislabelled as binary (header starting `solid`), hostile counts. |
| OBJ parse | **Low–medium** | Text, but polygons need triangulating, negative (relative) indices, `mtllib` side files (inside an archive or beside the file — and the decoder only gets *bytes*), no units. A crate does this well. |
| 3MF core parse | **Medium** | OPC package (ZIP + `_rels/.rels` → the root model part — don't hard-code `3D/3dmodel.model`), XML mesh, `<build>` items with 3×4 transforms, `<components>` (recursive → depth limit + cycle check against hostile files), units (micron…metre), base materials and colour groups. `zip` is already in our tree; `quick-xml` needs a new ≥ 0.41 (advisories, see *Libraries*). |
| 3MF Production extension | **Medium** | Components that point at *other* model parts in the package (`p:path`) — what Bambu Studio and Orca write. Without it a Bambu project renders **empty**. Not optional for "3MF works". |
| Slicer fidelity (#152) | **Medium–high** | Vendor-specific, undocumented-ish config files: plates, filament colours, per-object extruder, per-triangle paint (`paint_color`). Real value, real rabbit hole — separate task. |
| Still render (CPU) | **Medium** | Z-buffer rasterizer, flat shading, supersampled edges, camera auto-framing. Well-trodden; the hard part is making it fast on 1M+ triangles and making it *look* good. |
| Info panel facts | **Low–medium** | New fact rows on both shells (egui + native inspector): a model has no pixel resolution, it has a size in mm. |
| OS integration | **Low** | Picker filters, macOS document types (3MF needs an imported UTI), Windows `OpenWithProgids` — never claim the default handler. |
| Live 3D (#153) | **High** | A new render pass in `pb-render` (depth buffer, MSAA, mesh buffers), a camera, a new interaction mode that has to coexist with 2D pan/zoom and nav keys, input on two shells (incl. trackpad gestures), VRAM budgeting, and seamless still → live hand-off. Roughly the size of the export arc's rungs 1–3. |
| glTF / PLY / AMF / OFF (#154) | **Low–medium each** | Once the canonical mesh + renderer exist, each is a parser. glTF's PBR materials + textures are the expensive part if done properly. |
| STEP / IGES | **Very high** | B-rep CAD, not meshes: needs a tessellation kernel. Out of scope; research note only. |

**Overall:** phase 1 (#151) is a medium-sized feature, comparable to adding RAW support —
most of the work is the 3MF parser and a good-looking, fast rasterizer. Phase 2 (#153) is a
large one, mostly in `pb-render` and the interaction model.

## Libraries

Researched 2026-11-23 against crates.io / GitHub / NVD / RustSec. Versions are as of that
date; re-check before adding a dependency.

### Picks

| Format | Pick | Why |
|---|---|---|
| STL | **Hand-rolled in `pb-mesh`** | The format is trivial, and we need what no crate gives: a budget pre-flight from the header count, cancel checks, and lowering straight into our `Model`. The popular crate (`stl_io` 0.11, MIT) has no fuzzing; ours gets a target. |
| OBJ | **`tobj` 4.0.5** (MIT, fuzzed, MTL support) | ⚠ Its triangulation only does fans, so concave polygons come out wrong: load faces untriangulated and ear-clip them ourselves (or accept fans in v1 and pin it with a test). Alternative: `mesh-loader` 0.1.13 (Apache-2.0, fuzzed; STL + OBJ + Collada). |
| 3MF | **Hand-rolled in `pb-mesh`** over `zip` + `quick-xml` **≥ 0.41** | No crate is adoptable as-is (table below): none offers cancellation or refuse-before-reserve budgets, the best-tested one doesn't merge Production-extension meshes, and none reads slicer painting. The [3MF Consortium's conformance suites](https://github.com/3MFConsortium) (positive + negative files) are the test corpus. Check their licence before vendoring any file into this public repo. |
| CPU rasterizer | **Hand-rolled** | `euc` is the only established crate and its last release is from 2021. Edge functions + z-buffer + Lambert is a few hundred lines. |

### 3MF crates considered

| Crate | Licence | Production (meshes in other parts) | Materials | Fuzzed | Verdict |
|---|---|---|---|---|---|
| `threemf` 0.8.1 (Fornjot) | 0BSD | ✗ (every part parsed as an unrelated model) | base only | ✗ | Too thin |
| `threemf2` 0.4.1 | MIT OR Apache-2.0 | ✓ | ✓ (unstable API) | ✗ | Best reference / A-B candidate; one developer |
| `lib3mf` 0.1.7 (telecos) | MIT | parses + validates, doesn't merge | ✓ full | ✓ nightly + Miri + conformance suites | Best tested, but young and mostly agent-written |
| `lib3mf-core` 0.4.0 | BSD-2-Clause | ✓ (+ Bambu config parsing) | ✓ | ✓ (7 targets) | Quiet since March |
| lib3mf 2.5.0 (official, C++) | BSD-2-Clause | ✓ | ✓ | not in OSS-Fuzz; 1 CVE (2021) | No Rust bindings; ~2.3 MB native |

### Ruled out, and why

- **Assimp (via `russimp`)** — `russimp` was archived 2025-08-12, and Assimp's CVE stream is still
  running: 35 published in 2025 and 48 so far in 2026, including a heap out-of-bounds write in
  the PLY importer. Never point it at hostile files.
- **macOS QuickLook (`QLThumbnailGenerator`)** — it **writes every thumbnail to a persistent
  system cache** (confirmed 2026-11-23 on macOS 27.2: ThumbnailsAgent logs "Adding image to
  cache" then writes `com.apple.QuickLook.thumbnailcache`). That is a viewing trace and violates
  the Second Directive. It also can't do 3MF.
- **ModelIO / SceneKit** — ModelIO imports STL/OBJ/PLY/USD but not 3MF/glTF; SceneKit is deprecated
  from macOS 26. Our own cross-platform path is needed for 3MF regardless.
- **Windows.Graphics.Printing3D** — the API is still documented, but the 3D Builder and 3D Viewer apps
  around it have left the Store (the last of them on 2026-07-01). Not a foundation to build on.

### Dependency hygiene this arc has to respect

- `quick-xml` < 0.41 carries RUSTSEC-2026-0194 (quadratic duplicate-attribute check) and
  RUSTSEC-2026-0195 (unbounded namespace allocation in `NsReader`) — both reachable from a hostile
  3MF. The 3MF parser must use ≥ 0.41. (Already in the lock at 0.30 / 0.37 / 0.39 through other
  dependencies; that is task #155.)
- `zip` 2.4.2 with `aes-crypto` (what `pb-source` and `pb-app-core` use today) carries
  RUSTSEC-2026-0545 (AES entries accepted without their HMAC when truncated or empty), which is
  fixed in ≥ 9.0. Tracked as #155.
- **`pb-mesh` is on `zip` 9.8, not the tree's 2.x, and must stay ≥ 4** (2026-11-23). Bambu Studio
  writes 3MFs whose central directory carries a ZIP64 extra field with a **wrong local-header
  offset** next to a correct 32-bit one. The spec says the ZIP64 value applies only to fields
  set to `0xFFFFFFFF`; zip 2.x used it anyway, failed the entry, retried for an earlier end
  record and reported the misleading `Could not find EOCD`. zip ≥ 4 (and `unzip`) read these
  files. Pinned by `bambu_style_stale_zip64_offsets_still_open`. `pb-source` has the same bug
  for any ZIP written this way, which is another reason for #155.

### Slicer 3MF layouts (for #152 and the thumbnail rung)

From each slicer's current source, 2026-11-23. Treat these as leads, and treat the corpus as the authority.

| | Bambu Studio / OrcaSlicer | PrusaSlicer 2.9 | Cura 5.x |
|---|---|---|---|
| Thumbnail in `_rels/.rels` (standard OPC thumbnail relationship) | ✓ → `/Metadata/plate_1.png` or a designer cover `/Auxiliaries/.thumbnails/thumbnail_3mf.png`; plus Bambu-specific `cover-thumbnail-middle` / `-small` types | ✓ → `/Metadata/thumbnail.png` | ✓ → `/Metadata/thumbnail.png` |
| Per-plate images | `Metadata/plate_N.png`, `plate_N_small.png` (128²), `plate_no_light_N.png`, `top_N.png`. ⚠ **`pick_N.png` is an object-ID picking render: never show it.** Sliced projects add `plate_N.gcode` + `plate_N.json` | — | — |
| Meshes | `3D/3dmodel.model` → Production components into `3D/Objects/*.model` | one `3D/3dmodel.model` (the 3.0 alphas split objects out) | `3D/3dmodel.model` |
| Filament colours | `Metadata/project_settings.config` (JSON, `filament_colour`); also `slice_info.config` | `Metadata/Slic3r_PE.config` (`; filament_colour = …`) | not found |
| Extruder per object | `Metadata/model_settings.config`: `<object>` / `<part>` → `<metadata key="extruder" value="N"/>` | `Metadata/Slic3r_PE_model.config`: `<volume firstid lastid>` ranges | `cura:extruder_nr` |
| Per-triangle paint | `paint_color="…"` on `<triangle>` | `slic3rpe:mmu_segmentation` | — |

The paint encoding (shared by both) is a reverse-nibble hex string that encodes a **recursive
subdivision** of the triangle: 2 bits give the number of split sides, 2 bits the special side,
and leaves carry a state (the filament index; 0 = the object's default extruder; escape
nibbles for ≥ 3). It can multiply the triangle count, so count the subdivisions against the
budget before expanding. ⚠ **Bambu Studio, OrcaSlicer and PrusaSlicer are AGPL-3.0.** Read
them to learn the format, then implement from a written description (this section): never
port or translate their code.

### Typical sizes (for budgets)

Thingi10K (9,997 Thingiverse models): median **4.6k** triangles (0.23 MB binary STL), p95 201k
(10 MB), p99 719k (36 MB), max 5.67M (284 MB). 54 models exceed 1M triangles. Modern sculpts and
scans run larger; MakerWorld reportedly warns at ~1M triangles (unverified). Binary STL = 84 +
50 bytes per triangle.

### Formats for #154

| Format | Candidate | Note |
|---|---|---|
| glTF / GLB | `gltf` 1.4.1 (MIT OR Apache-2.0) | Parse with `Gltf::from_slice`. **Never** use `import`, which follows external URIs to disk (the path-traversal class Assimp keeps getting CVEs for); external buffers only through the sibling-file seam. Not fuzzed; no release since 2024-05. |
| PLY | `ply-rs-bw` 4.0.1 (MIT) | Maintained fork of the dead `ply-rs`; no `unsafe`; its CI fuzzes. |
| OFF | `off-rs` 2.0.0 (MIT OR Apache-2.0), or hand-roll | Fuzzed; the format is trivial. |
| AMF | hand-roll | XML, sometimes zipped; nothing established. |
| Collada | `dae-parser` 0.11 or `mesh-loader` | `dae-parser` is not fuzzed. |
| FBX | `ufbx` 0.11.5 bindings (MIT OR Unlicense, C via FFI) | Heavily fuzzed upstream (AFL + structured). Viable, not just research. |
| USD / USDZ | `openusd` 0.9.0 (MIT, pure Rust) | Very active, but no fuzzing and 14 `unsafe` sites, and `.usdc` is a big attack surface. |
| STEP / IGES | `truck-stepio` 0.3 / `monstertruck` (Apache-2.0); OpenCascade 8.0.2 (LGPL-2.1 + exception) | B-rep: needs surface tessellation too. OCCT is acceptable only dynamically linked; `cadrum` links it statically, which breaks our licence rule. |

## Architecture — where the code goes

Per `docs/where-code-goes.md`, first match wins:

1. **New crate `pb-mesh`** (pure: bytes in, no `std::fs`, no GPU, no `cfg`). It is a new
   crate rather than more of `pb-decode` because three crates consume it — `pb-decode` (the
   still), `pb-render` (GPU buffers, phase 2) and `pb-app-core` (the camera session) — and
   because the crate boundary *is* the A/B seam (parser choice, rasterizer choice). Holds:
   - `mesh.rs` — the canonical `Model`: positions (`f32`), triangle indices (`u32`), optional
     per-triangle colour, objects + build transforms resolved, bounding box, units, a
     `ModelFacts` block for the panel. One representation every format lowers to.
   - `budget.rs` — refuse-before-reserve pre-flight: triangle count from the STL header,
     uncompressed part sizes from the 3MF ZIP directory, a cap on triangles and bytes.
   - `stl.rs`, `obj.rs`, `threemf/` (`opc.rs`, `model.rs`, `materials.rs`, `production.rs`,
     later `slicer.rs` for #152).
   - `camera.rs` — auto-framing, orbit/turntable/dolly math (pure, property-tested; phase 2
     reuses it).
   - `raster.rs` — the CPU rasterizer behind a `MeshRasterizer` trait (so a GPU offscreen
     still can be A/B'd against it later).
2. **`pb-decode::model`** — `ModelDecoder: ImageDecoder`, routed **by extension** in
   `decode_named_bytes_inner` beside SVG/RAW/TGA (none of these formats has a usable magic
   sniff: 3MF is a ZIP, STL/OBJ are arbitrary text or bytes). Calls `pb-mesh` parse +
   render, hands RGBA8 to `common::finalize`, inside `catch_panics`. Extensions go into
   `is_supported_extension`, which automatically makes the **scanner and archive indexing**
   pick them up (`scan::is_supported_archive_entry` is `is_supported_extension ∪ video`).
3. **No new `LibraryItemKind` in phase 1.** A model is `LibraryItemKind::Image`. This is the
   cheap, safe choice: every `Image` guard already reads bytes, which a model needs, and no
   exhaustive match grows a fourth arm. Phase 2 marks a decoded model the way animation is
   marked today (`DecodedImage::animated` → play-hint pill), with a capability flag rather
   than a new kind — see *Owner decisions* D1.
4. **`AppCore` touch points (phase 1):** `full_res_eligible` must exclude models (like SVG
   they are rasterized per viewport — there is no geometry-independent "original"); the
   Info/Details panel facts (`meta.rs` / `media_details.rs`). Phase 2 adds a real subsystem:
   `model_session.rs` (camera state, input routing while live, mesh retention) **plus** its
   `app_core_impl/model.rs` concern file in the same commit (the two-halves rule).
5. **Shells:** picker filters (`pb-app` `IMAGE_FILTER_EXTS` → a "3D models" group; the Mac
   open panel), `packaging/macos/Info-swift-host.plist` document types (`Alternate` rank;
   STL/OBJ/PLY/USDZ have system UTIs, 3MF needs `UTImportedTypeDeclarations`), Windows
   `default_app.rs` `OpenWithProgids` only. Phase 2: gesture wiring on both shells.

## Rendering the still (phase 1)

- **CPU rasterizer on the decode pool first.** It fits the `ImageDecoder` seam with zero new
  plumbing — the prefetch ring, cancellation, thumbnails strip, Export, Copy, Describe and
  archive entries all work unchanged — it is deterministic (golden images need no GPU), and
  the pool renders N models in parallel. A GPU offscreen render is faster per mesh but needs
  the device on decode workers and either a readback or a new resident-texture hand-off; it
  goes behind `MeshRasterizer` as the A/B alternative if the corpus p95 says the CPU path
  can't keep up. (`pb-render` already does headless offscreen renders for golden tests, so
  the alternative is not exotic.) The 2026-11-23 research argued for GPU-first on exactly
  that basis. We start on the CPU anyway: `pb-decode` stays GPU-free, still renders don't
  contend with the presenter's queue mid-blaze, and goldens stay deterministic. The
  measurement in #151.5 settles it.
- **Fast path:** flat shading means one colour per triangle — the inner loop writes a
  constant colour + depth, no per-pixel lighting. Transform all vertices once, back-face cull
  only when the mesh is known closed (two-sided otherwise), half-space rasterization with
  bounding boxes, then tile-parallel with `rayon` if one thread measures too slow.
  Estimates to verify, not facts: ~1M triangles in tens of ms at display fit.
- **Look:** recompute face normals (STL's stored normals are frequently wrong); a key + fill
  light plus ambient, gamma-correct; **transparent background** so the viewer's own backdrop
  shows, like a transparent PNG. Colour: 3MF material colours when present, else a default
  material (D3).
- **Antialiasing rides the refinement ladder.** At a 7680-wide fit, 2× supersampling is a
  15360×4320 buffer (~0.5 GB colour + depth), far too slow for a blaze frame. So the *fit*
  rung renders at 1× (hard edges, fast), and parking lets a *refined* rung re-render
  supersampled, downscaled through the existing Lanczos `finalize`, with the supersample
  factor capped by a pixel budget. That is the same "blaze shows what's ready, parking lets
  the ladder finish" contract every other format follows.
- **Camera:** a 3/4 view from front-right-above, mild perspective, auto-framed to the bounding
  sphere of everything in the build. **Up axis per format:** Z-up for STL / 3MF / AMF (3MF's
  spec mandates it; printing convention for STL), Y-up for OBJ / glTF (glTF's spec; OBJ
  convention). The aspect ratio of the rendered picture follows the framed silhouette, so a
  long thin part isn't letterboxed in a square.
- **Size semantics:** like SVG, `orig_width/orig_height` are the render size; zooming a still
  in phase 1 magnifies the fit render (honest, slightly soft — phase 2's dolly is the real
  zoom).

## Live 3D (phase 2, #153)

- **Renderer:** a mesh pass in `pb-render` — vertex/index buffers, `Depth32Float`, 4× MSAA —
  drawing into the existing fp16 scRGB intermediate, so the present / tone-map / HDR path is
  unchanged. Flat shading from screen-space derivatives (`dpdx`/`dpdy` cross product) means
  an indexed mesh with no per-face vertex duplication. Shading constants are **shared with the
  CPU rasterizer** and pinned by a parity golden test (CPU still vs GPU frame at the default
  camera, nv-flip tolerance), so the hand-off from still to live doesn't visibly jump.
- **Session:** `ModelSession` in `pb-app-core` owns the camera and routes `CoreEvent`s while
  live (pointer drag → orbit, scroll/pinch → dolly-to-cursor, secondary/modifier drag → pan),
  emits draw effects only when the camera changes (no idle 120 Hz loop), and ends on any nav
  input. Camera math lives in `pb-mesh::camera`.
- **Anticipate:** on park, upload the retained mesh to the GPU under a VRAM budget
  (refuse-before-reserve; over budget → stay a still with an honest toast). Both shells
  already forward pointer, scroll and pinch events to the core.
- **Interop:** Copy / Export / Describe of a live model capture the *current view* — the
  `frame_capture.rs` "which frame?" question gains a third answer.

## Details that bite

- **3MF is a ZIP, and must never become an archive door.** Today `archive_kind` is
  extension-only, so `.3mf` is safe — pin it with a test (`archive_kind("x.3mf") == None`,
  `classify_library_file` → `Image`) so nobody "improves" door detection into a magic sniff.
- **`full_res_eligible`** keeps an "original" for every `Image` except SVG and RAW; a model
  must be excluded the same way. (Aside: it matches `"svg"` but not `"svgz"`, a small
  existing inconsistency worth fixing at the same time.)
- **The thumbnail jump.** A slicer's embedded plate PNG is a perfect blaze-mode face *and*
  thumbnail-strip tile, but it is a different camera, lighting and colour from our render, so
  refining from it to our render visibly jumps. See D2. Find it through the standard OPC
  thumbnail relationship in `_rels/.rels` (every slicer declares one), not by guessing
  paths, and never show Bambu's `pick_N.png` (an object-ID render, not a picture).
- **OBJ polygons:** `tobj`'s built-in triangulation is fan-only, so concave faces render wrong
  unless we ear-clip them ourselves.
- **AGPL slicers:** the slicer formats are learned from AGPL-3.0 source code. Implement them from
  the written notes in *Libraries*; never port the code.
- **`.gcode.3mf`** (Bambu "sliced" projects) can carry plate thumbnails and G-code with little
  or no mesh — show the thumbnail, say so in the panel, don't error.
- **OBJ `mtllib`** references a sibling file; the decoder sees only bytes + a name. Phase 1
  ignores materials it can't reach (default material), and #154 adds a sibling-lookup seam
  (the same shape as subtitle sidecars: a list of sibling names, works for folders *and*
  archives).
- **Hostile input:** component recursion and cycles (depth limit + visited set), indices out
  of range, NaN/Inf vertices (would poison the bounding box and the camera), ZIP bombs (cap
  inflated part sizes the way `svg.rs` caps SVGZ), triangle counts that lie. Every parser gets
  a `cargo-fuzz` target in `fuzz/` before it ships.
- **Huge files:** 3MF parse is XML-bound. Stream it (`quick-xml` reader), don't build a DOM,
  and check the cancel flag every N elements so a nav input abandons a 100 MB parse.
- **Units:** STL and OBJ are unitless (assume mm and say "units" in the panel when unknown);
  3MF declares units.

## Privacy

Nothing new on disk: parse and render are RAM-only like every decode, the retained mesh
(phase 2) is a RAM/VRAM cache with a budget that dies with the process, and the no-trace test
gets a model fixture. **Never use macOS QuickLook thumbnailing** for 3D files: it was
confirmed to write every thumbnail into the system's persistent QuickLook cache (see
*Libraries → Ruled out*). Rendering is ours, end to end.

## Licensing

Pure-Rust parsers only (MIT / Apache / BSD class) for #151–#153, audited into
`THIRD-PARTY-NOTICES.md` as they land. No Assimp (see Libraries). `ufbx` (#154) would be the first
C dependency of the arc; it is permissive, so that is a build-risk question, not a licence one.
STEP via OpenCascade would be LGPL (dynamic linking only) and a large native dependency: a
separate decision, not part of this arc. Slicer source code is AGPL-3.0, so read it but never
port it.

## Tests (TDD — write first)

- **Parsers:** tiny hand-written fixtures in-tree (ASCII + binary STL, OBJ with quads and
  negative indices, 3MF core / production / materials / multi-item transforms / units), plus
  property tests: no parser panics on arbitrary bytes, a framed camera contains the whole
  bounding box, a resolved component tree never exceeds the depth limit.
- **Rasterizer:** golden images (nv-flip, as `pb-render` does) for a cube, a sphere, a
  coloured 3MF, a degenerate mesh; deterministic across platforms.
- **Integration:** a model in a folder and in a ZIP scans and decodes; `.3mf` is never a
  door; the no-trace test with a model in the sandbox; panic-safety through `catch_panics`.
- **Conformance:** run the 3MF Consortium's positive / negative conformance suites from a local
  checkout (vendor files into the repo only if their licence allows it).
- **Fuzz:** one `cargo-fuzz` target per parser.
- **Corpus** (local only — the repo is public; paths go in `.taskmaster/local/notes.md`): real
  Printables/Thingiverse STLs, a Bambu Studio project 3MF (multi-plate, painted), an Orca and
  a PrusaSlicer 3MF, a `.gcode.3mf`, a 1M+ triangle stress mesh.

## Owner decisions (2026-11-23)

- **D1 — phase 2 entry: direct manipulation.** On a parked model a drag simply starts
  orbiting (the GPU mesh is already resident); `P` is the keyboard way in; the play-hint pill
  reads "3D". Nav keys always leave.
- **D2 — embedded slicer thumbnails: thumbnail strip + fallback.** Use them for the thumbnail
  strip and as the fallback (no mesh, over budget), and for the blaze-mode preview rung **only**
  when our render of that file is measurably slow (e.g. big 3MFs). Otherwise go straight to our
  render, so the picture doesn't change composition under the user.
- **D3 — default material colour: neutral grey.** 3MF colours are always honoured. A
  user-chosen default colour may come later (a Settings option, not in phase 1).
- **D4 — models load by default.** No "Show 3D models" toggle in phase 1: STL / OBJ / 3MF in a
  folder are simply part of the deck. A toggle (mirroring *Show Archives*) stays a possible
  later addition if photo folders with stray STLs turn out to annoy.
- **D5 — file associations: alternate handler only.** Register as an *alternate* handler on both
  platforms (Open With), never the default: a slicer owns `.3mf`.

## Handoff

### 2026-11-23 — phase-1 groundwork landed (Mac session)

**Built:** the `pb-mesh` crate (canonical `Model`, `Limits`, STL hand-rolled, OBJ via `tobj`,
3MF hand-rolled with core + Production + base materials / colour groups + metadata + the OPC
thumbnail, auto-framing camera, the CPU rasterizer behind `MeshRasterizer`);
`pb-decode::model` routed by extension, `is_supported_extension` + `is_model_extension`; models
(and `.svgz`) excluded from `full_res_eligible`; the Mac open panel, Mac `Info.plist` document
type (Alternate) + imported `io.3mf.3mf` UTI, the winit picker filter, a fourth Windows ProgId
`<APP_IDENT>.Model` (Open With only); a `mesh_parse` fuzz target; the no-trace test seeds an
STL; `examples/model_png` (renders a model to PNG through the real decode path); CHANGELOG.

- **Verified (macOS):** `pb-mesh` 37 tests, `pb-decode` 283, `pb-app-core` 1102, `pb-source` 91;
  clippy `-D warnings` on those crates; the Windows cross-check (`cargo clippy -p pb-app
  --all-targets --target x86_64-pc-windows-msvc -- -D warnings`), which is **compile-only**; the Swift host
  builds, and the built `Info.plist` carries the 3D Model type. A real-file sweep: all 93 models
  bundled with Bambu Studio (STLs + calibration 3MFs, which use Production components and
  colour groups) decode with no errors. At a 3840×2160 fit the render (release, one thread) takes
  **p50 12.6 ms, p95 26.9 ms, max 63 ms** (3DBenchy, 11 MB, ~225k triangles). The first cut
  tested every bounding-box pixel and was 7× slower on thin triangles; the rasterizer now walks
  per-row spans.
- **Not verified:**
  - **The app itself, on either platform.** Nobody has launched it on a folder of models yet.
    Owner to test on the Mac: a folder of STLs, a ZIP of STLs, a Bambu project 3MF, blaze
    through them, the thumbnail strip, Export / Copy of a model.
  - **Finder / Explorer "Open With".** LaunchServices has to pick up the new document type
    (an installed build or `lsregister`) and Windows has to re-register.
  - **The fuzz target was never run** (cargo-fuzz isn't installed on this Mac). The proptest
    "no parser panics on arbitrary bytes" is the only hostile-input sweep so far.
  - **OBJ on real files** (there were none in the corpus) and **real Bambu / Orca project files**
    (multi-plate, painted). The calibration 3MFs are small.
- **Known gaps in this cut (still #151):**
  - **No cancellation inside a parse**, so a nav input can't abandon a 100 MB 3MF mid-parse.
  - **The D2 thumbnail is only the fallback.** It isn't the blaze-mode preview rung yet.
  - **No supersampled refinement rung when parked.** Large fits render at 1×.
  - **The Info panel's "resolution" row shows the render size.** Model facts are #151.7.
  - **The limits are fixed constants**, not derived from the machine's RAM.
  - **Concave OBJ faces fan-triangulate wrongly** (`tobj`).
  - **No golden images yet.** The rasterizer tests are structural: coverage, shades, aspect,
    determinism.
- **Decisions / corrections:** D1–D5 settled by the owner 2026-11-23 (above). The default grey
  became `#C8CCD2` and peak lighting is capped at ~0.95 after a white-filament Bambu file
  (`#F6F6F3`) clipped flat at the first settings.
- **Correction (same day, owner test):** real Bambu Studio 3MFs failed to open with `not a 3MF
  package: … Could not find EOCD`. The bundled calibration files had passed because they don't
  carry the stale ZIP64 extras (see *Dependency hygiene*). Fixed by moving `pb-mesh` to zip 9.8;
  all 8 3MFs in the owner's reporting folder (Bambu projects and plain exports) now render.
  Lesson: the bundled-app corpus is not a stand-in for files the slicer *writes*.
- **Cross-platform debt:** **Windows must run** `pb-app`'s tests (`viewing_a_folder_writes_nothing_to_disk`
  now seeds `sub/part.stl` and expects 7 items) and **check** that the `.Model` ProgId registers and
  unregisters cleanly. It was added to the one `progids()` list that all four call sites walk.
- **Claimed:** nobody.
