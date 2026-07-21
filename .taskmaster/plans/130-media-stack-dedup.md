# Task 130 — De-duplicate the media stacks (audit #5): a `VideoProducer` trait + a shared YUV primitive

**Status:** planned — 2026-07-20. Remediation for **technical-debt audit finding #5** ("Parallel media
stacks with no unifying trait"). Two independent deliverables, sequenced by value: **Part A** — one shared
credit/seek loop behind a `VideoProducerBackend` trait, collapsing the ~180-line near-verbatim duplication
between the MF and FFmpeg producers (the real prize); **Part B** — a small shared YUV color primitive for the
constants both Rust crates hand-copy (modest, cheap). **Part C** (posters + audio decoders) is noted, not
committed — see §7.

## 0. ⚠ This is NOT a pure move — read the safety model first (§3)

Unlike #125/#128, this is a **behavioural refactor**: it extracts a trait and folds two loops into one.
`verify-pure-move.py`'s byte-hash does not apply and gives no safety here. The net is **characterization
tests** (the existing producer integration tests must pass unchanged) plus a **new mock-backend unit test**
that the trait extraction makes possible. If the plan tempts you toward "it's basically the same code," stop:
the value and the risk both live in getting the *shared loop* exactly behaviour-equivalent to both originals.

## 1. What the investigation found (grounding — done 2026-07-20)

Both headline items in finding #5 are real, but they are **very different in size**, and the audit's framing
over-sells one and under-counts the other. This was verified by reading the actual files, not the audit.

### 1a. The producer duplication (Part A) — substantial, exactly as billed

- **MF** `run_video_producer` (`mf_video_producer.rs`) and **FFmpeg** `run_ff_video_producer`
  (`ffmpeg/video_producer.rs`) are ~1,258 and ~1,320 lines. The FFmpeg one *comments itself* "the FFmpeg
  mirror of `run_video_producer`."
- The core is a hand-rolled select-over-one-channel **credit/seek loop**, organized into three
  identically-commented sections in both: **S1** "absorb messages, block only when idle", **S2** "land a
  pending seek", **S3** "spend one credit on the next sequential frame." Loop bodies: **MF 256–472**
  (~217 lines), **FFmpeg 166–342** (~177 lines). *(The audit's MF range 188-397 is stale — corrected here.)*
- **S1 and the credit/generation/seek-epoch machinery are near-verbatim identical.** The supersede-poll
  block and the landing-credit-block are character-for-character the same on both sides.
- **What genuinely differs is a ~9-operation reader seam** (the audit's "~5 reader calls" under-counts once
  the raw-decode/convert split and the per-backend frame construction are separated). This seam *is* the
  trait — see §4 for the exact table.
- **FFmpeg's `Reader` struct is already ~90% of the target trait.** The FFmpeg loop is what the shared loop
  should look like. The work is mostly turning MF's free functions + loop-locals (`active`, `reader_pos`,
  `kind`, `w`/`h`/`format`, `color`, `manager`) into an `impl VideoProducerBackend`.

### 1b. The YUV "triplication" (Part B) — real but MUCH narrower than the audit implies

The audit calls YUV "triplicated … correctness-critical copies that must agree." Reading the code:

- `pb-decode/src/yuv.rs` and `pb-render/src/yuv.rs` are **not** duplicate converters — they serve **different
  purposes**. pb-decode's is the dav1d/AVIF decode path (YUV → *source-gamut* RGBA8, used by `avis.rs`);
  pb-render's is the video NV12/P010 **CPU fallback + shader reference**, HDR-aware (PQ/HLG EOTFs, scene-linear
  `planar_to_scene`). Their bodies are genuinely different code doing different jobs.
- They overlap in **exactly one thing**: the ~6 luma constants (`Bt601 (0.299, 0.114)`, `Bt709 (0.2126,
  0.0722)`, `Bt2020 (0.2627, 0.0593)`) plus the `coeffs()` derivation from them. Two identical Rust copies
  (`Matrix::kr_kb` in pb-decode, `YuvMatrix::kr_kb` in pb-render) and a third in the `gpu.rs` WGSL shader.
