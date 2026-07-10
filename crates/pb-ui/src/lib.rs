//! `pb_ui` — the **design-system** crate for PhotoBlaze's egui chrome.
//!
//! The web-world instinct, applied to egui: one place for **tokens** (a 4px spacing
//! scale, corner radii, a uniform 32px control height, a light/dark palette, the
//! system font) and a handful of composable **components** (section labels, a
//! settings *card*, a card *row*, a toggle switch, primary/secondary buttons) so
//! every dialog reads as one family instead of each form inventing its own spacing.
//!
//! The visual target is the **Windows 11 settings-card** look (think the new Notepad
//! settings page): rounded cards holding `title + dim subtitle` on the left and a
//! right-aligned control, grouped under section labels, tracking the OS light/dark
//! theme. We can't use the actual WinUI `SettingsCard` control from Rust, but the
//! look is a layout pattern, not native magic — so we reproduce it here.
//!
//! This crate has no app dependencies (just `egui`) so it stays a reusable, testable
//! unit and powers the standalone gallery (`cargo run -p pb-ui --example gallery`).
//! It runs only while a dialog is open — **off the photo hot path** — so the per-frame
//! [`apply_style`] (cheap) and the one-time [`install_fonts`] (a font-file read) cost
//! nothing the viewer cares about.

use egui::{Color32, FontFamily, FontId, Margin, Rounding, Stroke, TextStyle};

pub mod icon;

// ── Spacing scale (4px grid, named like a type/space scale) ──────────────────
pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_6: f32 = 24.0;

// ── Corner radii ─────────────────────────────────────────────────────────────
/// Controls (buttons, fields, combos) — the Fluent control radius.
pub const RADIUS_CONTROL: f32 = 4.0;
/// Cards / surfaces — the Fluent card radius.
pub const RADIUS_CARD: f32 = 8.0;

// ── Sizing ───────────────────────────────────────────────────────────────────
/// Uniform interactive control height (Fluent's default). Setting this once is the
/// single biggest fix for "every control is a different size".
pub const CONTROL_H: f32 = 32.0;
/// Left/right inset of a settings page's content column.
pub const PAGE_MARGIN: f32 = 28.0;
/// Standard dialog action-button width.
pub const BUTTON_W: f32 = 96.0;
/// Gap between [`tab_bar`] labels. Deliberately roomy (a full [`SPACE_6`]) so a pivot
/// reads as section navigation rather than a packed button group.
pub const TAB_GAP: f32 = SPACE_6;
/// The selected/hovered [`tab_bar`] underline thickness — a thin accent indicator, never
/// a filled pill, so it can't be mistaken for a [`primary_button`].
pub const TAB_INDICATOR_H: f32 = 2.0;
/// How far the [`tab_bar`] underline extends past each edge of its label, so the indicator
/// reads as a deliberate marker rather than a too-tight rule clipped to the glyphs.
pub const TAB_INDICATOR_OVERHANG: f32 = SPACE_1;
/// A [`tab_bar`] label's optional leading icon size — a touch under the `SECTION_SIZE` label
/// so the glyph reads as a peer of the text, not heavier than it.
const TAB_ICON_SIZE: f32 = 15.0;
/// Stable minimum width for a slider's value box. egui sizes the box to the formatted
/// number, so it jitters as the digit count changes (3.0 → 11.7 → 400); pinning a
/// minimum wide enough for the widest value keeps it a fixed width. See [`slider`].
pub const SLIDER_VALUE_W: f32 = 76.0;
/// Section-header text size — a clear tier above the 14.5px card title, in the semibold
/// face. See [`section_label`].
pub const SECTION_SIZE: f32 = 17.0;
/// The custom font family for semibold headers, registered by [`install_fonts`] (falls
/// back to the regular face if Segoe UI Semibold is absent, so it always resolves).
pub const SEMIBOLD: &str = "semibold";
/// Interior padding of a text field — balanced on all four sides so it reads like the
/// other 32px controls instead of a cramped single line.
pub const FIELD_MARGIN: egui::Margin = egui::Margin {
    left: 8.0,
    right: 8.0,
    top: 8.0,
    bottom: 8.0,
};

