//! Build script for `pb-decode`.
//!
//! Only does something when the optional `libheif` feature is enabled: it points
//! the linker at the vcpkg-built static libheif + libde265 (HEVC decode) so the
//! `libheif` module's hand-rolled FFI resolves. No `vcpkg` build-dependency — the
//! install layout is fixed and the link directives are spelled out, which is
//! deterministic and self-documenting (no triplet-detection magic, no transitive
//! dependency guessing). When the feature is off this script is a no-op and the
//! crate stays pure-Rust with zero native build risk (ADR-015).
//!
//! Phase 0 setup (one-time, see docs/heic-decode-plan.md):
//!   <VCPKG_ROOT>/vcpkg install "libheif[core]:x64-windows-static-md"
//! `core` drops the x265 *encoder* default; libde265 (HEVC *decode*) is a hard
//! dependency so it's always present. static-md = static libs + dynamic CRT,
//! matching Rust's default MSVC CRT linkage (so no DLLs to ship in the MSI).

fn main() {
    #[cfg(feature = "libheif")]
    link_libheif();
}

#[cfg(feature = "libheif")]
fn link_libheif() {
    use std::path::Path;

    // VCPKG_ROOT, or the conventional ~/vcpkg from the Phase-0 install.
    let root = std::env::var("VCPKG_ROOT").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        format!("{home}\\vcpkg")
    });
    // static-md: static libs, dynamic CRT — matches Rust MSVC's default CRT.
    let triplet = "x64-windows-static-md";
    let libdir = format!("{root}\\installed\\{triplet}\\lib");

    if !Path::new(&libdir).join("heif.lib").exists() {
        panic!(
            "feature `libheif` is on but {libdir}\\heif.lib was not found.\n\
             Run Phase 0:  pwsh scripts/setup-libheif.ps1\n\
             (it bootstraps vcpkg + builds a decode-only, plugin-loader-free static\n\
             libheif). Or set VCPKG_ROOT if your vcpkg lives elsewhere.",
        );
    }

    println!("cargo:rustc-link-search=native={libdir}");
    // libheif calls into libde265 for HEVC; both are static, link both. MSVC
    // embeds the C++ runtime default-lib directives in the objects, so the C++
    // stdlib resolves automatically under the dynamic CRT (static-md).
    println!("cargo:rustc-link-lib=static=heif");
    println!("cargo:rustc-link-lib=static=libde265");
    // Relink if the static lib is rebuilt (e.g. a vcpkg reinstall with different
    // options) — Cargo doesn't otherwise track the external lib.
    println!("cargo:rerun-if-changed={libdir}\\heif.lib");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
}
