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
    /// The pending debounced apply (trailing-edge, 250 ms) — see `scheduleApply`.
    @State private var applyTask: Task<Void, Never>?

    var body: some View {
        TabView {
            generalPane
                .tabItem { Label("General", systemImage: "gearshape") }
            appearancePane
                .tabItem { Label("Appearance", systemImage: "paintbrush") }
            ShortcutsPane(model: model)
                .tabItem { Label("Shortcuts", systemImage: "keyboard") }
        }
        // Fixed size, sized to the content: wide enough for the forms, tall enough
        // for the General tab with the pinned-folder row expanded (the Shortcuts list
        // scrolls). NOT resizable — vertical-only resizing was attempted five times
        // (scene resizability modes, AppKit min/max pins, deferred re-asserts) and the
        // SwiftUI Settings scene re-clobbered its window's sizing every time; a fixed
        // fit-to-content window is the reliable, also-idiomatic outcome (owner call,
        // 2026-07-03). Don't re-attempt without new information.
        .frame(width: 560, height: 680)
        .onAppear {
            if !loaded {
                draft = SettingsDraft(form: model.settingsForm())
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
                    format: "%.0f photos/s"
                )
                labeledSlider("Ramp up over", value: $draft.rampSecs, in: 0...30, format: "%.1f s")
                labeledSlider(
                    "Max speed", value: $draft.maxFps, in: 1...draft.refreshHz,
                    format: draft.maxFps >= draft.refreshHz ? "display refresh" : "%.0f photos/s"
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
                // "Current photo's folder" is deliberately not "last used folder": the
                // dialog follows what you're *viewing* (archive → its containing folder,
                // nothing open → Pictures) and never the OS's own last-browsed memory
                // (a privacy trace) — see `engine::picker_start_dir`.
                Picker("Open files in", selection: $draft.pickerFixed) {
                    Text("Current photo's folder").tag(false)
                    Text("A pinned folder").tag(true)
                }
                .pickerStyle(.radioGroup)
                if draft.pickerFixed {
                    HStack {
                        Text(draft.pickerDir.isEmpty ? "No folder chosen" : draft.pickerDir)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .foregroundStyle(draft.pickerDir.isEmpty ? .secondary : .primary)
                        Spacer()
                        Button("Choose…") { choosePinnedFolder() }
                    }
                }
            }
        }
        .formStyle(.grouped)
    }

    /// The Appearance tab — egui's Display tab mirrored: how a photo is framed and how
    /// the chrome around it looks.
    private var appearancePane: some View {
        Form {
            Section("Appearance") {
                Picker("Default scale", selection: $draft.scaleMode) {
                    Text("Fit").tag(0)
                    Text("Fill").tag(1)
                    Text("Original").tag(2)
                }
                .pickerStyle(.segmented)
                ColorPicker("Background", selection: $draft.letterbox, supportsOpacity: false)
                labeledSlider(
                    "Info panel opacity", value: $draft.infoOpacity, in: 0...100, format: "%.0f%%"
                )
            }
        }
        .formStyle(.grouped)
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
    var letterbox: Color = .black
    var infoOpacity: Double = 60
    var startupMode = 2
    var slideshowInterval: Double = 4
    var pickerFixed = false
    var pickerDir = ""
    var muteLiveAudio = false

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
        letterbox = Color(
            red: Double(form.letterbox_r) / 255.0,
            green: Double(form.letterbox_g) / 255.0,
            blue: Double(form.letterbox_b) / 255.0
        )
        infoOpacity = Double(form.info_opacity)
        startupMode = Int(form.startup_mode)
        slideshowInterval = form.slideshow_interval_secs
        pickerDir = form.picker_dir.toString()
        pickerFixed = form.picker_fixed
        muteLiveAudio = form.mute_live_audio
    }

    func toForm() -> SettingsFormFfi {
        let rgb = NSColor(letterbox).usingColorSpace(.sRGB) ?? .black
        return SettingsFormFfi(
            start_speed: Float(startSpeed),
            ramp_secs: Float(rampSecs),
            max_fps: UInt32(maxFps.rounded()),
            refresh_hz: UInt32(refreshHz.rounded()),
            hold_delay_ms: UInt32(holdDelayMs.rounded()),
            scroll_action: UInt8(scrollAction),
            recursive: recursive,
            scale_mode: UInt8(scaleMode),
            letterbox_r: UInt8((rgb.redComponent * 255).rounded().clamped(0, 255)),
            letterbox_g: UInt8((rgb.greenComponent * 255).rounded().clamped(0, 255)),
            letterbox_b: UInt8((rgb.blueComponent * 255).rounded().clamped(0, 255)),
            info_opacity: UInt8(infoOpacity.rounded()),
            startup_mode: UInt8(startupMode),
            slideshow_interval_secs: slideshowInterval,
            picker_fixed: pickerFixed,
            picker_dir: RustString(pickerDir),
            mute_live_audio: muteLiveAudio
        )
    }
}

extension Double {
    fileprivate func clamped(_ lo: Double, _ hi: Double) -> Double {
        Swift.min(Swift.max(self, lo), hi)
    }
}
