//! **Export mode** (task #89, rung 2; plan of record: `.taskmaster/docs/export-plan.md`) — the
//! subsystem half: the settings the panel edits, what persists of them, and the panel's view
//! model. The `AppCore` methods (entering/leaving the mode, the setters, commit, the
//! contextual keys) live in `app_core_impl/export_panel.rs` (the two-halves rule).
//!
//! Export mode is a **mode, not a modal** (owner decision 11): the panel takes the right-hand
//! Inspector slot while the rest of the keyboard still drives the image — Space/Backspace
//! navigate with the settings kept, `R` rotates. `Esc` still quits and `Enter` is still
//! Random; the contextual key layer ([`export_key_override`]) is where that would change if
//! the pinned revisit flips it.
//!
//! Everything here is pure — no I/O, no `AppCore` — so the view model is unit-tested per
//! source × format.

use pb_encode::{ExportFormat, PngEffort, Resize};
use serde::{Deserialize, Deserializer, Serialize};

use crate::action::Action;
use crate::contract::KeyResolution;

/// The output formats the panel offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatChoice {
    #[default]
    Png,
    Jpeg,
}

impl FormatChoice {
    /// Every format, in the order the panel lists them.
    pub const ALL: [FormatChoice; 2] = [FormatChoice::Png, FormatChoice::Jpeg];

    /// The name the panel shows.
    pub fn label(self) -> &'static str {
        match self {
            FormatChoice::Png => "PNG",
            FormatChoice::Jpeg => "JPEG",
        }
    }

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            FormatChoice::Png => "png",
            FormatChoice::Jpeg => "jpg",
        }
    }

    /// Stable wire value for the shells (FFI / egui): 0 PNG, 1 JPEG.
    pub fn code(self) -> u8 {
        match self {
            FormatChoice::Png => 0,
            FormatChoice::Jpeg => 1,
        }
    }

    /// The inverse of [`code`](Self::code); unknown values are `None`.
    pub fn from_code(code: u8) -> Option<FormatChoice> {
        match code {
            0 => Some(FormatChoice::Png),
            1 => Some(FormatChoice::Jpeg),
            _ => None,
        }
    }
}

/// PNG compression as the panel names it — the encoder's effort levels. PNG is lossless at
/// every level; only the file size and the time differ (rung 1's corpus numbers: Balanced
/// p50 158 ms at 1.37 MB/MP; Faster 10 ms but +33 %; Smallest 2.8× slower for −1 %).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PngCompression {
    Faster,
    #[default]
    Balanced,
    Smallest,
}

impl PngCompression {
    pub const ALL: [PngCompression; 3] = [
        PngCompression::Faster,
        PngCompression::Balanced,
        PngCompression::Smallest,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PngCompression::Faster => "Faster",
            PngCompression::Balanced => "Balanced",
            PngCompression::Smallest => "Smallest",
        }
    }

    pub fn effort(self) -> PngEffort {
        match self {
            PngCompression::Faster => PngEffort::Fast,
            PngCompression::Balanced => PngEffort::Balanced,
            PngCompression::Smallest => PngEffort::High,
        }
    }

    /// Stable wire value: 0 Faster, 1 Balanced, 2 Smallest.
    pub fn code(self) -> u8 {
        match self {
            PngCompression::Faster => 0,
            PngCompression::Balanced => 1,
            PngCompression::Smallest => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<PngCompression> {
        PngCompression::ALL.into_iter().find(|c| c.code() == code)
    }

    /// One step smaller (`dir > 0`) or faster (`dir < 0`), stopping at the ends.
    fn step(self, dir: i32) -> PngCompression {
        let i = (self.code() as i32 + dir.signum()).clamp(0, 2);
        PngCompression::from_code(i as u8).unwrap_or(self)
    }
}

/// The long-edge size presets, largest first. A preset at or above the source's own long
/// edge is greyed out: exports are never upscaled (review F20).
pub const SIZE_PRESETS: [u32; 8] = [3840, 2560, 2048, 1920, 1600, 1280, 1024, 800];

/// The export size: the full resolution, or at most `n` pixels on the long edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SizeChoice {
    #[default]
    Full,
    LongEdge(u32),
}