/// Below this content width a [`card_row`] **stacks** (title, description, then the
/// control on its own line) instead of placing the control on the right — the WinUI
/// `SettingsCard` adaptive-wrap idiom, so cards don't fall apart at narrow widths.
pub const CARD_WRAP_WIDTH: f32 = 430.0;
/// Width reserved for a card row's right-hand control in the wide (side-by-side) layout,
/// so a long description wraps in the header rather than colliding with the control.
const CONTROL_RESERVE: f32 = 290.0;
/// Minimum width the header block keeps in the wide layout before the row would stack.
const HEADER_MIN: f32 = 140.0;
/// **The standard gap** between sibling blocks: between rows inside a [`group_card`],
/// between cards on a page, and (via the dialog button bar) between buttons. One value so
/// spacing reads consistent everywhere — the card's fill + border do the grouping, so no
/// gap contrast or dividers are needed. The one knob to tune density.
pub const GAP: f32 = 14.0;

/// The larger gap **above a section heading** that follows a card — bigger than [`GAP`] so a
/// [`group_card`] heading reads as belonging to the card *below* it, not floating equidistant
/// between two cards. (The first heading on a page uses the page's top inset instead, and the
/// heading→card gap stays a tight [`SPACE_2`], so grouping reads clearly.)
pub const SECTION_GAP: f32 = 22.0;

/// The resolved color roles for one theme. Built from a single `dark` flag so the
/// whole palette stays internally consistent and tracks the OS setting.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Page / window background (the Mica-equivalent base).
    pub page: Color32,
    /// A settings card's fill (sits subtly above `page`).
    pub card: Color32,
    /// A card's hairline border.
    pub card_stroke: Color32,
    /// A resting control (button/combo/field) fill.
    pub control: Color32,
    /// A hovered/active control fill.
    pub control_hover: Color32,
    /// Primary body text.
    pub text: Color32,
    /// Secondary text (a card subtitle, a hint).
    pub text_secondary: Color32,
    /// The accent (primary buttons, selection, slider fill, a toggle's "on" track).
    /// PhotoBlaze brand orange, centered on `#FF4614`.
    pub accent: Color32,
    /// The destructive action color (Delete fills, danger-tone icons) — a darker, redder
    /// sibling of [`accent`](Palette::accent), kept distinct so a destructive action never
    /// reads as the primary one.
    pub danger: Color32,
    /// A toggle switch's "off" track.
    pub toggle_off: Color32,
}

impl Palette {
    pub fn new(dark: bool) -> Self {
        if dark {
            Palette {
                dark,
                page: Color32::from_gray(0x20),
                card: Color32::from_gray(0x2b),
                card_stroke: Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                control: Color32::from_gray(0x35),
                control_hover: Color32::from_gray(0x3f),
                text: Color32::from_gray(0xe8),
                text_secondary: Color32::from_gray(0x9e),
                // The brand orange — same `#FF4915` as light mode (white button text reads
                // on it either way), kept identical across themes per the owner's call.
                accent: Color32::from_rgb(0xff, 0x49, 0x15),
                // The darker-red destructive sibling. Same deep red as light mode (white
                // text reads on it either way) — a lighter dark-mode red looked too close
                // to the orange accent.
                danger: Color32::from_rgb(0xc4, 0x24, 0x12),
                toggle_off: Color32::from_gray(0x5a),
            }
        } else {
            Palette {
                dark,
                page: Color32::from_gray(0xf3),
                card: Color32::from_gray(0xfb),
                card_stroke: Color32::from_rgba_unmultiplied(0, 0, 0, 18),
                control: Color32::from_gray(0xff),
                control_hover: Color32::from_gray(0xf0),
                text: Color32::from_gray(0x1a),
                text_secondary: Color32::from_gray(0x5e),
                // The PhotoBlaze brand orange (the logo color) at full strength.
                accent: Color32::from_rgb(0xff, 0x49, 0x15),
                // A darker, redder version of the brand for destructive actions — deep
                // enough that white text clears WCAG AA.
                danger: Color32::from_rgb(0xc4, 0x24, 0x12),
                toggle_off: Color32::from_gray(0x8a),
            }
        }
    }
}

