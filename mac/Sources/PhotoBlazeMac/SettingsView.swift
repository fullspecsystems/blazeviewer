import AppKit
import PbMacFfi
import SwiftUI

/// The Settings window (NS2 item 5 + the NS2.6 Shortcuts editor). Two tabs — General
/// (a SwiftUI form over the `SettingsFormFfi` mirror) and Shortcuts (the keybinding
/// editor over the Rust-side draft keymap). **Auto-saving, the macOS idiom — no
/// Save/Cancel:** every General edit applies + persists after a short debounce
/// (`settings_edited`; sliders drag continuously, so the trailing edge keeps the
/// atomic settings.toml write off every tick), and every Shortcuts gesture commits at
/// once (`keymapCommit`). Opened by ⌘, / the App menu (`ShowDialog("settings")` →
/// `openSettingsAction`); closing the window (⌘W / traffic light / Esc) just flushes
/// any pending edit and clears the core's dialog-open state.
struct SettingsView: View {
    let model: CoreModel

    /// The editable General-tab draft (Swift value types for clean SwiftUI bindings).
    @State private var draft = SettingsDraft()
    @State private var loaded = false
    /// The current image's containing folder at open ("" = nothing open) — shown,
    /// grayed, as the unpinned "Open files in" default.
    @State private var currentImageFolder = ""
    /// The pending debounced apply (trailing-edge, 250 ms) — see `scheduleApply`.
    @State private var applyTask: Task<Void, Never>?
    /// AI tab: the Test-connection probe state (runs off the main thread).
    @State private var connTesting = false
    @State private var connResult: ConnResult?
    /// Models the last probe listed (vision-capable first) — fills the Model picker.
    @State private var describeModels: [String] = []
    /// Whether the reactivate-and-retry (see `autoListModelsIfNeeded`) has already fired
    /// once this Settings-window session — a single follow-up attempt, not a poll.
    @State private var retriedModelsOnActivate = false

    /// A finished Test-connection probe: `ok` colors the summary line.
    private struct ConnResult { let ok: Bool; let message: String }

    var body: some View {
        // Each pane carries its own fixed size fit to ITS content, so the Settings
        // scene auto-sizes the window per selected tab (the System Settings behavior).
        // General's height is constant because the pinned-folder row never appears or
        // disappears — it's disabled instead. USER-resizing remains punted (five
        // failed attempts — see git history); don't re-attempt without new info.
        TabView {
            generalPane
                .frame(width: 560, height: 510)
                .tabItem { tabLabel("General", symbol: "gearshape") }
            appearancePane
                .frame(width: 560, height: 560)
                .tabItem { tabLabel("Appearance", symbol: "paintbrush") }
            aiPane
                .frame(width: 560, height: 645)
                .tabItem { tabLabel("AI", symbol: "sparkles") }
            ShortcutsPane(model: model)
                .frame(width: 560, height: 640)
                .tabItem { tabLabel("Shortcuts", symbol: "keyboard") }
        }
        .onAppear {
            if !loaded {
                draft = SettingsDraft(form: model.settingsForm())
                currentImageFolder = model.currentImageFolder()
                model.keymapBeginEdit()
                loaded = true
            }
        }
        // Auto-save: every draft change lands Rust-side after a quiet moment. The
        // onAppear load also trips this once; the core drops an unchanged form on the
        // floor, so that echo never touches disk.
        .onChange(of: draft) {
            scheduleApply()
        }
        // Esc closes the window (parity with the old Cancel shortcut); edits are
        // already applied, so it's just a close.
        .onExitCommand {
            closeWindow()
        }
        .onDisappear {
            // ⌘W / traffic light / Esc: flush any edit still sitting in the debounce
            // window (a no-op when clean), then tell the core the window is gone (it
            // clears its dialog-open state so the slideshow resumes, and drops the
            // Shortcuts draft).
            applyTask?.cancel()
            applyTask = nil
            model.settingsEdited(draft.toForm())
            model.settingsClosed()
            loaded = false
        }
    }

