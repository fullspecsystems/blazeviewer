// The archive door card (task #105) — the SwiftUI half of what the egui shell draws in
// `panels_ui::door_card`.
//
// An archive sitting in a folder is a typed deck item ("a door"), and its frame is a 1×1
// transparent sentinel: it draws *nothing*, deliberately. Browsing past a door must never
// decompress it, so there is no picture of one to show. This card is therefore an
// archive's entire on-screen presence — its artwork, its name, and the key that opens it —
// and it is chrome, not content: pushing the artwork through the photo pipeline instead is
// what produced a 12× upscaled glyph, a photo-sized ring slot for an icon, an invented grey
// backdrop, and a 2.1× magnification, in that order.
//
// The core owns every fact here (`door_card`: name, format, the live Open shortcut) and the
// artwork (`doorArtwork()`, one decode + one NSImage per process, blue on macOS to sit with
// Finder). This view only lays them out — in the shared panel language, so it reads as a
// sibling of Help and the Inspector rather than a lookalike that drifts.

import AppKit
import PbMacFfi
import SwiftUI

struct DoorCardView: View {
    /// The archive's full file name, e.g. `wedding-photos.zip` — elided to fit below.
    let name: String
    /// The heading, e.g. `ZIP Archive`.
    let format: String
    /// The Open key, from the live keymap.
    let shortcut: String
    /// The folder artwork, or `nil` if it couldn't be decoded — the card then degrades to
    /// text and a button rather than the door vanishing.
    let artwork: NSImage?
    let opacity: Double
    /// The unobstructed content area the overlay grants — the window minus the edge insets
    /// and any open side pane. The card centres in *this*, so an open panel shifts it
    /// rather than sitting on top of it, and shrinks it rather than clipping it.
    let available: CGSize
    let onOpen: () -> Void

    /// Plain values rather than the `CoreModel` (the `HelpPanelView` idiom): the view then
    /// renders with no core behind it, which is what lets `--pb-door-shot` shoot it
    /// offscreen — the only way to *see* this card in an environment where screen capture
    /// and Accessibility are unavailable.
    init(
        name: String, format: String, shortcut: String, artwork: NSImage?,
        opacity: Double = 1, available: CGSize, onOpen: @escaping () -> Void
    ) {
        self.name = name
        self.format = format
        self.shortcut = shortcut
        self.artwork = artwork
        self.opacity = opacity
        self.available = available
        self.onOpen = onOpen
    }

    /// The card for the door the core currently presents.
    init(model: CoreModel, available: CGSize) {
        self.init(
            name: model.doorName, format: model.doorFormat, shortcut: model.doorShortcut,
            artwork: model.doorArtwork(), opacity: model.panelOpacity, available: available,
            // The same command `P` runs, and the same one the egui card's button pushes:
            // entering a door IS play/pause on an archive item.
            onOpen: { model.triggerPlay() })
    }

    /// The window's backing scale: the artwork is capped at its own native size for this
    /// display, so it is never magnified. Dragging between a 1× and a 2× display changes
    /// that cap, so the view has to observe it.
    @Environment(\.displayScale) private var displayScale

    /// The design cap for the artwork's width, in points.
    ///
    /// It sizes the *frame*; what the eye measures is the folder inside it, which is 91% of
    /// that (`engine::door_artwork` crops to the shadow — the shadow is what's left, and it
    /// is mirrored so the folder stays centred). Own the two together: 162 pt of frame ≈
    /// 148 pt of folder, 25% more than this drew before the crop was fixed, which is the
    /// size the owner asked for (2026-07-17). Keep it in step with the egui card's
    /// `DOOR_ART_PT`.
    private static let artCap: CGFloat = 162
    /// The card **adapts** to its filename between these bounds. Most archive names are
    /// short and a card sized for the worst case is a slab of empty space; an unbounded one
    /// becomes a banner that collides with the panels. Past the max — or past whatever the
    /// window allows — the name middle-elides instead of widening it further.
    private static let widthMin: CGFloat = 216
    private static let widthMax: CGFloat = 340
    /// The inset around the body, the **same** on every side, so the card's padding reads as
    /// one value rather than four guesses; and the one gap between the body's rows, so
    /// art→name and name→button match.
    private static let bodyPad: CGFloat = 16
    private static let gap: CGFloat = 12
    /// The Open button's height, held as a constant only so the artwork's vertical budget
    /// can be computed before layout. Deliberately a slight over-estimate of what
    /// `.bordered`/`.large` renders: over-estimating shrinks the art a touch early on a
    /// cramped window, while under-estimating would push the card past the window's edge.
    private static let buttonHeight: CGFloat = 34
    /// Below this the artwork is more speck than picture — drop it and keep the name and
    /// the Open control at readable sizes, which is what a 520×360 window needs.
    private static let artFloor: CGFloat = 32

