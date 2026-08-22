//! Shell-neutral rich-panel **models** (task #54 Phase 0, ADR-023): the semantic
//! "what a panel shows" — rows, states, copy payloads — owned by the core so every
//! presenter (the CPU HUD today, egui / native SwiftUI later) renders the same data
//! and none derives its own. Pure data + pure derivations: no I/O, no rasterizer,
//! no `egui`/`winit`/AppKit. `AppCore` builds these from its RAM-only caches
//! (`details_panel` / `help_panel` / `text_panel` / `describe_panel`); the interim
//! `lines()` projections feed the HUD's paragraph renderer and retire with it.

/// A content snapshot of the Inspector's **active** tab, for the native host's
/// change-detection (task #54): the tick builds this each frame the Inspector is
/// natively visible and re-signals [`crate::contract::CoreEffect::PanelsChanged`] when it
/// differs from the last — so a tab switch, a photo change, and an async OCR / describe
/// result landing all re-pull the SwiftUI panel, without a per-tick marker storm.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InspectorSnapshot {
    Details(DetailsPanel),
    Text(TextPanel),
    Describe(DescribePanel),
}

/// One row of the Details (full-EXIF) table — mirrors the HUD's table shapes so the
/// interim renderer is a 1:1 map, but owned here so presenters and copy payloads
/// never depend on the rasterizer crate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DetailRow {
    /// A full-width line spanning both columns (filename header, section titles).
    Span { text: String, bold: bool },
    /// A two-column row: label + value.
    Pair { label: String, value: String },
    /// A bold section heading carrying **inline action buttons** (task #137) — the
    /// Generation block's Copy Prompt / Copy Data pair.
    ///
    /// The buttons live here rather than in the context menu because they act on
    /// *this section*, and a context menu that lists them for every photo would
    /// offer two commands that fail on almost all of them.
    ///
    /// Carries [`Action`](crate::action::Action) values, not strings: both shells
    /// already dispatch that vocabulary (winit directly, macOS via
    /// [`Action::id`](crate::action::Action::id)), so a button needs no new
    /// plumbing and cannot drift from the menu item that runs the same command.
    Section {
        text: String,
        actions: Vec<RowAction>,
    },
    /// A label plus an **italic, muted note** — not a value, but an explanation of
    /// why there is no value (task #137): *Prompt — assembled by PromptCombinator*.
    ///
    /// Distinct from [`Pair`](DetailRow::Pair) because the reader must be able to
    /// tell a fact about the image from a fact about our *knowledge* of it at a
    /// glance. Rendering it like every other value invites it being copied out and
    /// pasted as if it were the prompt.
    Note { label: String, text: String },
    /// A full-width **wrapped paragraph** with no label column.
    ///
    /// Currently **unused**: generation prompts moved to the labelled [`Pair`](DetailRow::Pair)
    /// column for consistency with the rest of the Details table (owner call 2026-08-07). The
    /// variant + its per-shell renderers are retained as the reusable full-width-paragraph
    /// primitive — and because dropping it would mean editing the winit `panels_ui.rs` arm, which
    /// a macOS session can't compile-check (the cross-platform trap). `#[allow(dead_code)]` rather
    /// than a blind delete; a Windows session can remove it wholesale if it stays unwanted.
    #[allow(dead_code)]
    Body { text: String },
}

/// One inline button on a [`DetailRow::Section`] heading (task #137).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RowAction {
    /// Short button text — "Copy prompt", not "Copy Generation Prompt". The
    /// heading beside it already supplies the "generation" half.
    pub label: String,
    pub action: crate::action::Action,
}

/// The Inspector ▸ Details tab: the full metadata table for the displayed photo.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DetailsPanel {
    pub rows: Vec<DetailRow>,
}

