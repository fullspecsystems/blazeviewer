//! Overlay / toast / hint **state** types — the "what to show" of the HUD, owned by
//! [`AppCore`](crate::AppCore). The Hud *rasterizer* (the "how to draw" — CPU text/pill
//! compositing) stays shell-side (`pb-app`'s `hud.rs`) with the renderer for now.

use crate::folder_tree::TreeTarget;
use pb_hud::hud::{TreeHit, TreeRow};
use std::time::{Duration, Instant};

/// The folder-tree overlay's interactive state while shown: the bitmap size +
/// margin (its on-screen rect is derived from the live window at hit-test time —
/// resize/DPI-proof, like [`OpenPanel`]), the hit rects **within the bitmap**,
/// each row's click target, the cached display rows (so hover/page re-renders
/// skip the derivation and its `read_dir`s entirely), the hovered hit, and the
/// windowing page offset (the clickable "… n more" markers). RAM-only.
pub struct TreePanel {
    pub w: u32,
    pub h: u32,
    pub margin: u32,
    pub hits: Vec<(TreeHit, [u32; 4])>,
    pub targets: Vec<Option<TreeTarget>>,
    pub rows: Vec<TreeRow>,
    pub hovered: Option<TreeHit>,
    pub page: i32,
    /// When this bitmap was rasterized — rate-limits mid-flight rebuilds.
    pub built: Instant,
}

/// Which info overlay is active (`i` basic / `Shift+I` full EXIF / `?` help /
/// `T` recognized text / off). One shared overlay slot, so they replace each other.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InfoMode {
    Off,
    Basic,
    Full,
    Help,
    /// The "text in image" panel (task #45): OCR lines + QR payloads for the
    /// displayed photo, or its busy/error state while the scan runs.
    Text,
    /// The AI-description panel (task #44): the vision model's description (or answer to
    /// a typed question) for the displayed photo, or its busy/error state while the
    /// backend runs. Shares the overlay slot with the others.
    Describe,
}

/// A transient bottom-center status toast (e.g. "Recursive folders: on"): a pill
/// rasterized once, held briefly at full opacity, then faded out by re-uploading the
/// bitmap with scaled alpha. Command feedback with no other on-screen cue — deliberately
/// NOT shown for next/prev/zoom.
pub struct Toast {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub started: Instant,
    /// Alpha last pushed to the renderer, so the fade re-uploads only on change.
    pub uploaded_alpha: f32,
}

impl Toast {
    /// Full-opacity hold, then a short linear fade (~1.3 s total).
    pub const HOLD: Duration = Duration::from_millis(950);
    pub const FADE: Duration = Duration::from_millis(380);

    /// The toast's alpha at `now`, or `None` once it has fully expired.
    pub fn alpha(&self, now: Instant) -> Option<f32> {
        let e = now.saturating_duration_since(self.started);
        if e <= Self::HOLD {
            Some(1.0)
        } else {
            let f = (e - Self::HOLD).as_secs_f32() / Self::FADE.as_secs_f32();
            (f < 1.0).then_some(1.0 - f)
        }
    }
}

/// The interactive **play hint** — the `▶ Play  P` affordance shown when parked on an
/// animated still / Live Photo. A real button riding on the transient toast layer:
/// hovering pauses the fade and lights it, a click plays. `Some` only while the current
/// toast *is* the play hint. Its click rect is derived from the live window size.
#[derive(Clone, Copy)]
pub struct PlayHint {
    /// The toast bitmap size, for the bottom-center hit rect.
    pub w: u32,
    pub h: u32,
    /// The leading icon (play ▶ or the Live Photo mark), to re-render lit on hover.
    pub icon: &'static str,
    /// Whether the pointer is over it (lit + fade paused).
    pub hovered: bool,
}

/// Which empty-state **open button** the pointer is over (the call-to-action panel shown
/// when no photo is loaded). Two interactive buttons — Open File and Open Folder.
#[derive(Clone, Copy, PartialEq)]
pub enum OpenButton {
    File,
    Folder,
}

/// The empty-state open panel's geometry while it's shown: the bitmap `(w, h)` and each
/// button's `[x, y, w, h]` rect **within the bitmap**. On-screen click rects are derived
/// from the live window size at hit-test time — resize- and DPI-proof.
#[derive(Clone, Copy)]
pub struct OpenPanel {
    pub w: u32,
    pub h: u32,
    pub file: [u32; 4],
    pub folder: [u32; 4],
}