/// The app's type scale, mapped onto egui's named text styles.
fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    [
        (
            TextStyle::Heading,
            FontId::new(26.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
    ]
    .into()
}

/// Build the egui [`Visuals`](egui::Visuals) for a palette: card/page fills, control
/// fills, accent-driven selection + slider fill, and a single control corner radius.
fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.page;
    v.window_fill = p.page;
    v.window_rounding = Rounding::same(RADIUS_CARD);
    v.window_stroke = Stroke::new(1.0_f32, p.card_stroke);
    // Text-input / "extreme" backgrounds sit at control fill.
    v.extreme_bg_color = p.control;
    v.faint_bg_color = if p.dark {
        Color32::from_gray(0x28)
    } else {
        Color32::from_gray(0xee)
    };

    let radius = Rounding::same(RADIUS_CONTROL);
    // Resting label text.
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.card_stroke);
    // Resting interactive control (button/combo/field).
    v.widgets.inactive.bg_fill = p.control;
    v.widgets.inactive.weak_bg_fill = p.control;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.card_stroke);
    v.widgets.inactive.rounding = radius;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    // Hovered.
    v.widgets.hovered.bg_fill = p.control_hover;
    v.widgets.hovered.weak_bg_fill = p.control_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v.widgets.hovered.rounding = radius;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    // Active (pressed).
    v.widgets.active.bg_fill = p.control_hover;
    v.widgets.active.weak_bg_fill = p.control_hover;
    v.widgets.active.rounding = radius;
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    // Open (e.g. an expanded combo).
    v.widgets.open.bg_fill = p.control;
    v.widgets.open.weak_bg_fill = p.control;
    v.widgets.open.rounding = radius;

    // Accent: text selection, the slider's trailing fill, links.
    v.selection.bg_fill = p.accent.linear_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0_f32, p.text);
    v.slider_trailing_fill = true;
    v.hyperlink_color = p.accent;
    v
}

/// Build the full design-system [`Style`](egui::Style) for a theme: the type scale,
/// the spacing tokens, and the palette-driven visuals. Used to set the style on a
/// whole context ([`apply_style`]) or scope it to a single [`Ui`](egui::Ui)
/// ([`apply_to_ui`] — e.g. a light and a dark gallery column side by side).
pub fn style(dark: bool) -> egui::Style {
    let p = Palette::new(dark);
    let spacing = egui::style::Spacing {
        item_spacing: egui::vec2(SPACE_2, SPACE_2),
        button_padding: egui::vec2(SPACE_3, SPACE_2),
        interact_size: egui::vec2(40.0, CONTROL_H),
        indent: SPACE_6,
        // Track width; the value box is a separate, fixed-width box (see `slider`).
        slider_width: 200.0,
        combo_width: 160.0,
        ..Default::default()
    };
    egui::Style {
        text_styles: text_styles(),
        spacing,
        visuals: visuals(&p),
        ..Default::default()
    }
}

/// Apply the design system to an entire [`Context`](egui::Context). Cheap (builds a
/// `Style`); call at the top of every dialog frame so the look is stable regardless of
/// egui's own light/dark resolution. Fonts are installed separately ([`install_fonts`]).
pub fn apply_style(ctx: &egui::Context, dark: bool) {
    ctx.set_style(style(dark));
}

/// Scope the design system to a single [`Ui`](egui::Ui) — so two differently-themed
/// regions (e.g. the gallery's light and dark columns) can render in one window.
pub fn apply_to_ui(ui: &mut egui::Ui, dark: bool) {
    *ui.style_mut() = style(dark);
}