    /// A tab-bar label whose icon is a pre-configured NSImage symbol rather than
    /// `Label(_, systemImage:)`. The SwiftUI form resolves the symbol's metrics from the
    /// environment, which isn't final until the window is on screen — so the icons first
    /// rendered slightly small/low and visibly snapped up ~1pt on the first re-render
    /// (first tab switch or key-state change; window resizes never trip it). A fixed
    /// NSImage has nothing left to re-resolve, so the first render is already the
    /// settled one. `.medium` scale was measured pixel-identical to the rendering
    /// AppKit itself settles on (screenshot-diff verified); `.large` sits 2pt taller.
    private func tabLabel(_ title: String, symbol: String) -> some View {
        Label {
            Text(title)
        } icon: {
            Image(nsImage: Self.tabIcon(symbol))
        }
    }

    private static func tabIcon(_ name: String) -> NSImage {
        let img = NSImage(systemSymbolName: name, accessibilityDescription: nil)
            ?? NSImage()
        return img.withSymbolConfiguration(.init(pointSize: 0, weight: .regular, scale: .medium))
            ?? img
    }

    /// Trailing-edge debounce for the auto-apply: each change restarts a 250 ms timer;
    /// only the last state of a slider drag / color scrub reaches `settings_edited`.
    private func scheduleApply() {
        applyTask?.cancel()
        applyTask = Task {
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            model.settingsEdited(draft.toForm())
        }
    }

