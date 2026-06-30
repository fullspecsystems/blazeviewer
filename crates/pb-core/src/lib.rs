//! `pb-core` — the pure heart of PhotoBlaze.
//!
//! Navigation, precomputed-random ordering, prefetch-window policy, and
//! cache-residency planning live here as **deterministic, dependency-free,
//! fully unit-testable** functions. No file I/O, no decoding, no GPU.
//!
//! Why this separation matters for the project's prime directive ("will this
//! make it faster, or have basically zero performance impact?"):
//!
//! * The hot path (keypress -> show next photo) reduces to a cache lookup. The
//!   logic that decides *what to have ready* is exactly this crate, and getting
//!   it right is what keeps the cache warm so the lookup is a hit.
//! * Because it is pure, we can A/B different prefetch and eviction policies in
//!   microbenchmarks with zero GPU/I/O noise, and reach >80% coverage trivially.

pub mod cache;
pub mod open;
pub mod playlist;
pub mod prefetch;
pub mod ring;
pub mod rng;
pub mod shuffle;

pub use cache::{plan_residency, ResidencyPlan};
pub use open::{plan, resolve_cursor, Cursor, LaunchInput, OpenPlan, Source};
pub use playlist::{Direction, Playlist};
pub use prefetch::{full_ring, prefetch_targets, prefetch_targets_scanning};
pub use ring::{Reservation, ResidentRing};
pub use rng::SplitMix64;
pub use shuffle::ShuffleOrder;