impl SizeChoice {
    /// The resize `prepare` applies (`None` = full resolution).
    pub fn resize(self) -> Option<Resize> {
        match self {
            SizeChoice::Full => None,
            SizeChoice::LongEdge(n) => Some(Resize::LongEdge(n)),
        }
    }

    /// Wire / settings value: 0 = full size, else the long edge.
    pub fn code(self) -> u32 {
        match self {
            SizeChoice::Full => 0,
            SizeChoice::LongEdge(n) => n,
        }
    }

    pub fn from_code(code: u32) -> SizeChoice {
        match code {
            0 => SizeChoice::Full,
            n => SizeChoice::LongEdge(n),
        }
    }
}

/// JPEG quality bounds and step (`[` / `]` and the slider's tick).
pub const JPEG_QUALITY_MIN: u8 = 10;
pub const JPEG_QUALITY_MAX: u8 = 100;
pub const JPEG_QUALITY_STEP: u8 = 5;
/// The default JPEG quality: rung 2's corpus run used 85 (visually clean on every file at a
/// sensible size). Tuned with `ssimulacra2` in rung 4.
pub const JPEG_QUALITY_DEFAULT: u8 = 85;

/// What the panel edits and an export request freezes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportSettings {
    pub format: FormatChoice,
    pub jpeg_quality: u8,
    pub png: PngCompression,
    pub size: SizeChoice,
    /// *Strip metadata* — on at every entry to the mode, never persisted (plan Q7).
    pub strip_metadata: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings::from_prefs(&ExportPrefs::default())
    }
}

impl ExportSettings {
    /// The settings a fresh entry to Export mode starts from: the saved preferences, with
    /// Strip metadata back on.
    pub fn from_prefs(p: &ExportPrefs) -> ExportSettings {
        ExportSettings {
            format: p.format,
            jpeg_quality: p.jpeg_quality.clamp(JPEG_QUALITY_MIN, JPEG_QUALITY_MAX),
            png: p.png_compression,
            size: SizeChoice::from_code(p.long_edge),
            strip_metadata: true,
        }
    }

    /// What of these settings persists.
    pub fn to_prefs(&self) -> ExportPrefs {
        ExportPrefs {
            format: self.format,
            jpeg_quality: self.jpeg_quality,
            png_compression: self.png,
            long_edge: self.size.code(),
        }
    }

    /// The configured codec.
    pub fn export_format(&self) -> ExportFormat {
        match self.format {
            FormatChoice::Png => ExportFormat::Png(self.png.effort()),
            FormatChoice::Jpeg => ExportFormat::Jpeg {
                quality: self.jpeg_quality,
            },
        }
    }

    pub fn extension(&self) -> &'static str {
        self.format.extension()
    }

    /// `[` / `]` in the mode: JPEG quality by [`JPEG_QUALITY_STEP`], or PNG compression one
    /// level. Returns whether anything changed.
    pub fn step_quality(&mut self, dir: i32) -> bool {
        let before = *self;
        match self.format {
            FormatChoice::Jpeg => {
                // Snap to the step grid first, so 87 → 85 / 90 rather than 82 / 92.
                let q = self.jpeg_quality as i32;
                let step = JPEG_QUALITY_STEP as i32;
                let next = if dir > 0 {
                    (q / step + 1) * step
                } else {
                    ((q + step - 1) / step - 1) * step
                };
                self.jpeg_quality =
                    next.clamp(JPEG_QUALITY_MIN as i32, JPEG_QUALITY_MAX as i32) as u8;
            }
            FormatChoice::Png => self.png = self.png.step(dir),
        }
        *self != before
    }
}

