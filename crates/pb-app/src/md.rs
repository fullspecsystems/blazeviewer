//! A lightweight **block-level Markdown** renderer for the Inspector's Describe tab — the
//! egui counterpart of the macOS `MarkdownBlocksView`. AI descriptions lean on headings and
//! bullet/numbered lists with inline emphasis; egui has no Markdown, so this parses the text
//! into blocks and renders each with its own style (inline **bold** / _italic_ / `code` /
//! `[links]` become one `LayoutJob` per line). Self-contained, no external dependency.
//!
//! The [`parse`] block splitter and [`inline_spans`] tokenizer are pure and unit-tested; only
//! [`render`] touches egui. Deliberately small — the shapes AI text actually uses. Code
//! fences / block quotes / rules aren't styled; they fall through to plain paragraphs.

use egui::text::{LayoutJob, TextWrapping};
use egui::{Color32, FontFamily, FontId, TextFormat};

use pb_ui::Palette;

/// One parsed Markdown block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Block {
    Heading { level: u8, text: String },
    Bullet(String),
    Numbered { marker: String, text: String },
    Paragraph(String),
}

/// Split Markdown `md` into block elements (ATX headings, `-/*/+` bullets, `N.` numbered
/// lists, else paragraphs joined across soft-wrapped lines) — a direct port of the mac
/// `MarkdownBlock.parse`.
pub fn parse(md: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    macro_rules! flush_para {
        () => {
            if !para.is_empty() {
                blocks.push(Block::Paragraph(para.join(" ")));
                para.clear();
            }
        };
    }

    for raw in md.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            flush_para!();
            continue;
        }

        // ATX heading: 1–6 '#' then a space.
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            flush_para!();
            blocks.push(Block::Heading {
                level: hashes as u8,
                text: line[hashes..].trim().to_string(),
            });
            continue;
        }

        // Bullet: '-', '*' or '+' then a space.
        let mut cs = line.chars();
        if let Some(first) = cs.next() {
            if matches!(first, '-' | '*' | '+') && cs.next() == Some(' ') {
                flush_para!();
                blocks.push(Block::Bullet(line[2..].trim().to_string()));
                continue;
            }
        }

        // Numbered: digits then '.' then a space (not a decimal like "3.5").
        if let Some(dot) = line.find('.') {
            let (head, rest) = line.split_at(dot);
            if !head.is_empty()
                && head.chars().all(|c| c.is_ascii_digit())
                && rest[1..].starts_with(' ')
            {
                flush_para!();
                blocks.push(Block::Numbered {
                    marker: head.to_string(),
                    text: rest[1..].trim().to_string(),
                });
                continue;
            }
        }

        para.push(line);
    }
    flush_para!();
    blocks
}

/// One inline run and its emphasis.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
}

/// Tokenize a single line's inline Markdown into styled spans: `**bold**`, `*`/`_italic_`,
/// `` `code` `` (no nested styling inside code), and `[text](url)` links (rendered as their
/// text). Unclosed markers degrade gracefully to literal text.
pub fn inline_spans(s: &str) -> Vec<Span> {
    let ch: Vec<char> = s.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut i = 0;
    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                spans.push(Span {
                    text: std::mem::take(&mut cur),
                    bold,
                    italic,
                    ..Default::default()
                });
            }
        };
    }
    while i < ch.len() {
        let c = ch[i];
        // Inline code — everything up to the next backtick is literal.
        if c == '`' {
            flush!();
            i += 1;
            let start = i;
            while i < ch.len() && ch[i] != '`' {
                i += 1;
            }
            let text: String = ch[start..i].iter().collect();
            if !text.is_empty() {
                spans.push(Span {
                    text,
                    code: true,
                    ..Default::default()
                });
            }
            i += (i < ch.len()) as usize; // consume the closing backtick when present
            continue;
        }
        // Link: [text](url) → keep the text, drop the URL.
        if c == '[' {
            if let Some(sp) = try_link(&ch, i) {
                flush!();
                spans.push(Span {
                    text: sp.0,
                    link: true,
                    ..Default::default()
                });
                i = sp.1;
                continue;
            }
        }
        // Bold **…**.
        if c == '*' && i + 1 < ch.len() && ch[i + 1] == '*' {
            flush!();
            bold = !bold;
            i += 2;
            continue;
        }
        // Italic *…* or _…_.
        if c == '*' || c == '_' {
            flush!();
            italic = !italic;
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    flush!();
    spans
}

/// If `ch[at]` begins a well-formed `[text](url)`, return `(text, index-after-')')`.
fn try_link(ch: &[char], at: usize) -> Option<(String, usize)> {
    debug_assert_eq!(ch[at], '[');
    let close = (at + 1..ch.len()).find(|&j| ch[j] == ']')?;
    if ch.get(close + 1) != Some(&'(') {
        return None;
    }
    let paren = (close + 2..ch.len()).find(|&j| ch[j] == ')')?;
    let text: String = ch[at + 1..close].iter().collect();
    Some((text, paren + 1))
}

/// Render `text` as block-level Markdown into `ui`, wrapping at `wrap_w`.
pub fn render(ui: &mut egui::Ui, p: &Palette, text: &str, wrap_w: f32) {
    // Pin the width: egui's `Label` overrides a `LayoutJob`'s own wrap width with the ui's
    // available width, which is *unbounded* in an auto-sized Window — so without this the
    // paragraphs don't wrap and each long line blows the panel out (the Details-tab lesson).
    ui.set_width(wrap_w);
    ui.spacing_mut().item_spacing.y = 7.0;
    for block in parse(text) {
        match block {
            Block::Heading { level, text } => {
                let size = match level {
                    1 => 18.0,
                    2 => 15.5,
                    _ => 13.5,
                };
                let job = inline_job(&text, size, true, p.text, p, wrap_w);
                ui.add(egui::Label::new(job).wrap().selectable(true));
            }
            Block::Bullet(text) => list_row(ui, p, "•".into(), &text, wrap_w),
            Block::Numbered { marker, text } => {
                list_row(ui, p, format!("{marker}."), &text, wrap_w)
            }
            Block::Paragraph(text) => {
                let job = inline_job(&text, 13.5, false, p.text, p, wrap_w);
                ui.add(egui::Label::new(job).wrap().selectable(true));
            }
        }
    }
}

/// A list item: a muted marker in a small fixed gutter + the wrapped item text.
fn list_row(ui: &mut egui::Ui, p: &Palette, marker: String, text: &str, wrap_w: f32) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.add_sized(
            [16.0, 0.0],
            egui::Label::new(egui::RichText::new(marker).color(panel_secondary(p))),
        );
        let job = inline_job(text, 13.5, false, p.text, p, (wrap_w - 23.0).max(40.0));
        // `.wrap()` is essential: a `Label` in a *horizontal* layout defaults to `Extend`
        // (no-wrap), so without this the item runs off and widens the whole panel.
        ui.add(egui::Label::new(job).wrap().selectable(true));
    });
}