    var body: some View {
        let width = cardWidth
        let art = artSize(in: width)
        VStack(spacing: 0) {
            // The card's own header, centred: it takes no ✕ — a door isn't dismissible,
            // you navigate off it — so there's nothing to balance a leading title against.
            // Same height and groove as every other panel (`PanelMetrics`), so it sits in
            // the family.
            Text(format)
                .font(.headline)
                .frame(height: PanelMetrics.headerHeight)
            PanelDivider()

            VStack(spacing: Self.gap) {
                if let art, let image = artwork {
                    Image(nsImage: image)
                        .resizable()
                        .interpolation(.high)
                        .frame(width: art.width, height: art.height)
                }
                // Middle elision keeps the extension — the rule the strip's cells use, and
                // the extension is the one part of an archive's name you can't infer from
                // the card around it.
                Text(name)
                    .font(.body)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Button(action: onOpen) {
                    HStack(spacing: 6) {
                        Image(systemName: "doc.zipper")
                        Text("Open")
                        if !shortcut.isEmpty {
                            ShortcutView(shortcut: shortcut)
                        }
                    }
                }
                .buttonStyle(.bordered)
                .controlSize(.large)
                // The core owns the keyboard; a focus ring on the only button here reads as
                // an errant default (the welcome surface makes the same call).
                .focusEffectDisabled()
            }
            .padding(Self.bodyPad)
        }
        .frame(width: width)
        .panelBackground(opacity: opacity)
        // The full name, for when it's elided — and for VoiceOver.
        .help(name)
        .arrowCursorOnHover()
    }

    /// The card's width: as wide as its name wants, clamped to the bounds above and to the
    /// room it has. Measured off the text rather than let to size naturally, because the
    /// name has to elide at the cap — and a `Text` that elides reports no natural width to
    /// clamp.
    private var cardWidth: CGFloat {
        let nameW = textWidth(name, .preferredFont(forTextStyle: .body))
        let formatW = textWidth(format, .preferredFont(forTextStyle: .headline))
        let wanted = max(
            nameW + 2 * Self.bodyPad,
            formatW + 2 * PanelMetrics.headerLeadingInset)
        return min(
            wanted.clamped(to: Self.widthMin...Self.widthMax),
            max(Self.widthMin, available.width))
    }

    /// The artwork's drawn size, or `nil` when there's no room worth giving it.
    ///
    /// Capped at the design size **and** at the asset's native size for this display, then
    /// fitted to the window — and on a cramped window the **art gives way first**, so the
    /// name and the Open button keep their readable sizes rather than everything scaling
    /// down together into an unreadable card.
    private func artSize(in width: CGFloat) -> (width: CGFloat, height: CGFloat)? {
        // The asset's size is in **pixels** (`CoreModel.doorArtwork` builds it that way), so
        // it is both the aspect and the never-magnify cap in one.
        guard let px = artwork?.size, px.width > 0, px.height > 0 else { return nil }
        let aspect = px.width / px.height
        let native = px.width / max(displayScale, 1)
        // Everything the card needs *besides* the art and its gap.
        let chrome =
            PanelMetrics.headerHeight + 1 + 2 * Self.bodyPad
            + lineHeight(.preferredFont(forTextStyle: .body)) + Self.gap + Self.buttonHeight
        let vertical = (available.height - chrome - Self.gap) * aspect
        let w = min(Self.artCap, native, width - 2 * Self.bodyPad, vertical)
        guard w >= Self.artFloor else { return nil }
        return (width: w, height: w / aspect)
    }

    private func textWidth(_ s: String, _ font: NSFont) -> CGFloat {
        (s as NSString).size(withAttributes: [.font: font]).width
    }

    private func lineHeight(_ font: NSFont) -> CGFloat {
        font.ascender - font.descender + font.leading
    }
}

