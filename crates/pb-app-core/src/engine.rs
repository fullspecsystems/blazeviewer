//! Engine tuning constants + pure helper functions (NS0 5.5 / Phase B).
//!
//! These migrated verbatim out of the winit shell (`pb-app/src/main.rs`) so the
//! orchestration methods that use them can live on [`AppCore`](crate::AppCore) instead of
//! `impl App`. They are shell-neutral: pure math + the decode entry points (which route
//! through `pb-decode`/`pb-source`, both already core deps). The shell still references a
//! few of these (the ones it shares) via `pb_app_core::engine::*`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pb_decode::{decode_named_bytes, DecodeError, DecodedImage, FitBox, PixelFormat};
use pb_render::{Rotation, ScaleMode};
use pb_source::ItemSource;

use crate::meta::PhotoMeta;
use crate::settings::ScaleModePref;
use crate::{Action, Nav};

/// A fresh, non-reproducible seed for a new [`pb_core::Playlist`]'s shuffle order.
///
/// `pb-core` stays pure/dependency-free (no `rand` crate, no clock) so its shuffle is
/// a deterministic function of `(len, seed)` — the seed has to come from the caller.
/// Mixes wall-clock nanos with a process-local counter so two decks opened in the
/// same clock tick (e.g. rapid folder switches) still diverge.
pub fn fresh_shuffle_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ count.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// VRAM budget for the resident texture ring (~1.5 GB → ~16–32 fit-size slots on
/// a 7680-wide display, far more on smaller ones). Capacity is clamped to [4, 64].
pub const RING_BUDGET_BYTES: u64 = 1_500_000_000;
/// Cap on decoded-but-not-yet-uploaded bytes held by the decode pool (backpressure).
/// Shared by every shell's pool construction (the winit `App::new`, the macOS host's
/// [`AppCore::new_host`](crate::AppCore::new_host)).
pub const POOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Max slot uploads performed per `about_to_wait` tick, so a burst of finished
/// decodes can't blow the frame budget.
pub const UPLOADS_PER_TICK: usize = 2;
/// Cap on the **full-resolution** "sharp ring" upgraded around the cursor when
/// parked (`upgrade_set`). The preview window can be much larger (up to the ring's
/// 64-slot capacity on small windows), but holding more than this many *fulls*
/// resident is wasted decode — nobody pause-and-steps two dozen photos before
/// either blazing (previews carry that) or stopping. Keeps the on-park decode burst
/// bounded. On a 7680 fullscreen the byte-budgeted capacity (~12–32) binds first.
pub const MAX_FULL_RING: usize = 24;

/// Gigapixel safety ceiling for the parked full-res tier (#106.7 §9): never *request* or
/// retain a full-resolution `Original` whose true pixel count exceeds this — the item stays
/// fit-only (a screen can't show more than the downscaled view anyway, and 1:1 of a gigapixel
/// is a meaningless crop). Sits **above** the 24–100 MP pro range (an A7R V is 61 MP, a GFX
/// 100 MP) so instant-zoom covers the files where it matters, and well below a true gigapixel
/// whose RGBA8 decode buffer would be multiple GB. Bounds the retained texture *and* the
/// transient decode buffer.
pub const FULL_RES_MAX_PIXELS: u64 = 200_000_000;

/// ADR-024 watchdog: how long the displayed photo may linger as a **resident preview** before
/// its full is force-requested regardless of `held_nav` (the level-triggered safety net). A lost
/// key-up can leave the held-key map stuck `Some`, which suppresses the sharpen (both the tick's
/// re-issue and `sharpen_now` gate on `held_nav().is_none()`) until a focus change fires the
/// release net — the unreproducible stuck-preview race.
///
/// The deadline must comfortably exceed the **slowest legitimate single-photo dwell while a nav
/// key is genuinely held**, or a real (slow) blaze would trip it and put a forced full decode
/// ahead of the previews the blaze needs. Settings allow a 1 s hold delay (`hold_delay_ms` cap)
/// and a 1 photo/s advance-rate floor (`max_advance_rate` min), so the legit ceiling is ~1 s;
/// 2 s gives 2× margin. A stuck preview self-heals in 2 s where before it lingered until a
/// focus change — still well inside "the app fixed itself before I reached for the mouse".
pub const PREVIEW_WATCHDOG_AFTER: Duration = Duration::from_millis(2000);

/// #110 ScalePolicy A/B levers (the 110c harness formalizes these into `ScalePolicy` variants).
/// `PB_SCALE_POLICY=cpu` disables the GPU derive — the incumbent CPU Lanczos settle re-decode
/// runs instead; unset/anything else = derive on (the 110b default).
pub fn gpu_derive_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PB_SCALE_POLICY").map_or(true, |v| v != "cpu"))
}

/// `PB_DERIVE_KERNEL` — Lanczos lobe count for the GPU derive: 2 (safer, less halo) or
/// 3 (more detail, more ring). **Default 3, confirmed by the 110c A/B data** (`ab_report`):
/// L3 beat L2 on every pattern × ratio at equal measured cost (L2's narrower kernel also
/// leaked visible aliasing on 1-px diagonals below 2×, detail ratio up to 15.8×).
pub fn derive_kernel() -> u32 {
    static K: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *K.get_or_init(|| {
        std::env::var("PB_DERIVE_KERNEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|k| *k == 2 || *k == 3)
            .unwrap_or(3)
    })
}

/// `PB_DERIVE_MIP_BIAS` — source-mip policy for the GPU derive: 0 = last eligible box mip
/// (fastest, most box-prefiltered), −1 = one level finer (residual 2–4×, wider scale-aware
/// kernel — the #110 §3a quality/perf fork). **Default −1, picked from the 110c A/B data**
/// (`pb-render ab_report`, 2026-07-18, RTX 5090): L3b−1 lands FLIP ≤ 0.012 vs the linear-light
/// Lanczos reference on every pattern × ratio (zone plate, slanted/coloured edges, 1-px
/// diagonals, foliage noise; 1.25–6.9×) with detail ratio ≈ 1.00, where bias 0 inherits the
/// box chain's softness/phase error above 2× (FLIP 0.013–0.024) and collapses to the raw box
/// mip at exactly 2×; measured cost was equal (~0.4 ms at 1024×768-scale outputs).
pub fn derive_mip_bias() -> i32 {
    static B: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        std::env::var("PB_DERIVE_MIP_BIAS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|b| (-2..=1).contains(b))
            .unwrap_or(-1)
    })
}

/// Exact VRAM of a mip-chained texture: `Σ_levels max(1, w>>l)·max(1, h>>l)·bpp` (mip plan
/// §4d). The Original rep is uploaded WITH a full mip chain, so recording only L0
/// (`img.pixels.len()`) under-counted true residency by ~4/3× — enough to blow the ring's
/// byte budget once several full-res Originals are retained (item-6).
pub fn mip_chain_bytes(w: u32, h: u32, bpp: u64) -> u64 {
    let levels = 32 - w.max(h).max(1).leading_zeros();
    (0..levels)
        .map(|l| ((w >> l).max(1) as u64) * ((h >> l).max(1) as u64) * bpp)
        .sum()
}

/// How many watchdog fires one photo gets per visit (see `PreviewWatchdog::retries`): a
/// full-decode error after a fire re-arms the watchdog for another 2 s cycle, so a transient
/// SMB hiccup converges to sharp — this caps a permanently-failing decode at a few attempts
/// instead of an endless every-2 s retry while parked on it.
pub const MAX_WATCHDOG_RETRIES: u8 = 3;

/// How long a resize settle waits after a **discrete fullscreen toggle** before the crisp
/// re-derive/re-decode (#110 §4). The standard 180 ms settle debounces interactive drag-resize
/// streams; a toggle is ONE event, so waiting the full debounce just delays the sharp frame.
pub const FULLSCREEN_SETTLE: Duration = Duration::from_millis(50);
/// The standard interactive-resize settle (drag streams need real debouncing).
pub const RESIZE_SETTLE: Duration = Duration::from_millis(180);

/// How many bytes the preview-first path (#106.5) reads from the front of a JPEG to find
/// its embedded EXIF thumbnail + SOF header. The IFD1 thumbnail is ~22 KB near the file
/// start; 256 KB comfortably covers it (and the SOF) while staying a small fraction of a
/// 39 MB entry over SMB. A thumbnail beyond this simply falls back to the full decode.
pub const PREVIEW_PREFIX_BYTES: usize = 256 * 1024;

/// Whether `name` ends in a JPEG extension — the only format the preview-first prefix
/// path (#106.5) applies to.
fn is_jpeg_name(name: &str) -> bool {
    let lower = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(lower.as_str(), "jpg" | "jpeg" | "jpe" | "jfif")
}

/// Hold-to-zoom curve: the e-folding zoom rate (per second) ramps from a gentle
/// start (fine tuning) to a fast max over `ZOOM_RAMP_SECS`, along the
/// **quadratic ease-in** [`hold_ramp`] so a brief tap barely moves — the fine
/// control the owner asked for. Time-based so it's frame-rate independent.
/// `MIN_RATE` is the **tap** speed (a quick tap barely leaves the ramp floor):
/// halved 2026-07-14 so a tap nudges ~1–3 px on a high-refresh display.
/// Retuned 2026-07-19 (task #67, owner feel note — the ramp still accelerated
/// too aggressively): `MIN_RATE` halved again, `RAMP_SECS` doubled, `MAX_RATE`
/// cut to ~2/3 — gentler start, slower build, lower ceiling. `RAMP_SECS`
/// nudged back down the same day (owner: "a little faster") after smoke.
pub const ZOOM_MIN_RATE: f32 = 0.09;
pub const ZOOM_MAX_RATE: f32 = 1.47;
pub const ZOOM_RAMP_SECS: f32 = 1.4;

/// Eased progress of a hold ramp: `0` at press → `1` after `ramp_secs`, with a
/// **quadratic ease-in** (`p²`) so the first part of a hold moves slowly (fine
/// adjustment) and speed builds the longer the key is held. Shared by
/// hold-to-zoom and hold-to-pan so the two feel identical.
pub fn hold_ramp(elapsed_secs: f32, ramp_secs: f32) -> f32 {
    let p = (elapsed_secs / ramp_secs.max(1e-3)).clamp(0.0, 1.0);
    p * p
}

/// Scales macOS's incremental trackpad magnification (`PinchGesture` delta) into a zoom
/// factor (`1 + delta·gain`). Read by `AppCore::handle`'s `Pinch` arm.
pub const PINCH_GAIN: f32 = 1.0;

/// Scroll-wheel / trackpad tuning (read by `AppCore::scroll`). `WHEEL_ZOOM_STEP` is the per-line
/// zoom factor for a line-precise wheel/swipe (Ctrl+scroll, or the Zoom setting).
pub const WHEEL_ZOOM_STEP: f32 = 0.1;

/// Eased scroll-zoom shaping (smooths the coarse `LineDelta` pinch notches — see [`ZoomEase`]).
/// `ZOOM_EASE_TAU` is the exponential time constant (seconds) of the glide — each tick closes
/// `1 - exp(-dt/TAU)` of the remaining gap to the target, so smaller = snappier, larger =
/// floatier. `ZOOM_EASE_EPS` is the "close enough" ratio at which the ease snaps to the target
/// and finishes (avoids an asymptotic tail that never ends). Tuned by feel; A/B off with
/// `PB_EASE_ZOOM=0`.
pub const ZOOM_EASE_TAU: f32 = 0.06;
/// Finish the ease when the live zoom is within this fraction of the target.
pub const ZOOM_EASE_EPS: f32 = 0.002;

