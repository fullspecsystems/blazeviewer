//! **Toasts and the pie** — the `AppCore` methods that queue and age the HUD's transient
//! chrome (task #125).
//!
//! A *topic*: the rasterizer lives in the shell-neutral `pb-hud` crate, and what stays here
//! is only the queueing and the per-tick ageing. `tick` calls `tick_toast`/`tick_pie` and
//! stays in the parent — this file is the control surface, not the frame loop.
//!
//! ⚠ The HUD composites text + icons into one software RGBA8 pill, rebuilt only on change,
//! deliberately off the photo hot path. Nothing here should grow per-frame work.
//!
//! Named `toast.rs`, not `hud.rs`: `app_core_impl.rs` does `use pb_hud::{hud, icon}`, so a
//! `mod hud;` here would be E0255 and bare `hud::` inside would resolve to this module. The
//! name it wanted was the wrong one anyway — this is toasts and the pie, not the HUD.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Flash a transient status message at the bottom-center (tasks.json #10) — for
    /// commands that otherwise give no visual feedback, e.g. the recursion toggle.
    /// A new toast replaces any current one.
    pub fn show_toast(&mut self, msg: &str) {
        self.show_toast_icon(msg, ToastIcon::None);
    }

    /// Like [`show_toast`] but with a leading semantic [`ToastIcon`] — e.g. the save glyph, or
    /// an icon-only pill (empty `msg`) for the rotate toasts. Each shell picks its own art:
    /// the HUD rasterizes a Font Awesome glyph; the native macOS shell (`native_toast`) instead
    /// gets the message + icon as data and draws a SwiftUI pill. Always redraws (HUD path), so a
    /// caller that also changed the view (e.g. `rotate`) renders even without a system font.
    pub fn show_toast_icon(&mut self, msg: &str, kind: ToastIcon) {
        // Native shell: hand the shell the data and let it render the pill; no CPU raster.
        if self.native_toast {
            self.toast_seq = self.toast_seq.wrapping_add(1);
            self.toast_native = Some(NativeToast {
                message: msg.to_string(),
                icon: kind,
                started: self.now,
                seq: self.toast_seq,
            });
            // Still redraw: some callers (e.g. `rotate`) change the *view* and rely on this to
            // render it — and it wakes the shell so the toast pill appears from idle.
            self.draw();
            return;
        }
        let px = (26.0 * self.viewport.scale_factor).max(16.0);
        let pad = (12.0 * self.viewport.scale_factor).round().max(4.0) as u32;
        // Map the semantic icon to the HUD's Font Awesome glyph.
        let fa = match kind {
            ToastIcon::None => None,
            ToastIcon::Mute => Some(icon::assets::VOLUME_SLASH),
            ToastIcon::Unmute => Some(icon::assets::VOLUME),
            ToastIcon::Save => Some(icon::assets::FLOPPY),
            ToastIcon::Undo => Some(icon::assets::UNDO),
            ToastIcon::Delete => Some(icon::assets::TRASH),
            ToastIcon::Recycle => Some(icon::assets::RECYCLE),
            ToastIcon::Pin => Some(icon::assets::THUMBTACK),
            ToastIcon::Unpin => Some(icon::assets::THUMBTACK_SLASH),
            ToastIcon::RotateLeft => Some(icon::assets::ROTATE_LEFT),
            ToastIcon::RotateRight => Some(icon::assets::ROTATE_RIGHT),
            ToastIcon::Copy => Some(icon::assets::CLIPBOARD),
            ToastIcon::Captions => Some(icon::assets::CAPTIONS),
            ToastIcon::CaptionsOff => Some(icon::assets::CAPTIONS_SLASH),
            // The winit HUD has no dedicated audio-track glyph; the speaker reads correctly
            // for "you're now hearing a different track" and the set already carries it.
            ToastIcon::AudioTrack => Some(icon::assets::VOLUME),
            ToastIcon::AudioTrackFailed => Some(icon::assets::VOLUME_SLASH),
            ToastIcon::Sorted => Some(icon::assets::FOLDER_DOWN),
        };
        if let Some(hud) = self.hud.as_ref() {
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, fa, hud.theme().bg) {
                self.toast = Some(Toast {
                    rgba,
                    w,
                    h,
                    started: self.now,
                    uploaded_alpha: -1.0,
                });
                self.push_toast(1.0);
            }
        }
        self.draw();
    }

    /// Upload the current toast bitmap to the renderer at `alpha` (its alpha
    /// channel scaled), centered near the bottom.
    pub fn push_toast(&mut self, alpha: f32) {
        let (faded, w, h) = {
            let Some(t) = self.toast.as_mut() else {
                return;
            };
            t.uploaded_alpha = alpha;
            (scale_alpha(&t.rgba, alpha), t.w, t.h)
        };
        // The toast rides a fixed 64px bottom margin — always well above the info line
        // (which sits at the small `overlay_margin` inset), so the two never collide
        // vertically even when both are centered. No line reserve needed here.
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_toast(Some((&faded, w, h)), margin);
        }
    }

    /// Advance the toast's hold/fade and return whether one is still active (so the
    /// event loop keeps ticking). Re-uploads only on a meaningful alpha change;
    /// clears the layer once expired.
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        // Native path: the shell draws the pill and animates its own fade-out on removal — the
        // core just expires the data after the hold+fade window and keeps the pump ticking
        // (returning `true`) while it's live so the expiry actually fires.
        if self.native_toast {
            if let Some(t) = &self.toast_native {
                if now.saturating_duration_since(t.started) > Toast::HOLD + Toast::FADE {
                    self.toast_native = None;
                }
            }
            return self.toast_native.is_some();
        }
        let Some(alpha) = self.toast.as_ref().and_then(|t| t.alpha(now)) else {
            if self.toast.take().is_some() {
                if let Some(a) = self.renderer.as_mut() {
                    a.set_toast(None, 0);
                }
                self.draw();
            }
            return false;
        };
        let changed = self
            .toast
            .as_ref()
            .is_some_and(|t| (alpha - t.uploaded_alpha).abs() > 0.02);
        if changed {
            self.push_toast(alpha);
            self.draw();
        }
        true
    }

    /// The current keypress brighten-pulse intensity (0..=1), decaying to 0 over
    /// `PIE_GLOW_DUR` after the last dropped nav press.
    pub fn pie_glow(&self, now: Instant) -> f32 {
        match self.pie_glow_started {
            Some(t) => (1.0 - (now - t).as_secs_f32() / PIE_GLOW_DUR).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// Drive the top-right "not-ready" loading pie (#2). While the next photo is
    /// still decoding (a miss outlasting `PIE_SHOW_DELAY`), show a pie that eases
    /// asymptotically toward — but never reaches — full, on a time constant
    /// self-calibrated to how long misses usually take (`decode_ewma`). Once the
    /// photo lands, learn from the wait, then snap to full and fade. Returns
    /// whether the pie still needs the loop to keep ticking.
    pub fn tick_pie(&mut self, now: Instant) -> bool {
        // Epoch-aware so the pie also shows while a same-index frame is being re-decoded
        // at a new fit (resize / scale-mode). Live video re-resolves every frame
        // (`present_video_frame`), so it stays caught-up and never shows the pie.
        //
        // Also keep the pie up while the on-screen photo is only a **preview** and its full
        // decode is still coming (#106.5): preview-first paints an instant blurry thumbnail, so
        // `target_caught_up` flips true immediately — but without the pie a slow big-photo open
        // looks *finished* at the blurry stage, and the owner's fear is a user seeing soft
        // photos, thinking "these are terrible," and deleting them before they sharpen.
        // `sharpen_now()` is exactly this state: parked, displayed is a resident preview not yet
        // upgraded (and it is `None` while blazing and once the full lands, so the pie doesn't
        // spin during a fast blaze or after sharpening).
        let not_ready = self.target_pending() || self.sharpen_now().is_some();
        if not_ready {
            self.pie_finish = None;
            let start = *self.wait_started.get_or_insert(now);
            let elapsed = (now - start).as_secs_f32();
            if elapsed >= PIE_SHOW_DELAY {
                let tau = self.decode_ewma.max(PIE_TAU_MIN);
                // Asymptotic ease: ~half-full at one tau, approaching the cap but
                // never quite arriving (the deliberate, honest-ish "lie").
                let progress = (1.0 - 2f32.powf(-elapsed / tau)).min(PIE_FILL_CAP);
                let glow = self.pie_glow(now);
                self.push_pie(progress, glow, 1.0);
            } else {
                self.clear_pie();
            }
            return true; // keep ticking while we wait
        }
        // Caught up. If we were mid-wait, learn how long it took (so the estimate
        // tracks this machine + folder), and if the pie was up, play the finish.
        if let Some(start) = self.wait_started.take() {
            let waited = (now - start).as_secs_f32();
            self.decode_ewma = (self.decode_ewma * (1.0 - PIE_EWMA_ALPHA)
                + waited * PIE_EWMA_ALPHA)
                .clamp(PIE_TAU_MIN, 2.0);
            if self.pie_drawn {
                self.pie_finish = Some(now);
            }
        }
        if let Some(fstart) = self.pie_finish {
            let t = (now - fstart).as_secs_f32();
            if t < PIE_FINISH_FADE {
                let glow = self.pie_glow(now);
                self.push_pie(1.0, glow, 1.0 - t / PIE_FINISH_FADE);
                return true;
            }
            self.pie_finish = None;
        }
        self.clear_pie();
        false
    }

    /// Rasterize + upload the pie at `progress`/`glow`, scaled by a global `alpha`
    /// (the finish fade). Re-uploads + redraws only when the visible result
    /// changes (quantized), so the slow tail of the asymptote doesn't churn.
    pub fn push_pie(&mut self, progress: f32, glow: f32, alpha: f32) {
        let want = (progress, glow, alpha);
        let unchanged = self.pie_pushed.is_some_and(|(p, g, a)| {
            (p - progress).abs() < 0.01 && (g - glow).abs() < 0.04 && (a - alpha).abs() < 0.02
        });
        if unchanged && self.pie_drawn {
            return;
        }
        let diameter = (PIE_DIAMETER * self.viewport.scale_factor)
            .round()
            .max(12.0) as u32;
        let (mut rgba, w, h) = hud::render_pie(diameter, progress, glow, self.hud_dark);
        if alpha < 1.0 {
            rgba = scale_alpha(&rgba, alpha);
        }
        let margin = (PIE_MARGIN * self.viewport.scale_factor).round().max(4.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_pie(Some((&rgba, w, h)), margin);
        }
        self.pie_drawn = true;
        self.pie_pushed = Some(want);
        self.draw();
    }

    /// Clear the pie layer if it's up (and redraw to remove it).
    pub fn clear_pie(&mut self) {
        if self.pie_drawn {
            if let Some(a) = self.renderer.as_mut() {
                a.set_pie(None, 0);
            }
            self.pie_drawn = false;
            self.pie_pushed = None;
            self.draw();
        }
    }
}
