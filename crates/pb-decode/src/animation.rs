//! Animated-image support: GIF, APNG, and animated WebP — multi-frame decode.
//!
//! This is a **separate, opt-in path** from the still decoder, and that separation
//! is the whole point. Flicking through a folder only ever decodes the *first*
//! frame (the still path in [`crate::decode_bytes`] — `image::load_from_memory`
//! returns frame 0 for all three of these containers). The full sequence is
//! decoded **only** when the user asks to play it (press `P`), so nothing here ever
//! touches the still hot path, the prefetch ring, or the keypress→photon budget.
//!
//! Three jobs live here, each independently testable:
//!
//! 1. **Detection** ([`detect_animation`]) — a cheap header sniff: a GIF with more
//!    than one image, a PNG carrying an `acTL` chunk (= APNG), or a WebP whose
//!    `VP8X` chunk has the animation flag set. Reads headers only, never decodes.
//! 2. **Frame timing** ([`normalize_delay`] + the per-format parsers) — the fiddly
//!    encoder-quirk handling (GIF sub-threshold clamp, APNG `delay_den == 0`, WebP
//!    milliseconds), matched to browser behavior and unit-tested in isolation.
//! 3. **Decode** ([`decode_animation`]) — returns fully **composited** RGBA8 frames.
//!    Dispose/blend is done by the decoder (the `image` crate here, Image I/O on
//!    macOS later); we never hand-roll it.
//!
//! Privacy (task #2): every frame produced here is a RAM-only cache, dropped when
//! playback stops or the user navigates away. Nothing is serialized.

use std::time::Duration;

use crate::{common, ColorTransform, DecodeError, FitBox};

/// Which animated container a file is. The cheap sniff returns this; the decoder
/// routes on it (and the timing layer keys its quirk-handling off it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationKind {
    Gif,
    /// Animated PNG (an `acTL` chunk before the first `IDAT`).
    Apng,
    /// Animated WebP (a `VP8X` chunk with the animation flag).
    Webp,
    /// An ISOBMFF image **sequence** — animated AVIF (`avis`) or HEIC (`msf1`).
    /// Only decodable via the macOS Image I/O backend (no pure-Rust AV1/HEVC
    /// sequence decoder), so it's only ever produced on macOS.
    Heif,
    /// The motion component of an Apple **Live Photo** — a short QuickTime `.mov`
    /// (H.264 / HEVC) that lives in a *separate* file alongside the still, decoded on
    /// macOS via AVFoundation (task #38). Not a byte-sniff of the still: produced by
    /// the still↔`.mov` pairing, not `detect_animation`.
    LivePhoto,
}

impl AnimationKind {
    /// Stable, content-derived codec label for the info panel / logs. (For `Heif`
    /// the precise brand — AVIF vs HEIC — is resolved by the decoder and overrides
    /// this generic label.)
    pub fn codec(self) -> &'static str {
        match self {
            AnimationKind::Gif => "GIF",
            AnimationKind::Apng => "APNG",
            AnimationKind::Webp => "WebP",
            AnimationKind::Heif => "HEIF",
            AnimationKind::LivePhoto => "Live Photo",
        }
    }
}

/// One fully-composited animation frame: straight-alpha RGBA8 at the (post
/// decode-to-fit) canvas size, shown for `delay`.
#[derive(Clone)]
pub struct AnimFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// How long this frame is displayed (already normalized; see [`normalize_delay`]).
    pub delay: Duration,
}

/// A decoded animation: every frame pre-composited, plus the loop count.
///
/// `loop_count == 0` means **loop forever** (the GIF NETSCAPE / APNG `num_plays` /
/// WebP loop-count convention, where 0 is infinite). Any positive value is how many
/// times the whole sequence should play before stopping on the last frame.
pub struct Animation {
    pub kind: AnimationKind,
    /// Canvas dimensions (every frame shares these).
    pub width: u32,
    pub height: u32,
    pub frames: Vec<AnimFrame>,
    /// Times to play the whole sequence; `0` = infinite.
    pub loop_count: u32,
    pub codec: &'static str,
    /// Source→sRGB color transform the renderer applies per texel (every frame
    /// shares the container's profile). sRGB passthrough for the pure-Rust path;
    /// the macOS Image I/O path draws into Display-P3 and carries the P3→BT.709
    /// transform, like the still HEIC path.
    pub color: ColorTransform,
    /// True if the sequence was cut short by [`MAX_FRAMES`] / [`MAX_DECODED_BYTES`]
    /// — we play the bounded prefix rather than exhaust RAM on a pathological file.
    pub truncated: bool,
}