/// The fraction of the remaining *multiplicative* zoom gap to close this tick, for the
/// [`ZOOM_EASE_TAU`] time constant. Frame-rate independent (driven by real `dt`);
/// `dt <= 0` returns `0.0` so the first (zero-elapsed) tick just latches the clock and the glide
/// starts on the next tick. Approaches `1.0` as `dt` grows, so a long frame still lands near the
/// target rather than crawling.
pub fn zoom_ease_alpha(dt: f32) -> f32 {
    if dt <= 0.0 {
        0.0
    } else {
        1.0 - (-dt / ZOOM_EASE_TAU).exp()
    }
}
/// Per-**pixel** zoom factor for a pixel-precise scroll (a macOS trackpad two-finger swipe). Much
/// smaller than [`WHEEL_ZOOM_STEP`] because a trackpad delivers many events of tens of pixels each;
/// `0.0025` gives ~1.6× over a full swipe.
pub const PIXEL_ZOOM_STEP: f32 = 0.0025;
/// Pixels panned per scroll *line* (`LineDelta`). Fractional high-res lines from a trackpad pan
/// smoothly, while a 1.0 mouse notch makes one comfortable step.
pub const WHEEL_PAN_STEP: f32 = 80.0;
/// Sign of two-finger trackpad panning. `+1.0` makes the image follow the fingers (grab-and-drag);
/// flip to `-1.0` to invert.
pub const GESTURE_PAN_DIR: f32 = 1.0;

/// Pan-inertia deceleration: the fraction of the current velocity surviving each
/// millisecond. `0.998` is UIKit's `UIScrollView.DecelerationRate.normal` — a time constant
/// of ~500 ms — deliberately borrowed so a flick on a Windows touchscreen decays like the
/// momentum macOS generates for a trackpad, rather than like a constant we invented.
/// (UIKit's `.fast` is `0.99`, ~100 ms, if this ever feels floaty.)
pub const PAN_INERTIA_DECAY_PER_MS: f32 = 0.998;
/// Below this speed (physical px/sec) the glide is over — ends the asymptotic tail.
pub const PAN_INERTIA_MIN_SPEED: f32 = 24.0;
/// A lift slower than this (physical px/sec) was placing the photo, not throwing it, and
/// starts no glide at all.
pub const PAN_FLING_MIN_SPEED: f32 = 120.0;

/// Zoom-inertia deceleration, per millisecond. Deliberately faster than
/// [`PAN_INERTIA_DECAY_PER_MS`] (~250 ms vs ~500 ms): a pinch is a *precision* act — the
/// user pinches to a size — so a long glide overshoots the size they chose. Note that
/// neither iOS nor macOS glides zoom at all; this exists to be A/B'd, not because a
/// platform does it.
pub const ZOOM_INERTIA_DECAY_PER_MS: f32 = 0.988;
/// Constant deceleration (e-folds/sec²) applied *on top of* the viscous decay above.
///
/// Exponential decay alone has an asymptotic tail — it never actually stops, it only gets
/// cut off at a floor — and that tail is what made the first version of the zoom glide
/// overrun. A constant friction term brings it to a **definite** stop, which is what makes
/// it read as controlled rather than merely slow.
pub const ZOOM_INERTIA_FRICTION: f32 = 4.0;
/// Ceiling on the launch velocity. Total travel is roughly `v₀ · τ`, so an uncapped fast
/// pinch adds *several* e-folds of zoom after release — a second gesture, not a follow
/// through. Capped, the glide contributes at most ~1.2×.
pub const ZOOM_FLING_MAX_SPEED: f32 = 2.5;
/// Below this |log-zoom| speed (e-folds/sec) the zoom glide is over.
pub const ZOOM_INERTIA_MIN_SPEED: f32 = 0.20;
/// A pinch releasing slower than this (e-folds/sec) was settling on a size, not throwing.
pub const ZOOM_FLING_MIN_SPEED: f32 = 0.60;

/// Fraction of the **log-space** zoom velocity surviving a tick of `dt` seconds, at the
/// [`ZOOM_INERTIA_DECAY_PER_MS`] rate. Same shape as [`pan_inertia_decay`], separate
/// constant so the two tune independently — they want different curves.
pub fn zoom_inertia_decay(dt: f32) -> f32 {
    if dt <= 0.0 {
        1.0
    } else {
        ZOOM_INERTIA_DECAY_PER_MS.powf(dt * 1000.0)
    }
}

/// One tick of zoom-glide friction: the viscous decay above, plus [`ZOOM_INERTIA_FRICTION`]
/// as a constant deceleration so the glide reaches zero instead of asymptotically
/// approaching it. Never crosses zero — friction stops motion, it doesn't reverse it.
pub fn zoom_inertia_step(v_log: f32, dt: f32) -> f32 {
    if dt <= 0.0 {
        return v_log;
    }
    let decayed = v_log * zoom_inertia_decay(dt);
    let drop = ZOOM_INERTIA_FRICTION * dt;
    if decayed.abs() <= drop {
        0.0
    } else {
        decayed - decayed.signum() * drop
    }
}

/// Fraction of the pan velocity surviving a tick of `dt` seconds, at the
/// [`PAN_INERTIA_DECAY_PER_MS`] rate. Frame-rate independent (driven by real `dt`);
/// `dt <= 0` returns `1.0`, so a zero-length tick latches the clock and slows nothing —
/// the mirror of [`zoom_ease_alpha`]'s `0.0`.
pub fn pan_inertia_decay(dt: f32) -> f32 {
    if dt <= 0.0 {
        1.0
    } else {
        PAN_INERTIA_DECAY_PER_MS.powf(dt * 1000.0)
    }
}

/// Hold-to-pan curve: pan speed (px/sec) ramps from a gentle start to a fast max
/// over `PAN_RAMP_SECS`, along the same [`hold_ramp`] ease-in as zoom (per the
/// owner's note). Time-based, frame-rate independent. `MIN_SPEED` is the **tap**
/// speed (a quick tap barely leaves the ramp floor): halved 2026-07-14 so a tap
/// nudges ~1–3 px on a high-refresh display.
/// Retuned 2026-07-19 (task #67, owner feel note — the ramp still accelerated
/// too aggressively): `MIN_SPEED` halved again, `RAMP_SECS` doubled, `MAX_SPEED`
/// cut to ~2/3 — gentler start, slower build, lower ceiling. Same shape as zoom.
/// `RAMP_SECS` nudged back down the same day (owner: "a little faster") after smoke.
pub const PAN_MIN_SPEED: f32 = 85.0;
pub const PAN_MAX_SPEED: f32 = 1800.0;
pub const PAN_RAMP_SECS: f32 = 1.4;

/// Repeat interval for the held frame-step scrub (`,`/`.`), after the initial tap
/// delay (`initial_delay`). ~14 fps — quick enough to scrub, slow enough to read (#37).
pub const FRAME_STEP_REPEAT: Duration = Duration::from_millis(70);

/// Repeat interval for the held video seek (task #79 phase 6): 5 steps/s of ±2 s
/// (±10 s shifted) — brisk scrubbing, and comfortably above the measured 4K HEVC
/// seek-landing time (~350 ms) with latest-value coalescing absorbing the rest.
pub const VIDEO_SEEK_REPEAT: Duration = Duration::from_millis(200);

/// How long a seek run must go without a NEW seek intent before its landed
/// position commits the ONE platform audio seek (+ resume) — plan 1D. The
/// **fallback** window: it must exceed [`VIDEO_SEEK_REPEAT`] so that when the
/// held-key signal is somehow missed, a held key or a scrubber drag still never
/// restarts audio for intermediate targets.
///
/// The common case doesn't wait this long: [`VIDEO_SEEK_AUDIO_QUIET`] commits far
/// sooner once the seek **key is released** (a discrete tap). Measured: with the
/// flat 250 ms, a single `+2 s` tap left audio ~172 ms behind the picture; keying
/// off the release drops that to the ~10 ms WASAPI reseek.
pub const VIDEO_SEEK_AUDIO_SETTLE: Duration = Duration::from_millis(250);

/// The **fast** audio-commit window for a discrete tap: once the seek key is no
/// longer held, only this much quiet (no new intent) is needed before the audio
/// seek commits — enough to coalesce a rapid tap-tap burst, but far below
/// [`VIDEO_SEEK_AUDIO_SETTLE`] so a single tap's audio lands with the picture
/// rather than a fifth of a second behind it. Held scrubbing keeps the key down,
/// so this never applies mid-scrub; the cheap ~10 ms reseek (measured) is what
/// makes committing this eagerly safe.
pub const VIDEO_SEEK_AUDIO_QUIET: Duration = Duration::from_millis(60);

/// How long the info line stays flashed as the video seek/step OSD when the `i`
/// toggle is off (each further seek re-arms it). Replaces the `m:ss / m:ss` toast
/// — the line's playback row is the better readout (owner call 2026-07-11).
pub const VIDEO_OSD_HOLD: Duration = Duration::from_millis(1800);

/// Session-only video resume policy (task #94.2). Don't bother remembering a
/// position in the first [`RESUME_MIN`] (a glance at the opening) or the last
/// [`RESUME_END_GUARD`] (the credits / a clip about to end and restart anyway);
/// when we do resume, back up [`RESUME_REWIND`] for a moment of re-orienting
/// context. These are deliberately generous — resuming the wrong way (mid-credits,
/// or one second in) is worse than just starting over.
pub const RESUME_MIN: Duration = Duration::from_secs(5);
pub const RESUME_END_GUARD: Duration = Duration::from_secs(5);
pub const RESUME_REWIND: Duration = Duration::from_secs(2);

/// The position to resume a returned-to video at, or `None` to restart from 0
/// (task #94.2). `pos` is where the viewer left off; `dur` the clip length. Pure
/// so the policy is unit-tested without a session. Remembers only a position
/// meaningfully into a long-enough clip and not near the end; the returned target
/// is rewound a touch (never below 0) for context.
pub fn video_resume_target(pos: Duration, dur: Duration) -> Option<Duration> {
    if pos < RESUME_MIN || dur <= RESUME_END_GUARD {
        return None;
    }
    if pos >= dur.saturating_sub(RESUME_END_GUARD) {
        return None;
    }
    Some(pos.saturating_sub(RESUME_REWIND))
}

/// The hover **controls zone**: the bottom fraction of the window where pointer
/// movement reveals the playback controls while a video is active (the info
/// line's home corner — every video player's convention).
pub const VIDEO_HOVER_ZONE: f32 = 0.25;

/// Deleting a video whose reader is still retiring (HEVC teardown ~1 s) retries
/// on this cadence, up to [`DELETE_RETRY_MAX`] times (~1.8 s total) — enough to
/// outlast the measured retirement, bounded so a genuinely locked file still
/// reports "Delete failed" promptly.
pub const DELETE_RETRY_INTERVAL: Duration = Duration::from_millis(300);
pub const DELETE_RETRY_MAX: u32 = 6;

/// How long the user must rest on an animated still before we eagerly decode the whole
/// sequence in the background (so a slow WebP/AVIF plays instantly on `P`). Long enough
/// that tapping straight through a folder of animations never kicks a decode (#37).
pub const EAGER_PREP_DELAY: Duration = Duration::from_millis(250);

/// How long a finished Live Photo lingers on its last motion frame before reverting to
/// the crisp still — "a beat after the video finishes" (task #38).
pub const LIVE_REVERT_DELAY: Duration = Duration::from_millis(450);