/// Load the OS UI font (Segoe UI on Windows, SF Pro on macOS, DejaVu Sans on Linux) as the
/// proportional family, once, so the chrome reads as a native app rather than egui's default face.
/// Falls back to egui's bundled font silently if no file can be read. This is a read of
/// a system font file — never a photo, never a write — so it's outside the no-trace
/// view path. The lists carry both platforms' paths (the absent one is just skipped),
/// keeping this crate free of platform `cfg`s.
/// Vertical-centering correction for the loaded system UI fonts. egui positions a font's
/// glyphs from its ascent/descent metrics, and both Segoe UI and SF Pro report enough top
/// leading that text drifts a hair *low* inside any vertically-centered region — buttons,
/// toggles, fields (the same quirk the card padding used to fight by hand). A small
/// negative `y_offset_factor` lifts every glyph by this fraction of its font size, fixing
/// the centering at the source. It's proportional, so it holds across the whole type ramp,
/// and it doesn't change row height (so control auto-sizing is untouched). Tune by eye in
/// the gallery.
pub const TEXT_Y_NUDGE: f32 = -0.10;

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let tweak = egui::FontTweak {
        y_offset_factor: TEXT_Y_NUDGE,
        ..Default::default()
    };

    // Regular native UI face → front of the proportional family (preferred over egui's
    // bundled default; the default's fallbacks, e.g. emoji, stay behind it).
    //
    // Prefer the *static* "SF Pro Text" optical cut over the variable `SFNS.ttf`. epaint's
    // layout (ab_glyph) applies neither kerning pairs nor SF's size-dependent `trak`
    // tracking, and the variable face's default optical size is tuned for large/display
    // sizes — so at 14.5px its spacing reads loose and uneven. The "Text" cut is metrically
    // spaced for body sizes (≤19pt) *without* needing kerning, which is exactly what we get.
    // Pairs with the SF Pro Text semibold below for one consistent family. Falls back to the
    // always-present variable face, then Segoe, then egui's bundled font.
    if let Some(bytes) = read_first(&[
        "/Library/Fonts/SF-Pro-Text-Regular.otf", // macOS: static SF Pro Text (best small-size spacing)
        "/System/Library/Fonts/SFNS.ttf", // macOS fallback: SF Pro (variable; default = Regular)
        r"C:\Windows\Fonts\segoeui.ttf",  // Windows: Segoe UI
        // Linux: DejaVu Sans (the pb-hud overlay font too) — good metrics + glyph coverage (arrows,
        // symbols); without a real face egui falls back to its bundled font, which drifts the layout
        // and shows tofu for glyphs it lacks. Liberation/Noto cover distros without DejaVu.
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", // Debian/Ubuntu
        "/usr/share/fonts/TTF/DejaVuSans.ttf",             // Arch/Fedora layout
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    ]) {
        fonts.font_data.insert(
            "system-ui".to_owned(),
            egui::FontData::from_owned(bytes).tweak(tweak),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "system-ui".to_owned());
    }

    // Semibold face for headers (see `section_label`). Register the `SEMIBOLD` family
    // pointing at it, with the proportional stack behind as fallback — so if the file is
    // missing, headers still render (in the regular face) rather than failing to resolve.
    // macOS ships no static SF semibold, so unless Apple's SF Pro family is installed the
    // headers fall back to SF Pro Regular (one native typeface, just no extra weight).
    let proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let semibold_stack = if let Some(bytes) = read_first(&[
        "/Library/Fonts/SF-Pro-Text-Semibold.otf", // macOS: real SF semibold if installed
        r"C:\Windows\Fonts\seguisb.ttf",           // Windows: Segoe UI Semibold
        "/System/Library/Fonts/SFNS.ttf",          // macOS fallback: SF Pro (Regular)
        // Linux: DejaVu Sans Bold stands in for the semibold weight (as pb-hud does).
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", // Debian/Ubuntu
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",             // Arch/Fedora layout
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    ]) {
        fonts.font_data.insert(
            "system-ui-semibold".to_owned(),
            egui::FontData::from_owned(bytes).tweak(tweak),
        );
        let mut stack = vec!["system-ui-semibold".to_owned()];
        stack.extend(proportional);
        stack
    } else {
        proportional
    };
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold_stack);

    ctx.set_fonts(fonts);
}

/// Read the first readable file from `paths` (a system font), if any.
fn read_first(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

// ── Components ───────────────────────────────────────────────────────────────

/// The page title (H1) — the largest tier, in the semibold face.
pub fn page_title(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .font(FontId::new(30.0, FontFamily::Name(SEMIBOLD.into())))
            .color(p.text),
    );
}

/// A standalone section label above a group of cards. The grouped-settings pattern puts
/// the heading *inside* a [`group_card`] instead; this stays for catalog-style sections
/// (e.g. the gallery's component groups).
pub fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(SPACE_4);
    ui.label(
        egui::RichText::new(text)
            .font(FontId::new(SECTION_SIZE, FontFamily::Name(SEMIBOLD.into())))
            .color(p.text),
    );
    ui.add_space(SPACE_2);
}

