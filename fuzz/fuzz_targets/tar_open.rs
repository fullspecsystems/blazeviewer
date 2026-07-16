//! Fuzz the tar-family archive opens (task #102): arbitrary bytes through the
//! lazy plain-tar index pass and the eager compressed streaming pass (all four
//! codecs) must never panic, over-allocate, or spin — only `Ok`/`Err`. The
//! first input byte selects the path: bit 2 lazy/eager, bits 0-1 the codec.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    if sel & 4 == 0 {
        pb_source::fuzz::tar_index(rest);
    } else {
        pb_source::fuzz::tar_stream(sel, rest);
    }
});
