import AppKit
import PbMacFfi
import SwiftUI

/// The **Subtitles** settings tab (task #90.4) — the owner's eight axes, over a live
/// preview.
///
/// Its own pane, and its own draft/debounce pair separate from `SettingsDraft`, because
/// the preview needs the *draft* style on every slider tick: folding it into the 37-field
/// settings form would mean shipping all of it across the FFI per frame to redraw one
/// swatch, and would couple two panes that have nothing to say to each other.
///
/// ## What each control is measured in, and why
///
/// - **Size** and **Vertical** are % of the *viewport*, never points — a subtitle sized in
///   points reads differently on a 1× ultrawide and a 2× Studio.
/// - **Outline**, **Blur**, and the shadow **offsets** are px on the `REFERENCE_FONT_PX`
///   scale: stored as a fraction of the real text size (so they hold their proportions as
///   you resize, instead of every size change demanding a re-tune of everything else) and
///   shown as the px they'd be at the default size, because "0.06" is not a quantity
///   anyone can picture. The Rust side owns both halves; see `pb_app_core::subtitle`.
///
/// ## Why opacity is a slider and not the colour picker's alpha
///
/// Opacity is the setting people actually reach for; the hue is a once-a-year decision.
/// Burying it inside a `ColorPicker` popover also made **Background do nothing** — it
/// starts at alpha 0, and picking a hue in that popover does not raise the alpha, so the
/// box never appeared no matter what you chose. A slider on the surface makes the on/off
/// visible and that failure impossible.
struct SubtitlesPane: View {
    let model: CoreModel
    @State private var draft = SubtitleStyleDraft()
    @State private var loaded = false
    @State private var applyTask: Task<Void, Never>?