impl Animation {
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

/// Hard cap on the number of frames we hold resident, so a pathological
/// thousand-frame file can't exhaust RAM. The bounded prefix still plays.
pub const MAX_FRAMES: usize = 2048;

/// Hard cap on total decoded (post-fit) frame bytes held resident — the other half
/// of the OOM guard, for a modest frame count at a huge canvas. ~1.5 GiB.
pub const MAX_DECODED_BYTES: u64 = 1536 * 1024 * 1024;

// ── Streaming motion decode (task #69: play while decoding) ─────────────────────────

/// One message from a streaming Live Photo motion decode (`decode_live_motion_streaming`
/// — FFmpeg on Linux, AVAssetReader on macOS). The consumer installs a streaming
/// `Playback` on the first [`Frame`](MotionChunk::Frame), appends later frames as they
/// arrive, and finalizes on [`Done`](MotionChunk::Done) — so a Live Photo starts playing
/// within a frame or two instead of after the whole `.mov` decodes.
///
/// The chunk sequence is a contract every producer upholds:
/// - **Success:** one [`Header`](MotionChunk::Header), one or more `Frame`s, exactly one
///   `Done`.
/// - **Setup / zero-frame failure:** one [`Failed`](MotionChunk::Failed), possibly
///   preceded by a `Header` (never by a `Frame`).
/// - **Midstream failure:** `Header`, one or more `Frame`s, then `Failed`.
/// - **Cancellation:** any valid non-terminal prefix, then the producer returns
///   *silently* — no terminal chunk (the consumer has already dropped the stream).
///
/// Never a duplicate `Header`, never a `Frame` before the `Header`, and never anything
/// after a terminal `Done`/`Failed`.
pub enum MotionChunk {
    /// Sent once, before any frame: the sequence's display dimensions + color, enough to
    /// build the `Animation` header.
    Header(MotionHeader),
    /// A decoded, scaled, rotation-corrected frame (in emission = playback order).
    Frame(AnimFrame),
    /// The stream finished cleanly; `truncated` if the frame or byte cap was hit.
    Done { loop_count: u32, truncated: bool },
    /// The stream failed. If a Playback is already installed the consumer can treat this as
    /// a truncated finish; if not, it's a play failure to surface.
    Failed(DecodeError),
}

/// Header for a streaming Live Photo motion decode — the post-rotation display size and the
/// color transform every frame shares.
pub struct MotionHeader {
    pub width: u32,
    pub height: u32,
    pub color: ColorTransform,
    pub codec: &'static str,
}

/// Accumulates a [`MotionChunk`] stream into a whole [`Animation`], **validating the chunk
/// contract** along the way — the batch `decode_live_motion` wrappers are built on this, so
/// a producer bug (frame before header, chunk after terminal, dims drifting from the
/// header) surfaces as a decode error instead of a corrupt `Animation`.
// Only the macOS batch wrapper uses it today; the contract tests run on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Default)]
pub(crate) struct MotionCollector {
    header: Option<MotionHeader>,
    frames: Vec<AnimFrame>,
    done: Option<(u32, bool)>,
    failed: Option<DecodeError>,
    violation: Option<&'static str>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl MotionCollector {
    pub(crate) fn push(&mut self, chunk: MotionChunk) {
        if self.violation.is_some() {
            return; // already invalid — keep the first violation
        }
        if self.done.is_some() || self.failed.is_some() {
            self.violation = Some("chunk after a terminal Done/Failed");
            return;
        }
        match chunk {
            MotionChunk::Header(h) => {
                if self.header.is_some() {
                    self.violation = Some("duplicate Header");
                } else {
                    self.header = Some(h);
                }
            }
            MotionChunk::Frame(f) => match &self.header {
                None => self.violation = Some("Frame before Header"),
                Some(h) if f.width != h.width || f.height != h.height => {
                    self.violation = Some("Frame dimensions differ from the Header")
                }
                Some(_) => self.frames.push(f),
            },
            MotionChunk::Done {
                loop_count,
                truncated,
            } => {
                if self.frames.is_empty() {
                    self.violation = Some("Done without any Frame");
                } else {
                    self.done = Some((loop_count, truncated));
                }
            }
            MotionChunk::Failed(e) => self.failed = Some(e),
        }
    }

