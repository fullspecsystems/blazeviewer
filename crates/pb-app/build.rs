//! Build script: embed the application manifest, icon, and version metadata into
//! the Windows executable.
//!
//! The **manifest** is the important one for behavior: it declares PhotoBlaze a
//! modern Windows 10/11, Per-Monitor-V2-DPI app. Without it Windows treats the
//! binary as *legacy* and renders the window's non-client area (notably during
//! the borderless-fullscreen transition, when DWM briefly stops compositing it)
//! in the old "Basic" theme — that light-blue Vista-style caption — instead of
//! the modern composited title bar. The icon + version strings drive Explorer,
//! the taskbar, and the file-association glyph.
//!
//! **Best-effort:** if the resource compiler (`rc.exe` / `llvm-rc`) isn't on the
//! machine, we emit a warning and continue — the build still succeeds. The
//! runtime window icon (set via winit from the embedded PNG) applies regardless.

/// Declares modern-OS support + Per-Monitor-V2 DPI awareness, and the
/// **Common-Controls v6** dependency. (winit also sets DPI awareness
/// programmatically; the manifest value matches, so it's a no-op there — but the
/// `supportedOS` block is what stops Windows applying legacy non-client rendering.)
/// The Common-Controls v6 `<dependency>` is required for the native About dialog:
/// `TaskDialogIndirect` is only exported by comctl32 **v6**, so without it the
/// process loads v5 and the call fails with "entry point not found".
#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="ca.fullspec.BlazeViewer" version="1.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}"/>
    </application>
  </compatibility>
</assembly>
"#;

/// Capture a short build identifier from git for the About dialog, so a local build can be
/// traced back to the exact commit it was built from (handy while smoke-testing a series of
/// commits). `Some("ca29dcc")`, or `Some("ca29dcc-dirty")` when the working tree has
/// uncommitted changes; `None` when git isn't available (e.g. a build from a source tarball),
/// in which case the About card just shows the version. Runs at build time only — no runtime
/// git call, no viewing trace (privacy #2).
fn git_build_id() -> Option<String> {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let hash = run(&["rev-parse", "--short", "HEAD"])?;
    let hash = String::from_utf8(hash.stdout).ok()?.trim().to_string();
    if hash.is_empty() {
        return None;
    }
    // `git status --porcelain` prints one line per changed/untracked path → non-empty = dirty.
    let dirty = run(&["status", "--porcelain"]).is_some_and(|o| !o.stdout.is_empty());
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}

/// Re-run this script (refreshing the build id) when the checked-out commit moves. Uses
/// `git rev-parse --git-path` so the paths are correct even inside a git *worktree* (where
/// `.git` is a file and HEAD lives under the parent repo's `worktrees/<name>/`). Unstaged
/// edits can't retrigger a rebuild by themselves, so the `-dirty` flag only refreshes on the
/// next rebuild — acceptable for a build stamp.
fn rerun_on_git_head() {
    for path in ["HEAD", "index"] {
        if let Some(out) = std::process::Command::new("git")
            .args(["rev-parse", "--git-path", path])
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            if let Ok(p) = String::from_utf8(out.stdout) {
                println!("cargo:rerun-if-changed={}", p.trim());
            }
        }
    }
}

fn main() {
    // pb-app is the Windows/Linux winit shell; macOS ships via the native SwiftUI host
    // (mac/ + pb-mac-ffi). Guard on the SUPPORTED targets HERE in the build script — it runs
    // before rustc type-checks the crate, so an unsupported target fails with ONE clean message
    // instead of a cascade of errors from the (intentionally) macOS-incomplete source. Build the
    // mac app with scripts/build-swift-host.sh.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" && target_os != "linux" {
        println!(
            "cargo::error=pb-app is the Windows/Linux winit shell; macOS ships via the SwiftUI \
             host (mac/ + pb-mac-ffi, built with scripts/build-swift-host.sh) — target OS \
             '{target_os}' is unsupported."
        );
        return;
    }

    if let Some(id) = git_build_id() {
        println!("cargo:rustc-env=PB_BUILD_ID={id}");
    }
    rerun_on_git_head();
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let icon = "icons/blazeviewer.ico";
        println!("cargo:rerun-if-changed={icon}");
        // The exe's VERSIONINFO — what Windows shows under File ▸ Properties ▸ Details.
        // Hardcoded rather than read from `pb_app_core::APP_NAME`: a build script cannot
        // depend on the crate it builds. Keep these in step with that constant, and with
        // NSHumanReadableCopyright in packaging/macos/Info-swift-host.plist. (CompanyName
        // used to say "PhotoBlaze", which was never right — it's the vendor, not the app.)
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon)
            .set("ProductName", "Blaze Viewer")
            .set("FileDescription", "Blaze Viewer")
            .set("CompanyName", "FullSpec Systems Inc.")
            .set("LegalCopyright", "© 2026 FullSpec Systems Inc.")
            .set_manifest(MANIFEST);
        if let Err(e) = res.compile() {
            println!(
                "cargo:warning=exe manifest/icon resource not embedded ({e}); \
                 the runtime window icon still applies but fullscreen may show the \
                 legacy caption"
            );
        }
    }
}
