//! The docked windowed **toolbar** (task #61) — the winit shell's mouse-driven quick-access
//! strip, the sibling of the macOS `NSToolbar` (#55, `mac/…/ToolbarController.swift`).
//!
//! A full-width [`egui::TopBottomPanel`] docked under the menu bar in windowed mode, drawn in
//! the shared egui overlay ([`crate::egui_overlay`]) and composited by `pb-render` — so it costs
//! nothing on the fullscreen speed path (which never shows it). Every button dispatches an
//! existing [`Action`] through the same `CoreEvent::MenuAction`/pointer-nav path a keypress uses:
//! it's a discoverability layer, not new behavior. Left-aligned groups (Windows idiom), with the
//! photo counter + Fullscreen pinned right.
//!
//! Styling comes from [`pb_ui`] tokens + [`Palette`]: flat icon buttons, subtle hover fill, a
//! sunken pressed fill, and an **accent** fill for an active toggle (Info / Details / Tree on,
//! Slideshow running) or a **playing** Play button — parity with the macOS toolbar's on-state.
//!
//! Candidate to promote [`icon_button`] into `pb_ui` once the look settles; kept here for now so
//! the visual can be iterated without churning the design-system crate.

use std::time::Duration;

use egui::{Align, Color32, Layout, Rect, Sense, Vec2};
use pb_app_core::{Action, Keymap};
use pb_ui::icon::{self, Icon};
use pb_ui::Palette;

use crate::panels_ui::PanelAction;

/// Logical height of the docked toolbar strip: [`pb_ui::CONTROL_H`] (32) plus breathing room.
pub const TOOLBAR_H: f32 = 44.0;

/// Square edge of an icon button.
const BTN: f32 = 32.0;
/// Glyph size inside a button (a touch under the button so it has air).
const ICON: f32 = 17.0;
/// Fixed width reserved for a button's trailing text label — only the slideshow's interval
/// uses one. Sized for the widest value (`"59.5s"`, 5 glyphs; the range is 0.1–60s in 0.5s
/// steps) so changing the speed never reflows the rest of the strip. Padding short values to
/// `"4.0s"` would keep the width stable too, but looks cluttered — this reserves the room
/// instead and left-aligns the label in it.
const LABEL_W: f32 = 6.0 + 5.0 * 7.0;
/// Gap between buttons *within* a group.
const ITEM_GAP: f32 = 2.0;
/// Gap *between* groups — the one standard [`pb_ui::GAP`].
const GROUP_GAP: f32 = pb_ui::GAP;
/// Left/right inset of the strip content.
const EDGE_PAD: f32 = 8.0;

/// The toolbar's live state — a `Copy + Eq` snapshot the shell diffs each turn to decide
/// whether to re-render the retained overlay (the counter + play state change on nav, so the
/// strip is genuinely dirty every photo — see the plan's perf note). Not `MenuState`: that is
/// the native-menu sync channel and would re-push the OS menu on every transition.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ToolbarState {
    pub dark: bool,
    /// Surface alpha (0–255), from the shared *Info panel opacity* slider so the toolbar and the
    /// info panels stay in visual lockstep (translucency experiment, #61).
    pub alpha: u8,
    /// `(idx+1, count)` of the *displayed* photo, or `None` before the first present.
    pub counter: Option<(usize, usize)>,
    /// The current item has a playable motion component (enables Play).
    pub has_motion: bool,
    /// A video/animation/Live-Photo is actively playing (lights Play accent).
    pub playing: bool,
    /// The slideshow is running (lights its accent).
    pub slideshow: bool,
    /// The slideshow interval, formatted compactly at draw ("4s", "2.5s"). A `Duration` is
    /// `Copy + Eq`, so the snapshot still diffs cleanly and never jitters (it changes only on an
    /// explicit adjust), while carrying the decisecond precision the old whole-`u32` dropped.
    pub slideshow_interval: Duration,
    /// Info line (`i`) shown.
    pub info_basic: bool,
    /// Inspector open on its Details/EXIF tab.
    pub info_full: bool,
    /// Folder-tree panel visible.
    pub tree_visible: bool,
    /// The current item is a real on-disk file (Delete/Reveal apply).
    pub can_delete: bool,
}