/// A rounded settings **card** surface (the WinUI `SettingsCard` look): card fill, a
/// hairline border, the card radius, and comfortable interior padding. Holds one or
/// more [`card_row`]s.
pub fn card<R>(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    card_frame(p)
        // Symmetric padding: the line-box-too-low quirk that used to force an asymmetric
        // top/bottom split is now corrected at the source by `TEXT_Y_NUDGE` (see
        // `install_fonts`), so equal padding optically centers the title/description block.
        .inner_margin(Margin {
            left: SPACE_4,
            right: SPACE_4,
            top: SPACE_3,
            bottom: SPACE_3,
        })
        .show(ui, |ui| {
            // Fill the parent's width so every card is the same width, rather than
            // egui's default shrink-to-content (which makes each card a different size).
            ui.set_min_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A **grouped settings card**: an optional semibold `header` **above** the card, then a
/// `body` of [`card_row`]s that auto-space by [`GAP`]. The macOS/Windows grouped-settings
/// pattern (SwiftUI `Form` `Section("…")`) — related settings share one card under a section
/// heading placed over the card, so a page is a few cards instead of one-per-setting (far
/// less scrolling). The caller's inter-card [`GAP`] becomes the gap above each header, so a
/// header reads as belonging to the card *below* it, not the one above.
pub fn group_card(
    ui: &mut egui::Ui,
    p: &Palette,
    header: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) {
    if let Some(h) = header {
        ui.label(
            egui::RichText::new(h)
                .font(FontId::new(SECTION_SIZE, FontFamily::Name(SEMIBOLD.into())))
                .color(p.text),
        );
        ui.add_space(SPACE_2);
    }
    card_frame(p)
        // Symmetric padding (like `card`) now the header lives outside the card — the first
        // row gets the same top breathing room as the last row's bottom.
        .inner_margin(Margin {
            left: SPACE_4,
            right: SPACE_4,
            top: SPACE_3,
            bottom: SPACE_3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            // One standard GAP between every row — no dividers needed; the caller just lists
            // `card_row`s and they auto-space.
            ui.spacing_mut().item_spacing.y = GAP;
            body(ui);
        });
}

/// The shared card surface — fill + hairline border + radius. `card` and `group_card`
/// add their own inner margins on top.
fn card_frame(p: &Palette) -> egui::Frame {
    egui::Frame::none()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.card_stroke))
        .rounding(Rounding::same(RADIUS_CARD))
}

/// One row inside a [`card`]: an optional lead icon, a `title` with an optional dim
/// `desc` subtitle, and a `control`. **Responsive** (a "size class"): when the card is
/// wide enough the control sits on the right and a long description wraps in the header;
/// when narrower than [`CARD_WRAP_WIDTH`] the row stacks (title, description, then the
/// control on its own line) so nothing collides. Returns whatever the control closure
/// returns (e.g. a [`Response`](egui::Response)).
pub fn card_row<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Option<&egui::TextureHandle>,
    title: &str,
    desc: Option<&str>,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut out = None;
    if ui.available_width() < CARD_WRAP_WIDTH {
        // Narrow: stack the control beneath the header, full width, left-aligned. The
        // header gets its own vertical so its tighter line spacing doesn't leak; the
        // outer vertical zeroes item spacing so a group's GAP doesn't widen the internals.
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.vertical(|ui| row_header(ui, p, icon, title, desc));
            ui.add_space(SPACE_2);
            out = Some(control(ui));
        });
    } else {
        // Wide: a width-capped header column (its description wraps within the cap) with
        // the control pinned right. Capping the header's width — rather than the earlier
        // `allocate_ui_with_layout(y=0)`, which centered a zero-height region in the
        // row's full available height and left a tall empty band — keeps the row only as
        // tall as its content while still leaving the control room not to collide.
        ui.horizontal(|ui| {
            ui.set_min_height(CONTROL_H);
            let header_w = (ui.available_width() - CONTROL_RESERVE).max(HEADER_MIN);
            ui.vertical(|ui| {
                ui.set_max_width(header_w);
                row_header(ui, p, icon, title, desc);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                out = Some(control(ui));
            });
        });
    }
    out.expect("card_row control closure always runs")
}

/// The left side of a card row: an optional lead icon beside the `title`, with an
/// optional dim `desc` beneath it (the description wraps to the available width). Sets
/// tight title↔description spacing on its own `Ui` (callers scope it in a vertical).
fn row_header(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Option<&egui::TextureHandle>,
    title: &str,
    desc: Option<&str>,
) {
    // Pull the description tight under the title so they read as one unit. Negative
    // because each label's line box already reserves descent + leading between them.
    ui.spacing_mut().item_spacing.y = -2.0;
    ui.horizontal(|ui| {
        if let Some(t) = icon {
            icon_sized(ui, t, 20.0);
            ui.add_space(SPACE_4);
        }
        ui.label(egui::RichText::new(title).color(p.text));
    });
    if let Some(d) = desc {
        ui.label(egui::RichText::new(d).size(12.5).color(p.text_secondary));
    }
}