    /// The collected [`Animation`], the producer's own [`MotionChunk::Failed`] error, or a
    /// contract-violation error. A stream that ended without a terminal chunk is an
    /// internal error here — the batch wrappers never cancel, so a silent return means the
    /// producer broke the contract.
    pub(crate) fn finish(self) -> Result<Animation, DecodeError> {
        if let Some(v) = self.violation {
            return Err(DecodeError::Corrupt(format!(
                "Live Photo motion stream contract violated: {v}"
            )));
        }
        if let Some(e) = self.failed {
            return Err(e);
        }
        let (Some(h), Some((loop_count, truncated))) = (self.header, self.done) else {
            return Err(DecodeError::Corrupt(
                "Live Photo motion stream ended without a terminal chunk".into(),
            ));
        };
        Ok(Animation {
            kind: AnimationKind::LivePhoto,
            width: h.width,
            height: h.height,
            frames: self.frames,
            loop_count,
            codec: h.codec,
            color: h.color,
            truncated,
        })
    }
}

// --- Detection (cheap header sniff) ------------------------------------------------

/// Sniff whether `bytes` is an animated image we can play, and which kind. Reads
/// only container headers (a few chunk/block walks) — never decodes a pixel — so
/// it's cheap enough to call while building a photo's metadata.
///
/// Returns `None` for a still image (including a *single-frame* GIF/PNG/WebP),
/// which is the overwhelmingly common case and stays on the normal still path.
pub fn detect_animation(bytes: &[u8]) -> Option<AnimationKind> {
    if is_animated_gif(bytes) {
        return Some(AnimationKind::Gif);
    }
    if is_apng(bytes) {
        return Some(AnimationKind::Apng);
    }
    if is_animated_webp(bytes) {
        return Some(AnimationKind::Webp);
    }
    // ISOBMFF image sequences (animated AVIF `avis` / HEIC `msf1`). A still AVIF/HEIC
    // (brand `avif`/`heic`/`mif1`) is NOT a sequence and stays on the still path.
    // Decodable on macOS (Image I/O) and on Linux with `livephoto` (the FFmpeg video
    // pipeline) — there's no pure-Rust AV1/HEVC sequence decoder, so elsewhere it's not
    // flagged (it would only produce a dead "▶" with nothing to play).
    #[cfg(target_os = "macos")]
    if crate::isobmff::isobmff_is_sequence(bytes) {
        return Some(AnimationKind::Heif);
    }
    #[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
    if isobmff_image_sequence(bytes) {
        return Some(AnimationKind::Heif);
    }
    None
}

/// Whether `bytes` is an ISOBMFF image **sequence** (animated AVIF `avis` / HEIC `msf1`):
/// major or a compatible brand is a sequence brand. A self-contained copy of the sniff in
/// `isobmff` (that module only compiles with the HEIC backends), so animated-AVIF detection
/// on Linux doesn't drag the whole `colr`-parsing module into a libheif-less build.
#[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
fn isobmff_image_sequence(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let is_seq = |b: &[u8]| matches!(b, b"avis" | b"msf1");
    if is_seq(&bytes[8..12]) {
        return true;
    }
    // Compatible-brands list (offset 16, after the 4-byte minor version), bounded by the
    // ftyp box length.
    let box_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let end = box_len.min(bytes.len());
    let mut i = 16;
    while i + 4 <= end {
        if is_seq(&bytes[i..i + 4]) {
            return true;
        }
        i += 4;
    }
    false
}

/// A GIF with **more than one image descriptor**. Walks the block stream just far
/// enough to find a second image (early-out), so the common single-frame case is a
/// couple of skips. Bounded by a guard against malformed inputs.
fn is_animated_gif(b: &[u8]) -> bool {
    if b.len() < 13 || (&b[0..6] != b"GIF87a" && &b[0..6] != b"GIF89a") {
        return false;
    }
    // Logical Screen Descriptor occupies bytes 6..13; its packed field is byte 10.
    let packed = b[10];
    let mut i = 13usize;
    // Skip the Global Color Table when present (3 bytes per entry, 2^(N+1) entries).
    if packed & 0x80 != 0 {
        i += 3 * (1usize << ((packed & 0x07) + 1));
    }
    let mut images = 0u32;
    let mut guard = 0u32;
    while i < b.len() {
        guard += 1;
        if guard > 1_000_000 {
            break;
        }
        match b[i] {
            // Image descriptor: 0x2C + 9 bytes (the packed field is the 10th).
            0x2C => {
                images += 1;
                if images >= 2 {
                    return true;
                }
                if i + 10 > b.len() {
                    break;
                }
                let img_packed = b[i + 9];
                i += 10;
                if img_packed & 0x80 != 0 {
                    i += 3 * (1usize << ((img_packed & 0x07) + 1));
                }
                // LZW minimum code size, then the image data sub-blocks.
                i = i.saturating_add(1);
                i = skip_sub_blocks(b, i);
            }
            // Extension: 0x21, label, then sub-blocks (GCE / comment / app / text).
            0x21 => {
                i = i.saturating_add(2);
                i = skip_sub_blocks(b, i);
            }
            // Trailer, or anything unexpected → stop.
            _ => break,
        }
    }
    images >= 2
}

/// Advance past a GIF sub-block chain: length-prefixed blocks terminated by a
/// zero-length block. Returns the index just after the terminator.
fn skip_sub_blocks(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        let len = b[i] as usize;
        i = i.saturating_add(1);
        if len == 0 {
            break;
        }
        i = i.saturating_add(len);
    }
    i
}

/// A PNG carrying an `acTL` (animation control) chunk **before** the first `IDAT` —
/// the APNG marker. A plain PNG (or one where `acTL` somehow follows `IDAT`, which
/// is invalid) reads as a still.
fn is_apng(b: &[u8]) -> bool {
    const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if b.len() < 8 || b[0..8] != PNG_SIG {
        return false;
    }
    let mut i = 8usize;
    let mut guard = 0u32;
    while i + 8 <= b.len() {
        guard += 1;
        if guard > 1_000_000 {
            break;
        }
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let typ = &b[i + 4..i + 8];
        if typ == b"acTL" {
            return true;
        }
        if typ == b"IDAT" {
            return false; // a valid APNG declares acTL before the default image
        }
        // 4 (length) + 4 (type) + len (data) + 4 (CRC).
        i = i.saturating_add(12).saturating_add(len);
    }
    false
}

/// A WebP whose `VP8X` extended-format chunk sets the animation flag (bit 1 of the
/// flags byte). Plain `VP8 `/`VP8L` WebPs are single-frame.
fn is_animated_webp(b: &[u8]) -> bool {
    if b.len() < 21 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return false;
    }
    // The first chunk's FourCC is at offset 12; VP8X's flags byte is at offset 20.
    if &b[12..16] == b"VP8X" {
        return b[20] & 0x02 != 0;
    }
    false
}

// --- Frame timing ------------------------------------------------------------------

/// The browser-matched per-frame delay policy, keyed by container.
///
/// - **GIF**: encoders routinely write a 0 or 1-centisecond delay meaning "as fast
///   as possible"; every browser clamps anything under ~20 ms up to 100 ms, so we
///   do too (a runaway 0 ms GIF would otherwise pin a core re-presenting).
/// - **APNG / WebP**: a genuine high-fps frame (e.g. 16 ms ≈ 60 fps) is honored;
///   only a nonsensical zero-length frame is clamped to 100 ms.
///
/// `raw` is the delay the container actually declared (after the per-format parsers
/// below have resolved their own quirks, e.g. APNG's `delay_den == 0`).
pub fn normalize_delay(kind: AnimationKind, raw: Duration) -> Duration {
    let ms = raw.as_millis();
    match kind {
        AnimationKind::Gif if ms < 20 => Duration::from_millis(100),
        // APNG / WebP / HEIF sequences: honor a genuine fast frame; only clamp a
        // nonsensical zero-length one (e.g. an AVIF sequence with no delay metadata).
        AnimationKind::Apng | AnimationKind::Webp | AnimationKind::Heif if ms == 0 => {
            Duration::from_millis(100)
        }
        _ => raw,
    }
}

