//! **The live 3D model view** (task #153; plan `.taskmaster/docs/3d-models-plan.md`).
//!
//! A model item shows as a still (task #151) until the user turns or zooms it. Then the GPU
//! draws the mesh live from a camera the core owns. This module holds that state and the
//! off-thread preparation; the `AppCore` side (input routing, drawing, teardown) is the
//! concern file `app_core_impl/live_model.rs`.
//!
//! **Anticipation, per the Prime Directive.** The likeliest next action on a parked model is to
//! turn it, so the mesh is parsed on a worker as soon as the user settles on one, and uploaded to
//! the GPU when it arrives. The first drag is then a camera update, never a parse or an upload.
//! (Owner decision D1: a drag simply starts orbiting, with no mode to enter.)
//!
//! **Privacy.** Everything here is RAM/VRAM and dies with the item: it is dropped the moment
//! the user moves on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use pb_mesh::{LiveView, ModelFormat};
use pb_render::MeshVertex;
use pb_source::ItemSource;

/// The most triangles the live view will put on the GPU. A soup costs 48 bytes per triangle,
/// so this caps the vertex memory near 290 MB. Bigger models keep their still (Thingi10K's
/// largest is 5.67M triangles).
pub const LIVE_MAX_TRIANGLES: usize = 6_000_000;

/// The turntable's cruising speed (`P` on a model): a full turn every 12 seconds, slow enough
/// to read the model as it goes round.
pub const SPIN_RADIANS_PER_SEC: f32 = std::f32::consts::PI / 6.0;
/// How long the turntable takes to reach speed, and to coast to a stop: a glide, not a jerk.
pub const SPIN_EASE_SECS: f32 = 0.4;

/// The turntable: a speed that eases towards its target, so starting and stopping glide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spin {
    /// Radians per second right now.
    speed: f32,
    /// Where `speed` is heading: cruising speed, or 0 while it coasts to a stop.
    target: f32,
    /// The last tick, so a step turns by the time actually elapsed.
    last: Option<Instant>,
}

impl Spin {
    /// From rest, spinning up. The first [`advance`](Self::advance) only starts the clock.
    pub fn start() -> Spin {
        Spin {
            speed: 0.0,
            target: SPIN_RADIANS_PER_SEC,
            last: None,
        }
    }

    /// Spinning or spinning up (`false` while it coasts to a stop).
    pub fn running(&self) -> bool {
        self.target > 0.0
    }

    /// Coast to a stop.
    pub fn stop(&mut self) {
        self.target = 0.0;
    }

    /// Spin back up (`P` again while it coasts).
    pub fn resume(&mut self) {
        self.target = SPIN_RADIANS_PER_SEC;
    }

    /// Coasted all the way to rest: nothing left to turn.
    pub fn stopped(&self) -> bool {
        self.target == 0.0 && self.speed == 0.0
    }

    /// Move the clock to `now` and return how far to turn (radians). A long gap between ticks
    /// (a stalled loop) counts as a tenth of a second, so the model never jumps.
    pub fn advance(&mut self, now: Instant) -> f32 {
        let dt = match self.last.replace(now) {
            Some(t) => now.saturating_duration_since(t).as_secs_f32().min(0.1),
            None => 0.0,
        };
        let v0 = self.speed;
        let step = SPIN_RADIANS_PER_SEC / SPIN_EASE_SECS * dt;
        self.speed = if self.target > v0 {
            (v0 + step).min(self.target)
        } else {
            (v0 - step).max(self.target)
        };
        (v0 + self.speed) * 0.5 * dt
    }
}

/// What the worker hands back: the model's camera and its GPU-ready soup.
pub struct Prepared {
    pub view: LiveView,
    pub vertices: Vec<MeshVertex>,
}

/// A preparation in flight.
pub struct Loading {
    pub item: usize,
    pub content_gen: u64,
    pub rx: Receiver<Result<Prepared, String>>,
    pub cancel: Arc<AtomicBool>,
}

impl Drop for Loading {
    /// Whoever drops a load (superseded, navigated away, torn down) stops its worker.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The model on the GPU, ready to turn.
pub struct Ready {
    pub item: usize,
    pub content_gen: u64,
    pub view: LiveView,
}

/// The live view's state on `AppCore`. Default: nothing loaded, nothing showing.
#[derive(Default)]
pub struct LiveModel {
    pub loading: Option<Loading>,
    pub ready: Option<Ready>,
    /// The live view is on screen in place of the still: set by the first turn or zoom.
    pub active: bool,
    /// The last item whose preparation failed, so it isn't retried every tick.
    pub failed: Option<(usize, u64)>,
    /// The turntable, while the model spins on its own (`P`).
    pub spin: Option<Spin>,
    /// `P` came before the model was ready: spin this item (and deck generation) as it lands.
    pub spin_wanted: Option<(usize, u64)>,
}

impl LiveModel {
    /// The ready model's camera, if it is `item` of deck generation `content_gen`.
    pub fn ready_for(&self, item: usize, content_gen: u64) -> Option<&Ready> {
        self.ready
            .as_ref()
            .filter(|r| r.item == item && r.content_gen == content_gen)
    }

    pub fn ready_for_mut(&mut self, item: usize, content_gen: u64) -> Option<&mut Ready> {
        self.ready
            .as_mut()
            .filter(|r| r.item == item && r.content_gen == content_gen)
    }