/// After a delete, hold the doomed photo (under a trash/recycle icon) for a beat before
/// advancing the playlist off it, so the feedback registers rather than the next photo
/// snapping in instantly (`do_delete` → `flush_pending_delete`).
pub const DELETE_ADVANCE_DELAY: Duration = Duration::from_millis(160);

/// Cap on the Live Photo motion's long edge when decoding its `.mov` (task #38). The
/// motion is a brief preview, not a pixel-peeping asset, so a ~1440px cap keeps the
/// whole pre-decoded RGBA sequence's RAM bounded (~0.5 GB worst case) without a visible
/// quality cost. Also clamped to the display fit, so a small window decodes smaller.
#[cfg(any(
    target_os = "macos",
    windows,
    all(unix, not(target_os = "macos"), feature = "livephoto")
))]
pub const MOTION_MAX_LONG_EDGE: u32 = 1440;

/// Max displayed characters for an EXIF value; longer ones are truncated so a
/// single field can't blow out the panel width.
pub const EXIF_VALUE_MAX: usize = 72;

/// Fixed width of the scan status card, in logical px (scaled by the display factor, then
/// clamped to the window). Wide enough for a reasonable current-folder path; longer paths are
/// truncated. Fixed so the card doesn't jitter as the count / live path change width.
pub const SCAN_CARD_WIDTH: f32 = 320.0;

/// "Not-ready" loading-pie tuning (the top-right affordance shown while the next
/// photo is still decoding). The fill is a deliberate "honest-ish" fake: there is
/// no true decode progress, so it eases asymptotically toward — but never reaches
/// — full, on a time constant self-calibrated to how long misses usually take.
/// Only appears once a wait outlasts `PIE_SHOW_DELAY`, so fast hits never flash it.
pub const PIE_SHOW_DELAY: f32 = 0.12; // s a wait must persist before the pie appears
pub const PIE_TAU_MIN: f32 = 0.06; // s floor on the fill time constant
pub const PIE_FILL_CAP: f32 = 0.93; // the wedge never quite completes (the "lie")
pub const PIE_FINISH_FADE: f32 = 0.18; // s to snap-to-full then fade once ready
pub const PIE_GLOW_DUR: f32 = 0.30; // s the keypress brighten-pulse decays over
pub const PIE_EWMA_ALPHA: f32 = 0.30; // weight of the latest wait in the time estimate
pub const PIE_DIAMETER: f32 = 46.0; // logical px (scaled by the display factor)
pub const PIE_MARGIN: f32 = 24.0; // logical px in from the top-right corner

/// Ring capacity from the per-slot byte size and the VRAM budget. Full-res
/// (Original) slots are several times bigger than fit slots, so the prefetch
/// window is correspondingly smaller — but still resident and async.
pub fn ring_capacity(slot_bytes: u64) -> usize {
    ((RING_BUDGET_BYTES / slot_bytes.max(1)) as usize).clamp(4, 64)
}

/// Split the ring into an ahead/behind prefetch window (the current item, always
/// resident, takes the remaining slot). Biased forward; a few behind so reversing
/// stays cheap.
pub fn window_for_capacity(cap: usize) -> (usize, usize) {
    let usable = cap.saturating_sub(1);
    let ahead = (usable * 4 / 5).max(1);
    let behind = usable.saturating_sub(ahead);
    (ahead, behind)
}

/// Translate the decoder's color transform into the renderer's (identical fields,
/// distinct crate types so neither crate depends on the other).
pub fn render_color(c: &pb_decode::ColorTransform) -> pb_render::ColorTransform {
    pb_render::ColorTransform {
        matrix: c.matrix,
        trc: c.trc,
        enabled: c.enabled,
    }
}

/// Whether a decoded image is HDR (scene-linear fp16 → the renderer's HDR path).
pub fn is_hdr(img: &DecodedImage) -> bool {
    img.format == PixelFormat::Rgba16F
}

/// Translate a video frame's YUV parameters into the renderer's (task 79.10) —
/// same crate-decoupling shim as [`render_color`].
pub fn render_yuv(c: &pb_decode::VideoColorInfo) -> pb_render::YuvParams {
    pb_render::YuvParams {
        matrix: match c.yuv_matrix {
            pb_decode::YuvMatrix::Bt601 => pb_render::YuvMatrix::Bt601,
            pb_decode::YuvMatrix::Bt709 => pb_render::YuvMatrix::Bt709,
            pb_decode::YuvMatrix::Bt2020 => pb_render::YuvMatrix::Bt2020,
        },
        full_range: c.full_range,
    }
}

/// Assemble the renderer's [`PlanarPresentation`](pb_render::PlanarPresentation)
/// for a planar (NV12 / P010) video frame (task #91 Phase 2): storage precision,
/// transfer, YUV matrix + range, primaries/parametric color, and the HDR peak —
/// all translated across the crate boundary. `format` is the frame's pixel format
/// (assumed planar; the caller dispatches on [`PixelFormat::is_planar_video`]).
pub fn render_planar_present(
    format: PixelFormat,
    color: &pb_decode::VideoColorInfo,
) -> pb_render::PlanarPresentation {
    pb_render::PlanarPresentation {
        format: match format {
            PixelFormat::P010 => pb_render::PlanarFormat::P010,
            _ => pb_render::PlanarFormat::Nv12,
        },
        transfer: match color.transfer {
            pb_decode::VideoTransfer::SrgbLike => pb_render::PlanarTransfer::SrgbLike,
            pb_decode::VideoTransfer::Parametric => pb_render::PlanarTransfer::Parametric,
            pb_decode::VideoTransfer::Pq => pb_render::PlanarTransfer::Pq,
            pb_decode::VideoTransfer::Hlg => pb_render::PlanarTransfer::Hlg,
        },
        yuv: render_yuv(color),
        color: render_color(&color.transform),
        peak: color.peak,
    }
}

/// The navigation direction for a nav [`Action`], or `None` for any non-nav action.
/// Bridges the central keymap vocabulary to the engine's `Nav` (used by the press
/// handler and `held_nav`).
pub fn nav_of(action: Action) -> Option<Nav> {
    match action {
        Action::Next | Action::SkipNext => Some(Nav::Forward),
        Action::Prev => Some(Nav::Backward),
        Action::Random => Some(Nav::Random),
        Action::RandomPrev => Some(Nav::RandomPrev),
        _ => None,
    }
}

/// A path shown relative to the scan root (forward-slashed), or its file name if
/// it isn't under the root.
pub fn rel_to_root(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    }
}

/// The info-panel metadata for `item`: its display path (root-relative for a real
/// file, the archive-relative entry name otherwise) plus the decoded dimensions
/// and codec.
pub fn meta_for(
    source: &dyn ItemSource,
    item: usize,
    root: &Path,
    img: &DecodedImage,
) -> PhotoMeta {
    let rel = match source.path(item) {
        Some(p) => rel_to_root(p, root),
        None => source.name(item).to_string(),
    };
    PhotoMeta {
        rel,
        w: img.orig_width,
        h: img.orig_height,
        // Only an archive door has one (FsSource resolves it on the scan worker); a
        // photo's size is never displayed, so nothing stats for it. Free here.
        size: source.size_hint(item),
        codec: img.codec,
        animated: img.animated,
        recovered: img.recovered.clone(),
    }
}

/// Resolve item `item`'s encoded bytes from `source` and decode them to fit. The
/// single decode entry point shared by the off-thread pool and the synchronous
/// (first-frame / resize / copy) paths, so a filesystem photo and a ZIP entry
/// decode through exactly the same routing. All reads are RAM-only.
pub fn decode_item(
    source: &dyn ItemSource,
    item: usize,
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    decode_item_cancellable(source, item, fit, allow_preview, &AtomicBool::new(false))
}

/// Whether this platform's decode layer implements the one-walk poster
/// selection (task #114). Windows (MF) only for now: the scheduler emits
/// selection wants only when true, so the other platforms keep the legacy
/// per-consumer poster walks bit-for-bit until the parity phase (plan §phases).
pub fn poster_select_supported() -> bool {
    cfg!(windows)
}

/// The phase-2 walk-variant lever (`PB_POSTER_WALK=native|fitted`): variant A
/// negotiates the selection reader at capped-native size (retaining the winner
/// for the phase-3 Original install), variant B keeps the fitted negotiation.
/// Default = fitted until the corpus A/B picks (plan §2; measure, don't guess).
pub fn poster_walk_native() -> bool {
    std::env::var("PB_POSTER_WALK")
        .map(|v| v == "native")
        .unwrap_or(false)
}

