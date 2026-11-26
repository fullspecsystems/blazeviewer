// Export mode's panel (task #89, rung 2; plan: .taskmaster/docs/export-plan.md) — the card
// that takes the Inspector's top-trailing slot while you set up an export: format, quality
// (Lossless for WebP and JPEG XL, compression for PNG), size and Strip metadata, then Export….
// The core owns every value and
// rule (presets that would upscale are disabled, what the notes say, whether this item can
// export at all); this view draws the model and sends each edit straight back.
//
// Export mode is a mode, not a modal: the keyboard still drives the photo (Space, R, …), so
// nothing here takes text input. `E` leaves, `⌘↩` / ⇧⌘E export, `[` / `]` step the quality,
// `Y` flicks between the export and the original (rung 3's live preview; a hold peeks).
// The controls are the stock AppKit-backed SwiftUI ones — pop-up menus, a segmented picker, a
// slider, checkboxes, a prominent button — so they behave and read as native controls,
// VoiceOver included.

import SwiftUI

/// One entry in the Format pop-up.
struct ExportFormatOption: Identifiable, Equatable {
    let code: Int  // FormatChoice::code
    let label: String
    var id: Int { code }
}

/// A run of the Format pop-up's entries, drawn as one menu section. Its id is a String so the
/// Picker can never take it for a format code (the selection's type is Int).
struct ExportFormatGroup: Identifiable {
    let group: Int
    var formats: [ExportFormatOption]
    var id: String { "group-\(group)" }
}

/// One entry in the Size pop-up.
struct ExportSizeOption: Identifiable, Equatable {
    let code: UInt32  // 0 = full size, else the long edge
    let label: String
    let enabled: Bool
    var id: UInt32 { code }
}

/// The panel's model, pulled from the core on `PanelsChanged` (`CoreModel.refreshExportPanel`).
struct ExportPanelModel: Equatable {
    var name = ""
    /// "Frame at 1:23", "Poster frame", … — "" for a photo exported as itself.
    var frame = ""
    var sourceDims = ""
    var outputDims = ""
    /// Why this item can't be exported, or "".
    var blocked = ""
    var canExport = false
    /// The format's code: 0 PNG, 1 JPEG, 2 WebP, 3 AVIF, 4 JPEG XL, 5 HEIC, 6 TIFF.
    var format = 0
    /// The Format menu: what this build writes, in order.
    var formats: [ExportFormatOption] = []
    /// The format has a Quality slider (each lossy format keeps its own value).
    var hasQuality = false
    var quality = 85
    /// The slider is live — not while Lossless is on.
    var qualityEnabled = false
    /// WebP and JPEG XL have a Lossless checkbox.
    var hasLossless = false
    var lossless = false
    /// 0 Faster, 1 Balanced, 2 Smallest.
    var pngCompression = 1
    var size: UInt32 = 0
    var sizes: [ExportSizeOption] = []
    var stripMetadata = true
    var notes: [String] = []
    /// The live preview (rung 3): the exact file size, or "Calculating…".
    var fileSize = ""
    /// Whether the Export / Original control applies.
    var canFlick = false
    /// 0 the export on screen, 1 the original.
    var previewSide = 0
    /// The export is smaller than the original, so "Show at export size" applies (F20).
    var previewResized = false
    var previewOwnSize = false
    /// What unchecking Strip metadata keeps, by category (camera, copyright, location,
    /// generation, original profile), and whether this photo has anything in each.
    var keep: [Bool] = [true, true, false, false, false]
    var keepAvailable: [Bool] = [true, true, true, true, true]
}

/// The panel's keyboard-focusable controls (Full Keyboard Access, task #89 Q9).
enum ExportFocus: Hashable {
    case format, lossless, quality, compression, size, strip, preview, ownSize, export
}

struct ExportPanelView: View {
    let model: CoreModel
    let maxHeight: CGFloat
    /// Which control has keyboard focus. Mirrored to `model.exportPanelFocused`, so the key
    /// monitor hands keys to the control instead of the viewer while one does.
    @FocusState private var focus: ExportFocus?

