//! Generate the swift-bridge Swift + C glue for the Xcode build (NS1).
//!
//! macOS-only: on any other host the `ffi` module in `lib.rs` is `cfg`'d out and the
//! `swift-bridge-build` dependency isn't present, so this is a no-op. The Rust side of the
//! bridge is produced by the `#[swift_bridge::bridge]` proc-macro during compilation; this
//! build script only emits the *Swift-facing* files (`generated/pb-mac-ffi/…`) that the
//! Xcode target imports.
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");

    #[cfg(target_os = "macos")]
    {
        let out_dir = std::path::PathBuf::from("generated");
        swift_bridge_build::parse_bridges(vec!["src/lib.rs"])
            .write_all_concatenated(out_dir, env!("CARGO_PKG_NAME"));
    }
}