    var body: some View {
        Form {
            Section {
                // Full width and short, per the owner. It shows the BOTTOM of a virtual
                // 16:9 frame — see `render_preview` for why that is not just a squash.
                SubtitlePreview(model: model, draft: draft)
                    .frame(height: 150)
                    .frame(maxWidth: .infinity)
                    .clipShape(RoundedRectangle(cornerRadius: 6))
                    .listRowInsets(EdgeInsets(top: 4, leading: 4, bottom: 4, trailing: 4))
            }

            Section("Text") {
                Picker("Font", selection: $draft.fontFamily) {
                    // "" is the system font — the FFI cannot carry an Option<String>, so
                    // empty means "no choice", exactly as an absent config key does.
                    Text("System").tag("")
                    Divider()
                    ForEach(fontChoices, id: \.self) { Text($0).tag($0) }
                }
                slider("Size", pct($draft.sizePct), 1...maxSizePct, "%.1f%%")
                // The MASTER opacity: the whole subtitle, faded as one object. Fading
                // just the glyphs would make them translucent onto their own outline,
                // which shows through and defeats the point (owner, 2026-07-15).
                slider("Opacity", $draft.opacity, 0...100, "%.0f%%")
            }

            Section("Legibility") {
                // Notched, in px: the owner's ask, and 99% of people want 0–4.
                slider("Outline", $draft.outlinePx, 0...4, "%.2f px", step: 0.25)
                slider("Outline opacity", $draft.outlineOpacity, 0...100, "%.0f%%")
                    .disabled(draft.outlinePx <= 0)

                // Alpha 0 = no background: the slider IS the on/off, so there is no toggle
                // that can disagree with it.
                slider("Background", $draft.backgroundOpacity, 0...100, "%.0f%%")

                Toggle("Drop shadow", isOn: $draft.shadowOn)
                if draft.shadowOn {
                    slider("Blur", $draft.shadowBlurPx, 0...maxBlurPx, "%.1f px", step: 0.5)
                    slider(
                        "Offset X", $draft.shadowDxPx, -maxOffsetPx...maxOffsetPx, "%.1f px",
                        step: 0.5)
                    slider(
                        "Offset Y", $draft.shadowDyPx, -maxOffsetPx...maxOffsetPx, "%.1f px",
                        step: 0.5)
                    slider("Shadow opacity", $draft.shadowOpacity, 0...100, "%.0f%%")
                }
            }

            Section("Position") {
                slider("Vertical", pct($draft.verticalOffsetPct), -50...90, "%.0f%%")
                Text(verticalHint)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            // A plain Section, like every other one here — NOT a DisclosureGroup.
            //
            // It was collapsed to save height, and that cost more than it saved: a
            // DisclosureGroup's *content* sits outside the grouped Form's row treatment,
            // so these rows lost their normal rhythm and insets (crowded, uneven, swatches
            // jammed against the edge) and the whole thing hid behind a ~12 pt chevron.
            // The window has the room now (+45 pt), and consistency is worth more than the
            // 6 rows it saved.
            //
            // Colours live here, opacities do not (owner): the hue is a once-a-year
            // decision, the opacity is the one you actually reach for.
            Section("Layout & Color") {
                ColorPicker("Text color", selection: $draft.color, supportsOpacity: false)
                ColorPicker("Outline color", selection: $draft.outlineColor, supportsOpacity: false)
                ColorPicker("Shadow color", selection: $draft.shadowColor, supportsOpacity: false)
                ColorPicker("Background color", selection: $draft.background, supportsOpacity: false)
                slider("Max width", pct($draft.maxLinePct), 20...100, "%.0f%%")
                slider("Line spacing", $draft.lineSpacing, 0.8...maxLineSpacing, "%.2f×")
            }

            Section {
                HStack {
                    Spacer()
                    Button("Restore Defaults") { draft = SubtitleStyleDraft() }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear {
            if !loaded {
                draft = SubtitleStyleDraft(form: model.subtitleStyleForm())
                loaded = true
            }
        }
        // Debounced exactly like the settings form: a slider drag or a colour scrub sends
        // only its last state, so a 200-event gesture is one write.
        .onChange(of: draft) { scheduleApply() }
        .onDisappear {
            applyTask?.cancel()
            model.subtitleStyleEdited(draft.toForm())
        }
    }

    // The bounds come from Rust — the SAME constants `SubtitleStyle::clamped` enforces —
    // so a control can never offer a value the clamp quietly takes back. A slider that
    // snaps when you let go is worse than one that never went there.
    private var maxSizePct: Double { Double(subtitle_max_size_pct()) * 100 }
    private var maxBlurPx: Double { Double(subtitle_max_shadow_blur_px()) }
    private var maxOffsetPx: Double { Double(subtitle_max_shadow_offset_px()) }
    private var maxLineSpacing: Double { Double(subtitle_max_line_spacing()) }

    /// The signed vertical offset is the setting almost no player gets right, and a bare
    /// number cannot explain it. The preview shows it; this says which way is which.
    private var verticalHint: String {
        if draft.verticalOffsetPct < -0.001 {
            return "Below the picture, in the letterbox bar."
        } else if draft.verticalOffsetPct > 0.001 {
            return "Inside the picture, above its bottom edge."
        }
        return "On the picture's bottom edge."
    }

    private var fontChoices: [String] { model.subtitleFontChoices() }

    private func scheduleApply() {
        applyTask?.cancel()
        applyTask = Task {
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            model.subtitleStyleEdited(draft.toForm())
        }
    }

    /// A `0.044`-style fraction shown as `4.4`. The wire and the rasterizer speak
    /// fractions; a human reading "0.044%" would have no idea what it meant.
    private func pct(_ b: Binding<Double>) -> Binding<Double> {
        Binding(get: { b.wrappedValue * 100 }, set: { b.wrappedValue = $0 / 100 })
    }

    /// One slider row.
    ///
    /// ⚠ The label and the readout are both **fixed-width**, and that is the whole point:
    /// a bare `HStack` gives each label its natural width, so "Size" and "Outline opacity"
    /// leave different remainders and every slider on the page ends up a different length
    /// — which reads as sloppy and, worse, makes two sliders' travel incomparable when you
    /// are trying to judge them against each other. Pinning both ends pins the middle.
    /// `LABEL_W` fits the longest label here ("Outline opacity").
    private func slider(
        _ label: String, _ value: Binding<Double>, _ range: ClosedRange<Double>,
        _ format: String, step: Double? = nil
    ) -> some View {
        HStack(spacing: 8) {
            Text(label)
                .lineLimit(1)
                .frame(width: Self.labelW, alignment: .leading)
            if let step {
                Slider(value: value, in: range, step: step)
            } else {
                Slider(value: value, in: range)
            }
            Text(String(format: format, value.wrappedValue))
                .monospacedDigit()
                .foregroundStyle(.secondary)
                .frame(width: Self.valueW, alignment: .trailing)
        }
    }

    /// Wide enough for "Outline opacity", the longest label on the page.
    private static let labelW: CGFloat = 104
    /// Wide enough for "-10.0 px" and "100%" without the slider twitching as digits come
    /// and go — a value box that resizes drags the slider with it.
    private static let valueW: CGFloat = 58
}

/// The live swatch.
///
/// Drawn by **Rust**, with the same rasterizer, `to_params`, and `place()` the real
/// overlay uses — so it cannot drift from what a film actually shows. That is the whole
/// reason the one-rasterizer decision was made; a SwiftUI `Text` mock-up here would be a
/// second implementation and would start lying the first time either side changed.
private struct SubtitlePreview: View {
    let model: CoreModel
    let draft: SubtitleStyleDraft
    /// ⚠ Not decoration — the fix for a real seam bug. The font system takes 261 ms on a
    /// worker, so the first `body` evaluation gets `nil` and shows the spinner. Nothing
    /// else changes state when the worker lands, so **SwiftUI would never re-render** and
    /// the spinner would spin forever — until you happened to nudge a slider, at which
    /// point it would work and look like a fluke. Flipping this drives the redraw.
    @State private var ready = false

    var body: some View {
        GeometryReader { geo in
            // PHYSICAL pixels: rasterize at the size it will be shown at and never let the
            // layer scale it up. This project's known sharp edge is exactly that (a 1x
            // ultrawide beside 2x Studios), so the preview honours the same discipline the
            // overlay does — and a backing-scale change re-renders, because the frame size
            // feeding this is recomputed by GeometryReader.
            let scale = NSScreen.main?.backingScaleFactor ?? 2
            let w = Int((geo.size.width * scale).rounded())
            let h = Int((geo.size.height * scale).rounded())
            if ready, let img = model.subtitlePreviewImage(draft.toForm(), w, h) {
                Image(nsImage: img)
                    .resizable()
                    .frame(width: geo.size.width, height: geo.size.height)
            } else {
                // FontSystem::new() is 261 ms and runs on a worker. Say so rather than
                // flashing an empty frame that reads as "the preview is broken".
                ZStack {
                    Rectangle().fill(Color.black.opacity(0.85))
                    ProgressView().controlSize(.small)
                }
            }
        }
        .task {
            // Poll until the worker lands. `subtitlePreviewReady` also *starts* it, so
            // opening this tab is what pays the 261 ms — not a film's first cue.
            while !Task.isCancelled && !model.subtitlePreviewReady() {
                try? await Task.sleep(for: .milliseconds(50))
            }
            ready = true
        }
    }
}

/// The Swift-native mirror of `SubtitleStyleFfi`.
///
/// Colour and opacity are held **apart** — an opaque `Color` plus a 0…100 `Double` — and
/// recombined in `toForm`. That is what lets the pane put opacity on the surface (where
/// people actually want it) and the hue away in Layout & Color, without the two ever
/// disagreeing about one `[u8; 4]`.
///
/// `Equatable` is load-bearing: `.onChange(of: draft)` is what triggers the debounced
/// save, and the Rust side hard no-ops when the folded style is unchanged, so the echo on
/// open never reaches the disk.
struct SubtitleStyleDraft: Equatable {
    var fontFamily = ""
    var sizePct: Double = 0
    var color = Color.white
    var opacity: Double = 100
    var outlinePx: Double = 0
    var outlineColor = Color.black
    var outlineOpacity: Double = 100
    var shadowOn = false
    var shadowDxPx: Double = 0
    var shadowDyPx: Double = 0
    var shadowBlurPx: Double = 0
    var shadowColor = Color.black
    var shadowOpacity: Double = 100
    var background = Color.black
    var backgroundOpacity: Double = 0
    var verticalOffsetPct: Double = 0
    var maxLinePct: Double = 0.9
    var lineSpacing: Double = 1.2

    /// The shipped defaults — **asked for, never restated**.
    ///
    /// ⚠ These used to be Swift literals mirroring `SubtitleStyle::default()`. That is a
    /// duplication with teeth: the moment either side is tuned, Restore Defaults and a
    /// fresh config hand back different looks — and tuning these numbers is the entire
    /// point of this pane, so the drift was a matter of when, not if. (It came due the
    /// first time the owner moved them.) The field initializers above exist only so the
    /// `@State` has a value for the instant before `onAppear`; every real default comes
    /// from Rust.
    init() {
        self.init(form: subtitle_style_defaults())
    }

    init(form f: SubtitleStyleFfi) {
        fontFamily = f.font_family.toString()
        sizePct = Double(f.size_pct)
        color = Color(.sRGB, red: Double(f.color_r) / 255, green: Double(f.color_g) / 255,
            blue: Double(f.color_b) / 255, opacity: 1)
        opacity = Double(f.opacity) * 100
        outlinePx = Double(f.outline_px)
        (outlineColor, outlineOpacity) = splitRGBA(
            f.outline_r, f.outline_g, f.outline_b, f.outline_a)
        shadowOn = f.shadow_on
        shadowDxPx = Double(f.shadow_dx_px)
        shadowDyPx = Double(f.shadow_dy_px)
        shadowBlurPx = Double(f.shadow_blur_px)
        (shadowColor, shadowOpacity) = splitRGBA(f.shadow_r, f.shadow_g, f.shadow_b, f.shadow_a)
        (background, backgroundOpacity) = splitRGBA(
            f.background_r, f.background_g, f.background_b, f.background_a)
        verticalOffsetPct = Double(f.vertical_offset_pct)
        maxLinePct = Double(f.max_line_pct)
        lineSpacing = Double(f.line_spacing)
    }

    func toForm() -> SubtitleStyleFfi {
        let c = color.rgbBytes()
        let o = outlineColor.rgbBytes()
        let s = shadowColor.rgbBytes()
        let b = background.rgbBytes()
        return SubtitleStyleFfi(
            font_family: RustString(fontFamily),
            size_pct: Float(sizePct),
            color_r: c.0, color_g: c.1, color_b: c.2,
            opacity: Float(opacity / 100),
            outline_px: Float(outlinePx),
            outline_r: o.0, outline_g: o.1, outline_b: o.2, outline_a: alpha(outlineOpacity),
            shadow_on: shadowOn,
            shadow_dx_px: Float(shadowDxPx),
            shadow_dy_px: Float(shadowDyPx),
            shadow_blur_px: Float(shadowBlurPx),
            shadow_r: s.0, shadow_g: s.1, shadow_b: s.2, shadow_a: alpha(shadowOpacity),
            background_r: b.0, background_g: b.1, background_b: b.2,
            background_a: alpha(backgroundOpacity),
            vertical_offset_pct: Float(verticalOffsetPct),
            max_line_pct: Float(maxLinePct),
            line_spacing: Float(lineSpacing)
        )
    }

    private func alpha(_ opacity: Double) -> UInt8 {
        UInt8(min(255, max(0, (opacity / 100 * 255).rounded())))
    }
}

/// An RGBA byte quad → an **opaque** colour plus its opacity, 0…100 — the pair the pane
/// edits separately.
private func splitRGBA(_ r: UInt8, _ g: UInt8, _ b: UInt8, _ a: UInt8) -> (Color, Double) {
    (
        Color(
            .sRGB, red: Double(r) / 255, green: Double(g) / 255, blue: Double(b) / 255, opacity: 1),
        Double(a) / 255 * 100
    )
}

extension Color {
    /// sRGB bytes, the way the letterbox pickers already do it — through `NSColor` in an
    /// explicit colour space, because a `Color` carries no components of its own. The
    /// alpha is dropped: opacity is the slider's job.
    fileprivate func rgbBytes() -> (UInt8, UInt8, UInt8) {
        guard let c = NSColor(self).usingColorSpace(.sRGB) else { return (255, 255, 255) }
        // Clamp before the UInt8 conversion: a wide-gamut pick can land outside 0...1 in
        // sRGB, and UInt8(-3.0) traps.
        let f = { (v: CGFloat) in UInt8(min(255, max(0, (v * 255).rounded()))) }
        return (f(c.redComponent), f(c.greenComponent), f(c.blueComponent))
    }
}
