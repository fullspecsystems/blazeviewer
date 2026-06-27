//! Display HDR / wide-gamut capability, queried from the OS.
//!
//! Picks the renderer's output path: when the desktop is in **HDR mode** we present
//! an fp16 **scRGB** surface (wide gamut + HDR headroom); otherwise an SDR 8-bit
//! surface. Windows uses DXGI `IDXGIOutput6::GetDesc1`; other platforms report SDR
//! for now (macOS EDR detection slots in here later, behind the same struct).

/// What the (primary) display can show.
#[derive(Debug, Clone, Copy)]
pub struct DisplayHdr {
    /// The desktop is in HDR mode (DXGI reports an HDR10 color space).
    pub hdr_on: bool,
    /// Peak luminance in nits (best-effort; 0 if unknown).
    pub max_nits: f32,
    /// The brightness, in nits, that SDR content should map to. scRGB defines
    /// 1.0 = 80 nits, so the output scale for SDR content is `sdr_white_nits / 80`.
    pub sdr_white_nits: f32,
}

impl DisplayHdr {
    /// The SDR-content output scale in scRGB units (1.0 = 80 nits).
    pub fn sdr_scale(&self) -> f32 {
        (self.sdr_white_nits / 80.0).max(1.0)
    }
}

impl Default for DisplayHdr {
    fn default() -> Self {
        // SDR desktop assumption.
        Self {
            hdr_on: false,
            max_nits: 0.0,
            sdr_white_nits: 80.0,
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
        max_nits: desc.MaxLuminance,
        // The real SDR white level lives behind the DisplayConfig API (the user's
        // "SDR content brightness" slider); 200 nits is the common Windows default
        // and a sane starting point until that query is wired in.
        sdr_white_nits: if hdr_on { 200.0 } else { 80.0 },
    })
}

#[cfg(not(windows))]
pub fn primary_hdr() -> DisplayHdr {
    DisplayHdr::default()
}
