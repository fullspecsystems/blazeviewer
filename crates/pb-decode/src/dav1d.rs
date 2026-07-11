//! Windows AV1 decode backend for animated AVIF (`avis`) playback — task #76.
//!
//! Everything dav1d goes through the C accessor shim (`csrc/dav1d_shim.c`),
//! compiled by build.rs against the pinned vcpkg tree's headers — dav1d's public
//! structs never cross the FFI boundary, so there is no hand-mirrored layout to
//! drift (see the task 76 plan, "ABI safety"). This module is the Rust half of
//! that surface: RAII wrappers over the shim's opaque pointers.
//!
//! Phase 2 scope: the version surface only (proves the vcpkg static lib links
//! and the shim compiles end to end). The decode pipeline (open → send/next →
//! YUV→RGB → `Animation`) lands with the demuxer in plan phases 3-6.

// Phase 2 stub: only the unit test consumes this surface until the decode
// pipeline (plan phases 3-6) wires it into decode_animation. Remove then.
#![allow(dead_code)]

use std::ffi::CStr;
use std::os::raw::c_char;

extern "C" {
    fn pb_dav1d_version() -> *const c_char;
    fn pb_dav1d_version_ok() -> i32;
}

/// The linked dav1d's version string (e.g. `"1.5.3"`, the vcpkg pin).
pub fn version() -> &'static str {
    // SAFETY: dav1d_version() returns a pointer to a static NUL-terminated
    // string inside the linked library; it is valid for the program's lifetime.
    unsafe { CStr::from_ptr(pb_dav1d_version()) }
        .to_str()
        .unwrap_or("<non-utf8>")
}

/// Whether the linked lib's API major matches the headers the shim was compiled
/// against. Checked before any call that hands dav1d a caller-owned struct;
/// `false` means a mixed/stale vcpkg tree and the backend must refuse to run
/// (return a `DecodeError`) rather than risk a struct-layout mismatch.
pub fn version_ok() -> bool {
    unsafe { pb_dav1d_version_ok() != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim + static lib link and agree on the API version, and the runtime
    /// version is the one the vcpkg pin promises (setup-libheif.ps1 -VcpkgRef).
    /// If the pin moves, update the expected prefix here deliberately.
    #[test]
    fn linked_dav1d_is_the_pinned_version() {
        assert!(
            version_ok(),
            "shim headers vs linked lib API-major mismatch"
        );
        assert!(
            version().starts_with("1.5"),
            "vcpkg pin moved? linked dav1d = {}",
            version()
        );
    }
}