    private var p: ExportPanelModel { model.exportPanel }
    /// The Size pop-up's "Custom…" entry — never a real size code (the core's codes keep the
    /// kind in the top four bits, and kind 15 doesn't exist).
    static let customSizeTag = UInt32.max
    private let width: CGFloat = 300
    /// The label column: the longest label, "Compression", is 74 pt in the callout font.
    private let labelWidth: CGFloat = 78

    var body: some View {
        VStack(spacing: 0) {
            PanelHeader(title: "Export", closeHelp: "Leave Export mode (E)") {
                model.closeExport()
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    sourceSection
                    if !p.blocked.isEmpty {
                        Label(p.blocked, systemImage: "exclamationmark.triangle")
                            .font(.callout)
                            .foregroundStyle(Color.panelSecondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    controls
                        .disabled(!p.blocked.isEmpty)
                    if !p.notes.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(p.notes, id: \.self) { note in
                                Text(note)
                                    .font(.footnote)
                                    .foregroundStyle(Color.panelSecondary)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                    exportButton
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .scrollBounceBehavior(.basedOnSize)
            .frame(maxHeight: max(160, maxHeight - PanelMetrics.headerHeight - 2))
            .fixedSize(horizontal: false, vertical: true)
        }
        .frame(width: width)
        .panelBackground(opacity: model.panelOpacity)
        .arrowCursorOnHover()
        // Full Keyboard Access (task #89 Q9): Tab moves focus here (the monitor bumps the
        // request); the monitor needs to know while a control has it.
        .onChange(of: model.exportFocusRequest) { _, _ in focus = .format }
        .onChange(of: focus) { _, f in model.exportPanelFocused = f != nil }
        .onDisappear { model.exportPanelFocused = false }
    }

    // MARK: - Sections

    /// What's being exported: the file name, the frame (for a video or animation), its size.
    private var sourceSection: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(p.name.isEmpty ? "Nothing to export" : p.name)
                .font(.headline)
                .lineLimit(1)
                .truncationMode(.middle)
                .help(p.name)
            let detail = [p.frame, p.sourceDims].filter { !$0.isEmpty }.joined(separator: " · ")
            if !detail.isEmpty {
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(Color.panelSecondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 12) {
            // Lossless sits beside the pop-up, so a format that has it is laid out like one that
            // doesn't: no row comes or goes when the format changes. The row labels only the
            // pop-up "Format" (the checkbox keeps its own name for VoiceOver). It fits with
            // ~14 pt to spare: the pop-up is 85 pt (sized to its widest entry, whatever is
            // chosen), the checkbox 71.
            row("Format", labelsContent: false) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    // A pop-up, grouped: the everyday two, the modern ones, then TIFF. The core
                    // lists only what this build writes (HEIC is Mac-only). Each group is a
                    // Section, which the menu separates with a line. Not a Divider inside the
                    // ForEach: a ForEach tags every view in an element with its id, so that
                    // divider carried WebP's code, the pop-up matched it before WebP, and showed
                    // a blank title.
                    Picker("Format", selection: binding(\.format, model.exportSetFormat)) {
                        ForEach(ExportPanelView.formatGroups(p.formats)) { group in
                            Section {
                                ForEach(group.formats) { f in
                                    Text(f.label).tag(f.code)
                                }
                            }
                        }
                    }
                    .pickerStyle(.menu)
                    .labelsHidden()
                    .fixedSize()
                    .accessibilityLabel("Format")
                    .focused($focus, equals: .format)
                    if p.hasLossless {
                        Toggle(
                            "Lossless",
                            isOn: Binding(get: { p.lossless }, set: { model.exportSetLossless($0) })
                        )
                        .toggleStyle(.checkbox)
                        .fixedSize()
                        .focused($focus, equals: .lossless)
                        .help("Keep every pixel exactly, in a bigger file")
                    }
                }
            }
            if p.hasQuality {
                row("Quality") {
                    HStack(spacing: 8) {
                        Slider(
                            value: Binding(
                                get: { Double(p.quality) },
                                set: { model.exportSetQuality(Int($0.rounded())) }),
                            in: 10...100, step: 5
                        ) {
                            Text("Quality")
                        }
                        .labelsHidden()
                        .focused($focus, equals: .quality)
                        .help("Higher keeps more detail in a bigger file. [ and ] step it.")
                        Text("\(p.quality)")
                            .font(.callout.monospacedDigit())
                            .frame(width: 28, alignment: .trailing)
                    }
                    // Lossless greys the slider out rather than removing it: the rows below
                    // stay put, and the value is still there when Lossless goes off.
                    .disabled(!p.qualityEnabled)
                }
            }
            if p.format == 0 {
                // A pop-up like Format and Size (and the egui panel's combo). It was a segmented
                // control, ~188 pt against the 180 pt column: picking PNG widened the panel's
                // contents and shifted every row sideways.
                row("Compression") {
                    Picker(
                        "Compression", selection: binding(\.pngCompression, model.exportSetPngCompression)
                    ) {
                        Text("Faster").tag(0)
                        Text("Balanced").tag(1)
                        Text("Smallest").tag(2)
                    }
                    .pickerStyle(.menu)
                    .labelsHidden()
                    .fixedSize()
                    // The Quality row it replaces is 22 pt (the slider); a 20 pt pop-up row
                    // moved every row below by 2 pt when switching to and from PNG.
                    .frame(minHeight: 22)
                    .focused($focus, equals: .compression)
                    .help("PNG is lossless at every level; this trades file size against time")
                }
            }
            row("Size") {
                VStack(alignment: .leading, spacing: 3) {
                    Picker(
                        "Size",
                        selection: Binding(
                            get: { p.size },
                            set: { code in
                                // Custom… opens the sheet; the selection stays on the current
                                // size until the sheet answers.
                                if code == ExportPanelView.customSizeTag {
                                    model.exportCustomSize()
                                } else {
                                    model.exportSetSize(code)
                                }
                            })
                    ) {
                        ForEach(p.sizes) { option in
                            Text(option.label).tag(option.code)
                                .disabled(!option.enabled)
                        }
                        Divider()
                        Text("Custom…").tag(ExportPanelView.customSizeTag)
                    }
                    .pickerStyle(.menu)
                    .labelsHidden()
                    .focused($focus, equals: .size)
                    .fixedSize()
                    if !p.outputDims.isEmpty {
                        Text(p.outputDims)
                            .font(.callout.monospacedDigit())
                            .foregroundStyle(Color.panelSecondary)
                            .accessibilityLabel("Exported at \(p.outputDims)")
                    }
                }
            }
            Toggle(
                "Strip metadata",
                isOn: Binding(get: { p.stripMetadata }, set: { model.exportSetStripMetadata($0) })
            )
            .toggleStyle(.checkbox)
            .focused($focus, equals: .strip)
            .help("Leave out camera details, dates and location. The color profile is kept.")
            // Unchecking reveals what to keep (plan § 3.7); a category the photo lacks is
            // greyed. Location starts off every time.
            if !p.stripMetadata {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(Array(Self.keepLabels.enumerated()), id: \.offset) { i, label in
                        Toggle(
                            label,
                            isOn: Binding(
                                get: { p.keep.indices.contains(i) && p.keep[i] },
                                set: { model.exportSetKeep(i, $0) })
                        )
                        .toggleStyle(.checkbox)
                        .disabled(!(p.keepAvailable.indices.contains(i) && p.keepAvailable[i]))
                    }
                }
                .padding(.leading, 20)
            }
            // Always present, blank while there's nothing to say (browsing, turning a model),
            // so the rows below never move. "Calculating…" says a preview is being made; no
            // spinner beside it (one sat off-centre and only repeated the word).
            row("File size") {
                Text(p.fileSize.isEmpty ? " " : p.fileSize)
                    .font(.callout.monospacedDigit())
            }
            row("Preview") {
                Picker("Preview", selection: binding(\.previewSide, model.exportSetPreviewSide)) {
                    Text("Export").tag(0)
                    Text("Original").tag(1)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .disabled(!p.canFlick)
                .focused($focus, equals: .preview)
                .help("Y switches between the export and the original; hold Y to peek")
            }
            // F20: a resized export is shown enlarged to the original's size unless this is on.
            // Always present (greyed at full size), so choosing a size never moves the rows.
            row("", labelsContent: false) {
                Toggle(
                    "Show at export size",
                    isOn: Binding(
                        get: { p.previewOwnSize }, set: { model.exportSetPreviewOwnSize($0) })
                )
                .toggleStyle(.checkbox)
                .disabled(!p.previewResized)
                .focused($focus, equals: .ownSize)
                .help("Lay the preview out at the export's own size; View ▸ Original 1:1 then shows its pixels")
            }
        }
    }

    private var exportButton: some View {
        HStack {
            Spacer()
            Button {
                model.commitExport()
            } label: {
                Text("Export…")
                    .frame(minWidth: 72)
            }
            .buttonStyle(.borderedProminent)
            .disabled(!p.canExport)
            .focused($focus, equals: .export)
            .help("Export with these settings (⌘↩)")
        }
    }

    // MARK: - Helpers

    /// The metadata categories, in the core's order (`MetadataKind`).
    static let keepLabels = [
        "Camera & date", "Copyright & credit", "Location", "Generation data",
        "Original color profile",
    ]

    /// The Format menu's groups: PNG and JPEG, the modern formats, TIFF.
    static func formatGroup(_ code: Int) -> Int {
        switch code {
        case 0, 1: return 0
        case 6: return 2
        default: return 1
        }
    }

    /// `formats`, in order, split into runs of the same `formatGroup`.
    static func formatGroups(_ formats: [ExportFormatOption]) -> [ExportFormatGroup] {
        var groups: [ExportFormatGroup] = []
        for f in formats {
            let g = formatGroup(f.code)
            if let last = groups.last, last.group == g {
                groups[groups.count - 1].formats.append(f)
            } else {
                groups.append(ExportFormatGroup(group: g, formats: [f]))
            }
        }
        return groups
    }

    /// A labelled row: a fixed leading label column, the control beside it.
    /// A labelled row. The label names every control in `content` for VoiceOver, unless
    /// `labelsContent` is false and the content names its own (a row holding two controls).
    private func row<Content: View>(
        _ label: String, labelsContent: Bool = true, @ViewBuilder _ content: () -> Content
    ) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(label)
                .font(.callout)
                .foregroundStyle(Color.panelSecondary)
                .frame(width: labelWidth, alignment: .leading)
                .accessibilityHidden(true)
            if labelsContent {
                content()
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityLabel(label)
            } else {
                content()
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    /// A binding that reads the model and writes through the core.
    private func binding<T>(_ key: KeyPath<ExportPanelModel, T>, _ set: @escaping (T) -> Void)
        -> Binding<T>
    {
        Binding(get: { p[keyPath: key] }, set: { set($0) })
    }
}

/// Export mode's **Custom…** size sheet (task #89): resize by long edge, width, height or
/// percentage, with the result for this image read out as you type. Return is OK, Esc is
/// Cancel — the sheet is a real modal, unlike the panel behind it.
struct ExportSizeSheetView: View {
    @Bindable var model: CoreModel
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Custom Size")
                .font(.headline)
            Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 10, verticalSpacing: 10) {
                GridRow {
                    Text("Resize by")
                        .gridColumnAlignment(.trailing)
                    Picker("Resize by", selection: $model.exportSizeBy) {
                        Text("Long edge").tag(0)
                        Text("Width").tag(1)
                        Text("Height").tag(2)
                        Text("Percentage").tag(3)
                    }
                    .labelsHidden()
                    .fixedSize()
                }
                GridRow {
                    Text(model.exportSizeBy == 3 ? "Percentage" : "Pixels")
                    HStack(spacing: 6) {
                        TextField("", text: $model.exportSizeText)
                            .labelsHidden()
                            .multilineTextAlignment(.trailing)
                            .frame(width: 90)
                            .focused($focused)
                            .onSubmit { model.exportSizeSubmit() }
                            .accessibilityLabel(model.exportSizeBy == 3 ? "Percentage" : "Pixels")
                        Text(model.exportSizeBy == 3 ? "%" : "px")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Text(model.exportSizeReadout)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            HStack(spacing: 8) {
                Spacer()
                Button("Cancel") { model.exportSizeCancel() }
                    .keyboardShortcut(.cancelAction)
                Button("OK") { model.exportSizeSubmit() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.exportSizeValid)
            }
        }
        .padding(20)
        .frame(width: 340)
        .onAppear { focused = true }
    }
}