/// Which optional groups the width plan keeps. The always-on groups (nav, playback, panels,
/// counter, fullscreen, delete) are never dropped; these three collapse under width pressure in
/// the order **folder-nav → rotate → random** (folder-nav first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LayoutPlan {
    pub random: bool,
    pub rotate: bool,
    pub folder_nav: bool,
}

/// Approx logical width a group occupies (n buttons + inner gaps).
fn group_w(buttons: usize) -> f32 {
    buttons as f32 * BTN + (buttons.saturating_sub(1)) as f32 * ITEM_GAP
}

/// The always-present left width (nav + playback + rotate-excluded + delete + panels), the
/// right cluster (counter + fullscreen), the group gaps between them, and the edge pads —
/// everything the plan must fit *before* it considers the optional groups.
fn base_width() -> f32 {
    let slideshow_w = BTN + LABEL_W; // Images icon + the fixed-width interval label
    let left_always = group_w(2)        // nav
        + BTN + slideshow_w             // playback (play + slideshow)
        + BTN                           // delete
        + group_w(3); // panels (info / details / tree)
    let right = 56.0 /* counter */ + BTN /* fullscreen */;
    // Groups on the left-always side: nav, playback, delete, panels = 4 → 3 internal gaps;
    // plus the gap before the right cluster. Be generous so left/right never collide.
    left_always + right + GROUP_GAP * 5.0 + EDGE_PAD * 2.0
}

/// Decide which optional groups fit in `avail` logical px. Pure + deterministic so it's unit
/// tested; the draw calls it once per frame. Adds optional groups back in priority order
/// (random highest, folder-nav lowest) while they still fit — i.e. drops folder-nav first.
pub fn plan(avail: f32) -> LayoutPlan {
    let mut used = base_width();
    let fits = |w: f32, used: &mut f32| {
        if *used + w + GROUP_GAP <= avail {
            *used += w + GROUP_GAP;
            true
        } else {
            false
        }
    };
    // Priority: keep random longest, then rotate, then folder-nav.
    let random = fits(group_w(2), &mut used);
    let rotate = fits(group_w(2), &mut used);
    let folder_nav = fits(group_w(2), &mut used);
    LayoutPlan {
        random,
        rotate,
        folder_nav,
    }
}

/// A blended-toward-`t` color (t=0 → `a`, t=1 → `b`). Small local mixer for the press/hover
/// tints (pb_ui keeps its own private one).
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), 255)
}

/// Draw a hold-to-fly **nav** button and fold its held-state into `pressed` (the single nav
/// slot the core tracks). A free fn — not a closure — so it borrows `pressed` only for the
/// call, leaving `pressed` free to read at the end of the frame.
#[allow(clippy::too_many_arguments)]
fn nav_btn(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    action: Action,
    mirror: bool,
    tip: &str,
    pressed: &mut Option<Action>,
) {
    let r = icon_button(ui, p, icon, false, true, mirror, None, tip);
    if r.is_pointer_button_down_on() {
        *pressed = Some(action);
    }
}