impl DetailsPanel {
    /// The panel's copy payload: every row as text, one per line (`label: value`).
    /// Display truncation never applies here — copy is always the full data.
    pub fn copy_text(&self) -> String {
        self.rows
            .iter()
            .map(|r| match r {
                DetailRow::Span { text, .. } | DetailRow::Section { text, .. } => text.clone(),
                DetailRow::Pair { label, value } => format!("{label}: {value}"),
                DetailRow::Note { label, text } => format!("{label}: {text}"),
                // Body text is already the value — a `label:` prefix would be
                // inventing one, and a pasted prompt must be paste-ready.
                DetailRow::Body { text } => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One section of the Help panel: a heading over `(description, shortcut)` rows.
/// Shortcut strings come from the **live** keymap, so a remap in Settings shows here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HelpSection {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

/// The Help panel: the keyboard/command reference, sourced from the live keymap +
/// menu model (never hardcoded chords).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HelpPanel {
    pub sections: Vec<HelpSection>,
}

/// The Inspector ▸ Text tab's semantic state (task #45).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TextBody {
    /// Empty deck — nothing to scan.
    NoPhoto,
    /// The OCR/QR scan is in flight (or about to start).
    Scanning,
    /// Scan finished: QR payloads + recognized paragraphs (either may be empty).
    /// `ocr_error` carries the engine's failure line when recognition failed.
    Ready {
        qr: Vec<String>,
        paragraphs: Vec<String>,
        ocr_error: Option<String>,
    },
}

/// The Inspector ▸ Text tab: recognized text + QR payloads for the displayed photo.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TextPanel {
    pub body: TextBody,
}

impl TextPanel {
    pub const TITLE: &'static str = "Text in image";

    /// Interim HUD projection: `(text, semibold)` lines for `render_paragraph`.
    /// QR payloads lead, then paragraphs separated by blank lines — exactly the
    /// panel the HUD drew before the model extraction. Retires with the HUD tab.
    pub fn lines(&self) -> Vec<(String, bool)> {
        let mut lines = vec![(Self::TITLE.to_string(), true)];
        match &self.body {
            TextBody::NoPhoto => {}
            TextBody::Scanning => lines.push(("Reading text…".to_string(), false)),
            TextBody::Ready {
                qr,
                paragraphs,
                ocr_error,
            } => {
                for q in qr {
                    lines.push((format!("QR code → {q}"), false));
                }
                if !qr.is_empty() && !paragraphs.is_empty() {
                    lines.push((String::new(), false));
                }
                for (i, p) in paragraphs.iter().enumerate() {
                    if i > 0 {
                        lines.push((String::new(), false));
                    }
                    lines.push((p.clone(), false));
                }
                if paragraphs.is_empty() {
                    if let Some(e) = ocr_error {
                        lines.push((e.clone(), false));
                    } else if qr.is_empty() {
                        lines.push(("No text found".to_string(), false));
                    }
                }
            }
        }
        lines
    }
}

/// The Inspector ▸ Describe tab's semantic state (task #44).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DescribeBody {
    /// Empty deck — nothing to describe.
    NoPhoto,
    /// No cached result and no scan running (the "press D" invitation).
    Idle,
    /// The vision backend is generating.
    Busy,
    /// The description (or the Ask answer) — the model's raw text, which may be
    /// Markdown. The native presenter renders it as Markdown; the HUD shows it as-is.
    Ready(String),
    /// The backend failed (misconfigured endpoint, refusal, transport error).
    /// Pressing `D` again retries — a cached error is cleared on the explicit key.
    Error(String),
}

/// The Inspector ▸ Describe tab: the AI description/answer for the displayed photo.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DescribePanel {
    pub body: DescribeBody,
}

impl DescribePanel {
    pub const TITLE: &'static str = "Description";

    /// The copy payload (Copy AI Description): only a real result copies.
    pub fn copy_text(&self) -> Option<&str> {
        match &self.body {
            DescribeBody::Ready(text) => Some(text),
            _ => None,
        }
    }

