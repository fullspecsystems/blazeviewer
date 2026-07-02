import AppKit
import PbMacFfi
import SwiftUI

/// The Settings window (NS2 item 5 + the NS2.6 Shortcuts editor). Two tabs — General
/// (a SwiftUI form over the `SettingsFormFfi` mirror) and Shortcuts (the keybinding
/// editor over the Rust-side draft keymap) — with a **global** Save/Cancel bar that
/// commits every tab's edits at once (egui parity). Opened by ⌘, / the App menu
/// (`ShowDialog("settings")` → `openSettingsAction`); Save folds + clamps + persists
/// Rust-side (`submit_settings` → `SettingsSaved`), Cancel / closing the window
/// discards both drafts.
struct SettingsView: View {
    let model: CoreModel

    /// The editable General-tab draft (Swift value types for clean SwiftUI bindings).
    @State private var draft = SettingsDraft()
    @State private var loaded = false
    /// Save/Cancel already told the core; a bare window-close then means "cancel".
    @State private var resolved = false

    var body: some View {
        VStack(spacing: 0) {
            TabView {
                generalPane
                    .tabItem { Label("General", systemImage: "gearshape") }
                ShortcutsPane(model: model)
                    .tabItem { Label("Shortcuts", systemImage: "keyboard") }
            }
            Divider()
            HStack {
                Spacer()
                Button("Cancel") {
                    resolved = true
                    model.settingsCancel()
                    closeWindow()
                }
                .keyboardShortcut(.cancelAction)
                Button("Save") {
                    resolved = true
                    model.settingsSave(draft.toForm())
                    closeWindow()
                }
                .keyboardShortcut(.defaultAction)
            }
            .padding(12)
        }
        // Resizable (owner request): min keeps every control usable, ideal is the
        // opening size, and growth is unbounded — the Shortcuts list especially earns
        // a taller window. (windowResizability(.contentMinSize) on the scene lets the
        // window range above these minimums.)
        .frame(minWidth: 520, idealWidth: 560, minHeight: 480, idealHeight: 640)
        .onAppear {
            if !loaded {
                draft = SettingsDraft(form: model.settingsForm())
                model.keymapBeginEdit()
                loaded = true
            }
        }
        .onDisappear {
            // Closed via the traffic light / ⌘W: same as Cancel (the core clears its
            // dialog-open state so the slideshow resumes, and drops both drafts).
            if !resolved {
                model.settingsCancel()
            }
            loaded = false
            resolved = false
        }
    }

    private var generalPane: some View {
        Form {
            Section("Hold to Fly") {
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
            }
            Section("Behavior") {
                Picker("Scroll wheel", selection: $draft.scrollAction) {
                    Text("Pan").tag(0)
                    Text("Zoom").tag(1)
                }
                .pickerStyle(.segmented)
                Picker("Default scale", selection: $draft.scaleMode) {
                    Text("Fit").tag(0)
                    Text("Fill").tag(1)
                    Text("Original").tag(2)
                }
                .pickerStyle(.segmented)
                Toggle("Include subfolders when opening a folder", isOn: $draft.recursive)
                labeledSlider(
                    "Slideshow interval", value: $draft.slideshowInterval, in: 1...60,
                    format: "%.0f s"
                )
            }
            Section("Appearance") {
                ColorPicker("Background", selection: $draft.letterbox, supportsOpacity: false)
                labeledSlider(
                    "Info panel opacity", value: $draft.infoOpacity, in: 0...100, format: "%.0f%%"
                )
            }
            Section("Startup") {
                Picker("Start", selection: $draft.startupMode) {
                    Text("Fullscreen").tag(0)
                    Text("Windowed").tag(1)
                    Text("Remember last").tag(2)
                }
                .pickerStyle(.segmented)
            }
            Section("Open Dialog") {
                Picker("Start in", selection: $draft.pickerFixed) {
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
/// Rust-side draft (which owns the steal-from-prior-owner semantics). Esc cancels; a
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
        VStack(alignment: .leading, spacing: 0) {
            Text(
                armed != nil
                    ? "Press a key to bind it. Esc cancels."
                    : (note.isEmpty ? " " : note)
            )
            .font(.callout)
            .foregroundStyle(armed != nil ? Color.accentColor : Color.secondary)
            .padding(.horizontal, 20)
            .padding(.vertical, 6)
            Form {
                ForEach(groups) { group in
                    Section(group.title) {
                        ForEach(group.commands) { cmd in
                            HStack {
                                Text(cmd.label)
                                Spacer()
                                slotView(cmd.id, 0)
                                slotView(cmd.id, 1)
                            }
                        }
                    }
                }
                Section {
                    Button("Reset Shortcuts to Defaults") {
                        model.keymapResetDefaults()
                        note = ""
                        disarm()
                        reload()
                    }
                }
            }
            .formStyle(.grouped)
        }
        .onAppear {
            groups = model.keymapGroups()
            reload()
        }
        .onDisappear { disarm() }
    }

    /// One chord slot. Idle: a button showing the bound chord ("Set…"/"Add…" when
    /// empty) — clicking arms capture. Armed: a "Press a key…" prompt (click = cancel)
    /// plus Clear, which removes the binding (egui parity).
    @ViewBuilder
    private func slotView(_ id: String, _ slot: Int) -> some View {
        if armed == Armed(id: id, slot: slot) {
            Button("Press a key…") { disarm() }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
            Button("Clear") {
                model.keymapClear(id: id, slot: slot)
                note = ""
                disarm()
                reload()
            }
            .controlSize(.small)
        } else {
            let display = displays[id]?[slot] ?? ""
            Button(display.isEmpty ? (slot == 0 ? "Set…" : "Add…") : display) {
                arm(id, slot)
            }
            .controlSize(.small)
            .foregroundStyle(display.isEmpty ? Color.secondary : Color.primary)
            .frame(minWidth: 64)
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
/// original; startup 0 fullscreen / 1 windowed / 2 remember.
struct SettingsDraft {
    var startSpeed: Double = 2
    var rampSecs: Double = 5
    var maxFps: Double = 60
    var refreshHz: Double = 60
    var holdDelayMs: Double = 200
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
