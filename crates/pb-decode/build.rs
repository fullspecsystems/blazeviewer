//! Build script for `pb-decode`.
//!
//! Handles the two optional native decode backends **independently** (a build may
//! enable either, both, or neither — task #76 restructured this from the old
//! libheif-only early-return so a dav1d-only build isn't silently skipped):
//!
//!   * **`libheif`** (HEVC/HEIC) — points the linker at libheif + libde265 and
//!     emits the `heic_libheif` cfg. Windows links the vcpkg static libs
//!     (decode-only, plugin-loader-free; `scripts/setup-libheif.ps1`); Linux/BSD
//!     links the system shared libheif.
//!   * **`dav1d`** (AV1, for animated AVIF — task #76) — **Windows-only** (macOS
//!     plays `avis` via Image I/O, Linux via FFmpeg): links the vcpkg static
//!     dav1d, compiles the C accessor shim (`csrc/dav1d_shim.c`) against the
//!     *same tree's* headers — so dav1d's structs never cross the FFI boundary
//!     by hand — and emits the `av1_dav1d` cfg. A no-op on other targets, so the
//!     feature is safe in a workspace-wide build.
//!
//! When both features are off this script only registers the cfgs and the crate
//! stays pure-Rust with zero native build risk (ADR-015). Both backends branch on
//! the **target** OS/arch (not the host — a build script runs on the host, so we
//! read `CARGO_CFG_TARGET_*`, which is correct under cross-compilation).

fn main() {
    // Register the cfgs we may set, so `#[cfg(heic_libheif)]` / `#[cfg(av1_dav1d)]`
    // never trip the unexpected-cfgs lint (Rust ≥ 1.80) when the features are off.
    println!("cargo:rustc-check-cfg=cfg(heic_libheif)");
    println!("cargo:rustc-check-cfg=cfg(av1_dav1d)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Cargo sets CARGO_FEATURE_<NAME> iff that feature is active for this build.
    if std::env::var_os("CARGO_FEATURE_LIBHEIF").is_some() {
        match target_os.as_str() {
            "windows" => {
                link_libheif_windows();
                println!("cargo:rustc-cfg=heic_libheif");
            }
            // Linux + the BSDs: system libheif. macOS is deliberately excluded (it
            // decodes HEIC via Image I/O; no libheif backend there).
            "linux" | "freebsd" | "dragonfly" | "netbsd" | "openbsd" => {
                link_libheif_unix();
                println!("cargo:rustc-cfg=heic_libheif");
            }
            // Any other target with the feature forced on: link nothing, set no
            // cfg — the backend simply isn't compiled, so the build stays sound.
            _ => {}
        }
    }

    if std::env::var_os("CARGO_FEATURE_DAV1D").is_some() && target_os == "windows" {
        build_dav1d_windows();
        println!("cargo:rustc-cfg=av1_dav1d");
    }

    if std::env::var_os("CARGO_FEATURE_FFMPEG").is_some() && target_os == "windows" {
        link_ffmpeg_windows_syslibs();
    }
}

/// Windows: the system libs a **static** vcpkg FFmpeg needs and `ffmpeg-sys-next`
/// doesn't emit (task #100).
///
/// Its build.rs says as much — *"vcpkg doesn't detect the 'system' dependencies"* — and
/// then emits only `ole32`, `secur32`, `ws2_32`, `bcrypt` and `user32`. That list is
/// short of what avformat/avcodec 8.1 actually reference, so the link fails with
/// `LNK2019` on three groups; each entry below is one of them, kept here rather than
/// patched upstream because this is the *linking* crate's job.
///
/// Nothing here changes the shipped feature set — these are OS import libraries, not
/// new dependencies. The FFmpeg objects that pull them in (its Schannel TLS and its own
/// MediaFoundation *encoder*) are dead weight for us: we only demux. Trimming them is
/// task #100's minimal-build subtask; until then they must resolve.
fn link_ffmpeg_windows_syslibs() {
    // Relink when the vcpkg FFmpeg is rebuilt. **This is not a nicety — it is a
    // correctness guard**, and it cost a real debugging detour to find (#100.1):
    // `ffmpeg-sys-next` emits no `rerun-if-changed` for the libs it links, and Cargo
    // cannot see that a `.lib` under vcpkg changed. Rebuild FFmpeg with different
    // configure options and, with no source edit to force its hand, Cargo happily
    // re-runs the *previously linked* binary — so a trimmed build silently tests as
    // the old one. That reads as "the trim broke the layout" when nothing broke at
    // all, and in a release it would ship an FFmpeg nobody chose. `link_libheif_windows`
    // guards heif.lib the same way, for the same reason.
    let (root, triplet) = vcpkg_tree();
    let libdir = format!("{root}\\installed\\{triplet}\\lib");
    for lib in ["avcodec", "avformat", "avutil"] {
        println!("cargo:rerun-if-changed={libdir}\\{lib}.lib");
    }
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");

    for lib in [
        // avformat's tls_schannel.o: Cert*/Crypt* (crypt32) and NCrypt* (ncrypt).
        "crypt32", "ncrypt",
        // avcodec's mfenc.o / mf_utils.o: IID_IMFTransform, IID_ICodecAPI,
        // IID_IMFMediaEventGenerator — the GUID definitions live in mfuuid.
        "mfuuid",
        // The DirectShow/MF GUID island mfuuid leans on (IID_IMFMediaEventGenerator's
        // neighbours); harmless when unreferenced.
        "strmiids",
    ] {
        println!("cargo:rustc-link-lib={lib}");
    }
}

/// Is this a **DLL** build of the native LGPL libraries? (`PB_VCPKG_DYNAMIC=1`)
///
/// Task #77 / #100.6: libheif + libde265 (LGPL-3.0 §4) and FFmpeg (LGPL-2.1 §6) are
/// *statically* linked by default, which does not satisfy the LGPL relink condition in a
/// proprietary binary. Dynamic linkage does, by construction — it is already how macOS and
/// Linux comply. This switch builds against the vcpkg **dynamic** triplet so the two can be
/// compared on real numbers before the owner picks a remedy.
///
/// ⚠ Not sufficient on its own: `ffmpeg-sys-next` picks its own triplet from
/// `CARGO_FEATURE_STATIC`, so a dynamic FFmpeg also needs `static` off in the
/// `ffmpeg-next` dependency (see pb-decode/Cargo.toml).
fn vcpkg_dynamic() -> bool {
    println!("cargo:rerun-if-env-changed=PB_VCPKG_DYNAMIC");
    std::env::var("PB_VCPKG_DYNAMIC").as_deref() == Ok("1")
}

/// The vcpkg tree the Windows native backends link from: `(root, triplet)`.
/// Root is `VCPKG_ROOT` or the conventional `~/vcpkg`; the triplet tracks the
/// *target* arch (read from `CARGO_CFG_TARGET_ARCH`, correct under
/// cross-compilation). static-md = static libs + dynamic CRT, matching Rust's
/// default MSVC CRT linkage (no DLLs to ship in the installer). Port versions are
/// pinned by `scripts/setup-libheif.ps1 -VcpkgRef` (libheif 1.23.0, libde265
/// 1.1.1, dav1d 1.5.3).
///
/// Under [`vcpkg_dynamic`] the triplet drops the `-static-md` suffix — vcpkg's plain
/// `x64-windows` / `arm64-windows` triplets are the DLL ones.
fn vcpkg_tree() -> (String, &'static str) {
    let root = std::env::var("VCPKG_ROOT").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        format!("{home}\\vcpkg")
    });
    let arm = matches!(
        std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("aarch64")
    );
    // x86_64 (and any other Windows arch we haven't special-cased) uses the x64 tree.
    let triplet = match (arm, vcpkg_dynamic()) {
        (true, false) => "arm64-windows-static-md",
        (false, false) => "x64-windows-static-md",
        (true, true) => "arm64-windows",
        (false, true) => "x64-windows",
    };
    (root, triplet)
}