/// A Windows/iOS-style **toggle switch** (egui has no built-in one). Animates the knob
/// and fills the track with the accent when on. Returns a click [`Response`](egui::Response).
pub fn toggle(ui: &mut egui::Ui, p: &Palette, on: &mut bool) -> egui::Response {
    let desired = egui::vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how = ui.ctx().animate_bool(resp.id, *on);
        let radius = rect.height() * 0.5;
        let track = lerp_color(p.toggle_off, p.accent, how);
        let painter = ui.painter();
        painter.rect(rect, Rounding::same(radius), track, Stroke::NONE);
        let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how);
        painter.circle(
            egui::pos2(knob_x, rect.center().y),
            radius * 0.72,
            Color32::WHITE,
            Stroke::NONE,
        );
    }
    resp
}

/// A toggle plus its trailing "On"/"Off" text (the Notepad row look), as one
/// self-contained `[switch] On` unit — so it reads correctly whether a [`card_row`]
/// places it on the right (wide) or beneath the header (stacked).
pub fn toggle_with_label(ui: &mut egui::Ui, p: &Palette, on: &mut bool) -> egui::Response {
    ui.horizontal(|ui| {
        let resp = toggle(ui, p, on);
        ui.add_space(SPACE_2);
        let label = if *on { "On" } else { "Off" };
        ui.label(egui::RichText::new(label).color(p.text_secondary));
        resp
    })
    .inner
}

/// The accent-filled **primary** action button (e.g. Save). Lightens on hover, darkens
/// when pressed — see [`filled_button`].
pub fn primary_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    filled_button(ui, p.accent, text)
}

/// A neutral **secondary** action button (e.g. Cancel) — inherits the themed control fill,
/// which already provides the hover/press states.
pub fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(text).min_size(egui::vec2(BUTTON_W, CONTROL_H)))
}

/// A [`secondary_button`] whose label is **dimmed** (the secondary text color) so it reads as
/// a *placeholder* rather than a set value — e.g. the empty "Set" / "Add" chord slots in the
/// shortcut editor. Same fill + hover/press as `secondary_button`; only the text is muted.
pub fn placeholder_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(p.text_secondary))
            .min_size(egui::vec2(BUTTON_W, CONTROL_H)),
    )
}

/// A **destructive** action button (e.g. Delete) — the themed [`danger`](Palette::danger)
/// fill (a darker red than the brand accent) with white text.
pub fn danger_button(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    filled_button(ui, p.danger, text)
}

/// A solid-`base`-filled, white-text button with proper **hover/press feedback**.
/// egui's `Button::fill()` would override the on-hover effect, so instead we set the fill
/// *per interaction state* (lighter on hover, darker pressed — matching how the neutral
/// buttons respond) via scoped widget visuals, then add a plain themed button.
fn filled_button(ui: &mut egui::Ui, base: Color32, text: &str) -> egui::Response {
    let hover = lerp_color(base, Color32::WHITE, 0.10);
    let active = lerp_color(base, Color32::BLACK, 0.12);
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = base;
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.active.weak_bg_fill = active;
        // No border; white text in every state (a filled button).
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            w.bg_stroke = Stroke::NONE;
            w.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        }
        ui.add(
            egui::Button::new(egui::RichText::new(text).color(Color32::WHITE))
                .min_size(egui::vec2(BUTTON_W, CONTROL_H)),
        )
    })
    .inner
}

/// A standard single-line **text field** with balanced [`FIELD_MARGIN`] padding so it
/// matches the 32px control height (rather than egui's cramped default). Chain
/// `.desired_width(..)`, `.password(true)`, `.hint_text(..)`, etc. on the result.
pub fn text_field<'t>(text: &'t mut String, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .hint_text(hint.to_owned())
        .margin(FIELD_MARGIN)
}