/// The persisted part of the export settings — `[export]` in `settings.toml`. Preferences
/// only (privacy #2): never what was exported or where.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportPrefs {
    pub format: FormatChoice,
    pub jpeg_quality: u8,
    pub png_compression: PngCompression,
    /// 0 = full size, else the long edge in pixels.
    pub long_edge: u32,
}

impl Default for ExportPrefs {
    fn default() -> Self {
        ExportPrefs {
            format: FormatChoice::Png,
            jpeg_quality: JPEG_QUALITY_DEFAULT,
            png_compression: PngCompression::Balanced,
            long_edge: 0,
        }
    }
}

impl ExportPrefs {
    /// Read `[export]` field by field, so one bad value (a format a later build added, a
    /// quality typed as a string) falls back alone — it never resets the other fields, and
    /// never the rest of `settings.toml` (review F16).
    pub fn from_value(v: &toml::Value) -> ExportPrefs {
        let mut p = ExportPrefs::default();
        let Some(t) = v.as_table() else {
            return p;
        };
        fn field<T: serde::de::DeserializeOwned>(t: &toml::Table, key: &str) -> Option<T> {
            t.get(key).and_then(|v| v.clone().try_into().ok())
        }
        if let Some(f) = field(t, "format") {
            p.format = f;
        }
        if let Some(q) = field::<i64>(t, "jpeg_quality") {
            p.jpeg_quality = q.clamp(JPEG_QUALITY_MIN as i64, JPEG_QUALITY_MAX as i64) as u8;
        }
        if let Some(c) = field(t, "png_compression") {
            p.png_compression = c;
        }
        if let Some(e) = field::<i64>(t, "long_edge") {
            p.long_edge = e.clamp(0, u32::MAX as i64) as u32;
        }
        p
    }
}

/// `#[serde(deserialize_with)]` for `Settings::export`: lenient, per [`ExportPrefs::from_value`].
pub fn lenient_prefs<'de, D: Deserializer<'de>>(d: D) -> Result<ExportPrefs, D::Error> {
    Ok(toml::Value::deserialize(d)
        .map(|v| ExportPrefs::from_value(&v))
        .unwrap_or_default())
}

/// The **contextual key layer** for Export mode (plan § 3.11, rung 2.0): a key press already
/// resolved through the keymap is re-meant while the mode is active. Returns the override,
/// or `None` to let the key do what it always does.
///
/// Today: `[` / `]` (bound to the slideshow speed — the slideshow is stopped in the mode)
/// step the quality. **The pinned revisit lives here:** making `Enter` commit, or `Esc`
/// leave the mode, is one more arm (`Random` → [`ExportKey::Commit`], `Quit` →
/// [`ExportKey::Leave`]) plus its test — not a redesign.
pub fn export_key_override(res: KeyResolution) -> Option<ExportKey> {
    match res {
        KeyResolution::OneShot(a) | KeyResolution::RepeatStart(a) => match a {
            Action::SlideshowFaster => Some(ExportKey::Quality(-1)),
            Action::SlideshowSlower => Some(ExportKey::Quality(1)),
            _ => None,
        },
        _ => None,
    }
}

/// What a key means in Export mode, when it differs from its everyday meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKey {
    /// Quality down (−1) / up (+1).
    Quality(i32),
    /// Export… with the current settings (reserved for the pinned `Enter` revisit).
    #[allow(dead_code)]
    Commit,
    /// Leave the mode (reserved for the pinned `Esc` revisit).
    #[allow(dead_code)]
    Leave,
}

/// What the panel model depends on — compared each tick so the native shells re-pull only
/// when something they show changed (no per-tick model build or allocation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelKey {
    pub item: Option<usize>,
    pub settings: ExportSettings,
    /// `None` while the frame is moving (a playing video shows a fixed line, not a clock).
    pub frame: Option<crate::frame_capture::FrameKey>,
    pub moving: bool,
    pub rotation: u8,
    pub dims: Option<(u32, u32)>,
}