/// The pool's `PosterSelect` entry (task #114): the ONE scored walk for a video
/// item — chooses the frame, remembers the absolute locator, cuts the fitted
/// poster + thumb tile from the same winner on this worker. Only scheduled for
/// video items where [`poster_select_supported`] is true.
pub fn select_item(
    source: &dyn ItemSource,
    item: usize,
    fit: Option<FitBox>,
    display_class: bool,
    replay: Option<(i64, i64)>,
    cancel: &AtomicBool,
) -> Result<pb_decode::PosterSelection, DecodeError> {
    #[cfg(windows)]
    {
        let input = match source.path(item) {
            Some(p) => pb_decode::VideoInput::Path(p.to_path_buf()),
            // An archive entry: the container bytes are fetched into RAM for the
            // walk and dropped with the job (playback re-fetches its own Arc).
            None => pb_decode::VideoInput::Bytes {
                data: std::sync::Arc::new(
                    source
                        .bytes(item)
                        .map_err(|e| DecodeError::Corrupt(format!("read failed: {e}")))?,
                ),
                name: source.name(item).to_string(),
            },
        };
        let thumb_fit = crate::thumbs::thumb_fit();
        // A replay hint (phase 3) is the cheap path: decode-forward straight to
        // the already-chosen frame — one GOP at most, no scoring. A failed
        // replay (edited file, bad index) falls back to a fresh scored walk;
        // cancellation aborts outright.
        if let Some((origin, rel)) = replay {
            match pb_decode::decode_video_poster_replay(
                &input,
                origin,
                rel,
                fit,
                thumb_fit,
                display_class,
                cancel,
            ) {
                Ok(sel) => return Ok(sel),
                Err(e) if e.is_cancelled() => return Err(e),
                Err(_) => {} // fall through to the walk
            }
        }
        pb_decode::decode_video_poster_select(
            &input,
            fit,
            thumb_fit,
            poster_walk_native(),
            display_class,
            cancel,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (source, item, fit, display_class, replay, cancel);
        Err(DecodeError::Unsupported)
    }
}

/// The pool's purpose-aware decode entry (task #83): a `Thumb`-purpose request
/// first tries the cheap embedded-thumbnail extraction (the EXIF IFD1 JPEG —
/// header-parse only, no full decode) before falling through to the normal
/// routing; a `Display` request never does (its previews come from the
/// format backends' own `allow_preview` paths at display size).
pub fn decode_item_for(
    source: &dyn ItemSource,
    item: usize,
    fit: Option<FitBox>,
    allow_preview: bool,
    purpose: crate::decode_pool::Purpose,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    // Preview-first for big JPEGs (#106.5): a `Display` preview want shows the embedded
    // EXIF thumbnail from a bounded ~256 KB prefix read INSTANTLY, then the full decode
    // (the sharpen/`allow_preview = false` want) upgrades it in place — turning a 7 s black
    // screen over SMB into an instant blurry→sharp. JPEG only (HEIC/RAW already preview-first
    // via their backends; other formats embed no usable IFD1 thumbnail). Falls through to the
    // full decode when there is no thumbnail, the read fails, or it isn't a plain image.
    if allow_preview
        && purpose == crate::decode_pool::Purpose::Display
        && is_jpeg_name(source.name(item))
        && matches!(
            crate::video::item_kind(source, item),
            crate::video::LibraryItemKind::Image
        )
    {
        if let Ok(prefix) = source.bytes_prefix(item, PREVIEW_PREFIX_BYTES) {
            if let Some(img) = pb_decode::jpeg_preview_first(&prefix) {
                return Ok(img);
            }
        }
    }
    if purpose == crate::decode_pool::Purpose::Thumb {
        // Videos poster through the normal routing below (already thumb-friendly);
        // for images, a cheap embedded EXIF thumbnail beats any full decode.
        //
        // The guard is **positive on purpose**: this branch reads the item's whole
        // encoded bytes, so only a kind we know is an image may reach it. Phrased
        // negatively (`!= Video`) any future kind would opt *into* the read by
        // default — which is how a non-image item ends up fully read just to draw a
        // thumbnail.
        if matches!(
            crate::video::item_kind(source, item),
            crate::video::LibraryItemKind::Image
        ) {
            if let Ok(bytes) = source.bytes(item) {
                if let Some(img) = pb_decode::exif_thumbnail(&bytes) {
                    return Ok(img);
                }
                // No embedded thumb: decode the already-read bytes to the thumb
                // box (preview-friendly) rather than re-reading via decode_item.
                return decode_named_bytes(source.name(item), &bytes, fit, allow_preview);
            }
        }
    }
    decode_item_cancellable(source, item, fit, allow_preview, cancel)
}

/// [`decode_item`] with a mid-job cancel flag — what the decode pool runs, so a
/// superseded video **poster probe** (task #79 phase 2: an open reader walking
/// frames) stops between samples instead of finishing a walk nobody wants. Image
/// decodes ignore the flag (bounded, single-shot); the pool still cancels them at
/// the queue and discards stale results.
pub fn decode_item_cancellable(
    source: &dyn ItemSource,
    item: usize,
    fit: Option<FitBox>,
    allow_preview: bool,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let _ = cancel; // referenced only on windows/macOS today (the video poster path)
                    // Typed dispatch BEFORE any bytes request (task #79 phase 1): a
                    // filesystem video is streamed by the platform readers — its encoded
                    // bytes never enter RAM here. Phase 2: the poster is the clip's first
                    // non-black frame via the OS reader, rotation + color identical to the
                    // playback path; any failure (missing codec, no container handler,
                    // corrupt file) degrades to the flat placeholder tile — the item stays
                    // visible, and the *play* attempt is where a precise error surfaces.
    match crate::video::item_kind(source, item) {
        crate::video::LibraryItemKind::Video(container) => {
            // Task #114 phase 1e (owner feedback): a PREVIEW want for a video is
            // the instant flat tile (task #79's placeholder — zero I/O), marked
            // `is_preview` so the selection's fitted poster upgrades it in place
            // exactly like a photo's blurry→sharp. Landing on a film — or the
            // sync first paint of a movie folder — never blocks on the
            // multi-second walk. Selection platforms only: the legacy platforms
            // keep walking here so their behavior is unchanged until parity.
            if allow_preview && poster_select_supported() {
                let mut img = video_placeholder(container);
                img.is_preview = true;
                return Ok(img);
            }
            #[cfg(any(windows, target_os = "macos"))]
            if let Some(path) = source.path(item) {
                match pb_decode::decode_video_poster(path, fit, cancel) {
                    Ok(img) => return Ok(img),
                    Err(e) => {
                        // macOS + ffvideo (task #84 §8, owner keep-both decision):
                        // AVFoundation stays primary; FFmpeg posters only what it
                        // refuses (MKV/WebM/… — the same split as playback).
                        #[cfg(all(target_os = "macos", feature = "ffvideo"))]
                        {
                            let input = pb_decode::VideoInput::Path(path.to_path_buf());
                            if let Ok(img) = pb_decode::ff_decode_video_poster(&input, fit, cancel)
                            {
                                return Ok(img);
                            }
                        }
                        // A cancelled walk (superseded thumb/prefetch job) is routine
                        // churn, not a failure — over SMB it floods the console.
                        if !e.is_cancelled() {
                            eprintln!("video poster failed: {}: {e}", path.display());
                        }
                    }
                }
            }
            // macOS archive entry with a container AVFoundation can't open: the Swift
            // AVAssetImageGenerator round-trip below would never produce a poster —
            // FFmpeg posters it in-process from the entry's bytes (task #84 §8).
            #[cfg(all(target_os = "macos", feature = "ffvideo"))]
            if source.path(item).is_none() && !container.macos_native() {
                match source.bytes(item) {
                    Ok(data) => {
                        let input = pb_decode::VideoInput::Bytes {
                            data: std::sync::Arc::new(data),
                            name: source.name(item).to_string(),
                        };
                        match pb_decode::ff_decode_video_poster(&input, fit, cancel) {
                            Ok(img) => return Ok(img),
                            Err(e) if !e.is_cancelled() => {
                                eprintln!("video poster failed: {}: {e}", source.name(item))
                            }
                            Err(_) => {}
                        }
                    }
                    Err(e) => eprintln!("video poster read failed: {}: {e}", source.name(item)),
                }
            }
            // Linux (task #84): the FFmpeg poster covers path and archive-byte items
            // alike (one in-process reader; the thumbs strip gets these for free).
            #[cfg(all(unix, not(target_os = "macos"), feature = "ffvideo"))]
            {
                let input = match source.path(item) {
                    Some(p) => Some(pb_decode::VideoInput::Path(p.to_path_buf())),
                    None => match source.bytes(item) {
                        Ok(data) => Some(pb_decode::VideoInput::Bytes {
                            data: std::sync::Arc::new(data),
                            name: source.name(item).to_string(),
                        }),
                        Err(e) => {
                            eprintln!("video poster read failed: {}: {e}", source.name(item));
                            None
                        }
                    },
                };
                if let Some(input) = input {
                    match pb_decode::ff_decode_video_poster(&input, fit, cancel) {
                        Ok(img) => return Ok(img),
                        Err(e) if !e.is_cancelled() => {
                            eprintln!("video poster failed: {}: {e}", source.name(item))
                        }
                        Err(_) => {}
                    }
                }
            }
            // An archive entry (no path): Windows posters it from the entry's in-RAM
            // bytes through the same MF reader configuration — a transient fetch this
            // decode worker drops after the poster (playback re-fetches and holds one
            // Arc for its session).
            #[cfg(windows)]
            if source.path(item).is_none() {
                match source.bytes(item) {
                    Ok(data) => {
                        let input = pb_decode::VideoInput::Bytes {
                            data: std::sync::Arc::new(data),
                            name: source.name(item).to_string(),
                        };
                        match pb_decode::decode_video_poster_input(&input, fit, cancel) {
                            Ok(img) => return Ok(img),
                            Err(e) => {
                                if !e.is_cancelled() {
                                    eprintln!("video poster failed: {}: {e}", source.name(item));
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("video poster read failed: {}: {e}", source.name(item)),
                }
            }
            // macOS archive entry: the poster is generated by the Swift shell (AVFoundation
            // can't build an AVAsset from bytes in Rust) and fed back into the ring as a
            // preview→full upgrade — so mark this placeholder a *preview* to make that upgrade
            // land in place (`drain_results`). See .taskmaster/plans/macos-archive-video-posters.md.
            #[cfg(target_os = "macos")]
            if source.path(item).is_none() {
                let mut ph = video_placeholder(container);
                ph.is_preview = true;
                return Ok(ph);
            }
            return Ok(video_placeholder(container));
        }
        // An archive door (task #104): return the tile **here**, above the bytes
        // request below. That placement is the feature's whole guarantee — a door
        // in the prefetch window costs a solid-colour tile, never a decompression,
        // so scrubbing past a folder of 2 GB archives touches none of them. The
        // archive is read only when the viewer presses `P` to enter it.
        crate::video::LibraryItemKind::Archive(kind) => return Ok(archive_placeholder(kind)),
        // Exhaustive on purpose: everything below this match reads the item's whole
        // encoded bytes, so only a kind we know is an image may fall through to it.
        // A new kind gets a compile error here rather than a silent full read.
        crate::video::LibraryItemKind::Image => {}
    }
    // PB_PERF (#106.2): split the two costs a decode actually pays — reading the encoded
    // bytes (for a ZIP over SMB, a fresh 39 MB network read; the byte cache (#106.1) will
    // target exactly this) versus decoding them. The 7 s first-photo is one of these; this
    // says which. Gated, so it prints nothing in a normal run.
    let perf = crate::perf::env_enabled();
    let t_read = perf.then(Instant::now);
    let bytes = source
        .bytes(item)
        .map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    let read_ms = t_read.map(|t| t.elapsed());
    let t_dec = perf.then(Instant::now);
    let mut img = decode_named_bytes(source.name(item), &bytes, fit, allow_preview)?;
    if let (Some(r), Some(t)) = (read_ms, t_dec) {
        eprintln!(
            "[perf] item {item}: read {} KB in {} ms, decode in {} ms",
            bytes.len() / 1024,
            r.as_millis(),
            t.elapsed().as_millis()
        );
    }
    // Cheap header sniff so the viewer knows an on-demand animation is available
    // (the ▶ P hint / `P` to play). Off the keypress path — this runs in the decode
    // worker (or the sync first-paint), never on the event loop. The pixels stay the
    // still first frame; only `decode_animation` (on `P`) decodes the whole sequence.
    img.animated = pb_decode::detect_animation(&bytes);
    Ok(img)
}

/// The phase-1 stand-in a video item displays: a small flat dark frame (16:9) whose
/// codec row names the container. Deliberately tiny — it's a solid color, so the GPU
/// upscale is invisible, and prefetch can hold many without denting the ring budget.
/// Phase 2 (posters) replaces this with the clip's first non-black frame; the reported
/// dimensions become real then too.
pub fn video_placeholder(container: crate::video::VideoContainer) -> DecodedImage {
    const W: u32 = 320;
    const H: u32 = 180;
    // A near-black neutral: visibly "an item", clearly not a decoded photo.
    let pixels: Vec<u8> = [24u8, 24, 26, 255].repeat((W * H) as usize);
    DecodedImage {
        width: W,
        height: H,
        orig_width: W,
        orig_height: H,
        codec: container.name(),
        format: PixelFormat::Rgba8,
        pixels,
        is_preview: false,
        color: pb_decode::ColorTransform::srgb(),
        peak: 1.0,
        animated: None,
        recovered: None,
    }
}

/// The **door artwork**: a zippered folder with photos peeking out, drawn by the owner
/// (2026-07-17) and shipped per platform to match the OS's own folder colour —
/// manila on Windows (Explorer), blue everywhere else (Finder, and GNOME Adwaita /
/// KDE Breeze both lean blue too).
///
/// A viewer only ever sees one, so the split costs nothing in consistency — unlike
/// keying colour off the *format*, which would have put `.cbz` and `.cbr` (both
/// comics) in different colours for a reason nothing on screen explains.
#[cfg(windows)]
const DOOR_ART: &[u8] = include_bytes!("../assets/folder-zip-yellow.webp");
#[cfg(not(windows))]
const DOOR_ART: &[u8] = include_bytes!("../assets/folder-zip-blue.webp");

/// The door artwork, decoded once for the process — **for the shells to draw**, not for
/// the deck.
///
/// Each shell uploads this to one cached texture (egui ctx / `CoreModel`) and draws it
/// in the door card. It is deliberately *not* an item's frame: a door is chrome, and
/// pushing artwork through the photo pipeline is what produced a 12× upscaled glyph, a
/// photo-sized ring slot for an icon, an invented grey backdrop, and a 2.1×
/// magnification, in that order (task #105).
///
/// One image serves every archive kind, so the WebP decode runs exactly once. `None` if
/// it can't be decoded — the card degrades to text and a button rather than vanishing.
///
/// **Cropped to its content.** The asset is a 1024² square with generous whitespace the
/// owner left for the drop shadow; the folder itself is only ~814×780 of it. Handed out
/// uncropped, that margin *is* invisible layout: the card's own padding lands outside it,
/// so the folder floats in a sea of space no one asked for and no constant explains
/// (owner, 2026-07-17: "way too much margin between the folder graphic and the filename").
/// Cropping here means a shell's padding means what it says — and it fixes both shells at
/// once, rather than each guessing the same magic inset.
///
/// The crop keeps every pixel of real shadow (see [`INK_ALPHA`] — the threshold is the
/// whole trick) and stays centred on the folder, never on the ink. Result: 894×860, i.e.
/// the folder went from 80% of the frame's width to **91%**, which is 14% more folder at
/// the same size cap and most of the vertical air gone.
pub fn door_artwork() -> Option<&'static DecodedImage> {
    use std::sync::OnceLock;
    static ART: OnceLock<Option<DecodedImage>> = OnceLock::new();
    ART.get_or_init(|| {
        let art = decode_named_bytes("door.webp", DOOR_ART, None, false).ok()?;
        Some(crop_to_content(&art).unwrap_or(art))
    })
    .as_ref()
}

/// The "can't display this image" placeholder artwork (task #127): a Polaroid bursting
/// into flames, for the decode-error card. Same decode-once + straight-alpha convention
/// as [`door_artwork`] — one WebP decode for the process, handed to every shell as a
/// cached texture/`NSImage`. `None` if it can't be decoded, in which case the card falls
/// back to its SF-symbol glyph. Not cropped: the smoke wisp is faint (low-alpha) content
/// an ink-threshold crop would eat, and the card centres the art in the layout instead.
const FIRE_ART: &[u8] = include_bytes!("../assets/photo-on-fire.webp");

pub fn decode_error_artwork() -> Option<&'static DecodedImage> {
    use std::sync::OnceLock;
    static ART: OnceLock<Option<DecodedImage>> = OnceLock::new();
    ART.get_or_init(|| decode_named_bytes("photo-on-fire.webp", FIRE_ART, None, false).ok())
        .as_ref()
}

/// The alpha a pixel needs before it counts as **ink**.
///
/// 🪤 Not `1`, and this is load-bearing: the asset is **lossy** WebP, and its alpha channel
/// carries a 1–3/255 haze over the *entire* square. At `>= 1` the ink box is the whole
/// 1023×1024 image, so the crop below found "already tight" and returned `None` — it did
/// **nothing**, silently, for as long as it has existed. That invisible ~1% haze is what
/// kept ~10% of dead margin a side in the artwork, which is why the folder looked small
/// inside its box and why the card's own padding never meant what it said.
///
/// `4` is the smallest value that clears the haze (measured, `examples/door_bbox.rs`: the
/// box collapses from 1017×1020 at `>= 2` to 863×822 at `>= 4`, and barely moves after),
/// so every pixel of *real* shadow survives. Re-run that example if the asset is ever
/// re-exported — and note a **lossless** export would make this whole problem go away.
const INK_ALPHA: u8 = 4;

/// Crop `img` to its content, **centred on the solid subject**.
///
/// Two bounding boxes, deliberately: the *ink* (the folder plus its soft drop shadow — see
/// [`INK_ALPHA`]) and the *subject* (near-opaque — the folder alone). Cropping to the ink
/// box alone looks wrong, and subtly: the shadow is cast down and to one side, so that box
/// is off-centre about the folder and a perfectly centred box then renders a visibly
/// off-centre folder — which is exactly what the owner saw ("the image doesn't even appear
/// centered"), and warned about again when this crop was fixed.
///
/// So the crop keeps every pixel of ink but sits **symmetric about the subject's centre**:
/// the folder lands dead centre, and the shadow survives in whatever room that leaves. The
/// mirror is why a one-sided shadow still costs margin on both sides — that is the price of
/// centring the folder, and it is the right trade.
///
/// `None` if the image is fully transparent, or already tight.
fn crop_to_content(img: &DecodedImage) -> Option<DecodedImage> {
    let (w, h) = (img.width as usize, img.height as usize);
    let bbox = |min_alpha: u8| -> Option<(usize, usize, usize, usize)> {
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if img.pixels[(y * w + x) * 4 + 3] >= min_alpha {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x0 <= x1 && y0 <= y1).then_some((x0, y0, x1, y1))
    };
    let (ix0, iy0, ix1, iy1) = bbox(INK_ALPHA)?; // ink: folder + shadow
    let (sx0, sy0, sx1, sy1) = bbox(200).unwrap_or((ix0, iy0, ix1, iy1)); // subject: the folder

    // Half-extents that reach every ink pixel from the subject's centre, mirrored so the
    // subject stays centred. Integer centres via doubled coordinates (no half-pixel drift).
    let (cx2, cy2) = (sx0 + sx1, sy0 + sy1);
    let half_x = (cx2 / 2 - ix0).max(ix1 - cx2.div_ceil(2));
    let half_y = (cy2 / 2 - iy0).max(iy1 - cy2.div_ceil(2));
    let x0 = (cx2 / 2).saturating_sub(half_x);
    let y0 = (cy2 / 2).saturating_sub(half_y);
    let x1 = (cx2.div_ceil(2) + half_x).min(w - 1);
    let y1 = (cy2.div_ceil(2) + half_y).min(h - 1);
    if x0 > x1 || y0 > y1 {
        return None;
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    if cw == w && ch == h {
        return None; // already tight
    }
    let mut pixels = Vec::with_capacity(cw * ch * 4);
    for y in y0..=y1 {
        let row = (y * w + x0) * 4;
        pixels.extend_from_slice(&img.pixels[row..row + cw * 4]);
    }
    Some(DecodedImage {
        width: cw as u32,
        height: ch as u32,
        orig_width: cw as u32,
        orig_height: ch as u32,
        pixels,
        ..img.clone()
    })
}

/// The frame an archive **door** presents: a 1×1 **fully transparent** sentinel.
///
/// It draws *nothing*. The scene pass clears to the configured letterbox colour
/// (`pb_render::gpu`'s `letterbox_linear(self.letterbox)`, default
/// `LETTERBOX = [10,10,12,255]`) and draws the photo quad with
/// `BlendState::ALPHA_BLENDING`, so `a = 0` leaves the letterbox exactly as it was —
/// proven by `pb-render`'s `transparent_image_blends_over_letterbox`. The real door is
/// the shell's card (task #105); this exists only so the item still occupies a ring
/// slot, which keeps the ring, present and nav paths untouched — no "an item with no
/// texture" case to plumb.
///
/// Returned by [`decode_item_cancellable`] **above** its `source.bytes()` request,
/// which is the door's whole contract: the archive is read only when the viewer presses
/// `P`. This is now the cheapest possible slot — 4 bytes.
///
/// ⚠ **1×1 is only safe because a door never reports dimensions.** `orig_width/height`
/// flow into `PhotoMeta` and out to the info line / Details / Copy Details; all three
/// show the archive's **size** instead (`PhotoMeta::size`). Print `1 × 1` anywhere and
/// this is a bug, not a tile to resize.
pub fn archive_placeholder(kind: pb_source::ArchiveKind) -> DecodedImage {
    DecodedImage {
        width: 1,
        height: 1,
        orig_width: 1,
        orig_height: 1,
        codec: kind.name(),
        format: PixelFormat::Rgba8,
        pixels: vec![0, 0, 0, 0],
        is_preview: false,
        color: pb_decode::ColorTransform::srgb(),
        peak: 1.0,
        animated: None,
        recovered: None,
    }
}

/// The off-thread decode for an on-demand motion sequence (tasks #37 / #38 / #39): a
/// Live Photo's companion `.mov` when `live` is set, otherwise the item's own bytes as a
/// multi-frame animation. Both return a unified [`pb_decode::Animation`] so playback
/// treats them identically. Since task #69 the `live` branch is **dead on every platform**:
/// Linux, macOS, and Windows all divert Live Photos to the streaming path
/// (`start_live_stream`) before this job is ever spawned. It's kept for the batch
/// `decode_live_motion` wrappers (still exercised by the `live_probe` example) and as a
/// safety fallback; the animation (GIF/APNG/WebP/HEIF sequence) branch below is the live one.
pub fn decode_motion_job(
    live: Option<PathBuf>,
    source: &Arc<dyn ItemSource>,
    item: usize,
    fit: Option<FitBox>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<pb_decode::Animation, DecodeError> {
    #[cfg(any(
        target_os = "macos",
        windows,
        all(unix, not(target_os = "macos"), feature = "livephoto")
    ))]
    if let Some(path) = &live {
        // Cap the motion's long edge to the display fit, but never above the RAM ceiling.
        let edge = fit
            .map(|f| f.max_width.max(f.max_height))
            .unwrap_or(MOTION_MAX_LONG_EDGE)
            .min(MOTION_MAX_LONG_EDGE);
        // Linux's FFmpeg decoder is cancellable (navigating away stops it mid-clip). The
        // macOS/Windows batch wrappers ignore the flag — but on all three platforms this
        // branch is only a compatibility fallback now (streaming handles Live Photos before
        // this job spawns).
        #[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
        return pb_decode::decode_live_motion_cancellable(path, edge, cancel);
        #[cfg(any(target_os = "macos", windows))]
        {
            let _ = cancel; // batch wrappers don't check the flag mid-clip
            return pb_decode::decode_live_motion(path, edge);
        }
    }
    // On platforms without a motion decoder `live` is always `None` and there's nothing to
    // cancel; acknowledge both so the parameters aren't flagged unused.
    #[cfg(not(any(
        target_os = "macos",
        windows,
        all(unix, not(target_os = "macos"), feature = "livephoto")
    )))]
    {
        let _ = &live;
        let _ = cancel;
    }
    let bytes = match source.bytes(item) {
        Ok(b) => b,
        Err(e) => return Err(DecodeError::Corrupt(format!("read error: {e}"))),
    };
    // An animated ISOBMFF sequence (animated AVIF `avis` / HEIC `msf1`) with a real file path
    // decodes via the FFmpeg video pipeline on Linux — reusing the decoder already linked for
    // Live Photo motion. (Windows plays `avis` via the dav1d backend inside `decode_animation`
    // itself — task #76; macOS via Image I/O.) Archive entries (no path) fall through to
    // `decode_animation`, which reports what it can't play as unsupported, gracefully.
    #[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
    if pb_decode::detect_animation(&bytes) == Some(pb_decode::AnimationKind::Heif) {
        if let Some(path) = source.path(item) {
            let edge = fit
                .map(|f| f.max_width.max(f.max_height))
                .unwrap_or(MOTION_MAX_LONG_EDGE)
                .min(MOTION_MAX_LONG_EDGE);
            return pb_decode::decode_image_sequence_cancellable(path, edge, cancel);
        }
    }
    // Cancellable (task #76): navigating away mid-decode stops the worker within
    // ~a frame instead of orphaning a multi-second AV1 batch decode.
    pb_decode::decode_animation_cancellable(&bytes, fit, cancel)
}

