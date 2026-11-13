//! **Slideshow** — the `AppCore` half of [`crate::slideshow`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `slideshow.rs` owns the dwell timing and the `[` / `]` hold curve; this file holds the
//! `impl AppCore` methods that start/stop it and adjust the interval. The advance itself
//! is driven from `tick`, which stays in the parent — this is only the control surface.

use super::*;
use crate::slideshow::{format_interval, HoldTick};

/// The interval direction an adjust action moves: `[` (faster) shortens, `]` lengthens.
fn adjust_dir(action: Action) -> i32 {
    match action {
        Action::SlideshowFaster => -1,
        Action::SlideshowSlower => 1,
        _ => 0,
    }
}

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

    /// Change the slideshow interval by `steps` taps (`-1` shortens, `+1` lengthens; each
    /// sized by `slideshow::ZONES`, 0.1s under 10s), clamped, and flash the new value
    /// (e.g. `2.0s`). This is the
    /// View ▸ Slideshow Faster / Slower path; the keys go through
    /// [`Self::slideshow_adjust_press`] so they can accelerate while held. Either way the
    /// current slide's dwell restarts at the new speed (task #140).
    pub fn adjust_slideshow(&mut self, steps: i32) {
        let interval = self.slideshow.adjust(steps);
        self.slideshow.adjusted_at = Some(self.now);
        self.show_toast(&format_interval(interval));
    }

    /// A `[` / `]` key went down (task #140): track it, take one tap step now, and let
    /// [`Self::tick_slideshow_adjust`] accelerate it while it stays down.
    pub fn slideshow_adjust_press(&mut self, key: PbKey, action: Action) {
        let dir = adjust_dir(action);
        if dir == 0 {
            return self.dispatch_action(action);
        }
        self.held.insert(key, action);
        let interval = self.slideshow.press(dir, self.now);
        self.show_toast(&format_interval(interval));
    }

    /// The net direction of the `[` / `]` keys held right now: `None` if neither is down,
    /// `Some(0)` if both are (they cancel out), else `Some(-1)` / `Some(1)`.
    pub fn held_slideshow_adjust(&self) -> Option<i32> {
        let mut any = false;
        let mut dir = 0;
        for &action in self.held.values() {
            let d = adjust_dir(action);
            if d != 0 {
                any = true;
                dir += d;
            }
        }
        any.then_some(dir.signum())
    }

    /// Drive a held `[` / `]` from the tick: step on the [`crate::slideshow::HOLD_CURVE`]
    /// schedule, and restart the current slide's dwell when the key comes up. Returns
    /// whether an adjust key is held, which pauses the slideshow advance and keeps the
    /// loop polling.
    pub fn tick_slideshow_adjust(&mut self, now: Instant) -> bool {
        let held = self.held_slideshow_adjust();
        match self.slideshow.tick_hold(held, now, self.initial_delay) {
            HoldTick::Stepped(interval) => {
                self.show_toast(&format_interval(interval));
                true
            }
            HoldTick::Held => true,
            HoldTick::Released | HoldTick::Idle => false,
        }
    }

    /// The current slideshow interval, formatted for display (e.g. `4s`, `0.5s`) — the
    /// same formatting the `[`/`]` adjust toast uses. The macOS toolbar shows this on its
    /// slideshow control (task #55). Reflects live adjustments, not just the configured
    /// default, since it reads the running `slideshow.interval`.
    pub fn slideshow_interval_display(&self) -> String {
        format_interval(self.slideshow.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{five_photos, test_core};
    use crate::contract::{CoreEffect, CoreEvent, Modifiers};

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn down(core: &mut AppCore, key: PbKey) {
        core.handle(CoreEvent::KeyDown {
            key,
            mods: Modifiers::NONE,
            repeat: false,
        });
    }

    fn up(core: &mut AppCore, key: PbKey) {
        core.handle(CoreEvent::KeyUp { key });
    }

    /// Tick at the frame rate for `d` of fake time.
    fn run_for(core: &mut AppCore, d: Duration) {
        let end = core.now + d;
        while core.now < end {
            core.now += core.frame_interval;
            core.tick();
        }
    }

    /// The core with the default 250 ms hold delay (independent of the host's settings).
    fn core() -> AppCore {
        let mut core = test_core();
        core.initial_delay = Duration::from_millis(250);
        core
    }

    /// Parked on photo 0 of five, caught up, with a 2 s slideshow running from `now`.
    fn running_slideshow() -> AppCore {
        let mut core = core();
        core.source = five_photos();
        core.playlist = Playlist::new(5, 0).with_cursor(0);
        core.targets = vec![0];
        core.target_item = Some(0);
        core.mark_resolved(0);
        core.slideshow.on = true;
        core.slideshow.interval = secs(2.0);
        core.last_present = Some(core.now);
        core
    }

    fn last_wake(core: &AppCore) -> Option<Instant> {
        core.effects
            .iter()
            .rev()
            .find_map(|e| match e {
                CoreEffect::SetWake(w) => Some(*w),
                _ => None,
            })
            .expect("tick emits SetWake")
    }

    #[test]
    fn a_bracket_tap_is_one_tenth() {
        let mut core = core();
        down(&mut core, PbKey::BracketRight);
        assert_eq!(core.slideshow.interval, secs(4.1));
        run_for(&mut core, Duration::from_millis(100)); // released well inside the hold delay
        up(&mut core, PbKey::BracketRight);
        run_for(&mut core, Duration::from_secs(1));
        assert_eq!(core.slideshow.interval, secs(4.1));

        down(&mut core, PbKey::BracketLeft);
        up(&mut core, PbKey::BracketLeft);
        assert_eq!(core.slideshow.interval, secs(4.0));
    }

    #[test]
    fn holding_a_bracket_accelerates_until_it_comes_up() {
        let mut core = core();
        down(&mut core, PbKey::BracketRight);
        run_for(&mut core, Duration::from_millis(200));
        assert_eq!(
            core.slideshow.interval,
            secs(4.1),
            "no repeat inside the hold delay"
        );
        run_for(&mut core, Duration::from_secs(2)); // ~2.2 s held: on the 0.5 s rung
        let held = core.slideshow.interval;
        assert!(
            held >= secs(7.0),
            "accelerated past the 0.1 s steps, got {held:?}"
        );
        assert_eq!(
            held.as_millis() % 500,
            0,
            "on the half-second grid, got {held:?}"
        );

        up(&mut core, PbKey::BracketRight);
        run_for(&mut core, Duration::from_secs(1));
        assert_eq!(core.slideshow.interval, held, "nothing moves after key-up");
        assert_eq!(core.slideshow.hold, None);
    }

    #[test]
    fn the_slideshow_pauses_while_held_and_the_slide_restarts_on_release() {
        let mut core = running_slideshow();
        let t0 = core.now;
        core.now = t0 + Duration::from_millis(1500);
        down(&mut core, PbKey::BracketLeft); // 2.0 → 1.9 s, already 1.5 s into the slide
        run_for(&mut core, Duration::from_secs(1));
        assert_eq!(
            core.target_item,
            Some(0),
            "no advance while [ is held, though the slide is past its interval"
        );

        up(&mut core, PbKey::BracketLeft);
        core.tick(); // the release tick re-anchors the dwell
        let released = core.now;
        let interval = core.slideshow.interval;
        assert_eq!(core.slideshow.adjusted_at, Some(released));
        while core.now + core.frame_interval < released + interval {
            core.now += core.frame_interval;
            core.tick();
            assert_eq!(
                core.target_item,
                Some(0),
                "a full interval at the new speed"
            );
        }
        core.now = released + interval;
        core.tick();
        assert_eq!(
            core.target_item,
            Some(1),
            "advances once the fresh interval is up"
        );
    }

    #[test]
    fn a_held_bracket_keeps_the_loop_polling() {
        let mut core = core();
        down(&mut core, PbKey::BracketRight);
        core.effects.clear();
        core.now += Duration::from_millis(10);
        core.tick();
        assert_eq!(last_wake(&core), Some(core.now + core.frame_interval));
    }

    #[test]
    fn a_menu_click_is_one_tap_and_restarts_the_slide() {
        let mut core = core();
        core.handle(CoreEvent::MenuAction(Action::SlideshowSlower));
        assert_eq!(core.slideshow.interval, secs(4.1));
        assert_eq!(core.slideshow.adjusted_at, Some(core.now));
        assert_eq!(core.slideshow.hold, None, "a click never starts a hold");
        core.handle(CoreEvent::MenuAction(Action::SlideshowFaster));
        assert_eq!(core.slideshow.interval, secs(4.0));
    }

    #[test]
    fn focus_loss_mid_hold_ends_it() {
        let mut core = core();
        down(&mut core, PbKey::BracketRight);
        run_for(&mut core, Duration::from_millis(500));
        core.handle(CoreEvent::FocusLost);
        core.tick();
        let at_loss = core.slideshow.interval;
        assert_eq!(core.slideshow.hold, None);
        run_for(&mut core, Duration::from_secs(2));
        assert_eq!(
            core.slideshow.interval, at_loss,
            "a lost key-up can't strand a hold"
        );
    }
}