/// Build a wrapped `LayoutJob` for one line's inline spans. `base_semibold` makes the whole
/// line semibold (headings); per-span bold also resolves to semibold (egui has no synthetic
/// bold), `code` to monospace, links to the accent. Italic falls back to upright (no italic
/// face is bundled) — the text still shows, just unslanted.
fn inline_job(
    s: &str,
    size: f32,
    base_semibold: bool,
    color: Color32,
    p: &Palette,
    wrap_w: f32,
) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width: wrap_w,
            ..Default::default()
        },
        ..Default::default()
    };
    for span in inline_spans(s) {
        let family = if span.code {
            FontFamily::Monospace
        } else if span.bold || base_semibold {
            FontFamily::Name(pb_ui::SEMIBOLD.into())
        } else {
            FontFamily::Proportional
        };
        let col = if span.link {
            p.accent
        } else if span.code {
            panel_secondary(p)
        } else {
            color
        };
        job.append(
            &span.text,
            0.0,
            TextFormat {
                font_id: FontId::new(size, family),
                color: col,
                ..Default::default()
            },
        );
    }
    job
}

/// The muted secondary color (mirrors `panels_ui::panel_secondary` so markers/code match the
/// rest of the panel chrome).
fn panel_secondary(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_gray(163)
    } else {
        Color32::from_gray(107)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_lists_and_paragraphs() {
        let md = "# Title\n\nA para that\nsoft-wraps.\n\n- one\n- two\n\n1. first\n2. second";
        let blocks = parse(md);
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 1,
                    text: "Title".into()
                },
                Block::Paragraph("A para that soft-wraps.".into()),
                Block::Bullet("one".into()),
                Block::Bullet("two".into()),
                Block::Numbered {
                    marker: "1".into(),
                    text: "first".into()
                },
                Block::Numbered {
                    marker: "2".into(),
                    text: "second".into()
                },
            ]
        );
    }

    #[test]
    fn decimal_is_not_a_numbered_list() {
        assert_eq!(
            parse("3.5 mm lens"),
            vec![Block::Paragraph("3.5 mm lens".into())]
        );
    }

    #[test]
    fn inline_bold_italic_code_and_links() {
        let spans = inline_spans("a **b** _i_ `c` [text](http://x)");
        let styled: Vec<(&str, bool, bool, bool, bool)> = spans
            .iter()
            .map(|s| (s.text.as_str(), s.bold, s.italic, s.code, s.link))
            .collect();
        assert_eq!(
            styled,
            vec![
                ("a ", false, false, false, false),
                ("b", true, false, false, false),
                (" ", false, false, false, false),
                ("i", false, true, false, false),
                (" ", false, false, false, false),
                ("c", false, false, true, false),
                (" ", false, false, false, false),
                ("text", false, false, false, true),
            ]
        );
    }

    #[test]
    fn malformed_link_is_literal() {
        // No `(url)` after the bracket → treated as plain text.
        let spans = inline_spans("[not a link]");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "[not a link]");
        assert!(!spans[0].link);
    }
}