/// One size choice in the panel's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeOption {
    pub choice: SizeChoice,
    pub label: String,
    /// Greyed out: at or above the source's long edge (never upscaled).
    pub enabled: bool,
}

/// What the panel knows about the item on screen.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PanelSource {
    /// The file name (an archive entry: its name).
    pub name: String,
    /// What exactly is exported when it isn't the file's own image: "Frame at 1:23",
    /// "Frame 13", "Poster frame".
    pub frame: Option<String>,
    /// Pixel size **as displayed** (the RAM rotation applied), when known.
    pub dims: Option<(u32, u32)>,
    /// Why this item can't be exported (a door, an unsupported build), if it can't.
    pub blocked: Option<String>,
}

/// The Export panel's view model — what both shells draw. Built by [`ExportPanel::build`];
/// edits go back through the `AppCore::export_set_*` methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportPanel {
    /// `None` on the empty deck.
    pub source: Option<PanelSource>,
    pub settings: ExportSettings,
    pub sizes: Vec<SizeOption>,
    /// The output size as displayed, when the source size is known.
    pub output_dims: Option<(u32, u32)>,
    /// Plain-language notes on what the export will do ("Transparent areas become white").
    pub notes: Vec<String>,
    /// Whether Export… can run on this item now.
    pub can_export: bool,
}

impl ExportPanel {
    pub fn build(settings: ExportSettings, source: Option<PanelSource>) -> ExportPanel {
        let long = source.as_ref().and_then(|s| s.dims).map(|(w, h)| w.max(h));
        let mut sizes = vec![SizeOption {
            choice: SizeChoice::Full,
            label: "Full size".into(),
            enabled: true,
        }];
        sizes.extend(SIZE_PRESETS.iter().map(|&n| SizeOption {
            choice: SizeChoice::LongEdge(n),
            label: format!("{n} px"),
            enabled: long.is_none_or(|l| n < l),
        }));
        // A custom long edge (a hand-edited setting) still shows as the current choice.
        if let SizeChoice::LongEdge(n) = settings.size {
            if !SIZE_PRESETS.contains(&n) {
                sizes.push(SizeOption {
                    choice: settings.size,
                    label: format!("{n} px"),
                    enabled: long.is_none_or(|l| n < l),
                });
            }
        }

        let dims = source.as_ref().and_then(|s| s.dims);
        let output_dims = dims.map(|(w, h)| {
            settings
                .size
                .resize()
                .and_then(|r| r.target(w, h))
                .unwrap_or((w, h))
        });

        let mut notes = Vec::new();
        if let (SizeChoice::LongEdge(n), Some(l)) = (settings.size, long) {
            if n >= l {
                notes.push(format!(
                    "This image is already {l} px or smaller, so it's exported at full size."
                ));
            }
        }
        if settings.format == FormatChoice::Jpeg {
            notes.push("JPEG has no transparency: transparent areas become white.".into());
        }
        if settings.strip_metadata {
            notes.push(
                "Camera details, dates and location are left out; the color profile is kept."
                    .into(),
            );
        } else {
            // Honest about rung 2's gap: the metadata categories aren't written yet.
            notes.push(
                "The original color profile is kept as is. Camera details, dates and location \
                 aren't copied yet."
                    .into(),
            );
        }

        let can_export = source.as_ref().is_some_and(|s| s.blocked.is_none());
        ExportPanel {
            source,
            settings,
            sizes,
            output_dims,
            notes,
            can_export,
        }
    }
}

/// A pixel size as the panel prints it: `4000 × 3000`.
pub fn dims_label((w, h): (u32, u32)) -> String {
    format!("{w} × {h}")
}