/// Resolve a GIF frame delay (its native unit is **centiseconds**, 1/100 s) to a
/// normalized [`Duration`]. Applies the sub-threshold clamp (see [`normalize_delay`]).
pub fn gif_delay(centiseconds: u16) -> Duration {
    normalize_delay(
        AnimationKind::Gif,
        Duration::from_millis(centiseconds as u64 * 10),
    )
}

/// Resolve an APNG frame delay (`delay_num / delay_den` **seconds**) to a normalized
/// [`Duration`]. Per the APNG spec a `delay_den` of 0 is treated as **100**, so a
/// `delay_num`-only frame is `delay_num/100` s — the classic gotcha. A resulting
/// zero (`delay_num == 0`) clamps to 100 ms.
pub fn apng_delay(delay_num: u16, delay_den: u16) -> Duration {
    let den = if delay_den == 0 {
        100u32
    } else {
        delay_den as u32
    };
    let secs = delay_num as f64 / den as f64;
    normalize_delay(AnimationKind::Apng, Duration::from_secs_f64(secs))
}

/// Resolve a WebP frame duration (its native unit is already **milliseconds**) to a
/// normalized [`Duration`]. A zero duration clamps to 100 ms.
pub fn webp_delay(ms: u32) -> Duration {
    normalize_delay(AnimationKind::Webp, Duration::from_millis(ms as u64))
}

// --- Decode ------------------------------------------------------------------------

/// Decode an animated image's **whole bounded sequence** into composited RGBA8
/// frames, downscaled to `fit` (the display size) like the still path. The sniff
/// ([`detect_animation`]) decides the container; an un-animated / unrecognized input
/// is [`DecodeError::Unsupported`].
///
/// **Panic-safe** like the still decoders: a third-party decoder panicking on a
/// hostile file becomes a [`DecodeError`], not an app crash (the release profile is
/// `panic = "unwind"`; see [`crate::decode_bytes`]).
///
/// Frames are produced eagerly and held in RAM — bounded by [`MAX_FRAMES`] /
/// [`MAX_DECODED_BYTES`]; on overflow the returned [`Animation`] has `truncated =
/// true` and the playable prefix.
pub fn decode_animation(bytes: &[u8], fit: Option<FitBox>) -> Result<Animation, DecodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_animation_inner(bytes, fit)
    })) {
        Ok(r) => r,
        Err(_) => Err(DecodeError::Corrupt("animation decoder panicked".into())),
    }
}

// The `return` is load-bearing on non-macOS (a second cfg block follows), but reads
// as "needless" on macOS where that block is compiled out — allow it for both.
#[allow(clippy::needless_return)]
fn decode_animation_inner(bytes: &[u8], fit: Option<FitBox>) -> Result<Animation, DecodeError> {
    let Some(kind) = detect_animation(bytes) else {
        return Err(DecodeError::Unsupported);
    };
    // macOS prefers the OS Image I/O backend (hardware-assisted, and the only path
    // that can decode AVIF/HEIC sequences) for every animated kind; the pure-Rust
    // `image`-crate path is the universal baseline everywhere else.
    #[cfg(target_os = "macos")]
    {
        return imageio_animation::decode(kind, bytes, fit);
    }
    #[cfg(not(target_os = "macos"))]
    {
        decode_with_image_crate(kind, bytes, fit)
    }
}

/// The pure-Rust `image`-crate backend (GIF/APNG/WebP). It composites dispose/blend
/// and exposes the NETSCAPE/`num_plays`/loop-count via `AnimationDecoder`, so we
/// never parse those ourselves. This is the universal baseline (every non-macOS
/// target); macOS routes to the Image I/O backend instead, so here it's compiled in
/// only off-macOS — plus in `test` builds, where the unit tests exercise it directly
/// for deterministic, backend-pinned assertions on every platform.
#[cfg(any(not(target_os = "macos"), test))]
fn decode_with_image_crate(
    kind: AnimationKind,
    bytes: &[u8],
    fit: Option<FitBox>,
) -> Result<Animation, DecodeError> {
    use image::AnimationDecoder;
    use std::io::Cursor;

    let corrupt = |e: image::ImageError| DecodeError::Corrupt(e.to_string());
    match kind {
        AnimationKind::Gif => {
            let dec = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let loops = loop_count_to_u32(dec.loop_count());
            collect_frames(kind, dec.into_frames(), loops, fit)
        }
        AnimationKind::Apng => {
            let dec = image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let apng = dec.apng().map_err(corrupt)?;
            let loops = loop_count_to_u32(apng.loop_count());
            collect_frames(kind, apng.into_frames(), loops, fit)
        }
        AnimationKind::Webp => {
            let dec = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let loops = loop_count_to_u32(dec.loop_count());
            collect_frames(kind, dec.into_frames(), loops, fit)
        }
        // ISOBMFF sequences never reach this backend (the pure-Rust path can't decode
        // AV1/HEVC) — they're only ever detected on macOS, which routes to Image I/O.
        // Live Photo motion is a separate `.mov` decoded via AVFoundation (`livephoto`),
        // never `decode_animation`, so it never reaches here either.
        AnimationKind::Heif | AnimationKind::LivePhoto => Err(DecodeError::Unsupported),
    }
}

