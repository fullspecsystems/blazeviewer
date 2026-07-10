//! Build script for `pb-decode`.
//!
//! Only does something when the optional `libheif` feature is enabled: it points
//! the linker at libheif + libde265 (HEVC decode) so the `libheif` module's
//! hand-rolled FFI resolves, and emits the `heic_libheif` cfg the crate gates the
//! backend on. When the feature is off this script is a no-op and the crate stays
//! pure-Rust with zero native build risk (ADR-015).
//!
//! Two link strategies, picked by the **target** OS (not the host — a build script
//! runs on the host, so we read `CARGO_CFG_TARGET_OS`, which is correct under
//! cross-compilation):
//!   * **Windows** — a vcpkg-built *static* libheif+libde265, decode-only and
//!     plugin-loader-free (Phase 0, `scripts/setup-libheif.ps1`). Static so there
//!     are no DLLs to ship in the installer.
//!   * **Linux** (and other non-mac unixes) — the *system* shared libheif
//!     (`apt install libheif-dev`); libheif loads libde265 as a runtime plugin, so
//!     we link only `heif`.

fn main() {
    // Register the cfg we may set, so `#[cfg(heic_libheif)]` never trips the
    // unexpected-cfgs lint (Rust ≥ 1.80) when the feature is off.
    println!("cargo:rustc-check-cfg=cfg(heic_libheif)");

    // Cargo sets this env var iff the `libheif` feature is active for this build.
    if std::env::var_os("CARGO_FEATURE_LIBHEIF").is_none() {
        return;
    }

    // Branch on the *target* OS (a build script otherwise sees only the host).
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => {
            link_libheif_windows();
            println!("cargo:rustc-cfg=heic_libheif");
        }
        // Linux + the BSDs: system libheif. macOS is deliberately excluded (it
        // decodes HEIC via Image I/O; no libheif backend there).
        Ok("linux" | "freebsd" | "dragonfly" | "netbsd" | "openbsd") => {
            link_libheif_unix();
            println!("cargo:rustc-cfg=heic_libheif");
        }
        // Any other target with the feature forced on: link nothing, set no cfg —
        // the backend simply isn't compiled, so the build stays sound.
        _ => {}
    }
}

/// Windows: point the linker at the vcpkg static libheif + libde265.
///
/// Phase 0 setup (one-time, see docs/heic-decode-plan.md):
///   <VCPKG_ROOT>/vcpkg install "libheif[core]:x64-windows-static-md"
/// `core` drops the x265 *encoder* default; libde265 (HEVC *decode*) is a hard
/// dependency so it's always present. static-md = static libs + dynamic CRT,
/// matching Rust's default MSVC CRT linkage (so no DLLs to ship in the installer).
fn link_libheif_windows() {
    use std::path::Path;

    // VCPKG_ROOT, or the conventional ~/vcpkg from the Phase-0 install.
    let root = std::env::var("VCPKG_ROOT").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        format!("{home}\\vcpkg")
    });
    // static-md: static libs, dynamic CRT — matches Rust MSVC's default CRT. The triplet
    // tracks the *target* arch (read from CARGO_CFG_TARGET_ARCH, correct under cross-compile),
    // so a native ARM64 build links the arm64 vcpkg tree and an x64 build the x64 one. Run
    // `scripts/setup-libheif.ps1 -Triplet <arch>-windows-static-md` once per arch you ship.
    let triplet = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64-windows-static-md",
        // x86_64 (and any other Windows arch we haven't special-cased) uses the x64 tree.
        _ => "x64-windows-static-md",
    };
    let libdir = format!("{root}\\installed\\{triplet}\\lib");

    if !Path::new(&libdir).join("heif.lib").exists() {
        panic!(
            "feature `libheif` is on but {libdir}\\heif.lib was not found.\n\
             Run Phase 0:  pwsh scripts/setup-libheif.ps1 -Triplet {triplet}\n\
             (it bootstraps vcpkg + builds a decode-only, plugin-loader-free static\n\
             libheif for this arch). Or set VCPKG_ROOT if your vcpkg lives elsewhere.",
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

/// Linux / BSD: link the system shared libheif (`apt install libheif-dev`).
///
/// Unlike the Windows static build, the distro libheif loads its HEVC decoder
/// (libde265) as a *runtime plugin*, so we link only `heif` and let it pull the
/// codec in. Probe `pkg-config` for a non-standard install prefix; a stock
/// `libheif-dev` lands in the default linker search path, so the bare `-lheif`
/// resolves even if pkg-config isn't present.
fn link_libheif_unix() {
    if let Ok(out) = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", "libheif"])
        .output()
    {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for tok in stdout.split_whitespace() {
                if let Some(dir) = tok.strip_prefix("-L") {
                    println!("cargo:rustc-link-search=native={dir}");
                }
            }
        }
    }
    println!("cargo:rustc-link-lib=dylib=heif");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
}