    /// Whether anything (loading or ready) is held for an item other than `item`.
    pub fn holds_other_than(&self, item: Option<usize>, content_gen: u64) -> bool {
        let other = |i: usize, g: u64| Some(i) != item || g != content_gen;
        self.loading
            .as_ref()
            .is_some_and(|l| other(l.item, l.content_gen))
            || self
                .ready
                .as_ref()
                .is_some_and(|r| other(r.item, r.content_gen))
    }
}

/// The model format of `name` (a file name or archive entry name), if it is one.
pub fn model_format(name: &str) -> Option<ModelFormat> {
    let ext = std::path::Path::new(name).extension()?.to_str()?;
    ModelFormat::from_extension(ext)
}

/// The worker's job: read `item`'s bytes, parse, frame, and build the GPU soup. Checks
/// `cancel` between the expensive steps.
pub fn prepare(
    source: &dyn ItemSource,
    item: usize,
    cancel: &AtomicBool,
) -> Result<Prepared, String> {
    let format = model_format(source.name(item)).ok_or("not a 3D model")?;
    let bytes = source.bytes(item).map_err(|e| format!("read error: {e}"))?;
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let limits = pb_mesh::Limits {
        max_triangles: LIVE_MAX_TRIANGLES,
        ..pb_mesh::Limits::default()
    };
    // Parse under catch_unwind like every other decode: the bytes are hostile input.
    let model = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pb_mesh::parse(format, &bytes, &limits)
    }))
    .map_err(|_| "parser panicked".to_string())?
    .map_err(|e| e.to_string())?;
    drop(bytes);
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let view = LiveView::new(&model).ok_or("nothing to draw")?;
    let vertices = pb_mesh::live::corners(&model)
        .map(|(pos, [r, g, b])| MeshVertex {
            pos,
            color: [r, g, b, 255],
        })
        .collect();
    Ok(Prepared { view, vertices })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_format_reads_the_name() {
        assert_eq!(model_format("a/b/part.STL"), Some(ModelFormat::Stl));
        assert_eq!(model_format("plate.gcode.3mf"), Some(ModelFormat::ThreeMf));
        assert_eq!(model_format("photo.jpg"), None);
        assert_eq!(model_format("noext"), None);
    }

    #[test]
    fn dropping_a_load_cancels_its_worker() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = std::sync::mpsc::channel();
        drop(Loading {
            item: 0,
            content_gen: 0,
            rx,
            cancel: Arc::clone(&cancel),
        });
        assert!(cancel.load(Ordering::Relaxed));
    }

    /// Tick a spin every `step` for `secs`, returning the total turn.
    fn run(spin: &mut Spin, t: &mut Instant, secs: f32, step: std::time::Duration) -> f32 {
        let mut turned = 0.0;
        let end = *t + std::time::Duration::from_secs_f32(secs);
        while *t < end {
            *t += step;
            turned += spin.advance(*t);
        }
        turned
    }

    #[test]
    fn the_turntable_spins_up_cruises_and_coasts_to_a_stop() {
        let frame = std::time::Duration::from_millis(8);
        let mut t = Instant::now();
        let mut spin = Spin::start();
        assert_eq!(spin.advance(t), 0.0, "the first tick only starts the clock");
        // Spinning up covers half the cruising distance of the same time.
        let ease = run(&mut spin, &mut t, SPIN_EASE_SECS, frame);
        assert!((ease - SPIN_RADIANS_PER_SEC * SPIN_EASE_SECS / 2.0).abs() < 0.01);
        // Then a steady 30°/s: a turn every 12 s.
        let second = run(&mut spin, &mut t, 1.0, frame);
        assert!((second - SPIN_RADIANS_PER_SEC).abs() < 0.01, "{second}");
        assert!(spin.running());
        spin.stop();
        assert!(!spin.running() && !spin.stopped(), "coasting");
        let coast = run(&mut spin, &mut t, SPIN_EASE_SECS + 0.05, frame);
        assert!(coast > 0.0 && coast < SPIN_RADIANS_PER_SEC * SPIN_EASE_SECS);
        assert!(spin.stopped());
    }

    #[test]
    fn p_while_coasting_spins_back_up() {
        let frame = std::time::Duration::from_millis(8);
        let mut t = Instant::now();
        let mut spin = Spin::start();
        spin.advance(t);
        run(&mut spin, &mut t, 1.0, frame);
        spin.stop();
        run(&mut spin, &mut t, 0.1, frame);
        spin.resume();
        run(&mut spin, &mut t, 1.0, frame);
        assert!(spin.running());
        assert!((run(&mut spin, &mut t, 1.0, frame) - SPIN_RADIANS_PER_SEC).abs() < 0.01);
    }

    #[test]
    fn a_stalled_loop_never_makes_the_model_jump() {
        let mut t = Instant::now();
        let mut spin = Spin::start();
        spin.advance(t);
        run(&mut spin, &mut t, 1.0, std::time::Duration::from_millis(8));
        t += std::time::Duration::from_secs(5);
        assert!(spin.advance(t) <= SPIN_RADIANS_PER_SEC * 0.1 + 1e-6);
    }

    #[test]
    fn holds_other_than_tracks_item_and_generation() {
        let mut m = LiveModel::default();
        assert!(!m.holds_other_than(Some(3), 1));
        let cube = pb_mesh::parse(
            ModelFormat::Stl,
            b"solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid t\n",
            &pb_mesh::Limits::default(),
        )
        .unwrap();
        m.ready = Some(Ready {
            item: 3,
            content_gen: 1,
            view: LiveView::new(&cube).unwrap(),
        });
        assert!(!m.holds_other_than(Some(3), 1));
        assert!(m.holds_other_than(Some(4), 1));
        assert!(
            m.holds_other_than(Some(3), 2),
            "same index in a new deck is a different item"
        );
        assert!(m.holds_other_than(None, 1));
    }
}