/// `image`'s `LoopCount` → our `u32` convention (`0` = infinite).
#[cfg(any(not(target_os = "macos"), test))]
fn loop_count_to_u32(lc: image::metadata::LoopCount) -> u32 {
    match lc {
        image::metadata::LoopCount::Infinite => 0,
        image::metadata::LoopCount::Finite(n) => n.get(),
    }
}

/// Drain an `image::Frames` iterator into composited, fit-downscaled [`AnimFrame`]s,
/// applying the timing normalization and the resident-frame caps.
#[cfg(any(not(target_os = "macos"), test))]
fn collect_frames(
    kind: AnimationKind,
    frames: image::Frames<'_>,
    loop_count: u32,
    fit: Option<FitBox>,
) -> Result<Animation, DecodeError> {
    let mut out: Vec<AnimFrame> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut truncated = false;
    let (mut canvas_w, mut canvas_h) = (0u32, 0u32);

    for frame in frames {
        if out.len() >= MAX_FRAMES {
            truncated = true;
            break;
        }
        let frame = frame.map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        // `Delay` carries the per-frame timing as an exact (numer, denom) ms ratio;
        // the per-format quirks (GIF centiseconds, APNG delay_den == 0) are already
        // resolved by the decoder, so we only apply the browser clamp by kind.
        let (numer, denom) = frame.delay().numer_denom_ms();
        let raw = Duration::from_secs_f64(numer as f64 / denom.max(1) as f64 / 1000.0);
        let delay = normalize_delay(kind, raw);

        let buffer = frame.into_buffer();
        let (fw, fh) = (buffer.width(), buffer.height());
        let (rgba, w, h) = match fit {
            Some(fit) => common::downscale_to_fit(buffer.into_raw(), fw, fh, fit)?,
            None => (buffer.into_raw(), fw, fh),
        };
        if out.is_empty() {
            canvas_w = w;
            canvas_h = h;
        }
        total_bytes = total_bytes.saturating_add(rgba.len() as u64);
        out.push(AnimFrame {
            rgba,
            width: w,
            height: h,
            delay,
        });
        if total_bytes > MAX_DECODED_BYTES {
            truncated = true;
            break;
        }
    }

    if out.is_empty() {
        return Err(DecodeError::Corrupt("no frames decoded".into()));
    }
    Ok(Animation {
        kind,
        width: canvas_w,
        height: canvas_h,
        frames: out,
        loop_count,
        codec: kind.codec(),
        // The `image` crate hands back sRGB-encoded frames (it drops any ICC for
        // these formats); pass through. The macOS Image I/O path carries a real
        // P3 transform instead.
        color: ColorTransform::srgb(),
        truncated,
    })
}

/// The macOS Image I/O animation backend: the OS decodes every frame (compositing
/// dispose/blend itself) and exposes per-frame delays + loop count in the format
/// property dictionaries. Preferred on macOS for every kind, and the only path that
/// can decode AVIF/HEIC sequences. Frames arrive Display-P3 straight-alpha; we
/// downscale-to-fit, normalize timing, apply the caps, and carry the P3→BT.709
/// transform (exactly like the still HEIC path).
#[cfg(target_os = "macos")]
mod imageio_animation {
    use super::*;