- **A drift guard already exists**, deliberately. `pb-render/yuv.rs`'s own doc: "the coefficients live in
  exactly one place per crate boundary … the golden tests use an **independent from-spec reference**, not
  these helpers, so a bug in the shared math can't hide by matching itself."

So Part B's honest value is: **collapse two Rust copies of a ~30-line constant table + derivation into one
shared definition** so a new matrix family or a fix can't land in one crate and miss the other. It is *not*
"fix an unguarded correctness hazard" — the hazard is already golden-tested. Cheap, worth doing, but small.

## 2. Scope gate

- **In:** (A) the `VideoProducerBackend` trait + one shared loop, MF and FFmpeg both re-expressed as backends;
  (B) a shared color primitive for the YUV matrix constants + `coeffs()`, with both `yuv.rs` files consuming it.
- **Out (this task):** the 3 poster extractors and 2 audio decoders (§7 — a separate task, and it needs its own
  mapping); any change to the **wire protocol** (`VideoProducerMsg`/`VideoFrame` in `video.rs` stay byte-stable —
  they are the characterization anchor); any change to the WGSL shader math (Part B keeps it separate + cross-
  checked, does not try to generate it); any change to `VideoSession` (the consumer).
- **The pacing/credit/seek *semantics* must not change.** This is a de-dup, not a redesign. If the two loops
  have drifted (§5 complication 9), the refactor **preserves each backend's current observable behaviour** and
  files any genuine bug separately — it does not "fix while unifying."

## 3. The safety model (what stands in for the byte-hash)

Three layers, strongest first:

1. **A NEW mock-backend unit test of the shared loop — the primary net, and a first-class deliverable.**
   The whole point of the trait is that the loop becomes drivable by a fake backend with **no real MF/FFmpeg
   and no video file**. A `MockBackend` returns scripted frames/EOS/seek results; the test sends
   `Credit`/`SeekTo`/`Stop` on the real channel and asserts the exact `VideoProducerEvent` sequence — credit→
   frame, seek→run-up-discards→landing-frame stamped at target, supersede-mid-runup, EOS→park→replay-on-seek,
   generation stamping. This tests the *shared logic that both backends now share*, cross-platform, in
   milliseconds, deterministically. Write it against the trait **before** porting the second backend, so it
   pins the loop's contract.
2. **The existing producer integration tests must pass UNCHANGED.** `mf_video_producer.rs` already has ~10
   `#[test]`s that `spawn()` the real producer against a fixture video and assert on emitted events (they
   drive the wire protocol end-to-end). These are the characterization net for the MF backend. **Do not edit
   them** — a passing run is the proof the MF backend's observable behaviour survived. ⚠ Check whether the
   FFmpeg producer has equivalent coverage; if not, that gap is itself a reason these two flaky real-video
   tests aren't enough alone, and layer 1 is why.
