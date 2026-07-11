//! Fuzz the avis demuxer/probe (task #76): arbitrary bytes must never panic,
//! overflow, or over-allocate — only `Ok`/`Err`. The probe is pure Rust (no
//! dav1d), so this runs without any native decoder linked.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    pb_decode::fuzz_probe_avis(data);
});
