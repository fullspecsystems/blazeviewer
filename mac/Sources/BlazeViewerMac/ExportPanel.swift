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
}

struct ExportPanelView: View {
    let model: CoreModel
    let maxHeight: CGFloat

    private var p: ExportPanelModel { model.exportPanel }
    /// The Size pop-up's "Custom…" entry — never a real size code (the core's codes keep the
    /// kind in the top four bits, and kind 15 doesn't exist).
    static let customSizeTag = UInt32.max
    private let width: CGFloat = 300
    private let labelWidth: CGFloat = 86

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
            row("Format") {
                // A pop-up, grouped: the everyday two, the modern ones, then TIFF. The core
                // lists only what this build writes (HEIC is Mac-only). Each group is a Section,
                // which the menu separates with a line. Not a Divider inside the ForEach: a
                // ForEach tags every view in an element with its id, so that divider carried
                // WebP's code, the pop-up matched it before WebP, and showed a blank title.
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
            }
            if p.hasLossless {
                indented {
                    Toggle(
                        "Lossless",
                        isOn: Binding(get: { p.lossless }, set: { model.exportSetLossless($0) })
                    )
                    .toggleStyle(.checkbox)
                    .help("Keep every pixel exactly, in a bigger file")
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
                row("Compression") {
                    Picker(
                        "Compression", selection: binding(\.pngCompression, model.exportSetPngCompression)
                    ) {
                        Text("Faster").tag(0)
                        Text("Balanced").tag(1)
                        Text("Smallest").tag(2)
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
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
            .help("Leave out camera details, dates and location. The color profile is kept.")
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
                .help("Y switches between the export and the original; hold Y to peek")
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
            .help("Export with these settings (⌘↩)")
        }
    }

    // MARK: - Helpers

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

    /// A control in the controls column with no label of its own (a checkbox carries one).
    private func indented<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        HStack(spacing: 10) {
            Color.clear.frame(width: labelWidth, height: 1)
            content()
            Spacer(minLength: 0)
        }
    }

    /// A labelled row: a fixed leading label column, the control beside it.
    private func row<Content: View>(_ label: String, @ViewBuilder _ content: () -> Content)
        -> some View
    {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(label)
                .font(.callout)
                .foregroundStyle(Color.panelSecondary)
                .frame(width: labelWidth, alignment: .leading)
                .accessibilityHidden(true)
            content()
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityLabel(label)
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
