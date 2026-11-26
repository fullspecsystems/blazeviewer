//! Builds jpegli's encoder (`vendor/libjxl`, BSD-3-Clause) and `src/shim.cc` with the `cc` crate,
//! compiled against `vendor/highway`: a copy of the Highway headers jxl-sys builds and links.
//!
//! No Highway library is built or linked here. jpegli's runtime calls into Highway (picking the
//! CPU target, aligned allocation) resolve against jxl-sys's static libhwy, so the app carries
//! one Highway, and the code calling it was compiled against that same version's headers. The
//! jpegli-sys crate built a second libjxl with an older Highway whose library then won the link
//! for libjxl too (task #157, export-plan.md rung 4); this crate exists to avoid that.
//!
//! The headers must match jxl-sys's Highway exactly: Cargo.toml pins jxl-sys with `=`, and the
//! check below fails the build if `vendor/highway` is a different version. `vendor/README.md`
//! says how to refresh both together.

use std::path::Path;

/// The Highway release jxl-sys 0.1.12+libjxl-0.12.0 vendors (third_party/highway/hwy/base.h).
const HWY_VERSION: (u32, u32, u32) = (1, 2, 0);

/// jpegli's encoder half. The decoder files (decode*, idct, render, upsample, color_quantize,
/// source_manager) aren't vendored: nothing here decodes.
const JPEGLI_SOURCES: &[&str] = &[
    "adaptive_quantization.cc",
    "bit_writer.cc",
    "bitstream.cc",
    "color_transform.cc",
    "common.cc",
    "destination_manager.cc",
    "downsample.cc",
    "encode.cc",
    "encode_finish.cc",
    "encode_streaming.cc",
    "entropy_coding.cc",
    "error.cc",
    "huffman.cc",
    "input.cc",
    "memory_manager.cc",
    "quant.cc",
    "simd.cc",
];

fn main() {
    let vendor = Path::new("vendor");
    check_highway_version(&vendor.join("highway/hwy/base.h"));

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include(vendor.join("libjxl"))
        .include(vendor.join("libjxl/lib/include"))
        .include(vendor.join("libjpeg-turbo"))
        .include(vendor.join("highway"))
        // jxl-sys builds libhwy static, so its exported symbols carry no dllimport (Windows).
        .define("HWY_STATIC_DEFINE", None)
        // Optimized even in debug builds, as jxl-sys builds libjxl: the tests encode real images.
        .opt_level(2)
        .warnings(false)
        .file("src/shim.cc");
    for f in JPEGLI_SOURCES {
        build.file(vendor.join("libjxl/lib/jpegli").join(f));
    }
    if msvc {
        // The shim's error path throws through jpegli (see shim.cc); cc doesn't add /EHsc.
        // /bigobj: each file compiles once per Highway target.
        build
            .flag("/EHsc")
            .flag("/bigobj")
            .define("_CRT_SECURE_NO_WARNINGS", None);
    }
    build.compile("pb_jpegli");

    println!("cargo:rerun-if-changed=src/shim.cc");
    println!("cargo:rerun-if-changed=vendor");
}

/// Fail the build when `vendor/highway` isn't [`HWY_VERSION`].
fn check_highway_version(base_h: &Path) {
    let text = std::fs::read_to_string(base_h).expect("vendor/highway/hwy/base.h");
    let field = |name: &str| -> u32 {
        let prefix = format!("#define {name} ");
        text.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{name} not found in {}", base_h.display()))
    };
    let found = (field("HWY_MAJOR"), field("HWY_MINOR"), field("HWY_PATCH"));
    assert_eq!(
        found, HWY_VERSION,
        "vendor/highway is Highway {found:?}, but jxl-sys links {HWY_VERSION:?}: refresh both together (vendor/README.md)"
    );
}
