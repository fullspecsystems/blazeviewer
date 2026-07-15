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

    // Whichever of the three is on, a DLL build needs its DLLs beside the binaries cargo is
    // producing (task #77). Done once here rather than per-feature: the copy is by directory,
    // so `libheif` + `dav1d` + `ffprobe` would otherwise each redo the same work — and a
    // `ffprobe`-only build needs it just as much, without ever touching the libheif hook.
    let any_native = ["LIBHEIF", "DAV1D", "FFMPEG"]
        .iter()
        .any(|f| std::env::var_os(format!("CARGO_FEATURE_{f}")).is_some());
    if any_native && target_os == "windows" && vcpkg_dynamic() {
        let (root, triplet) = vcpkg_tree();
        copy_vcpkg_dlls(&root, triplet);
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

/// Windows links the native libraries as **DLLs** (`true`), with `PB_VCPKG_STATIC=1` as the
/// escape hatch back to the old static link.
///
/// **This is a licence requirement, not a preference (task #77).** libheif + libde265 are
/// LGPL-3.0 (§4) and FFmpeg is LGPL-2.1 (§6); both oblige us to let a user relink the app
/// against a modified copy of the library, and a static link in a proprietary binary does not.
/// A DLL does, by construction — which is why macOS and Linux were always compliant and only
/// Windows was not. Measured cost of the switch: **+0.8 MB** on-disk (exe 35.07 -> 29.04 plus
/// 6.82 MB of DLLs); the feared size regression did not materialise.
///
/// The escape hatch exists for A/B measurement only. **A `PB_VCPKG_STATIC=1` build must never
/// ship** — it is the non-compliant configuration by definition.
///
/// ⚠ Only half the story: `ffmpeg-sys-next` chooses its own triplet from `CARGO_FEATURE_STATIC`
/// and runs *before* this build script, so it cannot be steered from here. FFmpeg follows
/// because pb-decode's `ffmpeg-next` dependency deliberately does **not** enable `static` (see
/// Cargo.toml) — re-adding it there silently makes FFmpeg static again while these stay DLLs.
fn vcpkg_dynamic() -> bool {
    println!("cargo:rerun-if-env-changed=PB_VCPKG_STATIC");
    std::env::var("PB_VCPKG_STATIC").as_deref() != Ok("1")
}

/// Copy the vcpkg DLLs next to the binaries cargo is about to produce.
///
/// Windows resolves a DLL from the **executable's own directory** first, and cargo puts the
/// app in `target/<profile>/` but test binaries in `target/<profile>/deps/`. Without this,
/// switching to DLLs would leave every `cargo run` and `cargo test` failing to start with a
/// missing-DLL box — the classic tax for going dynamic on Windows, paid once here rather than
/// by every developer and CI lane in a PATH dance.
///
/// The release script stages its DLLs from `target/release/`, so this is also what makes the
/// shipped package's DLL set identical to the one the tests ran against.
fn copy_vcpkg_dlls(root: &str, triplet: &str) {
    use std::path::{Path, PathBuf};

    let bindir = PathBuf::from(format!("{root}\\installed\\{triplet}\\bin"));
    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out — three levels up is target/<profile>.
    let Ok(out) = std::env::var("OUT_DIR") else {
        return;
    };
    let Some(profile_dir) = Path::new(&out).ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&bindir) else {
        // No bin dir means a static tree is installed; the link step will report that far
        // more clearly than a copy failure would.
        return;
    };
    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        for dst_dir in [profile_dir.clone(), profile_dir.join("deps")] {
            if !dst_dir.is_dir() {
                continue;
            }
            let dst = dst_dir.join(name);
            // Copy when absent or stale. Racing cargo jobs can both land here, and a failed
            // copy is not fatal — the loader will say so at run time, loudly.
            let fresh = std::fs::metadata(&dst)
                .ok()
                .zip(entry.metadata().ok())
                .and_then(|(d, s)| Some((d.modified().ok()?, s.modified().ok()?)))
                .is_some_and(|(d, s)| d >= s);
            if !fresh {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }
    println!("cargo:rerun-if-changed={}", bindir.display());
}

/// The vcpkg tree the Windows native backends link from: `(root, triplet)`.
///
/// The triplet tracks the *target* arch (`CARGO_CFG_TARGET_ARCH`, so it stays correct under
/// cross-compilation) and the link kind from [`vcpkg_dynamic`]: plain `x64-windows` /
/// `arm64-windows` are vcpkg's DLL triplets and are what we ship; the `-static-md` suffix
/// (static libs + dynamic CRT) is the `PB_VCPKG_STATIC=1` escape hatch. Port versions are
/// pinned by `scripts/setup-libheif.ps1 -VcpkgRef` (libheif 1.23.0, libde265 1.1.1,
/// dav1d 1.5.3).
///
/// Root is `VCPKG_ROOT`, else the conventional `~/vcpkg`.
///
/// ⚠ `VCPKG_ROOT` is not as stable as it looks: `Enter-VsDevShell` **overwrites** it with the
/// vcpkg bundled inside Visual Studio, which carries none of our pinned ports — even if the
/// caller exported the right value first. Since `--features ffprobe` requires a Developer shell
/// (bindgen), every shipped build goes through one; `scripts/vs-dev-env.ps1` restores the value
/// afterwards. Anything else entering a VS shell by hand has to do the same.
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

/// Windows: point the linker at the vcpkg libheif + libde265 — as **DLLs** by default
/// (task #77: LGPL-3.0 §4 relink), or static libs under the `PB_VCPKG_STATIC=1` escape
/// hatch, per [`vcpkg_tree`].
///
/// Phase 0 setup (one-time, see docs/heic-decode-plan.md) — the DLL triplet, since that
/// is what ships; `-static-md` only for a `PB_VCPKG_STATIC=1` A/B measurement:
///   pwsh scripts/setup-libheif.ps1 -Triplet <arch>-windows
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
             (it bootstraps vcpkg + builds a decode-only, plugin-loader-free\n\
             libheif for this arch). Or set VCPKG_ROOT if your vcpkg lives elsewhere.\n\
             Note the triplet: the shipped build links DLLs for LGPL relink compliance\n\
             (task #77), so the tree must be the dynamic one unless PB_VCPKG_STATIC=1.",
        );
    }

    println!("cargo:rustc-link-search=native={libdir}");
    // libheif calls into libde265 for HEVC; link both. MSVC embeds the C++ runtime
    // default-lib directives in the objects, so the C++ stdlib resolves automatically
    // under the dynamic CRT (static-md).
    //
    // Under the DLL triplet these are import libs for heif.dll / libde265.dll (task #77's
    // LGPL relink remedy). ⚠ libde265's *import* lib is `de265.lib` while its *static* lib
    // is `libde265.lib` — the vcpkg triplets disagree on that one name (heif and dav1d keep
    // theirs), so the link name has to track the triplet, not just the link kind.
    let dynamic = vcpkg_dynamic();
    let kind = if dynamic { "dylib" } else { "static" };
    println!("cargo:rustc-link-lib={kind}=heif");
    println!(
        "cargo:rustc-link-lib={kind}={}",
        if dynamic { "de265" } else { "libde265" }
    );
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