/// A slider with a **stable-width value box**: egui sizes the box to the formatted
/// number (so it jitters as the digit count changes); we pin its minimum to
/// [`SLIDER_VALUE_W`] — wide enough for the widest value — so it stays put. `suffix` is
/// appended to the value, e.g. `"/s"` or `" ms"`.
pub fn slider<Num: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
    suffix: &str,
) -> egui::Response {
    let accent = Palette::new(ui.visuals().dark_mode).accent;
    // Both overrides are scoped to this slider (a cloned style), so other controls keep
    // the normal interact size and the global translucent text-selection color.
    ui.scope(|ui| {
        // The value box's min width is `interact_size.x`; widen it so it doesn't jitter.
        ui.spacing_mut().interact_size.x = SLIDER_VALUE_W;
        // The slider's trailing (filled) portion uses `selection.bg_fill`. Pin it to the
        // *solid* theme accent so the fill is the right shade per theme — the global
        // selection stays translucent (good for text), which otherwise made the light-mode
        // fill read washed-out / like the dark-mode blue.
        ui.visuals_mut().selection.bg_fill = accent;
        ui.add(egui::Slider::new(value, range).suffix(suffix.to_owned()))
    })
    .inner
}

/// Like [`slider`] but with an explicit `step` (the value snaps to multiples of it)
/// and a fixed number of `decimals` in the value box — for settings that should land
/// on clean increments and read consistently, e.g. a 0.5s-step interval shown as
/// `"2.0s"`. The value box stays type-in editable (egui's slider behavior).
pub fn slider_stepped<Num: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> egui::Response {
    let accent = Palette::new(ui.visuals().dark_mode).accent;
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.x = SLIDER_VALUE_W;
        ui.visuals_mut().selection.bg_fill = accent;
        ui.add(
            egui::Slider::new(value, range)
                .step_by(step)
                .fixed_decimals(decimals)
                .suffix(suffix.to_owned()),
        )
    })
    .inner
}

/// A determinate **progress bar**: a rounded inset groove filled with the accent up to
/// `fraction` (clamped to `0.0..=1.0`), spanning the available width at a slim fixed
/// height. Purely visual — no interaction; the caller draws any caption (a percentage,
/// byte counts) around it. Mirrors the slider's accent fill so it reads as one family.
/// Used by the archive-loading dialog.
/// A **pivot-style tab strip** for top-level section navigation (the Settings pages).
/// Unlike a button group it draws **no background fills**: the selected tab is the
/// semibold, full-strength label carrying a 2px accent underline that rides on a
/// full-width hairline; the rest are quiet [`text_secondary`](Palette::text_secondary)
/// labels that brighten on hover. Holding the accent to a thin indicator (never a filled
/// pill) is the whole point — it stops the active tab reading as a second
/// [`primary_button`]. `left_inset` insets the labels to the page margin while the
/// hairline still spans the strip's full width, so the active tab connects to the content
/// column below it. Returns `true` if the selection changed this frame.
pub fn tab_bar<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    current: &mut T,
    left_inset: f32,
    tabs: &[(T, &str, Option<crate::icon::Icon>)],
) -> bool {
    let mut changed = false;
    // Lay the labels out first, remembering each one's rect + state, so the indicators can
    // be painted *after* the hairline — they need to sit on top of it (connecting the tab
    // to its content), and egui paints in call order.
    let mut marks: Vec<(egui::Rect, bool, bool)> = Vec::with_capacity(tabs.len());
    let row = ui
        .horizontal(|ui| {
            ui.add_space(left_inset);
            ui.spacing_mut().item_spacing.x = TAB_GAP;
            for &(value, label, icon) in tabs {
                let selected = *current == value;
                let resp = tab_label(ui, p, selected, label, icon);
                marks.push((resp.rect, selected, resp.hovered()));
                if resp.clicked() && !selected {
                    *current = value;
                    changed = true;
                }
            }
        })
        .response
        .rect;

    // Full-width hairline along the strip's baseline, then the indicators on top of it.
    let baseline = row.bottom();
    let painter = ui.painter();
    painter.hline(
        ui.max_rect().x_range(),
        baseline - 0.5,
        Stroke::new(1.0_f32, p.card_stroke),
    );
    for (rect, selected, hovered) in marks {
        let color = if selected {
            p.accent
        } else if hovered {
            p.text_secondary
        } else {
            continue;
        };
        // Extend the bar a touch past each text edge so it reads as a deliberate marker.
        let bar = egui::Rect::from_min_max(
            egui::pos2(
                rect.left() - TAB_INDICATOR_OVERHANG,
                baseline - TAB_INDICATOR_H,
            ),
            egui::pos2(rect.right() + TAB_INDICATOR_OVERHANG, baseline),
        );
        painter.rect(bar, Rounding::same(1.0), color, Stroke::NONE);
    }
    changed
}

