//! Display HDR / wide-gamut capability, queried from the OS.
//!
//! Picks the renderer's output path: when the desktop is in **HDR mode** we present
//! an fp16 **scRGB** surface (wide gamut + HDR headroom); otherwise an SDR 8-bit
//! surface. Windows uses DXGI `IDXGIOutput6::GetDesc1`; other platforms report SDR
//! for now (macOS EDR detection slots in here later, behind the same struct).

/// What the (primary) display can show.
#[derive(Debug, Clone, Copy)]
pub struct DisplayHdr {
    /// The desktop is in HDR mode (DXGI reports an HDR10 color space) / the panel has
    /// EDR headroom (macOS). Drives whether highlights get extended range.
    pub hdr_on: bool,
    /// The panel covers a wider gamut than sRGB (≈Display-P3 or better). On macOS this
    /// is set even for an SDR P3 panel, so wide-gamut photos light up without needing
    /// EDR (the "best mode the display can handle" rule). On Windows it currently
    /// tracks `hdr_on` (wide gamut is gated behind HDR-desktop mode there).
    pub wide_gamut: bool,
    /// Peak luminance in nits (best-effort; 0 if unknown).
    pub max_nits: f32,
    /// The brightness, in nits, that SDR content should map to. scRGB defines
    /// 1.0 = 80 nits, so the output scale for SDR content is `sdr_white_nits / 80`.
    pub sdr_white_nits: f32,
    /// EDR roll-off target for the present pass (scene-linear, 1.0 = SDR white).
    /// **macOS** needs this because EDR hard-*clips* above the panel headroom (unlike
    /// Windows, where DWM tone-maps), so the present pass rolls highlights off toward
    /// this value instead of blowing them out: the panel's max EDR multiplier on an
    /// HDR panel, 1.0 on a wide-gamut-only (SDR) panel. **0.0 = "pass straight
    /// through"** — the Windows path (DWM handles the tone-map) and SDR surfaces.
    pub edr_headroom: f32,
}

impl DisplayHdr {
    /// The SDR-content output scale in scRGB units (1.0 = 80 nits).
    pub fn sdr_scale(&self) -> f32 {
        (self.sdr_white_nits / 80.0).max(1.0)
    }
}

impl Default for DisplayHdr {
    fn default() -> Self {
        // SDR, sRGB-gamut desktop assumption.
        Self {
            hdr_on: false,
            wide_gamut: false,
            max_nits: 0.0,
            sdr_white_nits: 80.0,
            edr_headroom: 0.0,
        }
    }
}

/// Query the primary display's HDR state.
#[cfg(windows)]
pub fn primary_hdr() -> DisplayHdr {
    unsafe { primary_hdr_win().unwrap_or_default() }
}

#[cfg(windows)]
unsafe fn primary_hdr_win() -> Option<DisplayHdr> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};

    let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
    let adapter = factory.EnumAdapters1(0).ok()?;
    let output = adapter.EnumOutputs(0).ok()?;
    let output6: IDXGIOutput6 = output.cast().ok()?;
    let desc = output6.GetDesc1().ok()?;
    let hdr_on = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    Some(DisplayHdr {
        hdr_on,
        // Windows gates wide gamut behind HDR-desktop mode (unchanged behavior); a
        // standalone SDR-P3 detection could light it up more, but not without testing.
        wide_gamut: hdr_on,
        max_nits: desc.MaxLuminance,
        // The real SDR white level lives behind the DisplayConfig API (the user's
        // "SDR content brightness" slider); 200 nits is the common Windows default
        // and a sane starting point until that query is wired in.
        sdr_white_nits: if hdr_on { 200.0 } else { 80.0 },
        // DWM tone-maps the scRGB swapchain to the panel, so the present pass passes
        // highlights straight through (no roll-off).
        edr_headroom: 0.0,
    })
}

/// Query the primary display's EDR / wide-gamut state from `NSScreen`. Uses the
/// *potential* EDR headroom (the panel's capability, independent of whether HDR
/// content is currently on screen) and the P3 gamut check, so an SDR-P3 Mac still
/// reports `wide_gamut` per the "best mode the display can handle" rule.
#[cfg(target_os = "macos")]
pub fn primary_hdr() -> DisplayHdr {
    macos_primary_hdr().unwrap_or_default()
}

// Force-link AppKit so the `NSScreen` Objective-C class is registered with the
// runtime (the real app gets it transitively via winit; a standalone pb-render
// build/test/example would otherwise fail to find the class).
#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
fn macos_primary_hdr() -> Option<DisplayHdr> {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send, MainThreadMarker};

    // NSScreen must be touched on the main thread; bail to the SDR default otherwise
    // (this is called during surface setup, which is on the winit main thread).
    MainThreadMarker::new()?;
    // SAFETY: standard AppKit calls on the main thread; `mainScreen` returns an
    // autoreleased NSScreen we only read scalars off (no storage past this scope).
    unsafe {
        let screen: Option<Retained<AnyObject>> = msg_send![class!(NSScreen), mainScreen];
        let screen = screen?;
        // Potential EDR headroom over SDR white: 1.0 = SDR, >1.0 = EDR-capable.
        let max_edr: f64 = msg_send![
            &*screen,
            maximumPotentialExtendedDynamicRangeColorComponentValue
        ];
        // NSDisplayGamut: SRGB = 0, P3 = 1.
        let p3: Bool = msg_send![&*screen, canRepresentDisplayGamut: 1isize];
        let p3 = p3.as_bool();

        let hdr_on = max_edr > 1.01;
        Some(DisplayHdr {
            hdr_on,
            // Every Apple-Silicon panel is P3; this confirms it (and catches a plain
            // sRGB external monitor, where we keep the narrow-gamut path).
            wide_gamut: p3 || hdr_on,
            // Reference SDR white on macOS internal panels ≈ 100 nits; the panel's peak
            // is that times the EDR headroom. 0 when there's no headroom.
            max_nits: if hdr_on {
                (max_edr * 100.0) as f32
            } else {
                0.0
            },
            // The macOS extended-linear-sRGB surface defines 1.0 = SDR white, so SDR
            // content needs no extra scale → 80 nits makes `sdr_scale()` == 1.0.
            sdr_white_nits: 80.0,
            // EDR clips above the panel headroom, so roll highlights off toward the
            // panel's max EDR multiplier; 1.0 (= clamp to SDR white) on a non-EDR panel.
            edr_headroom: if hdr_on { max_edr as f32 } else { 1.0 },
        })
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn primary_hdr() -> DisplayHdr {
    DisplayHdr::default()
}