/// Whether an EXIF `(tag, value)` is a binary blob better left out of the panel —
/// Apple's MakerNote/Padding render as kilobytes of hex, and any value that long
/// is binary noise, not human-readable metadata.
pub fn is_exif_blob(tag: &str, value: &str) -> bool {
    matches!(tag, "MakerNote" | "Padding") || value.len() > 256
}

/// Truncate an over-long EXIF value to `EXIF_VALUE_MAX` characters with an
/// ellipsis (counted in chars, so multibyte values aren't split mid-codepoint).
pub fn truncate_exif_value(value: &str) -> String {
    if value.chars().count() <= EXIF_VALUE_MAX {
        value.to_string()
    } else {
        let mut s: String = value.chars().take(EXIF_VALUE_MAX).collect();
        s.push('…');
        s
    }
}

/// A copy of `rgba` with its alpha channel scaled by `factor` (clamped 0..=1).
pub fn scale_alpha(rgba: &[u8], factor: f32) -> Vec<u8> {
    let f = factor.clamp(0.0, 1.0);
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        px[3] = (px[3] as f32 * f).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// The file name of a path-like source name (strips any directory prefix).
pub fn file_name_of(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

/// A human-readable decode-failure reason for the "can't display this image"
/// placeholder and the Details "Error" row (task #127). Strips our `DecodeError`
/// `Display` wrapper (`corrupt image: "…"`) and surrounding quotes so the user sees
/// the bare cause (e.g. `No more bytes`), never internal formatting.
pub fn clean_decode_reason(e: &DecodeError) -> String {
    let s = match e {
        DecodeError::Corrupt(s) => s.clone(),
        other => other.to_string(),
    };
    s.trim().trim_matches('"').trim().to_string()
}

/// Whether physical-px point `(x, y)` lies within `[x0, y0, x1, y1]` (inclusive) — the
/// overlay click hit-test (the scan-count chip today; EXIF copy buttons later).
pub fn point_in_rect([x0, y0, x1, y1]: [f32; 4], x: f32, y: f32) -> bool {
    x >= x0 && x <= x1 && y >= y0 && y <= y1
}

/// The window-title string for a photo: `name (idx/n)`.
pub fn title_for(name: &str, idx: usize, n: usize) -> String {
    format!("{} ({}/{n})", file_name_of(name), idx + 1)
}

/// The playlist cursor after removing item `removed` from a list of `len`, or `None` when the
/// list becomes empty. Stays on the *next* photo (same index) except when the **last** item was
/// removed, where it falls back to the new last index (the **previous** photo). Pure.
pub fn cursor_after_removal(len: usize, removed: usize) -> Option<usize> {
    if len <= 1 {
        return None;
    }
    let new_len = len - 1;
    Some(removed.min(new_len - 1))
}

/// The frame-step direction encoded by an action: `+1` next / `-1` previous / `0`
/// for anything else.
pub fn frame_step_dir(action: Action) -> i32 {
    match action {
        Action::FrameNext => 1,
        Action::FramePrev => -1,
        _ => 0,
    }
}

/// Map the persisted scale-mode preference to the renderer's [`ScaleMode`].
pub fn scale_mode_of(p: ScaleModePref) -> ScaleMode {
    match p {
        ScaleModePref::Fit => ScaleMode::Fit,
        ScaleModePref::Fill => ScaleMode::Fill,
        ScaleModePref::Original => ScaleMode::Original,
    }
}

/// The folder the Open dialog should start in.
///
/// Priority: a user-pinned `fixed` folder (the Settings preference) wins outright. Else,
/// for an **archive** source, the folder that *contains* the archive — never the archive
/// itself: the OS file dialog can't browse inside a `.zip`/`.7z`, and an *encrypted* one
/// errors outright ("Windows cannot open the folder…"). Else the current photo's folder
/// (the scanned folder, then the display root). When all of those are empty — a fresh
/// launch with nothing open — `last` (the remembered `settings::last_folder`) starts the
/// dialog back in the user's library, and only then `fallback` (the user's Pictures/
/// home), so the dialog never falls back to the *OS's* own last-folder memory (a
/// privacy trace).
pub fn picker_start_dir(
    fixed: Option<&Path>,
    container: Option<&Path>,
    scan_root: Option<&Path>,
    root: &Path,
    last: Option<&Path>,
    fallback: &Path,
) -> PathBuf {
    let non_empty = |p: Option<&Path>| {
        p.filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    if let Some(dir) = non_empty(fixed) {
        return dir;
    }
    if let Some(archive) = container {
        return non_empty(archive.parent()).unwrap_or_else(|| fallback.to_path_buf());
    }
    non_empty(scan_root)
        .or_else(|| non_empty(Some(root)))
        .or_else(|| non_empty(last))
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// A safe default folder for the Open dialog when nothing else applies (a bare launch):
/// the user's Pictures folder if it exists, else their home, else the current directory.
/// Used so the dialog always opens somewhere real instead of letting Windows fall back to
/// its remembered last folder.
pub fn default_picker_dir() -> PathBuf {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        let home = PathBuf::from(home);
        let pics = home.join("Pictures");
        if pics.is_dir() {
            return pics;
        }
        if home.is_dir() {
            return home;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// sRGB OETF (scene-linear → sRGB-encoded), matching the present shader's
/// `srgb_oetf` in `pb-render/src/gpu.rs`.
pub fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Extended-Reinhard tone-map with white point `lw`, matching the present shader's
/// `reinhard`. `lw = 1` is the identity (faithful SDR); a larger `lw` rolls HDR
/// highlights into the displayable range.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

/// Convert a decoded image to a straight-alpha RGBA8 buffer for the clipboard.
///
/// - `Rgba8` is taken as-is (source-encoded sRGB). The DIB clipboard format carries
///   no ICC profile, so a wide-gamut source pastes interpreted as sRGB — a
///   documented v1 limitation.
/// - `Rgba16F` (HDR scene-linear scRGB) is tone-mapped to SDR sRGB8 exactly as the
///   SDR present pass does: extended-Reinhard at the image `peak`, then sRGB-encode.
pub fn to_clipboard_rgba8(img: &DecodedImage) -> Vec<u8> {
    match img.format {
        PixelFormat::Rgba8 => img.pixels.clone(),
        PixelFormat::Rgba16F => {
            let lw = img.peak.max(1.0);
            let px_count = (img.width as usize) * (img.height as usize);
            let mut out = Vec::with_capacity(px_count * 4);
            // 4 half-floats (8 bytes) per pixel, little-endian.
            for px in img.pixels.chunks_exact(8) {
                for ch in 0..3 {
                    let h = half::f16::from_le_bytes([px[ch * 2], px[ch * 2 + 1]]);
                    let v = srgb_oetf(reinhard(h.to_f32(), lw));
                    out.push((v * 255.0 + 0.5) as u8);
                }
                out.push(255); // opaque; HDR sources have no meaningful alpha here
            }
            out
        }
        // Stills never carry a planar video format (NV12/P010 are video-session
        // frame formats — task 79.10 / #91); an empty buffer keeps this total
        // without inventing YUV params.
        f if f.is_planar_video() => Vec::new(),
        _ => Vec::new(),
    }
}

/// Rotate a tightly-packed RGBA8 buffer by a 90° quadrant (clockwise), returning the
/// rotated buffer and its new dimensions. `R0` clones unchanged. Used to bake the
/// in-RAM rotation override (the `r` / `Shift+R` overlay transform, which is a GPU
/// transform — not baked into the decoded pixels) into the copied image so the
/// clipboard is WYSIWYG.
pub fn rotate_rgba8(pixels: &[u8], w: u32, h: u32, rot: Rotation) -> (Vec<u8>, u32, u32) {
    if rot == Rotation::R0 {
        return (pixels.to_vec(), w, h);
    }
    let (wu, hu) = (w as usize, h as usize);
    let (new_w, new_h) = if rot.swaps_axes() { (h, w) } else { (w, h) };
    let nwu = new_w as usize;
    let mut out = vec![0u8; wu * hu * 4];
    for sy in 0..hu {
        for sx in 0..wu {
            // Destination pixel coordinates for this source pixel after the turn.
            let (dx, dy) = match rot {
                Rotation::R90 => (hu - 1 - sy, sx),
                Rotation::R180 => (wu - 1 - sx, hu - 1 - sy),
                Rotation::R270 => (sy, wu - 1 - sx),
                Rotation::R0 => unreachable!(),
            };
            let src = (sy * wu + sx) * 4;
            let dst = (dy * nwu + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    (out, new_w, new_h)
}

/// The QuickTime motion component paired with a Live Photo still: a sibling file with
/// the **same stem** and a `.mov`/`.qt` extension — Apple's on-disk convention
/// (`IMG_1234.HEIC` + `IMG_1234.MOV`). Filename pairing is fast (one `stat`, no metadata
/// read) and matches the export layout; a content-identifier cross-check to reject a
/// coincidental name collision is a possible refinement (task #38). Returns the motion
/// path if such a sibling exists on disk. Archive entries have no filesystem path, so
/// they never pair here (Live-Photos-in-archives is out of scope for v1).
///
/// Only where a motion decoder exists (AVFoundation on macOS, Media Foundation on
/// Windows) — elsewhere a pairing would have no consumer and be flagged unused.
#[cfg(any(
    target_os = "macos",
    windows,
    all(unix, not(target_os = "macos"), feature = "livephoto")
))]
pub fn companion_motion(still: &Path) -> Option<PathBuf> {
    let dir = still.parent()?;
    let stem = still.file_stem()?;
    for ext in ["mov", "MOV", "qt", "QT"] {
        let mut cand = dir.join(stem);
        cand.set_extension(ext);
        if cand != still && cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hold-to-zoom/pan ease-in (#67): 0 at press, 1 at the ramp end, and a
    /// quadratic middle (a tap barely moves) — clamped past the ramp, monotonic.
    #[test]
    fn hold_ramp_eases_in_quadratically() {
        assert_eq!(hold_ramp(0.0, 0.9), 0.0);
        assert!((hold_ramp(0.9, 0.9) - 1.0).abs() < 1e-6);
        assert!(
            (hold_ramp(1.8, 0.9) - 1.0).abs() < 1e-6,
            "clamps past the ramp"
        );
        // Half-way through the ramp → a quarter of the range (p² = 0.25), NOT half
        // (the linear value) — that's the fine-control zone.
        assert!((hold_ramp(0.45, 0.9) - 0.25).abs() < 1e-4);
        // Eased is always ≤ linear during the ramp (gentler start), and monotonic.
        let mut prev = -1.0;
        for i in 0..=10 {
            let t = i as f32 / 10.0 * 0.9;
            let e = hold_ramp(t, 0.9);
            assert!(e >= prev, "monotonic non-decreasing");
            assert!(e <= (t / 0.9) + 1e-6, "eased ≤ linear");
            prev = e;
        }
    }

    /// A zero-length tick must latch the clock without slowing the glide, or a burst of
    /// same-instant ticks would decay the throw for free.
    #[test]
    fn pan_inertia_decay_is_neutral_on_a_zero_tick() {
        assert_eq!(pan_inertia_decay(0.0), 1.0);
        assert_eq!(
            pan_inertia_decay(-1.0),
            1.0,
            "a negative dt is treated as zero"
        );
    }

    /// Frame-rate independence is the whole point: decaying once over 100 ms must equal
    /// decaying ten times over 10 ms, or the glide would depend on the refresh rate.
    #[test]
    fn pan_inertia_decay_is_frame_rate_independent() {
        let one_shot = pan_inertia_decay(0.1);
        let stepped = (0..10).fold(1.0_f32, |v, _| v * pan_inertia_decay(0.01));
        assert!(
            (one_shot - stepped).abs() < 1e-4,
            "one 100ms step ({one_shot}) should match ten 10ms steps ({stepped})"
        );
    }

    /// The borrowed UIKit rate: ~500 ms time constant, so roughly 1/e of the throw survives
    /// half a second. This pins the *feel* against the trackpad, which is why it's borrowed
    /// rather than tuned.
    #[test]
    fn pan_inertia_decay_matches_the_uikit_time_constant() {
        let after_500ms = pan_inertia_decay(0.5);
        let one_over_e = 1.0_f32 / std::f32::consts::E;
        assert!(
            (after_500ms - one_over_e).abs() < 0.02,
            "expected ~{one_over_e} of the velocity left after 500ms, got {after_500ms}"
        );
        assert!(
            pan_inertia_decay(2.0) < 0.02,
            "a throw is spent within a couple of seconds"
        );
    }

    /// Zoom settles faster than pan, deliberately: a pinch aims at a size, so a long glide
    /// would overshoot it. If this ever inverts, the feel rationale has been lost.
    #[test]
    fn zoom_inertia_settles_faster_than_pan() {
        assert_eq!(zoom_inertia_decay(0.0), 1.0);
        assert!(
            zoom_inertia_decay(0.25) < pan_inertia_decay(0.25),
            "a zoom throw must decay quicker than a pan throw"
        );
    }

    /// Friction must bring the glide to a *definite* stop and never reverse it. The
    /// asymptotic tail of pure exponential decay is exactly what made the first zoom glide
    /// overrun, so this pins the fix.
    #[test]
    fn zoom_inertia_step_stops_dead_and_never_reverses() {
        for launch in [ZOOM_FLING_MAX_SPEED, -ZOOM_FLING_MAX_SPEED] {
            let mut v = launch;
            let mut ticks = 0;
            while v != 0.0 && ticks < 1000 {
                let next = zoom_inertia_step(v, 0.008); // ~120 Hz
                assert!(next.abs() <= v.abs(), "friction must never speed it up");
                assert!(
                    next == 0.0 || next.signum() == launch.signum(),
                    "friction must never reverse the direction"
                );
                v = next;
                ticks += 1;
            }
            assert_eq!(v, 0.0, "the glide must reach zero, not merely approach it");
            assert!(
                ticks < 40,
                "a throw should be spent in well under a third of a second (got {ticks} ticks)"
            );
        }
    }

    /// The eased scroll-zoom step fraction. Zero at `dt == 0` (first tick
    /// latches only), in `(0, 1)` for a normal frame, and toward `1` for a long frame — always
    /// frame-rate independent and monotonic in `dt`.
    #[test]
    fn zoom_ease_alpha_is_bounded_and_monotonic() {
        assert_eq!(
            zoom_ease_alpha(0.0),
            0.0,
            "a zero-length tick moves nothing"
        );
        assert_eq!(
            zoom_ease_alpha(-1.0),
            0.0,
            "a negative dt is treated as zero"
        );
        let a = zoom_ease_alpha(0.008); // ~120 Hz
        assert!(
            a > 0.0 && a < 1.0,
            "a normal frame closes a fraction (got {a})"
        );
        assert!(
            zoom_ease_alpha(1.0) > 0.99,
            "a long frame lands near the target rather than crawling"
        );
        let (mut prev, mut dt) = (0.0, 0.001);
        while dt <= 0.5 {
            let cur = zoom_ease_alpha(dt);
            assert!(cur >= prev, "monotonic non-decreasing in dt");
            prev = cur;
            dt += 0.001;
        }
    }

    /// Driving the same exponential the ease uses in log-space (independent of any GPU geometry):
    /// a 2× zoom target must converge to within `ZOOM_EASE_EPS` in a reasonable number of
    /// ~120 Hz frames, and never overshoot — the property `apply_zoom_ease` relies on to finish.
    #[test]
    fn zoom_ease_converges_within_bounded_frames() {
        let target = 2.0_f32;
        let mut zoom = 1.0_f32;
        let mut frames = 0;
        loop {
            let ratio = target / zoom;
            if (ratio - 1.0).abs() <= ZOOM_EASE_EPS {
                break;
            }
            let step = ratio.powf(zoom_ease_alpha(0.008));
            zoom *= step;
            assert!(
                zoom <= target + 1e-6,
                "never overshoots the target (got {zoom})"
            );
            frames += 1;
            assert!(frames < 120, "must converge well under a second of frames");
        }
        assert!(
            (zoom - target).abs() < 0.01,
            "lands on the target (got {zoom})"
        );
    }

    /// Task #94.2 resume policy: remember only a position meaningfully into a
    /// long-enough clip and not near the end, rewound a touch for context.
    #[test]
    fn video_resume_target_policy() {
        let s = Duration::from_secs;
        // Deep in a long clip → resume, rewound by RESUME_REWIND.
        assert_eq!(video_resume_target(s(600), s(7200)), Some(s(598)));
        // In the first RESUME_MIN → don't bother (start over).
        assert_eq!(video_resume_target(s(3), s(7200)), None);
        assert_eq!(video_resume_target(RESUME_MIN, s(7200)), Some(s(3)));
        // Within RESUME_END_GUARD of the end → don't bother (credits / about to loop).
        assert_eq!(video_resume_target(s(7196), s(7200)), None);
        assert_eq!(video_resume_target(s(7194), s(7200)), Some(s(7192)));
        // Too-short clip (≤ end guard) → never resume.
        assert_eq!(video_resume_target(s(4), s(5)), None);
        // Rewind never underflows below 0.
        assert_eq!(video_resume_target(s(6), s(60)), Some(s(4)));
    }

    #[test]
    fn truncate_exif_value_caps_long_values() {
        assert_eq!(truncate_exif_value("1/250 s"), "1/250 s");
        let out = truncate_exif_value(&"a".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1); // value + ellipsis
        assert!(out.ends_with('…'));
        // Multibyte values are truncated on char boundaries, not bytes.
        let out = truncate_exif_value(&"é".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1);
    }

    #[test]
    fn fresh_shuffle_seed_varies_across_calls() {
        // Regression test: the shuffle seed passed into `Playlist::new` used to be a
        // hardcoded `0` at every real call site, making "random" navigation produce
        // the exact same order every launch for a given deck size. Successive calls
        // (even within the same clock tick) must diverge.
        let seeds: Vec<u64> = (0..64).map(|_| fresh_shuffle_seed()).collect();
        assert!(
            seeds.windows(2).all(|w| w[0] != w[1]),
            "consecutive seeds must differ: {seeds:?}"
        );
    }

    #[test]
    fn window_split_biases_forward_and_reserves_current() {
        let (ahead, behind) = window_for_capacity(11);
        assert_eq!(ahead + behind, 10); // one slot reserved for the current item
        assert!(ahead > behind);
    }

    #[test]
    fn point_in_rect_is_inclusive() {
        assert!(point_in_rect([0.0, 0.0, 10.0, 10.0], 0.0, 10.0));
        assert!(!point_in_rect([0.0, 0.0, 10.0, 10.0], 11.0, 5.0));
    }

    #[test]
    fn scale_alpha_scales_only_alpha() {
        let rgba = [10u8, 20, 30, 200];
        let out = scale_alpha(&rgba, 0.5);
        assert_eq!(&out[..3], &[10, 20, 30]);
        assert_eq!(out[3], 100);
    }

    #[test]
    fn remove_middle_keeps_cursor_on_the_next_photo() {
        // [0 1 2 3 4], remove 2 → [0 1 3 4]; cursor stays at 2 (was photo 3, the next).
        assert_eq!(cursor_after_removal(5, 2), Some(2));
        assert_eq!(cursor_after_removal(5, 0), Some(0));
    }

    #[test]
    fn remove_last_falls_back_to_the_previous() {
        // [0 1 2], remove 2 (the last) → [0 1]; cursor → 1 (the new last = previous).
        assert_eq!(cursor_after_removal(3, 2), Some(1));
        assert_eq!(cursor_after_removal(2, 1), Some(0));
    }

    #[test]
    fn remove_only_item_empties_the_playlist() {
        assert_eq!(cursor_after_removal(1, 0), None);
        assert_eq!(cursor_after_removal(0, 0), None);
    }

    #[test]
    fn cursor_stays_in_bounds_of_the_shrunk_list() {
        for len in 1..=10 {
            for removed in 0..len {
                if let Some(c) = cursor_after_removal(len, removed) {
                    assert!(
                        c < len - 1,
                        "cursor {c} must index the shrunk list (len {})",
                        len - 1
                    );
                }
            }
        }
    }

    #[test]
    fn picker_starts_in_the_folder_containing_an_archive() {
        // Archive source: container is the .7z file; the Open dialog must start in its
        // parent folder, never the archive itself (the OS dialog can't browse inside it,
        // and an encrypted one errors). Holds for zip + 7z, encrypted or not.
        let fb = Path::new("fallback");
        let archive = Path::new("photos/trips/spain.7z");
        let got = picker_start_dir(None, Some(archive), None, archive, None, fb);
        assert_eq!(got, Path::new("photos/trips"));

        let zip = Path::new("albums/2015.zip");
        assert_eq!(
            picker_start_dir(None, Some(zip), None, zip, None, fb),
            Path::new("albums")
        );
    }

    #[test]
    fn picker_uses_the_scanned_folder_for_a_normal_source() {
        // No archive, no pin: prefer the scanned folder, else the display root.
        let fb = Path::new("fallback");
        let folder = Path::new("photos/trips");
        assert_eq!(
            picker_start_dir(None, None, Some(folder), folder, None, fb),
            folder
        );

        let root = Path::new("photos");
        assert_eq!(picker_start_dir(None, None, None, root, None, fb), root);
    }

    #[test]
    fn picker_pinned_folder_wins_over_everything() {
        // A user-pinned folder is used regardless of the current source (incl. archives).
        let fb = Path::new("fallback");
        let pinned = Path::new("D:/AlwaysHere");
        let archive = Path::new("photos/trips/spain.7z");
        assert_eq!(
            picker_start_dir(Some(pinned), Some(archive), None, archive, None, fb),
            pinned
        );
        assert_eq!(
            picker_start_dir(
                Some(pinned),
                None,
                Some(Path::new("photos")),
                Path::new("photos"),
                None,
                fb
            ),
            pinned
        );
    }

    #[test]
    fn picker_falls_back_when_there_is_no_current_folder() {
        // Fresh launch: empty scan_root + empty root → the safe fallback, NOT an empty
        // path (which would let Windows surface its own remembered last folder).
        let fb = Path::new("fallback");
        let empty = Path::new("");
        assert_eq!(picker_start_dir(None, None, None, empty, None, fb), fb);
        assert_eq!(
            picker_start_dir(None, None, Some(empty), empty, None, fb),
            fb
        );
    }

    #[test]
    fn picker_starts_in_the_last_folder_when_nothing_is_open() {
        // Fresh launch with a remembered last_folder: the dialog starts back in the
        // user's library — but anything actually open (scan root / display root)
        // still wins, since "where you are" beats "where you were".
        let fb = Path::new("fallback");
        let empty = Path::new("");
        let last = Path::new("photos/library");
        assert_eq!(
            picker_start_dir(None, None, None, empty, Some(last), fb),
            last
        );
        let open_root = Path::new("photos/trip");
        assert_eq!(
            picker_start_dir(None, None, None, open_root, Some(last), fb),
            open_root
        );
    }

    /// Live Photo pairing: a still finds its same-stem sibling `.mov`; a still with no
    /// motion clip pairs to nothing. Filename-based (Apple's on-disk convention, #38).
    #[cfg(any(
        target_os = "macos",
        windows,
        all(unix, not(target_os = "macos"), feature = "livephoto")
    ))]
    #[test]
    fn companion_motion_pairs_a_same_stem_mov() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("pb_livepair_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let still = dir.join("IMG_1.heic");
        let motion = dir.join("IMG_1.mov");
        fs::write(&still, b"still").expect("seed");
        fs::write(&motion, b"motion").expect("seed");
        // A still with no companion motion clip pairs to nothing.
        let solo = dir.join("IMG_2.jpg");
        fs::write(&solo, b"solo").expect("seed");

        assert_eq!(companion_motion(&still), Some(motion));
        assert_eq!(companion_motion(&solo), None);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Task #79 phase 1: a video item resolves to the placeholder WITHOUT any bytes
    /// request — the path here does not exist, so a `bytes()` call would error, and
    /// success proves the typed dispatch fired first.
    #[test]
    fn video_items_dispatch_before_bytes_and_get_the_placeholder() {
        use pb_source::FsSource;
        let src = FsSource::new(vec![PathBuf::from(r"C:\definitely\not\here\clip.mp4")]);
        let img = decode_item(&src, 0, None, true).expect("placeholder needs no read");
        assert_eq!(img.codec, "MP4");
        assert!(
            img.is_well_formed(),
            "placeholder pixels match its geometry"
        );
        assert_eq!(img.animated, None, "phase 1: P has nothing to play yet");
        assert_eq!((img.width, img.height), (320, 180));
    }

    /// The same nonexistent path with an image extension DOES attempt the read and
    /// fails — locking the dispatch to video items only.
    #[test]
    fn image_items_still_read_bytes() {
        use pb_source::FsSource;
        let src = FsSource::new(vec![PathBuf::from(r"C:\definitely\not\here\photo.jpg")]);
        assert!(decode_item(&src, 0, None, true).is_err());
    }

    // --- archive doors (task #104) ----------------------------------------

    /// A source that **panics** if anyone reads an item's bytes.
    ///
    /// Deliberately harsher than the nonexistent-path trick above: a missing file
    /// proves a read would have *failed*, which a future caller could swallow. A
    /// panic proves the call never happened at all — and "the archive is never read
    /// until you press `P`" is the door's entire contract, so it is worth proving
    /// exactly rather than nearly.
    struct ExplodingSource(PathBuf);
    impl pb_source::ItemSource for ExplodingSource {
        fn len(&self) -> usize {
            1
        }
        fn name(&self, _i: usize) -> &str {
            self.0.file_name().and_then(|n| n.to_str()).unwrap_or("")
        }
        fn path(&self, _i: usize) -> Option<&Path> {
            Some(&self.0)
        }
        fn bytes(&self, _i: usize) -> std::io::Result<Vec<u8>> {
            panic!("a door read the archive: {}", self.0.display());
        }
    }

    #[test]
    fn a_door_dispatches_before_bytes_and_gets_the_sentinel() {
        let src = ExplodingSource(PathBuf::from(r"C:\photos\holiday.7z"));
        let img = decode_item(&src, 0, None, true).expect("a door needs no read");
        assert_eq!(img.codec, "7z");
        assert!(img.is_well_formed(), "pixels match the geometry");
        assert_eq!(
            (img.width, img.height),
            (1, 1),
            "the cheapest possible slot"
        );
        assert_eq!(
            img.pixels,
            vec![0, 0, 0, 0],
            "fully transparent — draws nothing"
        );
    }

    /// **The feature's central promise.** Both leaks Phase 0 fixed lived in entry
    /// points a `decode_item`-only test never touches: the thumbs strip goes through
    /// `decode_item_for(Thumb)`, which used to read bytes for anything that was not a
    /// video. Cover every door, through every entry.
    #[test]
    fn a_door_is_never_read_through_any_entry_point() {
        use crate::decode_pool::Purpose;
        let cancel = AtomicBool::new(false);
        for file in [
            "holiday.zip",
            "holiday.7z",
            "book.cbz",
            "book.cbr",
            "backup.tar.gz",
            "backup.tar.zst",
        ] {
            let src = ExplodingSource(PathBuf::from(r"C:\photos").join(file));
            for (entry, got) in [
                ("decode_item", decode_item(&src, 0, None, true)),
                (
                    "decode_item_cancellable",
                    decode_item_cancellable(&src, 0, None, true, &cancel),
                ),
                (
                    "decode_item_for/Display",
                    decode_item_for(&src, 0, None, true, Purpose::Display, &cancel),
                ),
                (
                    "decode_item_for/Thumb",
                    decode_item_for(&src, 0, None, true, Purpose::Thumb, &cancel),
                ),
            ] {
                let img = got.unwrap_or_else(|e| panic!("{file} via {entry}: {e:?}"));
                assert!(
                    !img.codec.is_empty() && img.is_well_formed(),
                    "{file} via {entry} should be the door tile"
                );
            }
        }
    }

    /// A door's slot is now 4 bytes — the ring's 64-slot cap binds by miles. Kept as a
    /// regression bar: the frame's cost has been argued from three times and been wrong
    /// three times, so state it against the real constant rather than a number.
    ///
    /// This is a **comfort** property, not the safety one. What makes a door safe to
    /// prefetch past is that its decode never reads the archive
    /// (`a_door_is_never_read_through_any_entry_point`), whatever the frame weighs.
    #[test]
    fn a_full_ring_of_doors_fits_the_byte_budget() {
        let img = archive_placeholder(pb_source::ArchiveKind::Zip);
        let slots = RING_BUDGET_BYTES / img.pixels.len() as u64;
        assert!(
            slots >= 64,
            "the ring budget affords only {slots} door sentinels"
        );
    }

    /// The artwork is the **shell's** to draw (task #105) — it must not sneak back into
    /// the deck's frame, which is what produced a 12× upscaled glyph, a photo-sized ring
    /// slot for an icon, an invented grey backdrop and a 2.1× magnification in turn.
    #[test]
    fn the_artwork_is_not_in_the_decoded_frame() {
        let img = archive_placeholder(pb_source::ArchiveKind::Zip);
        assert_eq!(
            img.pixels.len(),
            4,
            "a door's frame is a 1×1 sentinel, not artwork"
        );
        let art = door_artwork().expect("the art is available to the shells");
        assert!(art.pixels.len() > img.pixels.len(), "…and lives only there");
    }

    /// **Both** artworks decode, on every platform — not just the one this build ships.
    /// `DOOR_ART` is `cfg`-selected, so a corrupt or re-exported blue file would sail
    /// through every Windows build and CI run and only surface on a Mac. Same blind
    /// spot as the `cfg(macos)` routing arms; cheap to close here.
    #[test]
    fn both_platform_artworks_decode() {
        for (name, bytes) in [
            (
                "yellow",
                include_bytes!("../assets/folder-zip-yellow.webp").as_slice(),
            ),
            (
                "blue",
                include_bytes!("../assets/folder-zip-blue.webp").as_slice(),
            ),
        ] {
            let img = decode_named_bytes("door.webp", bytes, None, false)
                .unwrap_or_else(|e| panic!("the {name} door artwork does not decode: {e:?}"));
            assert!(img.is_well_formed(), "{name}");
            assert_eq!(img.width, img.height, "{name}: the art is square");
            assert!(
                img.pixels.chunks_exact(4).any(|p| p[3] < 255),
                "{name}: expected an alpha channel so the card blends onto its background"
            );
        }
    }

    /// The crop **runs**, and the folder stays centred while it does.
    ///
    /// Both halves are regressions that already happened. It silently did nothing for as
    /// long as it existed — the asset's lossy-WebP alpha haze made the ink box the whole
    /// square, so it decided "already tight" and returned `None`, and the shells laid out
    /// ~10% of dead margin a side as if it were design. And the fix for *that* has its own
    /// trap, which the owner named: crop to the ink alone and the one-sided shadow shoves
    /// the folder visibly off-centre. So assert the outcome, not the threshold.
    #[test]
    fn the_artwork_is_cropped_to_its_content_with_the_folder_still_centred() {
        let art = door_artwork().expect("artwork decodes");
        let full = decode_named_bytes("door.webp", DOOR_ART, None, false).expect("decodes");
        assert!(
            art.width < full.width && art.height < full.height,
            "the crop must actually crop ({}×{} vs {}×{}) — a no-op here is invisible \
             everywhere else",
            art.width,
            art.height,
            full.width,
            full.height
        );

        // The near-opaque subject: the folder, without its shadow.
        let (w, h) = (art.width as usize, art.height as usize);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if art.pixels[(y * w + x) * 4 + 3] > 200 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        let (left, right) = (x0 as i64, (w - 1 - x1) as i64);
        let (top, bottom) = (y0 as i64, (h - 1 - y1) as i64);
        assert!(
            (left - right).abs() <= 2 && (top - bottom).abs() <= 2,
            "the folder must sit dead centre in what we hand the shells \
             (l{left} r{right} t{top} b{bottom})"
        );
        // The whole point: the shells' size cap sizes the *frame*, but what the eye
        // measures is the folder inside it. It was 80% of the frame; anything near that
        // again means the crop regressed.
        assert!(
            (x1 - x0 + 1) * 100 / w >= 88,
            "the folder should fill the frame it's given, not float in it"
        );
    }

    /// One decode for the process: `door_artwork` hands every shell the same buffer, so
    /// a folder of forty doors never re-runs the WebP decode.
    #[test]
    fn the_artwork_is_decoded_once_and_shared() {
        let a = door_artwork().expect("artwork decodes");
        let b = door_artwork().expect("artwork decodes");
        assert!(std::ptr::eq(a, b), "same cached decode, not a re-decode");
        assert!(
            a.pixels.chunks_exact(4).any(|p| p[3] == 0),
            "alpha survives"
        );
    }

    /// The decode-error placeholder artwork (task #127) decodes through the same pipeline,
    /// once, with its transparency intact (the burning Polaroid sits on a clear ground).
    #[test]
    fn the_fire_artwork_decodes_once_with_alpha() {
        let a = decode_error_artwork().expect("fire artwork decodes");
        let b = decode_error_artwork().expect("fire artwork decodes");
        assert!(std::ptr::eq(a, b), "same cached decode, not a re-decode");
        assert!(a.width > 0 && a.height > 0);
        assert_eq!(a.pixels.len(), a.width as usize * a.height as usize * 4);
        assert!(
            a.pixels.chunks_exact(4).any(|p| p[3] == 0),
            "the transparent margin survives"
        );
    }

    /// Every kind names itself, so the tile never shows a blank format row.
    #[test]
    fn every_archive_kind_names_itself() {
        for kind in [
            pb_source::ArchiveKind::Zip,
            pb_source::ArchiveKind::SevenZ,
            pb_source::ArchiveKind::Tar,
            pb_source::ArchiveKind::TarGz,
            pb_source::ArchiveKind::TarBz2,
            pb_source::ArchiveKind::TarZst,
            pb_source::ArchiveKind::TarXz,
            pb_source::ArchiveKind::Rar,
        ] {
            assert!(!archive_placeholder(kind).codec.is_empty(), "{kind:?}");
        }
    }
}