/// One [`tab_bar`] label: a click-sensing text cell with **no fill** that reserves room
/// beneath the text for the underline the strip paints. Sized at [`SECTION_SIZE`] (the
/// card-group heading tier) in the semibold face — section nav reads as a peer of the
/// section headings below it. The face is **always semibold**; only the *color*
/// distinguishes states (selected → full text color, hover → full text color, otherwise
/// the quiet secondary tone) — so switching tabs never reflows the strip.
fn tab_label(
    ui: &mut egui::Ui,
    p: &Palette,
    selected: bool,
    label: &str,
    icon: Option<crate::icon::Icon>,
) -> egui::Response {
    let font = FontId::new(SECTION_SIZE, FontFamily::Name(SEMIBOLD.into()));
    // Lay out with a placeholder color so the real tint can be chosen after we know the
    // hover state (which needs the response, which needs the size — hence layout first).
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let text_size = galley.size();
    // Optional leading icon (SF-Symbol-style tab glyph), tinted to match the label color.
    let icon_w = if icon.is_some() { TAB_ICON_SIZE } else { 0.0 };
    let icon_gap = if icon.is_some() { SPACE_2 - 2.0 } else { 0.0 };
    let pad_top = SPACE_2;
    let gap_below = 3.0; // the text sits close to its underline (a tight pivot, not a tab box)
    let height = pad_top + text_size.y + gap_below + TAB_INDICATOR_H;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(icon_w + icon_gap + text_size.x, height),
        egui::Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        let color = if selected || resp.hovered() {
            p.text
        } else {
            p.text_secondary
        };
        let mut x = rect.left();
        if let Some(ic) = icon {
            // Center the icon on the text's vertical midline (a hair up, so the optically
            // heavier glyph doesn't read low next to the cap-height label).
            let cy = rect.top() + pad_top + text_size.y / 2.0 - 1.0;
            let irect = egui::Rect::from_center_size(
                egui::pos2(x + icon_w / 2.0, cy),
                egui::Vec2::splat(icon_w),
            );
            crate::icon::paint_tinted(ui, irect, ic, color);
            x += icon_w + icon_gap;
        }
        ui.painter()
            .galley(egui::pos2(x, rect.top() + pad_top), galley, color);
    }
    resp
}

pub fn progress_bar(ui: &mut egui::Ui, p: &Palette, fraction: f32) -> egui::Response {
    let frac = fraction.clamp(0.0, 1.0);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 12.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let radius = Rounding::same(rect.height() * 0.5);
        let painter = ui.painter();
        // Track: control fill + the card hairline, so it reads like an inset groove.
        painter.rect(rect, radius, p.control, Stroke::new(1.0_f32, p.card_stroke));
        if frac > 0.0 {
            // Clamp the fill to at least its own height so even tiny progress shows as a
            // visible rounded nub rather than nothing.
            let w = (rect.width() * frac).clamp(rect.height(), rect.width());
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
            painter.rect(fill, radius, p.accent, Stroke::NONE);
        }
    }
    resp
}

/// Draw an icon texture at a fixed `height`, preserving aspect ratio.
pub fn icon_sized(ui: &mut egui::Ui, tex: &egui::TextureHandle, height: f32) {
    let size = tex.size_vec2();
    let w = if size.y > 0.0 {
        height * size.x / size.y
    } else {
        height
    };
    ui.image(egui::load::SizedTexture::new(
        tex.id(),
        egui::vec2(w, height),
    ));
}

/// Linear blend of two colors in sRGB space, `t` in `0.0..=1.0`.
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_tracks_theme() {
        let dark = Palette::new(true);
        let light = Palette::new(false);
        assert!(dark.dark && !light.dark);
        // The page always sits "below" the card: darker than the card on dark, lighter
        // than the card on light.
        assert!(dark.page.r() < dark.card.r());
        assert!(light.page.r() < light.card.r());
    }

    #[test]
    fn lerp_color_endpoints() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(100, 200, 50);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
        // Midpoint is the average.
        assert_eq!(lerp_color(a, b, 0.5), Color32::from_rgb(50, 100, 25));
    }

    #[test]
    fn style_sets_control_height() {
        let s = style(true);
        assert_eq!(s.spacing.interact_size.y, CONTROL_H);
        assert!(s.visuals.dark_mode);
    }
}
