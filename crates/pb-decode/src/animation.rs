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

use crate::{common, DecodeError, FitBox};

/// Which animated container a file is. The cheap sniff returns this; the decoder
/// routes on it (and the timing layer keys its quirk-handling off it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationKind {
    Gif,
    /// Animated PNG (an `acTL` chunk before the first `IDAT`).
    Apng,
    /// Animated WebP (a `VP8X` chunk with the animation flag).
    Webp,
}

impl AnimationKind {
    /// Stable, content-derived codec label for the info panel / logs.
    pub fn codec(self) -> &'static str {
        match self {
            AnimationKind::Gif => "GIF",
            AnimationKind::Apng => "APNG",
            AnimationKind::Webp => "WebP",
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

// --- Detection (cheap header sniff) ------------------------------------------------

/// Sniff whether `bytes` is an animated image we can play, and which kind. Reads
/// only container headers (a few chunk/block walks) — never decodes a pixel — so
/// it's cheap enough to call while building a photo's metadata.
///
/// Returns `None` for a still image (including a *single-frame* GIF/PNG/WebP),
/// which is the overwhelmingly common case and stays on the normal still path.
pub fn detect_animation(bytes: &[u8]) -> Option<AnimationKind> {
    if is_animated_gif(bytes) {
        Some(AnimationKind::Gif)
    } else if is_apng(bytes) {
        Some(AnimationKind::Apng)
    } else if is_animated_webp(bytes) {
        Some(AnimationKind::Webp)
    } else {
        None
    }
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
        AnimationKind::Apng | AnimationKind::Webp if ms == 0 => Duration::from_millis(100),
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

fn decode_animation_inner(bytes: &[u8], fit: Option<FitBox>) -> Result<Animation, DecodeError> {
    let Some(kind) = detect_animation(bytes) else {
        return Err(DecodeError::Unsupported);
    };
    decode_with_image_crate(kind, bytes, fit)
}

/// The pure-Rust `image`-crate backend (GIF/APNG/WebP). It composites dispose/blend
/// and exposes the NETSCAPE/`num_plays`/loop-count via `AnimationDecoder`, so we
/// never parse those ourselves. (macOS will gain an Image I/O backend that's
/// preferred there; this stays the universal baseline.)
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
    }
}

/// `image`'s `LoopCount` → our `u32` convention (`0` = infinite).
fn loop_count_to_u32(lc: image::metadata::LoopCount) -> u32 {
    match lc {
        image::metadata::LoopCount::Infinite => 0,
        image::metadata::LoopCount::Finite(n) => n.get(),
    }
}

/// Drain an `image::Frames` iterator into composited, fit-downscaled [`AnimFrame`]s,
/// applying the timing normalization and the resident-frame caps.
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
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{
        codecs::gif::{GifEncoder, Repeat},
        Delay, Frame, Rgba, RgbaImage,
    };

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

    // --- decode: real round-trips through the image-crate backend ------------------

    #[test]
    fn decodes_every_gif_frame_with_normalized_timing_and_loop_count() {
        let gif = encode_gif(3, Repeat::Infinite, Delay::from_numer_denom_ms(100, 1));
        let anim = decode_animation(&gif, None).expect("decode animated gif");
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
        let anim = decode_animation(&gif, None).expect("decode");
        assert!(anim
            .frames
            .iter()
            .all(|f| f.delay == Duration::from_millis(100)));
    }

    #[test]
    fn decode_honors_a_finite_loop_count() {
        let gif = encode_gif(2, Repeat::Finite(3), Delay::from_numer_denom_ms(100, 1));
        let anim = decode_animation(&gif, None).expect("decode");
        assert_eq!(anim.loop_count, 3);
    }

    #[test]
    fn decode_downscales_frames_to_fit() {
        // Each frame is 8x8; fit to 4x4 → frames come back 4x4.
        let gif = encode_gif(2, Repeat::Infinite, Delay::from_numer_denom_ms(100, 1));
        let anim = decode_animation(
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
    }
}