    /// Sections mirror the egui General tab (parity, owner request): Navigation = the
    /// hold-to-fly sliders + scroll wheel + slideshow interval; Startup = window mode +
    /// the recursive-open default. Appearance is its own tab (egui's Display tab).
    private var generalPane: some View {
        Form {
            Section("Navigation") {
                labeledSlider(
                    "Start speed", value: $draft.startSpeed, in: 1...60,
                    format: "%.0f images/s"
                )
                labeledSlider("Ramp up over", value: $draft.rampSecs, in: 0...30, format: "%.1f s")
                labeledSlider(
                    "Max speed", value: $draft.maxFps, in: 1...draft.refreshHz,
                    format: draft.maxFps >= draft.refreshHz ? "display refresh" : "%.0f images/s"
                )
                labeledSlider(
                    "Hold delay", value: $draft.holdDelayMs, in: 0...2000, format: "%.0f ms"
                )
                Picker("Scroll wheel", selection: $draft.scrollAction) {
                    Text("Pan").tag(0)
                    Text("Zoom").tag(1)
                }
                .pickerStyle(.segmented)
                LabeledContent("Slideshow interval") {
                    HStack(spacing: 6) {
                        TextField(
                            "Slideshow interval",
                            value: slideshowIntervalBinding,
                            format: .number.precision(.fractionLength(0...1))
                        )
                        .labelsHidden()
                        // Grouped forms render text fields borderless (invisible as an
                        // input); the rounded border makes it read as a field.
                        .textFieldStyle(.roundedBorder)
                        .multilineTextAlignment(.trailing)
                        .frame(width: 64)
                        Text("s")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Section("Startup") {
                Picker("Window mode", selection: $draft.startupMode) {
                    Text("Fullscreen").tag(0)
                    Text("Windowed").tag(1)
                    Text("Remember last").tag(2)
                }
                .pickerStyle(.segmented)
                Toggle("Include subfolders when opening a folder", isOn: $draft.recursive)
                // "Current image's folder" is deliberately not "last used folder": the
                // dialog follows what you're *viewing* (archive → its containing folder,
                // nothing open → the last-used folder, then Pictures) and never the OS's
                // own last-browsed memory (a privacy trace) — `engine::picker_start_dir`.
                Picker("Open files in", selection: $draft.pickerFixed) {
                    Text("Current image's folder").tag(false)
                    Text("A pinned folder").tag(true)
                }
                .pickerStyle(.radioGroup)
                // Always present — a row that appears/disappears would change the
                // pane's fixed height. Unpinned, it shows (grayed, Choose… disabled)
                // the current image's folder the picker will actually use.
                HStack {
                    Text(pickerFolderDisplay)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .foregroundStyle(
                            draft.pickerFixed && !draft.pickerDir.isEmpty
                                ? .primary : .secondary)
                    Spacer()
                    Button("Choose…") { choosePinnedFolder() }
                        .disabled(!draft.pickerFixed)
                }
            }
        }
        .formStyle(.grouped)
    }

    /// The Appearance tab — egui's Display tab mirrored: how an image is framed and how
    /// the chrome around it looks.
    private var appearancePane: some View {
        Form {
            Section("Appearance") {
                // System/Light/Dark (#46): drives the HUD scheme, which background fills
                // the view, and — via NSApp.appearance — the app's own chrome.
                Picker("Theme", selection: $draft.appearanceMode) {
                    Text("System").tag(0)
                    Text("Light").tag(1)
                    Text("Dark").tag(2)
                }
                .pickerStyle(.segmented)
                Picker("Default scale", selection: $draft.scaleMode) {
                    Text("Fit").tag(0)
                    Text("Fill").tag(1)
                    Text("Original").tag(2)
                }
                .pickerStyle(.segmented)
                ColorPicker(
                    "Background (dark)", selection: $draft.letterbox, supportsOpacity: false
                )
                ColorPicker(
                    "Background (light)", selection: $draft.letterboxLight, supportsOpacity: false
                )
                labeledSlider(
                    "Panel opacity", value: $draft.panelOpacity, in: 50...100, format: "%.0f%%"
                )
            }
            // The one-line info readout (`i`): whether it starts shown, where it sits, and
            // which fields it lists. `i` still toggles it live regardless of the default.
            Section("Image Info") {
                Toggle("Show image info by default", isOn: $draft.showImageInfo)
                Picker("Position", selection: $draft.infoLineAlign) {
                    Text("Left").tag(0)
                    Text("Center").tag(1)
                    Text("Right").tag(2)
                }
                .pickerStyle(.segmented)
                // Field toggles apply live; the last enabled one can't be turned off (an empty
                // line reads as a bug), so there's always something to show. Folder is prepended
                // to the filename with a "/".
                Toggle("Folder", isOn: infoFieldBinding(\.infoShowFolder))
                Toggle("Filename", isOn: infoFieldBinding(\.infoShowFilename))
                Toggle("Resolution", isOn: infoFieldBinding(\.infoShowResolution))
                Toggle("Codec", isOn: infoFieldBinding(\.infoShowCodec))
            }
        }
        .formStyle(.grouped)
    }

    /// How many of the info fields are on — used to forbid turning off the last one.
    private var infoFieldsEnabledCount: Int {
        (draft.infoShowFolder ? 1 : 0)
            + (draft.infoShowFilename ? 1 : 0)
            + (draft.infoShowResolution ? 1 : 0)
            + (draft.infoShowCodec ? 1 : 0)
    }

    /// A toggle binding for one info field that refuses to turn off the last remaining field.
    private func infoFieldBinding(_ keyPath: WritableKeyPath<SettingsDraft, Bool>) -> Binding<Bool>
    {
        Binding(
            get: { draft[keyPath: keyPath] },
            set: { on in
                if !on && draft[keyPath: keyPath] && infoFieldsEnabledCount <= 1 { return }
                draft[keyPath: keyPath] = on
            }
        )
    }

    /// The **AI** tab (task #44): the description backend, endpoint/model, prompt, response
    /// length, and the auto-describe / speak toggles, plus a Test-connection probe. Mirrors
    /// the egui shell's AI tab. Off + local by default (privacy #2 / ADR-018).
    private var aiPane: some View {
        Form {
            Section("Image Descriptions") {
                Picker("Backend", selection: $draft.describeBackend) {
                    Text("Auto").tag(0)
                    Text("Apple on-device").tag(1)
                    Text("Local endpoint").tag(2)
                }
                .pickerStyle(.segmented)
                if draft.describeBackend == 1 {
                    // Apple Foundation Models isn't wired yet (needs macOS 27 + subtask 5).
                    Text("Apple's on-device model needs macOS 27 + Apple Intelligence — not "
                        + "available yet. Choose Auto or Local endpoint.")
                        .font(.caption).foregroundStyle(.secondary)
                } else {
                    TextField(
                        "Endpoint URL", text: $draft.describeEndpoint,
                        prompt: Text("http://localhost:1234/v1"))
                    // The dropdown fills from the last probe; picking a row sets the field.
                    ComboBox(
                        text: $draft.describeModel,
                        placeholder: "(loaded model)",
                        options: [Self.comboClearRow] + describeModels.map(Self.comboRowLabel),
                        resolve: Self.resolveComboSelection
                    )
                    HStack(spacing: 8) {
                        Button("Test & list models") { runConnTest() }
                            .disabled(connTesting || draft.describeEndpoint.isEmpty)
                        if connTesting { ProgressView().controlSize(.small) }
                        if let r = connResult {
                            Text(r.message)
                                .font(.caption)
                                .foregroundStyle(r.ok ? Color.accentColor : Color.red)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer()
                    }
                }
                Picker("Response length", selection: $draft.describeLength) {
                    Text("Brief").tag(0)
                    Text("Standard").tag(1)
                    Text("Detailed").tag(2)
                }
                .pickerStyle(.segmented)
                Toggle("Auto-describe while the panel is open", isOn: $draft.describeAuto)
                Toggle("Speak descriptions", isOn: $draft.speakDescriptions)
            }
            Section("Prompt") {
                // TextEditor has no macOS rounded-corner preset (RoundedBorderTextEditorStyle
                // is iOS-only), so its default square-cornered fill clashes with the grouped
                // Form's rounded card; clip it to a smaller nested radius instead — same
                // .textBackgroundColor so this only changes the corners, not the color.
                TextEditor(text: $draft.describePrompt)
                    .frame(minHeight: 84)
                    .font(.body)
                    .scrollContentBackground(.hidden)
                    .background(
                        RoundedRectangle(cornerRadius: 8, style: .continuous)
                            .fill(Color(nsColor: .textBackgroundColor))
                    )
                Text("Leave blank to use the built-in instruction. Placeholders: {filename} "
                    + "{folder} {datetime} {camera} {location} {context}")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section {
                Text("Images are sent to the model server you set above, so keep it local "
                    + "(this Mac or your own network) and one you trust — with auto-describe "
                    + "on, each photo is sent automatically. Online services may keep images "
                    + "and use them to train. Apple's on-device model (coming later) will run "
                    + "here with no server.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        // A fresh endpoint invalidates the last probe result.
        .onChange(of: draft.describeEndpoint) { connResult = nil }
        // Fires once, the first time this tab is actually looked at — not on every
        // Settings open — so the local-network permission prompt has a legible cause
        // ("I opened AI settings") instead of ambushing a General/Shortcuts visit.
        .onAppear { autoListModelsIfNeeded() }
        // macOS's Local Network permission has no live "just granted" callback for a
        // plain socket connection (only Apple's Network.framework gets that). Coming
        // back to the app is the closest proxy — the user likely just answered the
        // system prompt, or went to System Settings to flip it on — so retry once.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            guard !retriedModelsOnActivate else { return }
            retriedModelsOnActivate = true
            autoListModelsIfNeeded()
        }
    }

    /// Display-only marker mirroring the Rust `looks_like_vision_model` heuristic (the fetched
    /// list is already vision-first sorted by the core; this just badges the usable ones).
    private static func isLikelyVisionModel(_ id: String) -> Bool {
        let s = id.lowercased()
        return ["vl", "vision", "llava", "pixtral", "moondream", "vlm", "bakllava", "cogvlm",
                "internvl"].contains { s.contains($0) }
    }

    /// The Model combo box's reset-to-default row and the vision badge — `ComboBox` itself
    /// is a plain native combo box with no notion of either; only this one call site does.
    private static let comboClearRow = "(loaded model)"
    private static let comboVisionBadge = "  ◆ vision"

    private static func comboRowLabel(for id: String) -> String {
        isLikelyVisionModel(id) ? id + comboVisionBadge : id
    }

    private static func resolveComboSelection(_ row: String) -> String {
        if row == comboClearRow { return "" }
        if row.hasSuffix(comboVisionBadge) { return String(row.dropLast(comboVisionBadge.count)) }
        return row
    }

    /// Fires the Test-connection probe once, right after the draft loads, so opening the AI
    /// tab with a local endpoint already configured shows its model list immediately —
    /// without this, a first-time user has to open the Model dropdown, find it empty, and
    /// only then discover "Test & list models" separately fills it.
    private func autoListModelsIfNeeded() {
        guard describeModels.isEmpty, draft.describeBackend != 1, !draft.describeEndpoint.isEmpty
        else { return }
        runConnTest()
    }

    /// Run the Test-connection probe off the main thread (the FFI call blocks on a socket),
    /// then land the result on the main actor. Extracts primitives inside the detached task
    /// so nothing non-`Sendable` crosses the boundary.
    private func runConnTest() {
        let url = draft.describeEndpoint
        connTesting = true
        connResult = nil
        Task {
            let result = await Task.detached(priority: .userInitiated) { () -> (Bool, String, String) in
                let r = probe_describe_endpoint(RustString(url))
                return (r.ok, r.message.toString(), r.models.toString())
            }.value
            connTesting = false
            connResult = ConnResult(ok: result.0, message: result.1)
            describeModels = result.2.split(separator: "\n").map(String.init)
        }
    }

    /// The folder line under "Open files in": pinned → the chosen folder (or a
    /// placeholder), unpinned → the current image's folder the picker will use.
    private var pickerFolderDisplay: String {
        if draft.pickerFixed {
            return draft.pickerDir.isEmpty ? "No folder chosen" : draft.pickerDir
        }
        return currentImageFolder.isEmpty ? "No image open" : currentImageFolder
    }

    /// The slideshow-interval field, clamped on commit to the same 0.1–60 s range the
    /// core enforces (`slideshow::clamp_interval`) so a typed out-of-range value never
    /// sits in the field disagreeing with what actually got saved.
    private var slideshowIntervalBinding: Binding<Double> {
        Binding(
            get: { draft.slideshowInterval },
            set: { draft.slideshowInterval = min(max($0, 0.1), 60) }
        )
    }

    private func labeledSlider(
        _ label: String, value: Binding<Double>, in range: ClosedRange<Double>, format: String
    ) -> some View {
        HStack {
            Text(label)
            Slider(value: value, in: range)
            Text(
                format.contains("%")
                    ? String(format: format, value.wrappedValue) : format
            )
            .monospacedDigit()
            .foregroundStyle(.secondary)
            .frame(width: 110, alignment: .trailing)
        }
    }

    private func choosePinnedFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        if panel.runModal() == .OK, let url = panel.url {
            draft.pickerDir = url.path
        }
    }

    private func closeWindow() {
        NSApp.keyWindow?.performClose(nil)
    }
}

/// The NS2.6 keybinding editor: every command with a Primary and Secondary chord slot.
/// Clicking a slot arms capture — an app-local `NSEvent` monitor swallows the next key
/// (so a captured ⌘Q can't quit the app and Space can't advance the photo), maps it
/// through the same Carbon→`PbKey` table the viewer uses, and lands the edit on the
/// Rust-side draft (which owns the steal-from-prior-owner semantics) — then commits it
/// live (`keymapCommit`, auto-save). Esc cancels the capture; a
/// bare-modifier press waits for a real key (modifiers arrive as `flagsChanged`, not
/// `keyDown`, so the monitor never sees them). Esc itself is not rebindable through
/// capture — a recorded punt (see ns2-shortcut-capture-notes.md).
struct ShortcutsPane: View {
    let model: CoreModel

    @State private var groups: [CoreModel.ShortcutGroup] = []
    @State private var displays: [String: [String]] = [:]
    @State private var armed: Armed?
    @State private var note = ""
    @State private var monitor: Any?

    struct Armed: Equatable {
        let id: String
        let slot: Int
    }

    var body: some View {
        Form {
            ForEach(groups) { group in
                Section(group.title) {
                    ForEach(group.commands) { cmd in
                        HStack {
                            if cmd.menuChord.isEmpty {
                                Text(cmd.label)
                            } else {
                                // ⌘-accelerators live in the menu bar, not the keymap
                                // — surface them so the row doesn't imply the editable
                                // slots are the only shortcuts (⌘C copies too).
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(cmd.label)
                                    Text("\(cmd.menuChord) in the menu")
                                        .font(.caption)
                                        .foregroundStyle(.tertiary)
                                }
                            }
                            Spacer()
                            // Armed: the prompt replaces the armed slot's own button
                            // in place (same width), and Clear appears at intrinsic
                            // width in the spacer gutter LEFT of the slot columns —
                            // so the fixed columns, the other slot's chord, and the
                            // label all hold perfectly still (owner refinement).
                            if let a = armed, a.id == cmd.id {
                                Button("Clear") {
                                    model.keymapClear(id: a.id, slot: a.slot)
                                    model.keymapCommit()
                                    note = ""
                                    disarm()
                                    reload()
                                }
                                .controlSize(.small)
                            }
                            slotView(cmd.id, 0)
                            slotView(cmd.id, 1)
                        }
                    }
                }
            }
            Section {
                Button("Reset Shortcuts to Defaults") {
                    model.keymapResetDefaults()
                    model.keymapCommit()
                    note = ""
                    disarm()
                    reload()
                }
                .frame(maxWidth: .infinity, alignment: .center)
            }
        }
        .formStyle(.grouped)
        // The capture prompt / steal note floats over the list as a material capsule,
        // shown only when there's something to say — not a permanently reserved strip
        // above the Form (that strip left a dead band under the tab bar that scrolling
        // rows clipped into), and not a conditional one (appearing would shift every
        // row down). Floating: nothing moves, and the Form fills the pane like General.
        .overlay(alignment: .top) {
            if armed != nil || !note.isEmpty {
                Text(armed != nil ? "Press a key to bind it. Esc cancels." : note)
                    .font(.callout)
                    .foregroundStyle(armed != nil ? Color.accentColor : Color.secondary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 5)
                    .background(.regularMaterial, in: Capsule())
                    .padding(.top, 10)
            }
        }
        .onAppear {
            groups = model.keymapGroups()
            reload()
        }
        .onDisappear { disarm() }
    }

    /// Every chord button's label width. Fixing the *label* (not an outer `.frame`,
    /// which only reserves space around an intrinsic-width bezel) makes every bezel
    /// identical, so the two slots form straight columns across rows — the "crooked
    /// teeth" fix. The compact Mac key glyphs (`key_symbol`) keep even modifier-heavy
    /// chords inside this width.
    private static let slotWidth: CGFloat = 72

    /// One chord slot. Idle: a button showing the bound chord (a dimmed "Set"/"Add"
    /// when empty) — clicking arms capture. Armed: the "Press a key…" prompt in the
    /// slot's own position at the same fixed width (click = cancel); the row adds
    /// Clear in the gutter to the left of the slot columns.
    @ViewBuilder
    private func slotView(_ id: String, _ slot: Int) -> some View {
        if armed == Armed(id: id, slot: slot) {
            Button {
                disarm()
            } label: {
                Text("Press a key…").frame(width: Self.slotWidth)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.small)
        } else {
            let display = displays[id]?[slot] ?? ""
            Button {
                arm(id, slot)
            } label: {
                // Dim the empty-slot placeholders. The style must sit on the Text
                // itself — on a bordered macOS button, a foregroundStyle applied
                // outside the Button is overridden by the style's label rendering.
                Text(display.isEmpty ? (slot == 0 ? "Set" : "Add") : display)
                    .foregroundStyle(display.isEmpty ? .tertiary : .primary)
                    .frame(width: Self.slotWidth)
            }
            .controlSize(.small)
        }
    }

    private func arm(_ id: String, _ slot: Int) {
        armed = Armed(id: id, slot: slot)
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            capture(event)
        }
    }

    private func disarm() {
        armed = nil
        if let m = monitor {
            NSEvent.removeMonitor(m)
            monitor = nil
        }
    }

    private func capture(_ event: NSEvent) -> NSEvent? {
        guard let a = armed else { return event }
        // A key the Carbon table can't name can't form a persistable binding — swallow
        // it and stay armed for one the keymap can express (egui parity).
        guard let name = KeyMap.pbKeyName(for: event.keyCode) else { return nil }
        if name == "Escape" {
            disarm() // cancel, leave the binding unchanged
            return nil
        }
        let f = event.modifierFlags
        if model.keymapCapture(
            id: a.id, slot: a.slot, key: name,
            ctrl: f.contains(.control), shift: f.contains(.shift),
            alt: f.contains(.option), logo: f.contains(.command)
        ) {
            model.keymapCommit()
            note = model.keymapNote()
            disarm()
            reload()
        }
        return nil // swallowed: a captured chord must never reach the app/menu
    }

    private func reload() {
        var d: [String: [String]] = [:]
        for group in groups {
            for cmd in group.commands {
                d[cmd.id] = [
                    model.keymapSlotDisplay(id: cmd.id, slot: 0),
                    model.keymapSlotDisplay(id: cmd.id, slot: 1),
                ]
            }
        }
        displays = d
    }
}

/// A Swift-native mirror of `SettingsFormFfi` (Doubles/Color for SwiftUI controls). The
/// encodings match the FFI struct: scroll 0 pan / 1 zoom; scale 0 fit / 1 fill / 2
/// original; startup 0 fullscreen / 1 windowed / 2 remember. Equatable so the window's
/// auto-save `onChange(of: draft)` can watch the whole draft at once.
struct SettingsDraft: Equatable {
    var startSpeed: Double = 2
    var rampSecs: Double = 5
    var maxFps: Double = 60
    var refreshHz: Double = 60
    var holdDelayMs: Double = 250
    var scrollAction = 0
    var recursive = true
    var scaleMode = 0
    var appearanceMode = 0
    var infoLineAlign = 2  // 0 left / 1 center / 2 right (task #54)
    var showImageInfo = false  // info line's launch default (task #54)
    var infoShowFolder = false
    var infoShowFilename = true
    var infoShowResolution = true
    var infoShowCodec = true
    var letterbox: Color = .black
    var letterboxLight: Color = .white
    var infoOpacity: Double = 60
    var panelOpacity: Double = 92
    var startupMode = 2
    var slideshowInterval: Double = 4
    var pickerFixed = false
    var pickerDir = ""
    var muteLiveAudio = false
    // AI image descriptions (task #44). backend 0 Auto / 1 Apple / 2 Local; length is a
    // preset index (0 Brief / 1 Standard / 2 Detailed) into `describeLengthPresets`.
    var describeBackend = 0
    var describeEndpoint = "http://localhost:1234/v1"
    var describeModel = ""
    var describePrompt = ""
    var describeLength = 1
    var describeAuto = false
    var speakDescriptions = false

    init() {}

    init(form: SettingsFormFfi) {
        startSpeed = Double(form.start_speed)
        rampSecs = Double(form.ramp_secs)
        maxFps = Double(form.max_fps)
        refreshHz = Double(form.refresh_hz)
        holdDelayMs = Double(form.hold_delay_ms)
        scrollAction = Int(form.scroll_action)
        recursive = form.recursive
        scaleMode = Int(form.scale_mode)
        appearanceMode = Int(form.appearance_mode)
        infoLineAlign = Int(form.info_line_align)
        showImageInfo = form.show_image_info
        infoShowFolder = form.info_show_folder
        infoShowFilename = form.info_show_filename
        infoShowResolution = form.info_show_resolution
        infoShowCodec = form.info_show_codec
        letterbox = Color(
            red: Double(form.letterbox_r) / 255.0,
            green: Double(form.letterbox_g) / 255.0,
            blue: Double(form.letterbox_b) / 255.0
        )
        letterboxLight = Color(
            red: Double(form.letterbox_light_r) / 255.0,
            green: Double(form.letterbox_light_g) / 255.0,
            blue: Double(form.letterbox_light_b) / 255.0
        )
        infoOpacity = Double(form.info_opacity)
        panelOpacity = Double(form.panel_opacity)
        startupMode = Int(form.startup_mode)
        slideshowInterval = form.slideshow_interval_secs
        pickerDir = form.picker_dir.toString()
        pickerFixed = form.picker_fixed
        muteLiveAudio = form.mute_live_audio
        describeBackend = Int(form.describe_backend)
        describeEndpoint = form.describe_endpoint.toString()
        describeModel = form.describe_model.toString()
        describePrompt = form.describe_prompt.toString()
        describeLength = SettingsDraft.lengthPresetIndex(form.describe_max_tokens)
        describeAuto = form.describe_auto
        speakDescriptions = form.speak_descriptions
    }

    /// Response-length presets (max_tokens) shown as Brief / Standard / Detailed — mirrors
    /// the egui shell's `LEN_PRESETS`.
    static let describeLengthPresets: [UInt32] = [256, 512, 1024]

    /// Nearest preset index for a saved `max_tokens` (a hand-set value snaps).
    static func lengthPresetIndex(_ maxTokens: UInt32) -> Int {
        describeLengthPresets
            .enumerated()
            .min { abs(Int($0.element) - Int(maxTokens)) < abs(Int($1.element) - Int(maxTokens)) }?
            .offset ?? 1
    }

    func toForm() -> SettingsFormFfi {
        let rgb = NSColor(letterbox).usingColorSpace(.sRGB) ?? .black
        let rgbLight = NSColor(letterboxLight).usingColorSpace(.sRGB) ?? .white
        return SettingsFormFfi(
            start_speed: Float(startSpeed),
            ramp_secs: Float(rampSecs),
            max_fps: UInt32(maxFps.rounded()),
            refresh_hz: UInt32(refreshHz.rounded()),
            hold_delay_ms: UInt32(holdDelayMs.rounded()),
            scroll_action: UInt8(scrollAction),
            recursive: recursive,
            scale_mode: UInt8(scaleMode),
            appearance_mode: UInt8(appearanceMode),
            info_line_align: UInt8(infoLineAlign),
            show_image_info: showImageInfo,
            info_show_folder: infoShowFolder,
            info_show_filename: infoShowFilename,
            info_show_resolution: infoShowResolution,
            info_show_codec: infoShowCodec,
            letterbox_r: UInt8((rgb.redComponent * 255).rounded().clamped(0, 255)),
            letterbox_g: UInt8((rgb.greenComponent * 255).rounded().clamped(0, 255)),
            letterbox_b: UInt8((rgb.blueComponent * 255).rounded().clamped(0, 255)),
            letterbox_light_r: UInt8((rgbLight.redComponent * 255).rounded().clamped(0, 255)),
            letterbox_light_g: UInt8((rgbLight.greenComponent * 255).rounded().clamped(0, 255)),
            letterbox_light_b: UInt8((rgbLight.blueComponent * 255).rounded().clamped(0, 255)),
            info_opacity: UInt8(infoOpacity.rounded()),
            panel_opacity: UInt8(panelOpacity.rounded()),
            startup_mode: UInt8(startupMode),
            slideshow_interval_secs: slideshowInterval,
            picker_fixed: pickerFixed,
            picker_dir: RustString(pickerDir),
            mute_live_audio: muteLiveAudio,
            describe_backend: UInt8(describeBackend),
            describe_endpoint: RustString(describeEndpoint),
            describe_model: RustString(describeModel),
            describe_prompt: RustString(describePrompt),
            describe_max_tokens: SettingsDraft.describeLengthPresets[
                Swift.min(Swift.max(describeLength, 0), 2)],
            describe_auto: describeAuto,
            speak_descriptions: speakDescriptions
        )
    }
}

extension Double {
    fileprivate func clamped(_ lo: Double, _ hi: Double) -> Double {
        Swift.min(Swift.max(self, lo), hi)
    }
}
