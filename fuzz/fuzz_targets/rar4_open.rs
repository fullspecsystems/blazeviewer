//! Fuzz the RAR4 container scan + solid decode (task #103): arbitrary bytes
//! through our own block-chain parser (`pb-source/src/rar4.rs`) and the compcol
//! rar3 codec must never panic, over-allocate, or spin — only `Ok`/`Err`. RAR4's
//! container is a completely different shape from RAR5, so it gets its own target
//! rather than relying on the `rar_open` target to flip the signature byte.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    pb_source::rar4_fuzz::rar4_open(data);
});
