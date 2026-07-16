# Task 102 — Archive support: tar, tar.gz, tar.bz2, tar.zst, tar.xz (+ bare .gz/.bz2/.zst images)

**Status:** implemented through phase 4 — rev2 (2026-07-16). Phases 0–4 landed on
`feat/enhanced-archives` (`d993fb6` pb-source, `92b4d44` wiring; 63 pb-source tests +
no-trace/dispatch integration tests, clippy/fmt clean). Remaining: phase 5 (optional bare
compressed images) and the fuzz run + corpus benchmarks (tasks.json #102.5/.6). The rev2
section below records the Codex-review disposition that drove the implementation.
**Proposed task id:** 102 (next free in `tasks.json`; add the task entry when this plan is approved)
**Scope:** all three platforms share the code (pure Rust, no per-platform decode), but the
*payoff* is Linux-first: tarballs are the native archive format there. Windows/macOS get it
for free through the same seam.

> **Branch handoff (2026-07-16).** This plan drives work on branch `feat/enhanced-archives`
> (worktree `~/code/blazeviewer-wt1`). That branch bundles **two** deliverables: the tar family
> below (#102) and **RAR5 viewing** (`RarSource`, formerly the candidate #103) — see the companion
> plan [`103-rar5-viewing.md`](103-rar5-viewing.md). **Ordering:** the tar family is unblocked and
> goes **first** (pure-Rust deps, no external gate) and establishes the `ArchiveKind` classifier
> RAR5 reuses; RAR5 is gated on the compcol x86-filter fix (upstream PR #121) merging + releasing —
> mechanics in the 103 plan. **Commit hygiene:** the user commits to `main` concurrently — stage
> explicit paths, never `git add -A`; re-check `git status` before every commit.

## Rev2 — Codex review disposition (2026-07-16, `archive-support-codex-review.md`)

The review confirmed the seam + lazy/eager split and raised 8 findings. Point-by-point,
with the decisions now binding on the implementation:

1. **[P0] Seek-aware indexing + off-thread opens — ADOPTED.** `entries_with_seek()` for
   plain tar (verified public in tar 0.4.46). **Every tar-family open runs off-thread**,
   including plain tar (O(entries) I/O on a huge/network tar): `ArchiveKind` gains
   `background_open()` (true for 7z + all tar; false for ZIP) *alongside* `eager()`
   (the RAM/access model) — scheduling and access model are different concepts. Both
   shells call one worker entry point, `scan::load_archive(path, kind, password,
   progress)`; the 7z pre-flight moves inside it (so the shells' duplicated
   preflight/mt_headroom code is deleted). The lazy tar open reports determinate
   progress for free (`set_total(file_len)`, done = header offset) and honors cancel.
2. **[P0] Budget crossing the crate boundary — ADOPTED** (and already implemented this
   way): the eager open takes `budget: u64` as a parameter; `pb-source` grew
   `OpenError::TooLarge { needed, budget }` mapped explicitly onto
   `ArchiveOpenError::TooLarge` (whose message already says "at least"). Accounting
   uses saturating adds (an overflow saturates to `u64::MAX`, which always refuses).
   **Linux `MemAvailable`**: `archive.rs` gets a real `/proc/meminfo` reader (parse
   helper unit-tested on every platform) — the 8 GB fallback undermined the budget on
   the feature's primary platform.
3. **[P0] Unbounded PAX/GNU metadata allocation inside the `tar` crate — ADOPTED, via a
   metered reader rather than raw iteration.** Verified: non-raw `next()` reads
   long-name/PAX payloads with `read_to_end` (unbounded growth; the 128 KiB figure is
   only initial capacity). But **raw mode is a trap**: the non-raw iterator applies
   PAX `size` overrides to stream advancement (archive.rs:335-354) — hand-rolled raw
   iteration would desync on any PAX-size entry (>8 GiB files in pax-format archives)
   and corrupt the whole listing. Instead: all entry *data* is read by our own code
   between `next()` calls, so bytes the crate pulls *inside* `next()` are only headers
   + metadata. A `MeteredReader` under the `Archive` is armed with a ~256 KiB quota
   before each `next()` and disarmed for our own data reads; hostile metadata then
   fails as `Corrupt` instead of aborting the process. Also adopted: entry-count +
   total-name-bytes caps (index-table bombs), `offset + size <= file_len` validation
   at lazy index time (truncated tail entries are skipped — they can never render),
   and exact reads in `bytes(i)` (short read → `UnexpectedEof`).
4. **[P0] zstd is not ready behind a bare `StreamingDecoder` — ADOPTED.** Verified in
   ruzstd 0.8.3: `StreamingDecoder` is single-frame; the 100 MB window cap is enforced
   on `reset` but **not** on the first frame (`FrameDecoderState::new` — an unbounded
   first-frame window allocation). Implementation: a `MultiFrameZstd` wrapper (one
   reusable `FrameDecoder`, EOF probe + replay at frame boundaries, skippable-frame
   drain) + a **first-frame window pre-check** (parse magic/descriptor/window bytes
   ourselves, reject > ruzstd's own 100 MB cap before `init` can allocate) + per-frame
   **checksum verification** via `get_checksum_from_data`/`get_calculated_checksum`
   (upstream never compares them). Dictionary-requiring frames error cleanly →
   `Corrupt`. Not deferred: with these three fixes the reader shape is sound.
   **xz analog:** `lzma-rust2`'s LZ decoder preallocates the declared dict
   (`vec![0; dict_size]`, up to 4 GiB) — the first block's LZMA2 dict-size byte is
   pre-checked (cap 256 MiB) before constructing `XzReader`; later blocks/streams
   declaring bigger dicts remain a documented residual (bounded at 4 GiB of mostly
   untouched zeroed pages; physical use is bounded by our work cap). Upstream ask: a
   mem-limit parameter on `XzReader`.
5. **[P1] Completion/truncation/work-limit precision — ADOPTED.** Short entry reads are
   `Corrupt` (not silent success); decode-stream `UnexpectedEof`/`InvalidData` map to
   `Corrupt` while real filesystem errors stay `Io`; after tar iteration the codec is
   **drained to EOF** (validates gzip/bzip2 trailers + real 100% progress), bounded by
   cancel + the work cap; a **total expanded-work cap** (64 GiB) bounds skip-and-drain
   CPU on hostile archives (resident-byte accounting alone doesn't); `MultiGzDecoder`
   / `MultiBzDecoder` (not the single-stream variants) — concatenated-member tests for
   all four codecs. Cancellation-latency wording corrected: the 64 KiB chunking bounds
   *checks between output reads*; a decoder may process a whole internal block between.
6. **[P1] Picker made the headline formats unavailable — ADOPTED.** `gz`/`bz2`/`zst`/
   `xz` (+ `tbz`) join the picker filter in phase 4 (rfd matches the final extension,
   so `.tar.gz` needs `gz`); a picked bare `photo.jpg.gz` classifies as `None` and is
   rejected cleanly. Phase 5 (bare compressed single images) stays optional. **`.svgz`
   is removed from phase 5** — it is already a supported image (`svg.rs` inflates
   gzipped SVG with a bounded reader); reclassifying it as an archive would regress it.
7. **[P2] Virtual-path policy — ADOPTED (tar-side).** The tar opens reject/skip names
   that are empty, contain NUL or `..` components, or exceed 4096 bytes (cosmetic here
   — entries are read by offset, never by path — but cheap defense in depth). Sparse
   entries: `EntryType::is_file()` excludes GNU sparse ('S'); PAX-sparse (format 1.0)
   entries decode as garbage and fail at the image decoder — documented, not a safety
   issue. Hard links: **documented limitation** (skipped; they *can* name images, but
   safe aliasing isn't worth the complexity for v1).
8. **[P2] Wiring inventory — ADOPTED.** The audit now names: winit picker +
   `is_archive`, mac-ffi mirror, the Swift open-panel allowed types
   (`CoreModel.swift`), macOS Info.plist UTIs, `default_app.rs` Windows associations,
   and help text. **OS associations are intentionally unchanged** for every tar/gz
   suffix (same rule as ZIP: don't steal the OS archiver's types). Phase 0 spike text
   is superseded by its recorded findings; `.tar.xz` is IN (verified `XzReader`, xz on
   by default in lzma-rust2, already in-tree) with the dict-size pre-check above.

Review test-matrix additions adopted into the phases: metadata bomb, entry-count bomb,
truncated last payload (plain tar), concatenated members ×4 codecs, skippable frames,
corrupt stream/trailer, huge zstd window, expanded-work refusal, traversal-shaped
names, concurrent lazy `bytes(i)`, progress completion. A cargo-fuzz target over the
open paths rides with phase 4 if the repo's fuzz scaffolding accommodates it cheaply;
corpus benchmarks (index latency, codec throughput, peak RSS) are a follow-up note.

## Problem

Archive viewing (task #30) ships ZIP + 7z through the `ItemSource` seam
(`crates/pb-source/src/lib.rs`). The tar family — `.tar`, `.tar.gz`/`.tgz`,
`.tar.bz2`/`.tbz2`, `.tar.zst`/`.tzst` — is what Linux users actually have their photo sets
in, and today those files don't open at all: `is_archive` recognizes only `zip|7z`
(`crates/pb-app/src/main.rs:4284`, duplicated at `crates/pb-mac-ffi/src/lib.rs:3165`), and
`scan.rs::open_archive` (`crates/pb-app-core/src/scan.rs:608`) dispatches only those two.

High value for the effort: the architecture already has both access models these formats
need (lazy random-access = ZIP's shape; eager decode-to-RAM = 7z's shape), the progress/
cancel/RAM-budget plumbing (`OpenProgress`, `archive::ram_budget()`, `MAX_ENTRY_BYTES`)
is format-agnostic, and every needed codec has a pure-Rust decoder — zero C build risk,
zero LGPL exposure.

> **Note on "encrypted":** the tar family has **no standard encryption** (a GPG-wrapped
> tarball is a different file type). The existing password machinery (`PasswordRequired`,
> the prompt dialog) is untouched — these sources simply never return it. The `password`
> parameter stays in the shared signatures for uniformity and is ignored.

## The two shapes (which existing model each format reuses)

| Format | Access model | Why |
|---|---|---|
| `.tar` (plain) | **Lazy random-access** (ZIP's model) | Headers sit at known offsets; one index pass (seek over file data, read headers only) yields `(offset, size, name)` per entry. `bytes(i)` = open + seek + read. Open is near-instant even for a 100 GB tar. |
| `.tar.gz` / `.tgz` | **Eager decode-to-RAM** (7z's model) | One solid DEFLATE stream — no random access without decompressing everything before the target. Same reasoning as solid 7z (`lib.rs:27-35` already names "tar.gz" as the anticipated case). |
| `.tar.bz2` / `.tbz2` | Eager | Same (solid bzip2 stream). |
| `.tar.zst` / `.tzst` | Eager | Same (solid zstd stream). |
| bare `.gz`/`.bz2`/`.zst`/`.svgz` wrapping one image (phase 5, optional) | Trivial one-entry source | `photo.jpg.gz`, `drawing.svgz` — common on Linux. Decompress the single member to RAM; inner extension routes the decoder. |

## Crate picks (all pure Rust, MIT/Apache — no THIRD-PARTY-NOTICES/LGPL work)

| Concern | Pick | Notes / verify in phase 0 |
|---|---|---|
| tar parsing | `tar` | Read-side handles PAX/GNU long names; `Entry::raw_file_position()` + `size()` give the lazy index. Also used to *write* test fixtures. |
| gzip | `flate2` (default rust backend = miniz_oxide) | miniz_oxide is already in-tree via `zip`'s `deflate` feature. Confirm the default feature set stays pure Rust. |
| bzip2 | `bzip2` with the `libbz2-rs-sys` backend | `libbz2-rs-sys` is already in-tree via `sevenz-rust2`. Verify the feature name / that it's decode+encode (encode is only needed for test fixtures). |
| zstd | `ruzstd` (pure Rust, **decode-only**) | The C `zstd` crate is faster but is exactly the build risk `pb-source`'s Cargo.toml documents avoiding (`crates/pb-source/Cargo.toml:9-13`). Decode-only is all we need. If a benchmark on a real multi-GB `.tar.zst` shows the open is painfully slower than C, the reader seam (below) makes swapping/A-B-ing the C crate a one-file change — but don't start there. |

`xz` (`.tar.xz`/`.txz`): **optional, decide in phase 0.** `lzma-rust2` (already in-tree via
`sevenz-rust2`) may expose an XZ-container reader; if it does, `.tar.xz` is ~free and should
ride along. (The compcol spike below is a second route to xz — it lists an xz codec.)

RAR: **no longer an automatic non-goal** — see the compcol/fstool spike below. The old
blocker was "no acceptably-licensed decoder"; that may have changed in 2026.

## Spike 102.0 — `compcol` as a uniform codec layer, and the RAR door it opens

Owner request (2026-07-16): evaluate <https://docs.rs/crate/compcol/latest> — is it an easy
route to more codecs, is the licensing clean, and does it not import a pile of CVEs. RAR
alone is worth entertaining.

### Verified so far (docs.rs + GitHub, 2026-07-16 — spike re-verifies against source)

- **What it is:** pure-Rust, `no_std`, **codecs only** behind one streaming trait
  (`Encoder`/`Decoder`/`Algorithm`), each algorithm behind its own Cargo feature. ~40
  algorithms: deflate/zlib/gzip, bzip2, LZMA/xz, zstd, brotli, LZ4, PPMd, … plus
  **decode-only RAR 1/2/3/5** ("clean-room reimplementations"; every RAR *encoder* is
  "permanently Unsupported by design").
- **Safety surface:** `unsafe_code = "forbid"` crate-wide, **zero runtime dependencies**,
  no FFI/bindgen/C. So the classic archive-CVE class (memory corruption in a C
  decompressor) is structurally absent; residual risk is logic bugs, panics, and
  non-termination — all already covered by our harness (`catch_panics`, worker-thread
  isolation, `MAX_ENTRY_BYTES`, the RAM budget, cancellable chunked reads).
- **License:** MIT (Karpelès Lab Inc.). Decode-only + clean-room means RARLAB's unrar
  source-license restriction doesn't attach (that restriction covers *their* code;
  independent reimplementations are the libarchive / The Unarchiver precedent). Decode-only
  is also exactly our "we only ever DECODE" rule. Ship cost: an entry in
  `THIRD-PARTY-NOTICES.md`, nothing more.
- **Critical limitation:** compcol has **no container parsing**. A `.rar` is a container
  (headers, file entries, CRCs, solid groups, encryption) *plus* codecs; compcol supplies
  only the codecs.
- **The container gap has a candidate filler:** the same vendor's **`fstool`** (MIT,
  crates.io, library + CLI) reads **RAR5 containers including solid archives** via
  `compcol::rar5` — but explicitly **not RAR4, not encryption**.
- **Maturity caveat:** compcol is 0.6.x, first published 2026, single vendor. Claims ~566
  tests + cross-validation against system tools. New code parsing hostile bytes: pin exact
  versions, audit (zero-dep + forbid(unsafe) makes the audit tractable), and fuzz *our*
  integration paths regardless.

### What the spike answers (decision gates)

1. **Codec quality/perf:** benchmark compcol's gzip/bzip2/zstd decode against
   `flate2`/`bzip2`/`ruzstd` on corpus-sized streams. Gate: within ~2× of the incumbents
   (the eager open is one-time, but a 10× regression would be felt). If it passes, the
   phase-2 reader-factory seam can take compcol as the *single* codec provider for the
   whole tar family + xz — one audited dep instead of three, and the same dep later serves
   RAR. If it fails, keep the incumbents (the seam makes this a per-codec choice, not
   all-or-nothing).
2. **Robustness:** throw the fuzz corpus shapes at it — truncated streams, garbage,
   bombs. Confirm errors surface as `Err`/end-of-stream (not panics) and that a
   pathological stream can't spin forever between our cancel checks. Check RUSTSEC for
   advisories on compcol/fstool (expected none — too new — which cuts both ways).
3. **RAR route (the real prize):** assess `fstool`'s library API shape — can we read
   entry lists + per-entry streams without its CLI/disk-image baggage (feature gates?
   dep footprint?), and does its solid-group walk map onto our eager model? Then scope
   the delta:
   - **RAR5, unencrypted** (fstool today): non-solid → lazy per-entry (ZIP's model);
     solid → eager (7z's model). Both models already exist in pb-source.
   - **RAR4/RAR3-era archives** — the "20 years ago" case, and most old `.rar` files —
     need a **RAR4 container parser we'd write ourselves** (compcol has the `rar3` codec;
     the RAR4 header format is well documented; precedent: we wrote the `avis` demuxer,
     and this one is simpler — sequential headers, no sample tables). Fuzz it like `avis`.
   - **Encrypted RAR:** container-level AES (RAR4: AES-128, RAR5: AES-256) — only
     reachable if we own the container parser; the AES primitives are already in-tree
     via `zip`/`sevenz-rust2`. Defer unless the parser lands.
4. **Supply-chain posture:** exact-pin (`=x.y.z`) both crates, review the diff on every
   bump (zero-dep makes `cargo vet`-style review feasible), and keep them feature-gated
   so a pulled/broken release never blocks a Windows build (the libheif lesson).

### Spike deliverable

A short findings note appended to this plan (benchmarks, API verdicts) + a go/no-go per
gate. If gate 3 is a go, RAR becomes **task #103** with its own plan: `RarSource` in
pb-source (lazy for non-solid, eager for solid), fstool-or-own-parser decision, RAR4
parser scope, and the picker/classifier additions (`rar`, and note `cbr` comics = RAR
renamed — cheap to include and squarely a viewer use case, alongside `cbz` = ZIP which
we should add to the classifier anyway).

### Spike findings (2026-07-16) — source audit of both repos, cloned to `~/code/{compcol,fstool}`

Four parallel deep audits (tar-codec API/robustness, RAR decoders, project health,
fstool container) over the actual source at compcol `04a6db2` (v0.6.8) / fstool
`5c8ff99` (v0.4.20). Both test suites pass clean on our Windows box. Verified facts,
not README claims:

**Hygiene claims all check out.** `#![forbid(unsafe_code)]` enforced in Cargo.toml
*and* lib.rs; genuinely zero runtime deps (optional tokio only); 1,186 tests; 37 fuzz
targets; tri-platform CI; honest SECURITY.md whose caller-supplied-limit bomb model
(`LimitedDecoder`, `decompress_to_vec_capped`) maps directly onto our budget guards.
Both crates are openly AI-generated at scale (commits: "via parallel agents",
Co-Authored-By trailers) — breadth-first generation that *looks* uniformly plausible,
with correctness maturity varying by an order of magnitude per codec underneath.

**Gate 1 (tar-family codecs) — MIXED; keep the incumbents, compcol optional for xz.**
- gzip: mature. System-tool cross-validated, multi-member streams, dedicated fuzz
  target, no reachable panics, no-progress loop guards, benched ~1.6× system gzip.
- xz: LZMA2-only (rejects BCJ/delta filter chains — irrelevant for photo tarballs;
  those filters are for executables), cross-validated against system xz, ~1× xz decode
  speed. Viable for `.tar.xz` if `lzma-rust2` doesn't pan out.
- bzip2: full single-stream, cross-validated vs `bunzip2` — but **no concatenated
  streams** (`pbzip2` output stops silently after stream 1).
- zstd: **disqualifying flaw for our eager model** — the decoder never trims its
  history buffer, so peak RAM ≈ the full decompressed size *inside the decoder*, ~2×
  once we accumulate entries. A multi-GB `.tar.zst` would double our RAM budget cost.
  Also: no multi-frame/concatenated streams, dictionaries rejected, and hand-built
  fixtures only (no cross-validation against real zstd). `ruzstd` stays
  window-bounded and battle-tested → **keep ruzstd**.

**Gate 2 (robustness) — PASS for gzip/bzip2/zstd/xz** (all-`Result` errors, poisoned-
state on error, bounded allocations, dedicated fuzz targets). **FAIL for RAR**: no
dedicated fuzz target (only a shared dispatch target = fractions of a second of
fuzzing), and zero correctness fixes in the entire history because nothing exercises it.

**Gate 3 (RAR) — RAR5 conditionally viable; RAR4 no-go.**
- compcol `rar5` LZ+Huffman core: complete, defensively written (masked window,
  u64-accumulated distances, no production panics), validated against real RARLAB CLI
  fixtures — but only 3 tiny ones. **Missing: Delta + ARM filters** (`Unsupported`) —
  and WinRAR auto-picks Delta for uncompressed image data (TIFF/BMP/RAW), squarely our
  content. **No CRC/Blake2 verification** → corrupt input = silent garbage. Hostile
  header can demand a 1 GiB window — the container layer must cap it (fstool caps at
  64 MiB).
- compcol `rar3` (the RAR4-era codec): LZ path complete, but **PPMd refused** (the
  ppmd module is an order-0 subset, not PPMII-H — the README's "full ppmd" claim is
  false) and **all RarVM filters refused**. Validation = one 30-byte fixture that is
  actually a RAR2 stream. The "20-year-old archives" case is NOT buildable on this
  today.
- **fstool: reference implementation, not a dependency.** Its RAR5 container layer is
  exactly our shape (index without decode, per-entry `io::Read`, a decode-once
  `LiveSolid` cursor proving solid archives work at container level over compcol's
  decoder, real WinRAR fixtures cross-checked vs `unrar`) — but it's a monolithic
  crate: all ~15 filesystem backends (APFS/NTFS/qcow2/…) compile unconditionally plus
  a mandatory clap/serde/uuid/… dep floor, no CRC checks, no RAR fuzzing, and its
  `&mut dyn BlockDevice` threading forbids concurrent entry readers (bad fit for our
  decode pool). Its whole container layer is ~350 lines — reimplement in pb-source
  against `compcol::rar5` directly, using fstool's solid-cursor logic
  (`out_pos`/`starts[]`, stored-member-in-solid rejection) as the crib.

**Gate 4 (supply chain) — conditional pass.** Single vendor (Karpelès Lab), 6 weeks
old, rapid-fire releases; but zero-dep + forbid-unsafe makes per-bump diff review
feasible. Exact-pin (`=x.y.z`), feature-gate, review on bump.

**Verdicts:**
1. **Task #102 (tar family): unchanged** — ship on `flate2`/`bzip2(libbz2-rs)`/`ruzstd`
   as planned. compcol is *not* adopted for the big three (zstd RAM flaw, bzip2
   multi-stream gap, less field exposure than incumbents). `compcol::xz` (exact-pinned,
   `features=["xz"]`) is the fallback for `.tar.xz` if lzma-rust2 lacks an XZ reader.
2. **Task #103 (RAR5 viewing): GO, scoped honestly** — own ~350-line container parser
   in pb-source on exact-pinned `compcol::rar5`; lazy per-entry for non-solid, eager
   sequential walk for solid; our own CRC32 verification (`crc32fast` or in-tree);
   window cap ≤ 64 MiB pre-decoder; per-entry graceful "unsupported RAR feature"
   fallback for Delta/ARM-filtered entries (the archive still opens; those entries
   show an honest error); encrypted RAR detected → honest message (defer decrypt);
   `.cbr`/`.cbz` ride along. **We fuzz it ourselves**: a cargo-fuzz target over
   container parse + decode (compcol upstream doesn't). Validate against a real
   WinRAR corpus before shipping. RAR4: detect the signature, show "RAR4 isn't
   supported yet" — do not attempt decode until compcol grows PPMd-II + RarVM (watch
   upstream; it's on their roadmap per fstool's Cargo.toml comment).
3. **Licensing: clean** — MIT, credible clean-room provenance (libarchive/Unarchiver
   cited as algorithm sources, idiomatic non-transliterated code), decode-only matches
   our rule. THIRD-PARTY-NOTICES entry on adoption; FA-style redistribution issues: none.

### Spike execution results (2026-07-16) — fuzz targets, corpus, real bug found + fixed

Ran the due-diligence work in `~/code/compcol` (fork `jdlien/compcol`) + a differential
harness at `~/code/compcol-rar-corpus` (25-archive WinRAR matrix; fstool `RarFs` as
driver over the *local* compcol via `[patch.crates-io]`; `unrar` as oracle). Nothing
pushed — two PR-ready branches await owner review.

- **Real correctness bug found and fixed** (branch `rar5-x86-filter-fix`, TDD): compcol's
  rar5 **x86 (E8) filter — a filter it claims to support — corrupted real executables**:
  48 wrong bytes across 15 sites on a 32 KiB notepad.exe slice. Root cause: compcol
  flattened unrar's *nested* range checks into an `else if`, mis-transforming operands
  after false-positive `0xE8` bytes. Invisible to compcol's tiny fixtures; only real x86
  code exposes it. One-line-ish fix; whole corpus then decodes byte-identical to unrar.
- **Fuzzing clean**: dedicated `decoder_rar2/rar3/rar5` targets (branch
  `rar-fuzz-targets`; fills compcol's own unmet CONTRIBUTING promise) — **~6M execs, zero
  crashes/OOMs/timeouts**, peak RSS ≤196 MB.
- **Differential, post-fix: 0 MISMATCH.** Store (all dict sizes, solid + non-solid) and
  m3/m5 LZ/Huffman incl. x86-filtered → **byte-perfect vs WinRAR 7.23**. So compcol::rar5
  decodes its claimed subset correctly *after our fix*.
- **Delta filter confirmed as THE photo gap, and its trigger is now empirical**: WinRAR
  auto-applies Delta (filter type 0) to **gradient BMP / WAV-like content** →
  `Unsupported`; **JPEG and text are NOT delta-filtered** → decode fine. So the common
  photo-in-RAR case (JPEGs) works; BMP/TIFF/RAW-shaped content fails gracefully. This
  makes the **RAR5 Delta filter** the highest-value upstream patch (small transform).
- **Container-layer gaps are OURS, and fstool demonstrates why we don't just link it**:
  fstool doesn't detect per-file encryption (feeds AES ciphertext to compcol → generic
  "corrupt" instead of a clean password prompt) and lists zero entries for a
  header-encrypted archive. A solid group containing *any* Delta-filtered member poisons
  the shared stream (5 downstream ERRORs). Our `RarSource` (#103) must: detect encryption
  at the container layer, and treat a Delta/unsupported member inside a solid group as
  "this group's later members are unavailable" rather than erroring the archive.
- **RAR4 (real `-ma4` archives from WSL rar 6.24: m0/m3/m5+PPMd/solid, + a signature
  stub): all cleanly `Unsupported`** — zero misparse/panic/garbage. The graceful-reject
  design is validated on real RAR4 input. (Note: whether compcol::rar3's *existing* LZ
  path could decode stored/LZ RAR4 was NOT tested here — fstool's RAR5-only container
  refuses RAR4 before reaching the codec; that's a separate future probe.)

**Net:** RAR5 verdict upgraded from "conditionally viable" to **empirically byte-correct
on its supported subset (after our fix), fuzz-clean, honest on the rest**. The remaining
ship-blockers for RAR *photo* viewing are all container-layer (ours to build in #103) plus
the Delta filter (a bounded upstream patch). Two clean upstream PRs are ready as the
maturity probe: the fuzz targets and the x86-filter fix.

## Design

### 1. One classifier, replacing three duplicated predicates

New in `pb-source`:

```rust
pub enum ArchiveKind { Zip, SevenZ, Tar, TarGz, TarBz2, TarZst, TarXz }
pub fn archive_kind(path: &Path) -> Option<ArchiveKind>
impl ArchiveKind {
    pub fn eager(&self) -> bool           // RAM/access model: decode-to-RAM + budget
    pub fn background_open(&self) -> bool // scheduling: open off-thread (rev2 §1 —
}                                         // all tar kinds, incl. lazy plain tar)
```

Double extensions are the subtlety `Path::extension()` misses: `.tar.gz` reports `gz`, so
the classifier checks the compression suffix first, then whether the remaining stem ends in
`.tar` (case-insensitive), plus the single-token forms (`tgz`, `tbz2`, `tbz`, `tzst`).
A bare `.gz` that is *not* `.tar.gz` is `None` until phase 5.

Adopters (this kills the drift risk — today's three copies already only agree by luck):
- `crates/pb-app/src/main.rs:4284` `is_archive` → `archive_kind(p).is_some()`
- `crates/pb-mac-ffi/src/lib.rs:3165` same
- `crates/pb-app-core/src/scan.rs:608` `open_archive`'s ad-hoc `is_7z` check (and the
  mirror inside `crates/pb-app/src/main.rs:1008` `begin_archive_open`, and
  `crates/pb-mac-ffi/src/lib.rs:2696`) → match on `ArchiveKind` / `kind.eager()`

### 2. `TarSource` (new module `crates/pb-source/src/tar.rs`)

`lib.rs` is ~2000 lines already; new code goes in a submodule, re-exported from the crate
root. One public type, two internal states:

```rust
pub struct TarSource { path: PathBuf, entries: Vec<TarEntry>, store: Store }
enum Store {
    Lazy,                       // plain .tar: TarEntry carries (offset, size)
    Eager(Vec<Vec<u8>>),        // compressed: decompressed bytes, index-aligned
}
```

- **Lazy open (plain tar):** iterate `tar::Archive::entries()` over a `BufReader<File>`,
  keep regular files only (`entry_type().is_file()` — symlinks/hardlinks/devices skipped;
  they'd be traversal/aliasing hazards and can't be images), record
  `raw_file_position()`/`size()`/normalized name. Keep **all** file names (not just
  supported ones) in a side list, exactly like ZIP keeps its central directory, so
  `sibling_names`/`sibling_bytes` work (subtitle sidecars, task #90.1). Index only
  supported entries (`is_supported` predicate param, same as Zip/7z) within
  `MAX_ENTRY_BYTES`. `bytes(i)` = `File::open` + `seek` + bounded read (no handle pool
  needed — unlike `ZipArchive` there is no per-handle parsed state worth reusing).
- **Eager open (compressed tar):** a single streaming pass, `tar::Archive` over a
  `Box<dyn Read>` built by stacking the codec reader for the `ArchiveKind` — this reader
  factory is the codec seam (one small `fn reader_for(kind, inner) -> io::Result<Box<dyn Read>>`).
  Mirrors `SevenZSource::open_with_progress` semantics
  (`crates/pb-source/src/lib.rs:733`): honors `OpenProgress` cancellation between chunks
  (reuse `read_cancellable`, `lib.rs:1085`), `try_reserve` per entry, skip-and-drain
  unsupported/oversized entries, `OutOfMemory`/`Cancelled`/`Corrupt` via the existing
  `OpenError`.
- Entry names go through the existing `normalize_entry_name` (`lib.rs:1122`) — GNU tar's
  `./dir/file` prefix is already handled — so the ⇧F folder tree, `ScopedSource`, and the
  title/panel work unchanged.
- Duplicate names (tar append mode): keep the **last** occurrence, matching `tar -x`
  semantics; document.
- Eager `sibling_*`: same documented gap as 7z (`lib.rs:950-963`) — the sidecar's bytes
  were never decompressed. Same cost/benefit, same follow-up seam.

### 3. RAM budget + progress for eager tarballs (differs from 7z — this is the one new mechanism)

7z pre-flights exactly (`seven_z_projected_bytes`) because its *header* lists every
decompressed size. A compressed tar can't: the per-entry sizes are tar headers interleaved
*inside* the compressed stream, and gzip's ISIZE trailer is mod-2³² (useless past 4 GiB).
So instead of predict-and-refuse:

- **Budget enforced during the stream:** a running `resident_bytes` counter of kept
  entries; when the next entry would push it past `archive::ram_budget()`, abort with a
  new `OpenError::TooLarge`-equivalent carrying `needed >= counter` (the app already
  renders that error for 7z — reuse `ArchiveOpenError::TooLarge` with a "at least" figure,
  or extend its message). This is still predict-and-refuse *per allocation* (`try_reserve`
  + the counter check happens **before** reserving), so the uncatchable-abort reasoning
  from `lib.rs:628-633` holds.
- **Progress = compressed bytes consumed:** wrap the `File` in a small counting reader
  *below* the codec, `set_total(file_len)`, `add_done(delta)` as compressed bytes are
  pulled. Determinate bar, no decompressed-total needed. (`OpenProgress::fraction` is
  already ratio-based — no UI change.)
- **Cancel latency:** `read_cancellable`'s 64 KiB chunking bounds it, same as 7z.

### 4. App wiring (per shell)

- `crates/pb-app-core/src/scan.rs`: `open_archive` grows `ArchiveKind::Tar` (sync, lazy —
  like ZIP) and a `load_tarball(path, progress) -> Result<Resolved, ArchiveOpenError>`
  (the `load_seven_z` analog, `scan.rs:688`). `is_supported_archive_entry`
  (`scan.rs:124`) is reused as-is — archived videos inside tars play from RAM through
  `VideoInput::Bytes` exactly like ZIP entries do today.
- `crates/pb-app/src/main.rs` `begin_archive_open` (`main.rs:1008`): the `is_7z` branch
  becomes `kind.eager()`; the 7z-specific *pre-flight* stays 7z-only (tarballs have none —
  their refusal happens mid-stream, surfacing through the same
  `finish_archive_open` error path). `mt_headroom` stays a 7z concept (single-stream
  codecs have no within-block MT).
- File picker filter (`main.rs:3109-3112`): add `tar,tgz,tbz2,tbz,tzst,txz` **and**
  `gz,bz2,zst,xz` — rfd filters match on the final extension, so without the bare
  suffixes the headline `.tar.gz`/`.tar.zst` files would be invisible in the picker
  (rev2 §6). A picked bare `photo.jpg.gz` classifies to `None` and is rejected cleanly;
  phase 5 would upgrade it to actually open.
- `crates/pb-mac-ffi/src/lib.rs`: mirror the `begin_archive_open` (`:2696`) dispatch and
  `is_archive` (`:3165`). Check the Swift host's Info.plist document types / open-panel
  allowed types for a hardcoded zip/7z list.
- Audit pass: `grep -rn '"7z"\|"zip"' crates/` at the end — `single_instance.rs`,
  `menu.rs`, `panels_ui.rs` matched an earlier archive grep; each is either fine (goes
  through `classify_inputs`) or needs the classifier.
- **Do NOT add OS file associations** for tar/gz — Blaze Viewer must not steal `.tar.gz`
  from the OS archiver. (We don't associate `.zip` today either; keep it that way.)

### 5. Phase 5 (optional, cheap once codecs exist): bare compressed single images

`photo.jpg.gz`, `photo.png.bz2`, `photo.jxl.zst`. Classifier: compression suffix whose
*inner* stem has a supported image extension. Implementation: a tiny one-entry eager
source (or a direct decompress-then-`decode_named_bytes` in the open path — decide at
impl time; the one-entry `ItemSource` keeps the flow uniform). Budget-checked with the
same streaming counter. (**Not `.svgz`** — rev2 §6: gzipped SVG is already a supported
ordinary image via `svg.rs`; making it an archive would regress it. The bare suffixes
are already in the picker as of phase 4; this phase makes them open instead of being
cleanly refused.)

## Phases

**Phase 0 — verification spikes (½–1 day):** confirm `tar` crate gives
`raw_file_position` on a plain read (lazy index viability); confirm
`bzip2`+`libbz2-rs-sys` feature name and encode support (fixtures); confirm `ruzstd` API
shape; check whether `lzma-rust2` exposes XZ (decides `.tar.xz`); measure ruzstd
throughput on a representative `.tar.zst` from the corpus to sanity-check "eager open in
seconds, not minutes." **Run spike 102.0 (above) here too** — its gate 1 decides whether
the phase-2/3 codec layer is compcol or the incumbent trio; gates 3–4 decide whether RAR
spawns task #103. The tar-family phases do not block on the RAR verdict.

**Phase 1 — `ArchiveKind` + lazy `TarSource` (plain .tar), TDD:** classifier unit tests
(the double-extension matrix, case-insensitivity, `tar.GZ`, non-archives); tar fixtures
written with the `tar` crate; tests mirroring the ZIP suite — names sorted + normalized,
bytes by index, out-of-range errors, oversized-entry skip, non-regular-entry skip,
sibling names/bytes scoped to the entry's directory, duplicate-name last-wins.

**Phase 2 — eager `.tar.gz`:** the codec-reader seam + counting reader + streaming budget
+ cancel; tests: round-trip fixture, cancellation mid-stream, budget refusal (tiny
`PB_ARCHIVE_RAM_BUDGET`), truncated/corrupt stream → `Corrupt`, bomb (huge declared entry)
→ skip-and-drain, progress reaches ~1.0.

**Phase 3 — bz2 + zstd:** same tests parameterized over codecs. zstd fixtures: `ruzstd`
can't encode — commit tiny pre-generated `.tar.zst` fixtures as test assets (generated
once with the system `zstd` CLI; a few hundred bytes each) rather than adding the C
`zstd` crate even as a dev-dependency.

**Phase 4 — app wiring (both shells) + integration:** scan.rs dispatch, winit + mac-ffi
shells, picker filter, no-trace tests (`viewing_a_tar{,_gz,_bz2,_zst}_writes_nothing_to_disk`
analogs of the existing ZIP/7z ones), a ⇧F folder-tree smoke over a nested tar, archived
video-in-tar plays (reuses the ZIP archive-video test shape, `scan.rs:723`).

**Phase 5 (optional, separate commit):** bare `.gz`/`.bz2`/`.zst`/`.svgz` single images +
picker filter additions.

**Docs:** CLAUDE.md archive bullets (the "later tar.gz" notes at `pb-source` lib.rs:31 and
the crate description in Cargo.toml go stale), CHANGELOG `Added` line, tasks.json entry.

## Risks / open questions

1. **bzip2 eager-open speed:** pure-Rust bzip2 decode is inherently slow (~tens of MB/s);
   a multi-GB `.tar.bz2` could take minutes. The progress bar + cancel make this honest
   rather than hung, and bz2 photo tarballs are rare — accept, document, don't optimize v1.
2. **ruzstd vs C zstd:** decide with a phase-0 measurement, not by taste. The reader-factory
   seam makes an A/B trivial later.
3. **Sparse tar entries** (GNU sparse): rare outside disk images; verify the `tar` crate's
   behavior in phase 0 and skip sparse entries if expansion is awkward — they're never
   photos.
4. **Memory shape difference from 7z:** no up-front refusal means a user only learns a
   tarball is too big *after* watching progress climb until the budget trips. Acceptable
   (it's also how the user learns with a progress bar today for cancel); the error message
   should say what the budget was.
5. **`.tar.xz`:** in or out per phase 0's `lzma-rust2` finding.
6. Fixture hygiene: committed `.tar.zst` test assets must be tiny and clearly generated
   (a comment with the exact generating command).

## Explicit non-goals

- RAR **within #102** (it's the spike-gated candidate task #103, not part of the tar
  work), ISO/CAB, tar *writing*, encrypted tarballs (no standard exists),
  multi-stream/pigz-parallel gz decode, extracting anything to disk (the no-trace
  guarantee: RAM-only, same as ZIP/7z — `viewing_*_writes_nothing_to_disk` enforces it).