/// Draw one icon button and return its response. `active` fills it with the accent (a toggle
/// that's on, or Play while playing); `enabled=false` dims it and ignores input; `mirror` flips
/// the glyph (random-prev). Optional trailing `label` (the slideshow interval) widens it.
#[allow(clippy::too_many_arguments)]
fn icon_button(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    active: bool,
    enabled: bool,
    mirror: bool,
    label: Option<&str>,
    tip: &str,
) -> egui::Response {
    // A fixed reserved width (not one scaled to the string) so the slideshow interval label
    // can change digits without reflowing the strip — see [`LABEL_W`].
    let label_w = if label.is_some() { LABEL_W } else { 0.0 };
    let size = Vec2::new(BTN + label_w, BTN);
    // **Drag** sense, not plain click: a hold-to-fly nav button is held far longer than egui's
    // `max_click_duration` (0.8 s) and the hand drifts past `max_click_dist` (6 px), both of which
    // drop a click-only widget's `is_pointer_button_down_on` to false mid-hold — the bug where the
    // fly "stopped after about a second." A drag-sensed widget keeps `potential_drag_id` (and thus
    // `is_pointer_button_down_on`) alive until the real mouse-up. One-shot buttons still read
    // `clicked()`, which fires on a normal quick click regardless.
    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    let pressed = enabled && resp.is_pointer_button_down_on();
    let hovered = enabled && resp.hovered();
    // Background: accent when active (a shade darker/lighter on press/hover), else a subtle
    // raised fill on hover and a sunken one on press; nothing at rest.
    let bg = if active {
        Some(if pressed {
            mix(p.accent, Color32::BLACK, 0.14)
        } else if hovered {
            mix(p.accent, Color32::WHITE, 0.10)
        } else {
            p.accent
        })
    } else if pressed {
        Some(mix(
            p.control_hover,
            Color32::BLACK,
            if p.dark { 0.12 } else { 0.10 },
        ))
    } else if hovered {
        Some(p.control_hover)
    } else {
        None
    };
    if let Some(c) = bg {
        ui.painter().rect_filled(rect, pb_ui::RADIUS_CONTROL, c);
    }

    let tint = if !enabled {
        p.text_secondary.gamma_multiply(0.4)
    } else if active {
        p.on_accent
    } else {
        p.text
    };
    // Center the glyph in the left BTN×BTN square (label sits to its right).
    let glyph_box = Rect::from_center_size(
        egui::pos2(rect.left() + BTN / 2.0, rect.center().y),
        Vec2::splat(ICON),
    );
    if mirror {
        icon::paint_tinted_flipped(ui, glyph_box, icon, tint);
    } else {
        icon::paint_tinted(ui, glyph_box, icon, tint);
    }
    if let Some(text) = label {
        let text_pos = egui::pos2(rect.left() + BTN, rect.center().y);
        ui.painter().text(
            text_pos,
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.5),
            tint,
        );
    }

    if enabled && !tip.is_empty() {
        resp.clone().on_hover_text(tip)
    } else {
        resp
    }
}

