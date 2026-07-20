//! **Slideshow** — the `AppCore` half of [`crate::slideshow`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `slideshow.rs` owns the dwell timing; this file holds the `impl AppCore` methods that
//! start/stop it and adjust the interval. The advance itself is driven from `tick`, which
//! stays in the parent — this is only the control surface.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Start / stop the slideshow (task #23, the `S` key + View ▸ Slideshow). Starting
    /// resets the timer (`last_present = now`) so the first slide shows for a full
    /// interval before advancing; `about_to_wait` drives the auto-advance from there.
    pub fn toggle_slideshow(&mut self) {
        let on = self.slideshow.toggle();
        if on {
            self.last_present = Some(self.now);
        }
        self.show_toast(if on { "Slideshow" } else { "Slideshow Stopped" });
    }

    /// Change the slideshow interval by `steps` × 0.5s (the `[` / `]` keys: `-1`
    /// shortens, `+1` lengthens), clamped, and flash the new value (e.g. `2.0s`). The
    /// change applies live: the deadline is `last_present + interval`, so a running
    /// slideshow's current slide gets more / less remaining time immediately.
    pub fn adjust_slideshow(&mut self, steps: i32) {
        let interval = self.slideshow.adjust(steps);
        self.show_toast(&crate::slideshow::format_interval(interval));
    }

    /// The current slideshow interval, formatted for display (e.g. `4s`, `0.5s`) — the
    /// same formatting the `[`/`]` adjust toast uses. The macOS toolbar shows this on its
    /// slideshow control (task #55). Reflects live adjustments, not just the configured
    /// default, since it reads the running `slideshow.interval`.
    pub fn slideshow_interval_display(&self) -> String {
        crate::slideshow::format_interval(self.slideshow.interval)
    }
}
