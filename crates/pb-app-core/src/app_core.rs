//! [`AppCore`] — the orchestration state the shell drives (NS0 step 5, ADR-021).
//!
//! The winit `App` god-object is being split into this platform-neutral core (the
//! orchestration state + logic) and a thin shell (`WinitShell` now, an AppKit host later)
//! that owns the window / menu / dialog surface and translates its events into
//! [`CoreEvent`](crate::CoreEvent)s / drains [`CoreEffect`](crate::CoreEffect)s.
//!
//! Filled **incrementally**: each step-5 increment relocates one low-coupling field group
//! off the shell into `AppCore` (reached as `self.core.*`) and stays green. First in: the
//! held-key + input-modifier + self-paced-advance **timing** state — already shell-neutral
//! (`PbKey`/`Action`/`Modifiers`/`Slideshow` + `std`), so it needs no engine-crate deps.
//! Nav/prefetch/decode/residency, the renderer (`Box<dyn Renderer>`), and the
//! `handle(CoreEvent)` dispatch follow (see the step-5 increment order in the brief).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::{Action, Modifiers, PbKey, Slideshow};

/// The platform-neutral orchestration state the shell drives. Grows as step 5 relocates
/// field groups off the winit shell; the shell holds one `AppCore` and reaches its state
/// through `self.core.*` (fields are `pub` during the incremental move — they collapse
/// behind `handle(CoreEvent)` / accessors once the split is complete).
pub struct AppCore {
    /// Physical keys currently held → the [`Action`] each resolved to at press time (the
    /// hold-to-fly / continuous-action set). OS key-repeat is ignored; focus loss clears it.
    pub held: HashMap<PbKey, Action>,
    /// When the current on-screen frame was presented — the anchor for the self-paced
    /// advance interval and the slideshow dwell deadline.
    pub last_present: Option<Instant>,
    /// The advance cadence cap (one frame per this interval), seeded to the monitor refresh.
    pub frame_interval: Duration,
    /// When the current nav key-hold began (drives the accelerating hold-to-fly ramp).
    pub hold_start: Option<Instant>,
    /// The tap-vs-hold delay before a held nav key starts flying.
    pub initial_delay: Duration,
    /// Slideshow state (on/off + dwell interval).
    pub slideshow: Slideshow,
    /// The current keyboard modifier state (the shell-neutral mirror of the OS modifiers).
    pub mods: Modifiers,
    /// Briefly guards Esc-to-quit after a modal (picker / dialog) closes, so its stray Esc
    /// leak can't also quit the app.
    pub esc_guard_until: Option<Instant>,
}

impl AppCore {
    /// Build the initial core. `initial_delay` and `slideshow_interval` come from user
    /// settings; the rest start at their launch defaults — nothing held, a ~120 Hz cadence
    /// until the real refresh rate is read, slideshow off, no modifiers, no esc-guard.
    pub fn new(initial_delay: Duration, slideshow_interval: Duration) -> Self {
        Self {
            held: HashMap::new(),
            last_present: None,
            frame_interval: Duration::from_micros(8_333),
            hold_start: None,
            initial_delay,
            slideshow: Slideshow {
                interval: slideshow_interval,
                ..Slideshow::default()
            },
            mods: Modifiers::NONE,
            esc_guard_until: None,
        }
    }
}