/// `--pb-door-shot <dir>` (dev diagnostics): render the door card straight to PNGs and
/// quit — the macOS analog of the winit shell's `--egui-shot`, and the same trick
/// `--pb-f-smoke` uses to prove behaviour without Accessibility permission.
///
/// It exists because this card cannot otherwise be *seen*: an agent (or a CI box, or a Mac
/// at the login window) has no screen capture, and a card whose whole job is to look right
/// is not verified by a green test. `ImageRenderer` needs no window and no core, which is
/// exactly why the view takes plain values.
///
/// Shoots the cases that have historically been where this card went wrong: a short name, a
/// long one (does it middle-elide, or become a banner?), the artwork missing (does it
/// degrade, or vanish?), the 520×360 minimum window (does the art give way, or does the card
/// overflow?), and both appearances.
enum DoorShot {
    @MainActor static func runIfRequested(artwork: NSImage?) {
        let args = ProcessInfo.processInfo.arguments
        guard let i = args.firstIndex(of: "--pb-door-shot"), i + 1 < args.count else { return }
        let dir = URL(fileURLWithPath: args[i + 1])
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

        let roomy = CGSize(width: 1200, height: 800)
        // The macOS minimum window (`.frame(minWidth: 520, minHeight: 360)`) less the edge
        // insets — the card must fit here whole. It does, with room to spare, so the art
        // never actually gives way at the minimum; `tiny` below is what proves the fallback
        // works at all, by asking for less room than any real window can offer.
        let minWindow = CGSize(width: 520 - 48, height: 360 - 48)
        let tiny = CGSize(width: 300, height: 200)
        let long = "a-really-quite-long-archive-name-that-should-middle-elide-nicely.zip"
        let cases: [(String, DoorCardView, Bool)] = [
            ("short-dark", card("wedding-photos.zip", "ZIP Archive", artwork, roomy), true),
            ("short-light", card("wedding-photos.zip", "ZIP Archive", artwork, roomy), false),
            ("long-dark", card(long, "7z Archive", artwork, roomy), true),
            ("no-art-dark", card("holiday.7z", "7z Archive", nil, roomy), true),
            ("min-window-dark", card("vacation.tar.gz", "TAR.GZ Archive", artwork, minWindow), true),
            ("tiny-dark", card("vacation.tar.gz", "TAR.GZ Archive", artwork, tiny), true),
        ]
        for (label, view, dark) in cases {
            // The letterbox behind it, so the material reads as it will in the app — a
            // translucent card shot over nothing tells you nothing about its contrast.
            let framed =
                ZStack {
                    Color(nsColor: NSColor(red: 10 / 255, green: 10 / 255, blue: 12 / 255, alpha: 1))
                    view
                }
                .frame(width: 640, height: 520)
                .environment(\.colorScheme, dark ? .dark : .light)
            shoot(framed, to: dir, "door-\(label)")
        }

        // Isolation shots: just the artwork Image over solid backgrounds, at the card's
        // display size — the SAME SwiftUI Image path the card uses, with nothing else in
        // frame. A broad shadow HERE that the correct off-thread composite (door_alpha's
        // door_over_white.png) doesn't have means the SwiftUI draw is the culprit, and the
        // white/gray pair says whether it's a background-contrast effect.
        if let art = artwork {
            for (bg, label) in [(Color.white, "white"), (Color(white: 0.92), "gray")] {
                let raw = ZStack {
                    bg
                    Image(nsImage: art)
                        .resizable()
                        .interpolation(.high)
                        .frame(width: 162, height: 162 * art.size.height / art.size.width)
                }
                .frame(width: 400, height: 400)
                shoot(raw, to: dir, "art-only-\(label)")
            }
        }

        // The full card over a LIGHT background, light scheme — the closest the harness gets
        // to the owner's live light-mode view. The material flattens to opaque here (so this
        // isn't the *material's* look), but the card's own `.shadow(radius: 18)` renders, so
        // this shows whether THAT reads as the broad halo over light.
        let cardLight =
            ZStack {
                Color.white
                card("wedding-photos.zip", "ZIP Archive", artwork, roomy)
            }
            .frame(width: 640, height: 560)
            .environment(\.colorScheme, .light)
        shoot(cardLight, to: dir, "card-over-white-light")

        // A pair of welcome-style pills (the same `panelBackground`) over light — the exact
        // surface where the owner first flagged the oversized shadow. Verifies the shadow
        // token fix on a small control, not just the big card.
        let pills =
            ZStack {
                Color.white
                VStack(spacing: 24) {
                    ForEach(["Open File", "Open Folder"], id: \.self) { label in
                        Text(label)
                            .font(.callout)
                            .padding(.horizontal, 20)
                            .padding(.vertical, 12)
                            .panelBackground(opacity: 0.92)
                    }
                }
            }
            .frame(width: 420, height: 320)
            .environment(\.colorScheme, .light)
        shoot(pills, to: dir, "pills-over-white-light")
        exit(0)
    }

    @MainActor private static func shoot<V: View>(_ view: V, to dir: URL, _ name: String) {
        let renderer = ImageRenderer(content: view)
        renderer.scale = 2
        guard let cg = renderer.cgImage else { return }
        let rep = NSBitmapImageRep(cgImage: cg)
        if let png = rep.representation(using: .png, properties: [:]) {
            try? png.write(to: dir.appendingPathComponent("\(name).png"))
            NSLog("[door-shot] wrote \(name).png")
        }
    }

    private static func card(
        _ name: String, _ format: String, _ art: NSImage?, _ available: CGSize
    ) -> DoorCardView {
        DoorCardView(
            name: name, format: format, shortcut: "P", artwork: art, opacity: 0.92,
            available: available, onOpen: {})
    }
}

extension Comparable {
    fileprivate func clamped(to r: ClosedRange<Self>) -> Self {
        min(max(self, r.lowerBound), r.upperBound)
    }
}
