//! Build script: embed the application icon + version metadata into the Windows
//! executable, so Explorer, the taskbar, and the file-association icon are real.
//!
//! **Best-effort:** if the resource compiler (`rc.exe` / `llvm-rc`) isn't on the
//! machine, we emit a warning and continue — the build still succeeds. The
//! runtime window icon (set via winit from the embedded PNG) applies regardless,
//! and the MSI installer references the same `.ico` independently.

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
            .set("LegalCopyright", "© PhotoBlaze");
        if let Err(e) = res.compile() {
            println!(
                "cargo:warning=exe icon/version resource not embedded ({e}); \
                 the runtime window icon still applies"
            );
        }
    }
}