    /// Interim HUD projection, mirroring the pre-extraction panel. Retires with
    /// the HUD tab.
    pub fn lines(&self) -> Vec<(String, bool)> {
        let mut lines = vec![(Self::TITLE.to_string(), true)];
        match &self.body {
            DescribeBody::NoPhoto => {}
            DescribeBody::Idle => {
                lines.push(("Press D to describe this image.".to_string(), false))
            }
            DescribeBody::Busy => lines.push(("Describing…".to_string(), false)),
            DescribeBody::Ready(text) | DescribeBody::Error(text) => {
                lines.push((text.clone(), false))
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_copy_text_is_every_row_untruncated() {
        let panel = DetailsPanel {
            rows: vec![
                DetailRow::Span {
                    text: "IMG_0001.jpg".into(),
                    bold: true,
                },
                DetailRow::Pair {
                    label: "Dimensions".into(),
                    value: "6048 × 4024".into(),
                },
                DetailRow::Pair {
                    label: "Codec".into(),
                    value: "JPEG".into(),
                },
            ],
        };
        assert_eq!(
            panel.copy_text(),
            "IMG_0001.jpg\nDimensions: 6048 × 4024\nCodec: JPEG"
        );
    }

    #[test]
    fn text_panel_lines_order_qr_then_paragraphs() {
        let panel = TextPanel {
            body: TextBody::Ready {
                qr: vec!["https://example.com".into()],
                paragraphs: vec!["First block".into(), "Second block".into()],
                ocr_error: None,
            },
        };
        let lines = panel.lines();
        assert_eq!(lines[0], (TextPanel::TITLE.to_string(), true));
        assert_eq!(lines[1].0, "QR code → https://example.com");
        assert_eq!(lines[2].0, "", "blank line between QR block and paragraphs");
        assert_eq!(lines[3].0, "First block");
        assert_eq!(lines[4].0, "", "blank line between paragraphs");
        assert_eq!(lines[5].0, "Second block");
    }

    #[test]
    fn text_panel_lines_cover_empty_error_and_busy_states() {
        let ready = |qr: Vec<String>, paras: Vec<String>, err: Option<&str>| TextPanel {
            body: TextBody::Ready {
                qr,
                paragraphs: paras,
                ocr_error: err.map(String::from),
            },
        };
        assert_eq!(ready(vec![], vec![], None).lines()[1].0, "No text found");
        assert_eq!(
            ready(vec![], vec![], Some("OCR failed")).lines()[1].0,
            "OCR failed"
        );
        // QR only: no "No text found" noise under a real payload.
        let qr_only = ready(vec!["wifi:pass".into()], vec![], None);
        assert_eq!(qr_only.lines().len(), 2);
        let busy = TextPanel {
            body: TextBody::Scanning,
        };
        assert_eq!(busy.lines()[1].0, "Reading text…");
        let none = TextPanel {
            body: TextBody::NoPhoto,
        };
        assert_eq!(none.lines().len(), 1, "headline only on the empty deck");
    }

    #[test]
    fn describe_panel_copies_only_real_results() {
        let ready = DescribePanel {
            body: DescribeBody::Ready("A red bird on a wire.".into()),
        };
        assert_eq!(ready.copy_text(), Some("A red bird on a wire."));
        for body in [
            DescribeBody::NoPhoto,
            DescribeBody::Idle,
            DescribeBody::Busy,
            DescribeBody::Error("endpoint down".into()),
        ] {
            assert_eq!(DescribePanel { body }.copy_text(), None);
        }
    }

    #[test]
    fn describe_panel_lines_cover_all_states() {
        let line1 = |body: DescribeBody| DescribePanel { body }.lines()[1].0.clone();
        assert_eq!(line1(DescribeBody::Idle), "Press D to describe this image.");
        assert_eq!(line1(DescribeBody::Busy), "Describing…");
        assert_eq!(line1(DescribeBody::Ready("text".into())), "text");
        assert_eq!(line1(DescribeBody::Error("oops".into())), "oops");
    }
}
