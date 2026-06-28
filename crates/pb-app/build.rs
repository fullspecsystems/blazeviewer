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

/// Declares modern-OS support + Per-Monitor-V2 DPI awareness. (winit also sets
/// DPI awareness programmatically; the manifest value matches, so it's a no-op
/// there — but the `supportedOS` block is what stops Windows applying legacy
/// non-client rendering.)
#[cfg(windows)]
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="com.photoblaze.PhotoBlaze" version="1.0.0.0"/>
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

fn main() {
    #[cfg(windows)]
    {
        let icon = "icons/photoblaze.ico";
        println!("cargo:rerun-if-changed={icon}");
        println!("cargo:rerun-if-changed=build.rs");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon)
            .set("ProductName", "PhotoBlaze")
            .set("FileDescription", "PhotoBlaze — a fast photo viewer")
            .set("CompanyName", "PhotoBlaze")
            .set("LegalCopyright", "© PhotoBlaze")
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
