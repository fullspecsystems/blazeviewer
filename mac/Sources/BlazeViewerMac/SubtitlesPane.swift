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
/// **Every size is a % of viewport height, never points.** A subtitle sized in points
/// reads differently on a 1× ultrawide and a 2× Studio; sized against the viewport it
/// looks the same everywhere, which is the entire point of a legibility setting. The
/// sliders show human numbers and the `*_pct` fields carry fractions — `sizePct` 4.4 on
/// screen is 0.044 on the wire (see `pctBinding`).
struct SubtitlesPane: View {
    let model: CoreModel
    @State private var draft = SubtitleStyleDraft()
    @State private var loaded = false
    @State private var applyTask: Task<Void, Never>?

    var body: some View {
        Form {
            Section {
                // 16:9 — a display. The picture inside it is 2.39:1, so there is always
                // a real letterbox to show the vertical offset against. ⚠ A swatch WIDER
                // than 2.39:1 pillarboxes instead and has NO bars, which would hide the
                // one setting this preview exists to explain — so this must not be
                // stretched to the full pane width (560pt / 2.39 would do exactly that).
                SubtitlePreview(model: model, draft: draft)
                    .aspectRatio(16.0 / 9.0, contentMode: .fit)
                    .frame(maxWidth: 420)
                    .clipShape(RoundedRectangle(cornerRadius: 6))
                    .frame(maxWidth: .infinity, alignment: .center)
            }

            Section("Text") {
                Picker("Font", selection: $draft.fontFamily) {
                    // "" is the system font — the FFI cannot carry an Option<String>, so
                    // empty means "no choice", exactly as an absent config key does.
                    Text("System").tag("")
                    Divider()
                    ForEach(fontChoices, id: \.self) { Text($0).tag($0) }
                }
                labeledSlider("Size", pctBinding($draft.sizePct), in: 1...25, format: "%.1f%%")
                ColorPicker("Color", selection: $draft.color, supportsOpacity: true)
            }

            Section("Legibility") {
                labeledSlider(
                    "Outline", pctBinding($draft.outlinePct), in: 0...2, format: "%.2f%%")
                ColorPicker("Outline color", selection: $draft.outlineColor, supportsOpacity: true)
                    .disabled(draft.outlinePct <= 0)

                Toggle("Drop shadow", isOn: $draft.shadowOn)
                if draft.shadowOn {
                    labeledSlider(
                        "Blur", pctBinding($draft.shadowBlurPct), in: 0...5, format: "%.2f%%")
                    labeledSlider(
                        "Offset X", pctBinding($draft.shadowDxPct), in: -5...5, format: "%.2f%%")
                    labeledSlider(
                        "Offset Y", pctBinding($draft.shadowDyPct), in: -5...5, format: "%.2f%%")
                    ColorPicker("Shadow color", selection: $draft.shadowColor, supportsOpacity: true)
                }

                // Alpha 0 = no background, so the opacity slider IS the on/off — one
                // control, no toggle that can disagree with it.
                ColorPicker("Background", selection: $draft.background, supportsOpacity: true)
                if draft.backgroundOn {
                    labeledSlider(
                        "Corner radius", pctBinding($draft.backgroundRadiusPct), in: 0...5,
                        format: "%.2f%%")
                    labeledSlider(
                        "Padding", pctBinding($draft.backgroundPadPct), in: 0...5, format: "%.2f%%")
                }
            }

            Section("Position & Layout") {
                labeledSlider(
                    "Vertical", pctBinding($draft.verticalOffsetPct), in: -50...90,
                    format: "%.0f%%")
                Text(verticalHint)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                // Collapsed: the Settings window is a FIXED 560x680 (resizing was punted
                // after five attempts — don't retry), and these two are the knobs nobody
                // tunes daily. Keeping them out of the default view is what lets the
                // preview stay big enough to actually judge.
                DisclosureGroup("Advanced") {
                    labeledSlider(
                        "Max width", pctBinding($draft.maxLinePct), in: 20...100, format: "%.0f%%")
                    labeledSlider("Line spacing", $draft.lineSpacing, in: 0.8...3, format: "%.2f×")
                }
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
    private func pctBinding(_ b: Binding<Double>) -> Binding<Double> {
        Binding(get: { b.wrappedValue * 100 }, set: { b.wrappedValue = $0 / 100 })
    }

    private func labeledSlider(
        _ label: String, _ value: Binding<Double>, in range: ClosedRange<Double>, format: String
    ) -> some View {
        HStack {
            Text(label)
            Slider(value: value, in: range)
            Text(String(format: format, value.wrappedValue))
                .monospacedDigit()
                .foregroundStyle(.secondary)
                .frame(width: 64, alignment: .trailing)
        }
    }
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
            if let img = model.subtitlePreviewImage(draft.toForm(), w, h) {
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
    }
}

/// The Swift-native mirror of `SubtitleStyleFfi`.
///
/// `Equatable` is load-bearing: `.onChange(of: draft)` is what triggers the debounced
/// save, and the Rust side hard no-ops when the folded style is unchanged, so the echo on
/// open never reaches the disk.
struct SubtitleStyleDraft: Equatable {
    var fontFamily = ""
    var sizePct: Double = 0.044
    var color = Color.white
    var outlinePct: Double = 0.003
    var outlineColor = Color.black
    var shadowOn = false
    var shadowDxPct: Double = 0.002
    var shadowDyPct: Double = 0.002
    var shadowBlurPct: Double = 0.004
    var shadowColor = Color.black.opacity(0.78)
    var background = Color.black.opacity(0)
    var backgroundRadiusPct: Double = 0.006
    var backgroundPadPct: Double = 0.008
    var verticalOffsetPct: Double = 0.05
    var maxLinePct: Double = 0.9
    var lineSpacing: Double = 1.2

    /// Alpha 0 = no background — one control, rather than a toggle that can disagree with
    /// the colour it guards.
    var backgroundOn: Bool { (NSColor(background).usingColorSpace(.sRGB)?.alphaComponent ?? 0) > 0 }

    init() {}

    init(form f: SubtitleStyleFfi) {
        fontFamily = f.font_family.toString()
        sizePct = Double(f.size_pct)
        color = Color(rgba: f.color_r, f.color_g, f.color_b, f.color_a)
        outlinePct = Double(f.outline_pct)
        outlineColor = Color(rgba: f.outline_r, f.outline_g, f.outline_b, f.outline_a)
        shadowOn = f.shadow_on
        shadowDxPct = Double(f.shadow_dx_pct)
        shadowDyPct = Double(f.shadow_dy_pct)
        shadowBlurPct = Double(f.shadow_blur_pct)
        shadowColor = Color(rgba: f.shadow_r, f.shadow_g, f.shadow_b, f.shadow_a)
        background = Color(rgba: f.background_r, f.background_g, f.background_b, f.background_a)
        backgroundRadiusPct = Double(f.background_radius_pct)
        backgroundPadPct = Double(f.background_pad_pct)
        verticalOffsetPct = Double(f.vertical_offset_pct)
        maxLinePct = Double(f.max_line_pct)
        lineSpacing = Double(f.line_spacing)
    }

    func toForm() -> SubtitleStyleFfi {
        let c = color.rgbaBytes()
        let o = outlineColor.rgbaBytes()
        let s = shadowColor.rgbaBytes()
        let b = background.rgbaBytes()
        return SubtitleStyleFfi(
            font_family: RustString(fontFamily),
            size_pct: Float(sizePct),
            color_r: c.0, color_g: c.1, color_b: c.2, color_a: c.3,
            outline_pct: Float(outlinePct),
            outline_r: o.0, outline_g: o.1, outline_b: o.2, outline_a: o.3,
            shadow_on: shadowOn,
            shadow_dx_pct: Float(shadowDxPct),
            shadow_dy_pct: Float(shadowDyPct),
            shadow_blur_pct: Float(shadowBlurPct),
            shadow_r: s.0, shadow_g: s.1, shadow_b: s.2, shadow_a: s.3,
            background_r: b.0, background_g: b.1, background_b: b.2, background_a: b.3,
            background_radius_pct: Float(backgroundRadiusPct),
            background_pad_pct: Float(backgroundPadPct),
            vertical_offset_pct: Float(verticalOffsetPct),
            max_line_pct: Float(maxLinePct),
            line_spacing: Float(lineSpacing)
        )
    }
}

extension Color {
    fileprivate init(rgba r: UInt8, _ g: UInt8, _ b: UInt8, _ a: UInt8) {
        self.init(
            .sRGB, red: Double(r) / 255, green: Double(g) / 255, blue: Double(b) / 255,
            opacity: Double(a) / 255)
    }

    /// sRGB bytes, the way the letterbox pickers already do it — through `NSColor` in an
    /// explicit colour space, because a `Color` carries no components of its own.
    fileprivate func rgbaBytes() -> (UInt8, UInt8, UInt8, UInt8) {
        guard let c = NSColor(self).usingColorSpace(.sRGB) else { return (255, 255, 255, 255) }
        // Clamp before the UInt8 conversion: a wide-gamut pick can land outside 0...1 in
        // sRGB, and UInt8(-3.0) traps.
        let f = { (v: CGFloat) in UInt8(min(255, max(0, (v * 255).rounded()))) }
        return (f(c.redComponent), f(c.greenComponent), f(c.blueComponent), f(c.alphaComponent))
    }
}