/// Draw the toolbar strip. Non-nav buttons push [`PanelAction::ToolbarAction`]; nav/random
/// buttons are press-and-hold (hold-to-fly), reconciled against `nav_held` and pushing
/// [`PanelAction::ToolbarNavPress`]/[`ToolbarNavRelease`](PanelAction::ToolbarNavRelease).
pub fn toolbar(
    ctx: &egui::Context,
    st: &ToolbarState,
    keymap: &Keymap,
    nav_held: &mut Option<Action>,
    actions: &mut Vec<PanelAction>,
) {
    let p = Palette::new(st.dark);
    let base = p.page;
    let bg = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), st.alpha);
    let plan = plan(ctx.screen_rect().width());

    // A tooltip = a short name + the action's **live** first binding, read from the keymap so a
    // remap is always reflected (never a hand-typed shortcut that silently lies after the user
    // rebinds a key). Unbound actions show the name alone.
    let tip = |name: &str, action: Action| -> String {
        match keymap.bindings_for(action).first() {
            Some(c) => format!("{name}  ({})", c.shortcut_label()),
            None => name.to_string(),
        }
    };

    // Accumulated across both nav groups, reconciled after the panel closes.
    let pressed_now = egui::TopBottomPanel::top("pb_toolbar")
        .exact_height(TOOLBAR_H)
        .show_separator_line(false)
        .frame(
            egui::Frame::none()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(EDGE_PAD, 0.0)),
        )
        .show(ctx, |ui| {
            pb_ui::apply_to_ui(ui, st.dark);
            let mut pressed: Option<Action> = None;
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(ITEM_GAP, 0.0);

                // ── Nav (prev / next) — hold to fly ──
                nav_btn(
                    ui,
                    &p,
                    Icon::ChevronLeft,
                    Action::Prev,
                    false,
                    &tip("Previous", Action::Prev),
                    &mut pressed,
                );
                nav_btn(
                    ui,
                    &p,
                    Icon::ChevronRight,
                    Action::Next,
                    false,
                    &tip("Next", Action::Next),
                    &mut pressed,
                );

                // ── Random (prev / next) — hold to fly ──
                if plan.random {
                    ui.add_space(GROUP_GAP - ITEM_GAP);
                    nav_btn(
                        ui,
                        &p,
                        Icon::Shuffle,
                        Action::RandomPrev,
                        true,
                        &tip("Previous random", Action::RandomPrev),
                        &mut pressed,
                    );
                    nav_btn(
                        ui,
                        &p,
                        Icon::Shuffle,
                        Action::Random,
                        false,
                        &tip("Random", Action::Random),
                        &mut pressed,
                    );
                }

                // ── Folder nav ──
                if plan.folder_nav {
                    ui.add_space(GROUP_GAP - ITEM_GAP);
                    if icon_button(
                        ui,
                        &p,
                        Icon::AnglesLeft,
                        false,
                        true,
                        false,
                        None,
                        &tip("Previous folder", Action::PrevFolder),
                    )
                    .clicked()
                    {
                        actions.push(PanelAction::ToolbarAction(Action::PrevFolder));
                    }
                    if icon_button(
                        ui,
                        &p,
                        Icon::AnglesRight,
                        false,
                        true,
                        false,
                        None,
                        &tip("Next folder", Action::NextFolder),
                    )
                    .clicked()
                    {
                        actions.push(PanelAction::ToolbarAction(Action::NextFolder));
                    }
                }

                // ── Playback (play/pause + slideshow) ──
                ui.add_space(GROUP_GAP - ITEM_GAP);
                let (play_icon, play_name) = if st.playing {
                    (Icon::Pause, "Pause")
                } else {
                    (Icon::Play, "Play")
                };
                if icon_button(
                    ui,
                    &p,
                    play_icon,
                    st.playing,
                    st.has_motion,
                    false,
                    None,
                    &tip(play_name, Action::PlayPause),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::PlayPause));
                }
                // Compact decisecond label: whole seconds → "4s", otherwise one decimal → "2.5s"
                // (the slideshow interval is settable in 0.5s steps, so it can be non-integer).
                let deci = (st.slideshow_interval.as_secs_f64() * 10.0).round() as u64;
                let interval = if deci.is_multiple_of(10) {
                    format!("{}s", deci / 10)
                } else {
                    format!("{}.{}s", deci / 10, deci % 10)
                };
                if icon_button(
                    ui,
                    &p,
                    Icon::Images,
                    st.slideshow,
                    true,
                    false,
                    Some(&interval),
                    &tip("Slideshow", Action::SlideshowToggle),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::SlideshowToggle));
                }

                // ── Image ops (rotate + delete) ──
                if plan.rotate {
                    ui.add_space(GROUP_GAP - ITEM_GAP);
                    if icon_button(
                        ui,
                        &p,
                        Icon::RotateLeft,
                        false,
                        true,
                        false,
                        None,
                        &tip("Rotate left", Action::RotateCcw),
                    )
                    .clicked()
                    {
                        actions.push(PanelAction::ToolbarAction(Action::RotateCcw));
                    }
                    if icon_button(
                        ui,
                        &p,
                        Icon::RotateRight,
                        false,
                        true,
                        false,
                        None,
                        &tip("Rotate right", Action::RotateCw),
                    )
                    .clicked()
                    {
                        actions.push(PanelAction::ToolbarAction(Action::RotateCw));
                    }
                }
                ui.add_space(GROUP_GAP - ITEM_GAP);
                if icon_button(
                    ui,
                    &p,
                    Icon::Trash,
                    false,
                    st.can_delete,
                    false,
                    None,
                    &tip("Delete", Action::Delete),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::Delete));
                }

                // ── Panels (folder tree / info line / info panel) ──
                // Folders dock on the LEFT of the viewer, so the folder-tree toggle leads the
                // group; the plain-`i` bottom info line sits in the middle; the circled-`i` info
                // panel (Details/EXIF) closes the group on the right. (Owner icon spec 2026-07-13.)
                ui.add_space(GROUP_GAP - ITEM_GAP);
                if icon_button(
                    ui,
                    &p,
                    Icon::FolderTree,
                    st.tree_visible,
                    true,
                    false,
                    None,
                    &tip("Folder tree", Action::FolderTree),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::FolderTree));
                }
                if icon_button(
                    ui,
                    &p,
                    Icon::InfoLine,
                    st.info_basic,
                    true,
                    false,
                    None,
                    &tip("Info line", Action::Info),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::Info));
                }
                if icon_button(
                    ui,
                    &p,
                    Icon::Info,
                    st.info_full,
                    true,
                    false,
                    None,
                    &tip("Info panel", Action::FullExif),
                )
                .clicked()
                {
                    actions.push(PanelAction::ToolbarAction(Action::FullExif));
                }

                // ── Right cluster: counter + fullscreen ──
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(ITEM_GAP, 0.0);
                    if icon_button(
                        ui,
                        &p,
                        Icon::Expand,
                        false,
                        true,
                        false,
                        None,
                        &tip("Fullscreen", Action::Fullscreen),
                    )
                    .clicked()
                    {
                        actions.push(PanelAction::ToolbarAction(Action::Fullscreen));
                    }
                    if let Some((idx, count)) = st.counter {
                        ui.add_space(GROUP_GAP - ITEM_GAP);
                        ui.label(
                            egui::RichText::new(format!("{idx} / {count}"))
                                .color(p.text_secondary)
                                .size(13.0),
                        );
                    }
                });
            });
            pressed
        })
        .inner;

    // Reconcile the single held-nav slot: a change means release the old and/or press the new.
    if pressed_now != *nav_held {
        if nav_held.is_some() {
            actions.push(PanelAction::ToolbarNavRelease);
        }
        if let Some(a) = pressed_now {
            actions.push(PanelAction::ToolbarNavPress(a));
        }
        *nav_held = pressed_now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_collapses_folder_nav_first_then_rotate_then_random() {
        // Very wide: everything shows.
        let wide = plan(4000.0);
        assert_eq!(
            wide,
            LayoutPlan {
                random: true,
                rotate: true,
                folder_nav: true
            }
        );
        // Very narrow: all optional groups drop, base still "fits" (clamped).
        let narrow = plan(0.0);
        assert_eq!(
            narrow,
            LayoutPlan {
                random: false,
                rotate: false,
                folder_nav: false
            }
        );
    }

    #[test]
    fn plan_is_monotonic_in_width() {
        // As width grows, a group never disappears once it has appeared (collapse is ordered).
        let mut prev = plan(0.0);
        let mut w = 0.0;
        while w < 4000.0 {
            let cur = plan(w);
            // folder-nav is lowest priority: it's only on if rotate is on; rotate only if random.
            assert!(!cur.rotate || cur.random, "rotate implies random at w={w}");
            assert!(
                !cur.folder_nav || cur.rotate,
                "folder_nav implies rotate at w={w}"
            );
            // Monotonic: growing width never removes a group.
            assert!(cur.random || !prev.random, "random regressed at w={w}");
            prev = cur;
            w += 25.0;
        }
    }
}