/// Windows: point the linker at the vcpkg static libheif + libde265.
///
/// Phase 0 setup (one-time, see docs/heic-decode-plan.md):
///   pwsh scripts/setup-libheif.ps1 -Triplet <arch>-windows-static-md
/// `core` drops the x265 *encoder* default; libde265 (HEVC *decode*) is a hard
/// dependency so it's always present.
fn link_libheif_windows() {
    use std::path::Path;

    let (root, triplet) = vcpkg_tree();
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
    // libheif calls into libde265 for HEVC; link both. MSVC embeds the C++ runtime
    // default-lib directives in the objects, so the C++ stdlib resolves automatically
    // under the dynamic CRT (static-md).
    //
    // Under PB_VCPKG_DYNAMIC these are import libs for heif.dll / libde265.dll (task
    // #77's LGPL relink remedy) — same file names, different link kind.
    let kind = if vcpkg_dynamic() { "dylib" } else { "static" };
    println!("cargo:rustc-link-lib={kind}=heif");
    println!("cargo:rustc-link-lib={kind}=libde265");
    // Relink if the static lib is rebuilt (e.g. a vcpkg reinstall with different
    // options) — Cargo doesn't otherwise track the external lib.
    println!("cargo:rerun-if-changed={libdir}\\heif.lib");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
}

/// Windows: link the vcpkg static dav1d and compile the C accessor shim (task #76).
///
/// The shim (`csrc/dav1d_shim.c`) is the whole ABI-safety strategy: it's compiled
/// against the headers *installed next to the lib we link*, so `Dav1dSettings` /
/// `Dav1dPicture` layouts are resolved by the C compiler every build — Rust only
/// ever sees opaque pointers plus the shim's own stable surface. (Layout drift
/// between our code and the pinned dav1d is therefore structurally impossible,
/// instead of merely asserted; see the task 76 plan, "ABI safety".)
fn build_dav1d_windows() {
    use std::path::Path;

    let (root, triplet) = vcpkg_tree();
    let libdir = format!("{root}\\installed\\{triplet}\\lib");
    let include = format!("{root}\\installed\\{triplet}\\include");

    if !Path::new(&libdir).join("dav1d.lib").exists() {
        panic!(
            "feature `dav1d` is on but {libdir}\\dav1d.lib was not found.\n\
             Run:  pwsh scripts/setup-libheif.ps1 -Triplet {triplet}\n\
             (it installs the pinned dav1d port alongside libheif). Or set\n\
             VCPKG_ROOT if your vcpkg lives elsewhere.",
        );
    }

    println!("cargo:rustc-link-search=native={libdir}");
    // dav1d is BSD-2-Clause, so unlike libheif/FFmpeg it has no relink obligation and
    // could stay static under a DLL remedy. It follows the switch here only because one
    // vcpkg triplet supplies the whole tree — worth revisiting if the DLL count matters.
    println!(
        "cargo:rustc-link-lib={}=dav1d",
        if vcpkg_dynamic() { "dylib" } else { "static" }
    );

    // cc emits the link line for the shim archive itself.
    cc::Build::new()
        .file("csrc/dav1d_shim.c")
        .include(&include)
        .compile("pb_dav1d_shim");

    println!("cargo:rerun-if-changed=csrc/dav1d_shim.c");
    println!("cargo:rerun-if-changed={libdir}\\dav1d.lib");
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
