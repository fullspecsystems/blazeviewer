//! Fuzz the RAR5 container scan + solid decode (task #103): arbitrary bytes
//! through our own block-chain parser and the compcol rar5 codec must never
//! panic, over-allocate, or spin — only `Ok`/`Err`. (compcol upstream has no
//! dedicated rar5 fuzz target; this covers our integration, like the spike's
//! fork branch covers the raw codec.)
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    pb_source::rar_fuzz::rar_open(data);
});