3. **Real-app characterization on the corpus.** `\\beenas\Media\Movies` — play, seek forward/back across
   keyframes, scrub, hit EOS, replay. Watch for pacing regressions, seek-landing frame correctness, and the
   AC-3/DTS audio path (unrelated to the producer but in the same play flow). This is behaviour-unverified
   until a human runs it (the two producer probe tests are already **flaky**, per the status doc's Known-red).

⚠ **Never call the producer refactor "verified" off a green `cargo test` alone** — the real-video tests are
timing-flaky and the FFmpeg side may be thin. Layer 1 (the mock loop test) is what makes a green run meaningful.

## 4. Part A — the `VideoProducerBackend` seam (the exact differing operations)

Every row is the *only* thing that differs at that point in the loop. This table is the trait.

| operation | MF | FFmpeg |
|---|---|---|
| open / negotiate | inlined pre-loop (`open_video_reader` → negotiate NV12/P010/RGB32) → `(IMFSourceReader, OutKind, w, h)` | `Reader::open(input, fit, cancel, opts)` — one call, encapsulates demux+decode+convert+HW |
| forward-decision | `should_hop(reader_pos, abs_target)` (pure) | `reader.can_decode_forward(target_units)` (folds `!parked && !eof_sent`) |
| seek (only when !forward) | `reopen_at(...)` — **recreates** the reader | `reader.seek_to_keyframe(...)` — **in-place** `avformat_seek_file` + flush |
| read-raw (run-up, no convert) | `read_raw(...) -> Read1Raw::{Frame{ts,sample},Eos,Gap}` then `convert_sample(...)` | `reader.decode_next_raw() -> Option<(i64, Video)>` then `reader.convert_frame(&f)` |
| read-next (sequential) | `read_one(...) -> Read1::{Frame{ts,pixels},Eos,Gap}` | `reader.next_frame() -> Option<(i64, Vec<u8>)>` (no Gap) |
| Duration → units | inline hns arithmetic | `reader.target_units(target)` |
| frame construction | inline `VideoFrame{…}` twice, cloning session-constant `color` | `reader.make_frame(...)` — computes color **per-format per-call** |
| close / drain | `retire_reader(r)` off-thread (HEVC teardown ~1 s) on every retire + exit | none — drops with `reader`; seeks reuse it |

**Proposed trait** (grounded in the above; the `type Raw` is owned — `IMFSample` / `ff::frame::Video` — so no
lifetime entanglement, and it forces a monomorphized `fn run<B: VideoProducerBackend>` rather than `dyn`, which
is correct here anyway since both backends are `!Send` COM/libav handles on their own thread):

```rust
trait VideoProducerBackend {
    type Raw;                    // owned pre-conversion frame
    fn open(input, fit, opts, cancel) -> Result<(Self, Opened, Option<(i64, Vec<u8>)>), String>;
    fn read_frame(&mut self)   -> Result<Option<(i64, Vec<u8>)>, String>;   // gaps hidden inside
    fn decode_raw(&mut self)   -> Result<Option<(i64, Self::Raw)>, String>;
    fn convert(&mut self, raw: &Self::Raw) -> Result<Vec<u8>, String>;
    fn target_units(&self, target: Duration) -> i64;
    fn can_decode_forward(&self, target_units: i64) -> bool;
    fn seek(&mut self, target_units: i64) -> Result<(), String>;
    fn anchor_origin_seek(&mut self, target_units: i64, target: Duration);
    fn anchor_origin_seq(&mut self, ts: i64);
    fn make_frame(&mut self, sid, gen, ts, pixels) -> VideoFrame;   // color differs per backend — keep here
    fn park(&mut self);
    fn close(self);
}
```

**Loop keeps:** `gen`, `credits`, `pending`, the optional primed-first-frame, the `session_id` + event sender.
**Backend owns:** the reader/decoder handle, `origin`, position (`last_ts`/`reader_pos`), `parked`/`eof_sent`,
`w`/`h`/`format`, `color`, `kind`/converter, `manager`, `input`, HDR peak.

### 4a. Sequencing (Part A)

1. **Define the trait + the shared `fn run<B>`** by lifting the FFmpeg loop *verbatim* into a generic function,
   with `reader.*` calls becoming `backend.*` trait calls. FFmpeg's `Reader` gets `impl VideoProducerBackend`
   with near-trivial method bodies (it already has them). `run_ff_video_producer` becomes a 3-line wrapper:
   open the backend, call `run`. **Verify:** the FFmpeg integration path + the app on real video are unchanged;
   this step touches only FFmpeg + the new shared module, so its blast radius is bounded.
2. **Write the mock-backend loop test (§3 layer 1)** against the now-existing trait. This pins the contract
   before MF is ported — so MF is ported *to a tested spec*, not into a vacuum.
3. **Port MF to `impl VideoProducerBackend`.** Turn its free functions (`read_one`/`read_raw`/`reopen_at`/
   `convert_sample`) + loop-locals into a backend struct; absorb the `Gap` retry (complication 2) and the
   `&mut kind` stride threading (complication 3) *inside* the struct so `read_frame`/`decode_raw` surface only
   Frame/EOS. `run_video_producer` becomes the same 3-line wrapper. **Verify:** the ~10 existing MF spawn tests
   pass unchanged (the whole point), plus the app on real video (seek-heavy).
4. **Delete the two dead loop copies.** Both `run_*` functions are now wrappers; the ~350 lines of duplicated
   loop body are gone, replaced by one `run<B>`.

One backend per commit; keep each `run_*` wrapper behaviour-identical so the integration tests bisect cleanly.

## 5. Part A complications (from the investigation, ranked)

1. **FFmpeg-only planar prime — the biggest asymmetry.** FFmpeg must decode the first frame *before* `Opened`
   to pick NV12/P010/fp16; MF negotiates from the media type and never primes. The shared S3 must carry an
   optional pre-decoded first frame (`open` returns `Option<(i64, Vec<u8>)>`; S3 prepends it). There is also an
   EOS-before-any-frame edge (`primed_empty`) with no MF analog — the loop must handle "primed frame is EOS."
2. **`Gap` surfaced (MF) vs hidden (FFmpeg).** MF returns a null-sample `Gap` tick to the loop, which retries;
   FFmpeg absorbs EAGAIN internally. Unify by pushing MF's gap-retry into the backend so the trait only yields
   Frame/EOS. Small, but re-shapes MF's read functions.
3. **`&mut kind` stride threading (MF wart).** MF threads `&mut OutKind` through its read/convert fns because a
   mid-stream media-type change can move the RGB32 stride. When those become backend methods, `kind` becomes a
   private field and drops from every signature — a net simplification, but it touches every MF read site.
4. **`reader_pos` (loop-level, MF) vs `last_ts` (backend-level, FFmpeg).** Unify on FFmpeg's model: the backend
   owns "current position" and `can_decode_forward(target_units)` needs no external position. MF's read methods
   must then update an internal position field.
5. **`color` session-constant (MF) vs per-frame (FFmpeg).** Keep `make_frame` in the trait — do **not** try to
   share the `VideoFrame{…}` construction. That is exactly what lets both coexist.
6. **`cancel: Arc<AtomicBool>` (FFmpeg-only)** wired into libav's AVIO interrupt. Fold into `open`'s options;
   MF ignores it.
7. **`!Send` / COM apartment.** `IMFSourceReader` is `!Send` and MF needs `ensure_mf()` on the producing thread;
   libav contexts are thread-bound too. Both already run on a dedicated owned thread — so **no `Send` bound**,
   and the `type Raw` associated type means no `dyn` anyway (`fn run<B>` monomorphizes). Fine as-is.
8. **cfg-gating.** MF is `cfg(windows)`; FFmpeg is behind the ffmpeg feature, cross-platform. The trait + `run<B>`
   live in a cfg-neutral module (the protocol types already are); each `impl` stays behind its own cfg. ⚠ This
   means **the MF backend and its `impl` are invisible on non-Windows** — but this whole task is Windows-verifiable
   (both backends compile here), so unlike the #125 macOS gap there is no cross-machine blind spot. Linux/mac only
   ever compile the FFmpeg backend, which is the same code path CI already builds.
9. **Minor drift to audit, not silently fix:** MF's seek diag (`video_diag()`/`eprintln!`) vs FFmpeg's `diag()`
   differ in format (same `PB_VIDEO_DIAG` gate); MF's seek origin-anchor uses inline hns while FFmpeg routes
   through `facts.duration_to_pts`. Logically equivalent. Route both through one shared helper so they cannot
   diverge — but preserve current observable behaviour; if a real difference is found, file it separately.

## 6. Part B — the shared YUV color primitive (small)

**What moves:** the matrix family enum + `kr_kb()` + `coeffs()` (the ~6 constants and the derivation). Nothing
else — the two converters stay where they are (different jobs, §1b).

**Home decision (the one design call).** pb-decode has no pb-crate deps today; pb-render depends only on
pb-core. Options:
- **(i) A `color` module in `pb-core`.** pb-core is pure (no I/O/GPU), and matrix constants are pure math, so it
  *fits the purity constraint* — but pb-core's identity is "nav" (playlist/ring/prefetch), and colour math is a
  thematic stretch. Adds a `pb-decode → pb-core` edge.
- **(ii) A new micro-crate `pb-color`.** Thematically clean, ~40 lines, both crates depend on it. Costs one more
  crate in the tree.
- **Recommendation:** (ii) `pb-color` if the owner is fine adding a crate; else (i). Decide before writing code —
  this is the kind of "where does it live" call `docs/where-code-goes.md` exists for, one level up (cross-crate).

**The WGSL stays separate.** The shader can't import a Rust `const`. Leave the matrix in the shader, and keep
pb-render's existing **independent-from-spec golden test** as the cross-check (it already exists and is the
correct guard). Optionally add a one-line doc pointer from the shader to the shared primitive. Do **not** build a
codegen step to emit WGSL from Rust — over-engineering for 6 numbers.

