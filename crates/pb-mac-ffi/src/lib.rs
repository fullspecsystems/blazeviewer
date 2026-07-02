//! PhotoBlaze macOS FFI bridge (NS1, ADR-021) — a [`swift-bridge`] staticlib that exposes
//! the platform-neutral [`pb_app_core::AppCore`] to a SwiftUI/AppKit host.
//!
//! **Shape** (mirrors the winit shell in `crates/pb-app/src/main.rs`, the worked reference):
//! - The Swift host owns the `NSWindow` + `MTKView`/`CAMetalLayer` and the run loop.
//! - **Events in:** the host translates `NSEvent` / gesture recognizers / menu clicks into
//!   calls on [`AppCoreHandle`] (`key_down` / `key_up` / `focus_lost` / `tick` / …), which
//!   build the shell-neutral [`pb_app_core::contract::CoreEvent`]s and call `AppCore::handle`.
//! - **Effects out:** the host drains [`AppCoreHandle::drain_effects`] **on the main actor**
//!   and executes each [`ffi::CoreEffectFfi`] (render, set title, wake, quit, …). A worker
//!   thread may only *schedule* a main-thread drain — never touch AppKit/render directly.
//!
//! **macOS-only.** The `swift-bridge` dependency and this whole module are target-gated, so
//! on Windows/Linux the crate compiles to an empty staticlib and the winit `pb-app` build is
//! untouched.
//!
//! **Slice 1 (this file)** proves the event→effect round-trip against the real
//! `AppCore::handle` on a *headless* core (no surface, no photos yet). A live `CAMetalLayer`
//! surface, a real photo source, and the remaining effects/events are layered on in the
//! following NS1 slices — see `.taskmaster/docs/macos-native-ui-plan.md` (§NS1).
#![cfg(target_os = "macos")]
// The `#[swift_bridge::bridge]` macro emits `extern "C"` shims with same-type pointer casts
// (`*mut AppCoreHandle` → `*mut AppCoreHandle`); that's generated glue we can't edit, so allow
// the lint crate-wide rather than fail `clippy -D warnings`.
#![allow(clippy::unnecessary_cast)]

use std::time::Instant;

use pb_app_core::contract::{self, CoreEvent, Modifiers};
use pb_app_core::{AppCore, PbKey, Viewport};

/// The opaque handle the Swift host holds — it owns the entire `AppCore` engine.
pub struct AppCoreHandle {
    core: AppCore,
}

impl AppCoreHandle {
    /// Construct a headless core at the given drawable size (`width`×`height` in physical
    /// pixels, `scale` = backing scale factor). A live surface + photo source are wired in a
    /// later slice; this is enough to drive the input/effect path through `handle`.
    fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle {
        AppCoreHandle {
            core: AppCore::headless(Viewport {
                width,
                height,
                scale_factor: scale,
            }),
        }
    }

    /// A physical key went down. `key` is a [`PbKey`] name (`PbKey::as_str`, e.g. `"Space"`,
    /// `"ArrowRight"`, `"KeyC"`) — the Swift host maps `NSEvent` → this name (the input-adapter
    /// job, NS1). Unknown names are ignored. OS auto-repeat is passed via `repeat`; the core
    /// drops it for held actions, exactly as the winit shell does.
    fn key_down(
        &mut self,
        key: &str,
        ctrl: bool,
        shift: bool,
        alt: bool,
        logo: bool,
        repeat: bool,
    ) {
        // The FFI is host/shell code, so it stamps the clock (the core never reads it — NS0).
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyDown {
                key,
                mods: Modifiers {
                    ctrl,
                    shift,
                    alt,
                    logo,
                },
                repeat,
            });
        }
    }

    /// A physical key was released.
    fn key_up(&mut self, key: &str) {
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyUp { key });
        }
    }

    /// The window lost key focus — the core clears held keys (the focus-loss release net).
    fn focus_lost(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::FocusLost);
    }

    /// A frame / idle tick: drives held-key pacing, slideshow dwell, prefetch, and animation.
    /// The host calls this each frame it draws and on the scheduled wake deadlines returned
    /// via `SetWake` (a `MTKViewDelegate.draw(in:)` + timer, per the plan).
    fn tick(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::Tick(self.core.now));
    }

    /// Drain the effects the core produced since the last drain, mapped to the FFI enum. The
    /// host runs these on the main actor. Slice 1 maps a representative subset; every other
    /// effect arrives as [`ffi::CoreEffectFfi::Other`] until it's bridged in a later slice.
    fn drain_effects(&mut self) -> Vec<ffi::CoreEffectFfi> {
        std::mem::take(&mut self.core.effects)
            .into_iter()
            .map(map_effect)
            .collect()
    }
}

/// Map a core [`contract::CoreEffect`] to the FFI enum the Swift host switches on.
fn map_effect(e: contract::CoreEffect) -> ffi::CoreEffectFfi {
    use contract::CoreEffect as C;
    use ffi::CoreEffectFfi as E;
    match e {
        C::RequestRender => E::RequestRender,
        C::SetTitle(title) => E::SetTitle(title),
        C::Quit => E::Quit,
        C::SetWake(None) => E::ClearWake,
        // A real Instant→deadline conversion needs a host time base; a later slice adds it.
        // For now the host just knows "wake again soon".
        C::SetWake(Some(_at)) => E::SetWakeSoon,
        // Menu state, dialogs, clipboard, reveal, context menu, live audio, window mode,
        // surface ops, … — each bridged in a later NS1 slice.
        _ => E::Other,
    }
}

// NOTE: inside `#[swift_bridge::bridge]`, use `//` comments only — a `///` doc comment
// becomes a `#[doc]` attribute that swift-bridge-ir's parser rejects (panics in codegen).
#[swift_bridge::bridge]
mod ffi {
    // The subset of `CoreEffect` bridged so far (NS1 slice 1). It grows as each effect is
    // wired to a native handler; anything not yet mapped arrives as `Other`.
    enum CoreEffectFfi {
        RequestRender,
        SetTitle(String),
        SetWakeSoon,
        ClearWake,
        Quit,
        Other,
    }

    extern "Rust" {
        type AppCoreHandle;

        #[swift_bridge(init)]
        fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle;

        fn key_down(
            &mut self,
            key: &str,
            ctrl: bool,
            shift: bool,
            alt: bool,
            logo: bool,
            repeat: bool,
        );
        fn key_up(&mut self, key: &str);
        fn focus_lost(&mut self);
        fn tick(&mut self);
        fn drain_effects(&mut self) -> Vec<CoreEffectFfi>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice-1 proof: an event driven in through the FFI produces effects the host can drain
    /// out — the full round-trip through the real `AppCore::handle`, and the queue is emptied
    /// by a drain. Escape resolves (default keymap) to Quit, which always enqueues an effect,
    /// so this holds even on a headless core with no photos.
    #[test]
    fn event_in_produces_effects_out() {
        let mut h = AppCoreHandle::new(1920, 1080, 2.0);
        h.key_down("Escape", false, false, false, false, false);
        assert!(
            !h.drain_effects().is_empty(),
            "an event should produce a drainable effect"
        );
        assert!(
            h.drain_effects().is_empty(),
            "draining empties the effect queue"
        );
    }

    /// An unknown key name is ignored (no panic, no effects) — the host can be liberal in
    /// what it forwards.
    #[test]
    fn unknown_key_name_is_ignored() {
        let mut h = AppCoreHandle::new(800, 600, 1.0);
        h.key_down("NotAKey", false, false, false, false, false);
        assert!(h.drain_effects().is_empty());
    }
}