/// The panel's label for a frame's time: `0:07`, `1:23`, `1:02:03`.
pub fn clock_label(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(w: u32, h: u32) -> Option<PanelSource> {
        Some(PanelSource {
            name: "a.jpg".into(),
            frame: None,
            dims: Some((w, h)),
            blocked: None,
        })
    }

    #[test]
    fn a_fresh_entry_starts_from_the_prefs_with_strip_back_on() {
        let p = ExportPrefs {
            format: FormatChoice::Jpeg,
            jpeg_quality: 70,
            png_compression: PngCompression::Smallest,
            long_edge: 2048,
        };
        let s = ExportSettings::from_prefs(&p);
        assert_eq!(s.format, FormatChoice::Jpeg);
        assert_eq!(s.jpeg_quality, 70);
        assert_eq!(s.size, SizeChoice::LongEdge(2048));
        assert!(
            s.strip_metadata,
            "Strip metadata always starts checked (Q7)"
        );
        assert_eq!(
            s.to_prefs(),
            p,
            "the round trip keeps every persisted field"
        );
        assert_eq!(
            s.export_format(),
            ExportFormat::Jpeg { quality: 70 },
            "the codec the worker runs"
        );
        assert_eq!(s.extension(), "jpg");
    }

    #[test]
    fn prefs_load_leniently_field_by_field() {
        let parse = |s: &str| ExportPrefs::from_value(&s.parse::<toml::Table>().unwrap().into());
        // A format a later build added, a quality typed as text: each falls back alone.
        let p = parse("format = \"webp\"\njpeg_quality = \"high\"\nlong_edge = 1920");
        assert_eq!(p.format, FormatChoice::Png);
        assert_eq!(p.jpeg_quality, JPEG_QUALITY_DEFAULT);
        assert_eq!(p.long_edge, 1920, "the good field survives");
        // Out-of-range numbers clamp.
        let p = parse("jpeg_quality = 400\nlong_edge = -5");
        assert_eq!((p.jpeg_quality, p.long_edge), (100, 0));
        // Not a table at all → defaults.
        assert_eq!(
            ExportPrefs::from_value(&toml::Value::Integer(3)),
            ExportPrefs::default()
        );
    }

    #[test]
    fn quality_keys_step_on_a_grid_and_stop_at_the_ends() {
        let mut s = ExportSettings {
            format: FormatChoice::Jpeg,
            jpeg_quality: 87,
            ..ExportSettings::default()
        };
        assert!(s.step_quality(-1));
        assert_eq!(s.jpeg_quality, 85, "snaps down to the grid");
        s.jpeg_quality = 87;
        s.step_quality(1);
        assert_eq!(s.jpeg_quality, 90, "snaps up to the grid");
        s.jpeg_quality = JPEG_QUALITY_MAX;
        assert!(!s.step_quality(1), "already at the top");
        s.jpeg_quality = JPEG_QUALITY_MIN;
        assert!(!s.step_quality(-1), "already at the bottom");

        // PNG: one compression level per press.
        let mut s = ExportSettings::default();
        assert_eq!(s.png, PngCompression::Balanced);
        s.step_quality(1);
        assert_eq!(s.png, PngCompression::Smallest);
        assert!(!s.step_quality(1));
        s.step_quality(-1);
        s.step_quality(-1);
        assert_eq!(s.png, PngCompression::Faster);
    }

    #[test]
    fn presets_never_upscale_and_the_output_size_follows_the_choice() {
        let mut s = ExportSettings::default();
        let p = ExportPanel::build(s, src(3000, 2000));
        assert_eq!(p.output_dims, Some((3000, 2000)));
        let enabled: Vec<u32> = p
            .sizes
            .iter()
            .filter(|o| o.enabled)
            .map(|o| o.choice.code())
            .collect();
        assert_eq!(enabled, vec![0, 2560, 2048, 1920, 1600, 1280, 1024, 800]);

        s.size = SizeChoice::LongEdge(1920);
        let p = ExportPanel::build(s, src(3000, 2000));
        assert_eq!(p.output_dims, Some((1920, 1280)));
        // Portrait (or a quarter-turned photo — dims arrive as displayed).
        let p = ExportPanel::build(s, src(2000, 3000));
        assert_eq!(p.output_dims, Some((1280, 1920)));
        // A preset at or above the source: full size, and the panel says why.
        let p = ExportPanel::build(s, src(1200, 800));
        assert_eq!(p.output_dims, Some((1200, 800)));
        assert!(
            p.notes.iter().any(|n| n.contains("full size")),
            "{:?}",
            p.notes
        );
    }

    #[test]
    fn notes_explain_jpeg_flattening_and_stripping() {
        let mut s = ExportSettings::default();
        let p = ExportPanel::build(s, src(10, 10));
        assert!(!p.notes.iter().any(|n| n.contains("transparency")));
        assert!(p.notes.iter().any(|n| n.contains("location")));
        s.format = FormatChoice::Jpeg;
        s.strip_metadata = false;
        let p = ExportPanel::build(s, src(10, 10));
        assert!(p.notes.iter().any(|n| n.contains("transparency")));
        assert!(
            p.notes.iter().any(|n| n.contains("aren't copied yet")),
            "unchecked: honest that nothing but the profile is kept (rung 2's gap)"
        );
    }

    #[test]
    fn a_blocked_item_or_the_empty_deck_cannot_export() {
        let s = ExportSettings::default();
        assert!(ExportPanel::build(s, src(10, 10)).can_export);
        assert!(!ExportPanel::build(s, None).can_export);
        let door = Some(PanelSource {
            name: "set.zip".into(),
            blocked: Some("Open the archive to export its images".into()),
            ..PanelSource::default()
        });
        assert!(!ExportPanel::build(s, door).can_export);
        // Unknown dims: every preset stays available; no output size.
        let p = ExportPanel::build(
            s,
            Some(PanelSource {
                name: "x".into(),
                ..PanelSource::default()
            }),
        );
        assert!(p.sizes.iter().all(|o| o.enabled));
        assert_eq!(p.output_dims, None);
    }

    #[test]
    fn a_custom_long_edge_is_listed_as_the_current_choice() {
        let s = ExportSettings {
            size: SizeChoice::LongEdge(1500),
            ..ExportSettings::default()
        };
        let p = ExportPanel::build(s, src(3000, 2000));
        assert!(p
            .sizes
            .iter()
            .any(|o| o.choice == SizeChoice::LongEdge(1500) && o.enabled));
        assert_eq!(p.output_dims, Some((1500, 1000)));
    }

    #[test]
    fn the_key_layer_re_means_only_the_slideshow_speed_keys() {
        use KeyResolution as K;
        assert_eq!(
            export_key_override(K::RepeatStart(Action::SlideshowFaster)),
            Some(ExportKey::Quality(-1))
        );
        assert_eq!(
            export_key_override(K::OneShot(Action::SlideshowSlower)),
            Some(ExportKey::Quality(1))
        );
        // Owner decision 11: Esc and Enter keep their global meanings (pinned for revisit).
        assert_eq!(export_key_override(K::OneShot(Action::Quit)), None);
        assert_eq!(export_key_override(K::NavStart(Action::Random)), None);
        assert_eq!(export_key_override(K::NavStart(Action::Next)), None);
    }

    #[test]
    fn wire_codes_round_trip() {
        for f in FormatChoice::ALL {
            assert_eq!(FormatChoice::from_code(f.code()), Some(f));
        }
        for c in PngCompression::ALL {
            assert_eq!(PngCompression::from_code(c.code()), Some(c));
        }
        assert_eq!(SizeChoice::from_code(0), SizeChoice::Full);
        assert_eq!(SizeChoice::from_code(2048).code(), 2048);
        assert_eq!(clock_label(7), "0:07");
        assert_eq!(clock_label(83), "1:23");
        assert_eq!(clock_label(3723), "1:02:03");
    }
}