**Safety (Part B):** this *is* close to a pure move for the constants — the values are identical across the two
copies (verified: both `(0.299, 0.114)` / `(0.2126, 0.0722)` / `(0.2627, 0.0593)`). After consolidating, the two
crates' existing YUV unit tests + pb-render's golden tests must pass unchanged. A wrong constant would fail the
golden diff loudly.

## 7. Part C — posters + audio decoders (NOTED, not committed)

Finding #5 also lumps in **3 poster extractors** (`av_poster`, `mf_poster`, `ffmpeg/poster`) and **2 audio
decoders** (`MfAudioDecoder`, `FfAudioDecoder`). These are real, but:
- They were **not** investigated in depth for this plan; the producer + YUV work is scoped and sized, they are not.
- The poster extractors may share less than the producers do (different container entry points); the audio
  decoders straddle the FFmpeg-first WASAPI path (#audio work) and deserve their own read.
- **Decision:** finish Parts A + B, measure the win, then decide whether a `Poster`/`AudioDecoder` trait pays for
  itself or is cargo-culting the producer pattern onto code that doesn't rhyme as closely. File as a follow-up
  task with its own mapping. Do not pre-commit here.

## 8. Risks

| risk | severity | mitigation |
|---|---|---|
| The shared loop subtly changes pacing/seek behaviour for one backend | **the top risk** | §3 layer-1 mock test pins the contract; port FFmpeg first (its loop is the model, lowest-risk), then MF against the tested spec; existing MF spawn tests must pass unchanged |
| "Fix while unifying" quietly changes semantics (the drift in §5.9) | high — destroys behaviour-equivalence | §2 scope gate: preserve each backend's current behaviour, file real bugs separately |
| Real-video tests are flaky, mask a regression | high | §3: layer-1 mock test is deterministic and primary; treat green real-video runs as necessary-not-sufficient |
| MF backend port is large (`&mut kind`, Gap, reader_pos all move at once) | medium | §4a step 3 does them together *inside* the struct but behind the already-tested loop; the loop can't regress if the mock test holds |
| Part B home churn (pb-core vs pb-color) argued repeatedly | low | §6: decide once, up front, before code |
| Scope creep into Part C | medium | §7: explicitly deferred, its own task |

## 9. What this does NOT do

- It does not touch the wire protocol, `VideoSession`, or the WGSL math.
- It does not change what the user sees or hears — a correct execution is behaviour-identical; the win is one
  loop instead of two (~350 fewer duplicated lines) and one colour-constant definition instead of two.
- It does not unify posters/audio (Part C, deferred) — so finding #5 is *reduced*, not fully closed, by this task.