    pub(super) fn decode(
        kind: AnimationKind,
        bytes: &[u8],
        fit: Option<FitBox>,
    ) -> Result<Animation, DecodeError> {
        let raw = crate::imageio::decode_animation_frames(bytes, MAX_FRAMES).ok_or_else(|| {
            DecodeError::Corrupt("Image I/O could not decode the animation".into())
        })?;

        let mut out: Vec<AnimFrame> = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut truncated = raw.frames.len() >= MAX_FRAMES;
        let (mut canvas_w, mut canvas_h) = (0u32, 0u32);

        // AVIF/HEIC sequences carry no per-frame delay in Image I/O's still-image
        // property dictionaries — their timing lives in the movie track. Recover the
        // constant frame duration from `mdhd`/`stts` so the sequence plays at its true
        // rate instead of a guessed default. (GIF/APNG/WebP delays come per-frame.)
        let heif_fallback_secs = (kind == AnimationKind::Heif)
            .then(|| crate::isobmff::isobmff_sequence_frame_delay(bytes))
            .flatten();

        for rf in raw.frames {
            if out.len() >= MAX_FRAMES {
                truncated = true;
                break;
            }
            let secs = if rf.delay_secs > 0.0 {
                rf.delay_secs
            } else {
                heif_fallback_secs.unwrap_or(0.0)
            };
            let delay = normalize_delay(kind, Duration::from_secs_f64(secs.max(0.0)));
            let (rgba, w, h) = match fit {
                Some(fit) => common::downscale_to_fit(rf.rgba, rf.width, rf.height, fit)?,
                None => (rf.rgba, rf.width, rf.height),
            };
            if out.is_empty() {
                canvas_w = w;
                canvas_h = h;
            }
            total_bytes = total_bytes.saturating_add(rgba.len() as u64);
            out.push(AnimFrame {
                rgba,
                width: w,
                height: h,
                delay,
            });
            if total_bytes > MAX_DECODED_BYTES {
                truncated = true;
                break;
            }
        }
        if out.is_empty() {
            return Err(DecodeError::Corrupt("no frames decoded".into()));
        }

        // For an AVIF/HEIC sequence the precise brand is the codec label; the others
        // keep their kind label. Every Image I/O frame was drawn into Display-P3, so
        // they all carry the P3(SMPTE-432)→BT.709 + sRGB-TRC transform.
        let codec = match kind {
            AnimationKind::Heif => crate::isobmff::isobmff_brand(bytes).unwrap_or("HEIF"),
            other => other.codec(),
        };
        Ok(Animation {
            kind,
            width: canvas_w,
            height: canvas_h,
            frames: out,
            loop_count: raw.loop_count,
            codec,
            color: ColorTransform::from_cicp(12, 13, 0, true),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        codecs::gif::{GifEncoder, Repeat},
        Delay, Frame, Rgba, RgbaImage,
    };

    // --- MotionCollector: the streaming-chunk contract ------------------------------

    fn mc_header() -> MotionChunk {
        MotionChunk::Header(MotionHeader {
            width: 2,
            height: 2,
            color: ColorTransform::srgb(),
            codec: "Live Photo",
        })
    }

    fn mc_frame() -> MotionChunk {
        MotionChunk::Frame(AnimFrame {
            rgba: vec![0; 16],
            width: 2,
            height: 2,
            delay: Duration::from_millis(33),
        })
    }

    fn collect(chunks: Vec<MotionChunk>) -> Result<Animation, DecodeError> {
        let mut c = MotionCollector::default();
        for chunk in chunks {
            c.push(chunk);
        }
        c.finish()
    }

    #[test]
    fn motion_collector_builds_a_valid_stream() {
        let anim = collect(vec![
            mc_header(),
            mc_frame(),
            mc_frame(),
            MotionChunk::Done {
                loop_count: 1,
                truncated: false,
            },
        ])
        .expect("valid stream");
        assert_eq!(anim.frame_count(), 2);
        assert_eq!((anim.width, anim.height), (2, 2));
        assert_eq!(anim.loop_count, 1);
        assert!(!anim.truncated);
        assert!(matches!(anim.kind, AnimationKind::LivePhoto));
    }

    #[test]
    fn motion_collector_preserves_the_producers_error() {
        let err = collect(vec![
            mc_header(),
            MotionChunk::Failed(DecodeError::Corrupt("boom".into())),
        ])
        .err()
        .expect("expected an error");
        assert!(err.to_string().contains("boom"), "got: {err}");
    }

    #[test]
    fn motion_collector_rejects_contract_violations() {
        for (label, chunks) in [
            ("frame before header", vec![mc_frame()]),
            ("duplicate header", vec![mc_header(), mc_header()]),
            (
                "done without frames",
                vec![
                    mc_header(),
                    MotionChunk::Done {
                        loop_count: 1,
                        truncated: false,
                    },
                ],
            ),
            (
                "dims mismatch",
                vec![
                    mc_header(),
                    MotionChunk::Frame(AnimFrame {
                        rgba: vec![0; 4],
                        width: 1,
                        height: 1,
                        delay: Duration::from_millis(33),
                    }),
                ],
            ),
            (
                "chunk after terminal",
                vec![
                    mc_header(),
                    mc_frame(),
                    MotionChunk::Done {
                        loop_count: 1,
                        truncated: false,
                    },
                    mc_frame(),
                ],
            ),
        ] {
            let err = collect(chunks).err().expect("expected an error");
            assert!(
                err.to_string().contains("contract violated"),
                "{label}: expected a contract violation, got: {err}"
            );
        }
    }

    #[test]
    fn motion_collector_flags_a_stream_with_no_terminal() {
        let err = collect(vec![mc_header(), mc_frame()])
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("without a terminal"), "got: {err}");
    }

    // --- timing: the spec's required gotcha tests ---------------------------------

    #[test]
    fn gif_clamps_sub_threshold_delays_to_100ms() {
        // Encoders write 0 or tiny delays meaning "fast as possible"; browsers (and
        // we) bump anything under 20 ms to 100 ms.
        assert_eq!(gif_delay(0), Duration::from_millis(100)); // 0 cs
        assert_eq!(gif_delay(1), Duration::from_millis(100)); // 10 ms < 20
                                                              // 20 ms is the boundary and is honored (not clamped).
        assert_eq!(gif_delay(2), Duration::from_millis(20));
        assert_eq!(gif_delay(10), Duration::from_millis(100)); // a real 100 ms frame
        assert_eq!(gif_delay(5), Duration::from_millis(50));
    }

    #[test]
    fn apng_delay_den_zero_means_denominator_100() {
        // The classic APNG gotcha: delay_den == 0 is defined to mean 100, so a frame
        // of delay_num/0 is delay_num/100 seconds, NOT a divide-by-zero / forever.
        assert_eq!(apng_delay(5, 0), Duration::from_millis(50)); // 5/100 s
        assert_eq!(apng_delay(50, 0), Duration::from_millis(500)); // 50/100 s
                                                                   // A normal denominator is honored as num/den seconds.
        assert_eq!(apng_delay(1, 10), Duration::from_millis(100)); // 1/10 s
        assert_eq!(apng_delay(33, 1000), Duration::from_millis(33)); // ~30 fps, kept
                                                                     // delay_num == 0 (zero-length frame) clamps to 100 ms, not 0.
        assert_eq!(apng_delay(0, 0), Duration::from_millis(100));
        assert_eq!(apng_delay(0, 30), Duration::from_millis(100));
    }

    #[test]
    fn webp_delay_is_milliseconds_with_zero_clamp() {
        assert_eq!(webp_delay(40), Duration::from_millis(40));
        assert_eq!(webp_delay(1000), Duration::from_secs(1));
        // High-fps WebP is preserved (only GIF gets the 20 ms floor).
        assert_eq!(webp_delay(16), Duration::from_millis(16));
        // A zero duration clamps to 100 ms.
        assert_eq!(webp_delay(0), Duration::from_millis(100));
    }

    #[test]
    fn normalize_delay_only_floors_gif_below_20ms() {
        // GIF: sub-20 ms floored, 20 ms+ kept.
        assert_eq!(
            normalize_delay(AnimationKind::Gif, Duration::from_millis(5)),
            Duration::from_millis(100)
        );
        assert_eq!(
            normalize_delay(AnimationKind::Gif, Duration::from_millis(40)),
            Duration::from_millis(40)
        );
        // APNG/WebP: a fast frame is honored, only exact zero is clamped.
        assert_eq!(
            normalize_delay(AnimationKind::Apng, Duration::from_millis(5)),
            Duration::from_millis(5)
        );
        assert_eq!(
            normalize_delay(AnimationKind::Webp, Duration::ZERO),
            Duration::from_millis(100)
        );
    }

    // --- detection: header sniffs against crafted + encoded inputs -----------------

    /// Encode an `n`-frame GIF (each a distinct solid color), as the multi-frame
    /// fixture. `repeat` controls the NETSCAPE loop count.
    fn encode_gif(n: u32, repeat: Repeat, delay: Delay) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            enc.set_repeat(repeat).unwrap();
            for k in 0..n {
                let c = [(40 + k * 20) as u8, 80, (200 - k * 20) as u8, 255];
                let img = RgbaImage::from_pixel(8, 8, Rgba(c));
                enc.encode_frame(Frame::from_parts(img, 0, 0, delay))
                    .unwrap();
            }
        }
        buf
    }

    #[test]
    fn detects_a_multi_frame_gif_but_not_a_single_frame_one() {
        let multi = encode_gif(3, Repeat::Infinite, Delay::from_numer_denom_ms(100, 1));
        assert_eq!(detect_animation(&multi), Some(AnimationKind::Gif));

        let single = encode_gif(1, Repeat::Finite(0), Delay::from_numer_denom_ms(100, 1));
        assert_eq!(detect_animation(&single), None, "1-frame GIF is a still");
    }

    #[test]
    fn detects_apng_by_actl_before_idat() {
        // Minimal PNG-shaped bytes: signature + a chunk header. acTL → APNG.
        let apng = png_with_chunk(b"acTL");
        assert_eq!(detect_animation(&apng), Some(AnimationKind::Apng));
        // A plain PNG (IDAT, no acTL) is a still.
        let plain = png_with_chunk(b"IDAT");
        assert!(!is_apng(&plain));
    }

    /// PNG signature + IHDR + the given chunk type (length 0). Enough to exercise the
    /// chunk walker in [`is_apng`] without a full valid image.
    fn png_with_chunk(typ: &[u8; 4]) -> Vec<u8> {
        let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let push_chunk = |v: &mut Vec<u8>, t: &[u8; 4]| {
            v.extend_from_slice(&0u32.to_be_bytes()); // length 0
            v.extend_from_slice(t);
            v.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
        };
        push_chunk(&mut v, b"IHDR");
        push_chunk(&mut v, typ);
        v
    }

    #[test]
    fn detects_animated_webp_by_vp8x_flag() {
        let mut anim = riff_webp_vp8x(0x02); // animation bit set
        assert_eq!(detect_animation(&anim), Some(AnimationKind::Webp));
        // Clear the flag → still WebP.
        anim = riff_webp_vp8x(0x00);
        assert!(!is_animated_webp(&anim));
        // A non-VP8X (plain lossy) WebP is never animated.
        let mut still = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        still.extend_from_slice(&[0u8; 16]);
        assert!(!is_animated_webp(&still));
    }

    /// A RIFF/WEBP container whose first chunk is `VP8X` with `flags`.
    fn riff_webp_vp8x(flags: u8) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes()); // file size (ignored by the sniff)
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(b"VP8X");
        v.extend_from_slice(&10u32.to_le_bytes()); // VP8X payload size
        v.push(flags); // flags byte (offset 20)
        v.extend_from_slice(&[0u8; 9]); // reserved + canvas dims
        v
    }

    #[test]
    fn detection_rejects_garbage_and_short_inputs() {
        assert_eq!(detect_animation(&[]), None);
        assert_eq!(detect_animation(&[0u8; 4]), None);
        assert_eq!(detect_animation(b"not an image at all"), None);
    }

    // --- decode: round-trips through the image-crate backend -----------------------
    //
    // These call `decode_with_image_crate` directly (not `decode_animation`) so they
    // pin the pure-Rust backend's exact frame/timing/loop output on every platform —
    // on macOS `decode_animation` routes to Image I/O, which is covered by the corpus
    // smoke test instead.

    #[test]
    fn decodes_every_gif_frame_with_normalized_timing_and_loop_count() {
        let gif = encode_gif(3, Repeat::Infinite, Delay::from_numer_denom_ms(100, 1));
        let anim =
            decode_with_image_crate(AnimationKind::Gif, &gif, None).expect("decode animated gif");
        assert_eq!(anim.kind, AnimationKind::Gif);
        assert_eq!(anim.frame_count(), 3);
        assert_eq!(anim.loop_count, 0, "NETSCAPE infinite → 0");
        assert_eq!((anim.width, anim.height), (8, 8));
        for f in &anim.frames {
            assert_eq!(f.delay, Duration::from_millis(100));
            assert_eq!(f.rgba.len(), 8 * 8 * 4);
            assert!(!anim.truncated);
        }
    }

    #[test]
    fn decode_clamps_a_zero_delay_gif_to_100ms() {
        // delay 0 → the GIF sub-threshold clamp kicks in end-to-end.
        let gif = encode_gif(2, Repeat::Infinite, Delay::from_numer_denom_ms(0, 1));
        let anim = decode_with_image_crate(AnimationKind::Gif, &gif, None).expect("decode");
        assert!(anim
            .frames
            .iter()
            .all(|f| f.delay == Duration::from_millis(100)));
    }

    #[test]
    fn decode_honors_a_finite_loop_count() {
        let gif = encode_gif(2, Repeat::Finite(3), Delay::from_numer_denom_ms(100, 1));
        let anim = decode_with_image_crate(AnimationKind::Gif, &gif, None).expect("decode");
        assert_eq!(anim.loop_count, 3);
    }

    #[test]
    fn decode_downscales_frames_to_fit() {
        // Each frame is 8x8; fit to 4x4 → frames come back 4x4.
        let gif = encode_gif(2, Repeat::Infinite, Delay::from_numer_denom_ms(100, 1));
        let anim = decode_with_image_crate(
            AnimationKind::Gif,
            &gif,
            Some(FitBox {
                max_width: 4,
                max_height: 4,
            }),
        )
        .expect("decode");
        assert_eq!((anim.width, anim.height), (4, 4));
        assert!(anim.frames.iter().all(|f| f.rgba.len() == 4 * 4 * 4));
    }

    #[test]
    fn decode_rejects_a_still_image() {
        // A non-animated input has no animation path.
        let png = png_with_chunk(b"IDAT");
        assert!(matches!(
            decode_animation(&png, None),
            Err(DecodeError::Unsupported)
        ));
    }

    /// A minimal ISOBMFF `ftyp` box with the given major brand + compatible-brands list.
    #[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let len = 16 + compat.len() * 4;
        let mut b = (len as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(major);
        b.extend_from_slice(&[0, 0, 0, 0]); // minor version
        for c in compat {
            b.extend_from_slice(*c);
        }
        b
    }

    /// On Linux with `livephoto`, an ISOBMFF **sequence** brand (`avis`/`msf1`) — in the major
    /// slot or the compatible list — is flagged animated (→ the FFmpeg sequence decoder); a
    /// still `avif`/`heic` is not.
    #[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
    #[test]
    fn detects_isobmff_image_sequences_on_linux() {
        assert_eq!(
            detect_animation(&ftyp(b"avis", &[b"avif", b"mif1"])),
            Some(AnimationKind::Heif),
            "major-brand avis"
        );
        assert_eq!(
            detect_animation(&ftyp(b"mif1", &[b"msf1", b"heic"])),
            Some(AnimationKind::Heif),
            "compatible-brand msf1"
        );
        assert_eq!(
            detect_animation(&ftyp(b"avif", &[b"mif1"])),
            None,
            "a still avif is not a sequence"
        );
        assert_eq!(
            detect_animation(&ftyp(b"heic", &[b"mif1"])),
            None,
            "still heic"
        );
    }

    /// Local smoke test over JD's real corpus, when present. CI machines don't have
    /// `~/Downloads`, so this no-ops there; locally it proves the real GIF/APNG/WebP
    /// files decode to multiple frames with sane timing.
    #[test]
    fn corpus_files_decode_when_available() {
        let dir = std::path::Path::new("/Users/jdlien/Downloads/test-images/animated");
        if !dir.exists() {
            return;
        }
        for (name, kind) in [
            ("car-race.gif", AnimationKind::Gif),
            ("elephant.gif", AnimationKind::Gif),
            ("happy_birthday.gif", AnimationKind::Gif),
            ("elephant.png", AnimationKind::Apng),
            ("3.webp", AnimationKind::Webp),
            ("4.webp", AnimationKind::Webp),
        ] {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            assert_eq!(detect_animation(&bytes), Some(kind), "{name} kind");
            let anim = decode_animation(&bytes, None)
                .unwrap_or_else(|e| panic!("{name} decode failed: {e}"));
            assert!(anim.frame_count() > 1, "{name} should have >1 frame");
            assert!(
                anim.frames
                    .iter()
                    .all(|f| f.delay >= Duration::from_millis(20)),
                "{name} frame delays should be clamped sane"
            );
        }

        // Animated AVIF is an ISOBMFF sequence — only the macOS Image I/O backend
        // decodes it. Confirm the sequence sniff + decode work there.
        #[cfg(target_os = "macos")]
        {
            let avif = dir.join("3.avif");
            if avif.exists() {
                let bytes = std::fs::read(&avif).unwrap();
                assert_eq!(
                    detect_animation(&bytes),
                    Some(AnimationKind::Heif),
                    "3.avif kind"
                );
                let anim = decode_animation(&bytes, None).expect("3.avif decode");
                assert!(anim.frame_count() > 1, "3.avif should have >1 frame");
            }
        }
    }
}
