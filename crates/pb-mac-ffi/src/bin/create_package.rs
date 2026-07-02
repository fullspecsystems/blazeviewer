//! Package the **built** `pb-mac-ffi` staticlib + the swift-bridge glue from `generated/`
//! into a local Swift package at `crates/pb-mac-ffi/PbMacFfi/` (git-ignored build artifact)
//! — an `RustXcframework.xcframework` binary target + the generated `.swift` sources — which
//! the `mac/` SwiftPM app target consumes as a path dependency (NS1 item 1).
//!
//! A `[[bin]]` rather than a build.rs step because it needs the *already-built* `.a`.
//! Normally driven by `scripts/build-swift-host.sh`; by hand:
//!
//! ```sh
//! cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin
//! cargo run -p pb-mac-ffi --features package --bin create-package            # release
//! cargo run -p pb-mac-ffi --features package --bin create-package -- --debug # debug .a
//! ```

#[cfg(target_os = "macos")]
fn main() {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use swift_bridge_build::{create_package, ApplePlatform, CreatePackageConfig};

    let profile = if std::env::args().any(|a| a == "--debug") {
        "debug"
    } else {
        "release"
    };
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // arm64-only (ADR-021): the one platform slice in the xcframework.
    let lib = manifest.join(format!(
        "../../target/aarch64-apple-darwin/{profile}/libpb_mac_ffi.a"
    ));
    assert!(
        lib.exists(),
        "{} not found — build it first:\n  cargo build -p pb-mac-ffi {}--target aarch64-apple-darwin",
        lib.display(),
        if profile == "release" { "--release " } else { "" },
    );

    let out_dir = manifest.join("PbMacFfi");
    create_package(CreatePackageConfig {
        bridge_dir: manifest.join("generated"),
        paths: HashMap::from([(ApplePlatform::MacOS, lib)]),
        out_dir: out_dir.clone(),
        package_name: "PbMacFfi".to_string(),
    });

    // swift-bridge's generated runtime glue predates Swift 5.10's `@retroactive`: it
    // declares conformances (RustStr: Identifiable/Equatable) on types the compiler sees
    // as imported from the RustXcframework module, which warns on every build. Both sides
    // of the "conformance" are swift-bridge's own code, so the warning is moot — annotate
    // it away in the copy we just wrote. A plain string replace: if upstream's text ever
    // changes this becomes a harmless no-op (and the warning returns as the signal).
    let core_swift = out_dir.join("Sources/PbMacFfi/SwiftBridgeCore.swift");
    if let Ok(src) = std::fs::read_to_string(&core_swift) {
        let patched = src
            .replace(
                "extension RustStr: Identifiable {",
                "extension RustStr: @retroactive Identifiable {",
            )
            .replace(
                "extension RustStr: Equatable {",
                "extension RustStr: @retroactive Equatable {",
            );
        if patched != src {
            let _ = std::fs::write(&core_swift, patched);
        }
    }

    println!("Swift package written to {}", out_dir.display());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("create-package is macOS-only (it packages the Swift host's FFI bridge).");
    std::process::exit(1);
}
