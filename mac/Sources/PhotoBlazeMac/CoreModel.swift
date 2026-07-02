import AppKit
import Observation
import PbMacFfi

/// One drained-effect line in the on-screen log (id is a monotonic counter so the
/// capped log never reuses SwiftUI row identities).
struct EffectLine: Identifiable {
    let id: Int
    let text: String
}

/// The Swift-side owner of the Rust engine — the NS1 slice-1 host model.
///
/// Owns the opaque `AppCoreHandle` (the whole `AppCore` lives behind it), forwards input
/// events in, and pulls the effect queue dry on the main actor after every event/tick —
/// the FFI main-thread rule: a worker thread may only *schedule* a drain, never run one.
@MainActor
@Observable
final class CoreModel {
    /// The Rust engine. All calls happen on the main actor.
    private let core: AppCoreHandle
    /// Drained effects, oldest first, capped — the visible proof of the FFI round trip.
    private(set) var effectLog: [EffectLine] = []
    private var nextLineId = 0

    @ObservationIgnored private var keyMonitor: Any?
    @ObservationIgnored private var focusObserver: NSObjectProtocol?
    /// Slice-1 frame pump: a coarse fixed timer driving `tick()`. Replaced by the real
    /// MTKView-driven pump honoring SetWake in NS1 item 7.
    @ObservationIgnored private var tickTimer: Timer?

    init() {
        // Headless viewport at the main screen's pixel size (the live CAMetalLayer surface
        // is NS1 item 2; real construction over a photo source is item 3).
        let screen = NSScreen.main
        let scale = screen?.backingScaleFactor ?? 2.0
        let frame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1920, height: 1080)
        core = AppCoreHandle(
            UInt32(frame.width * scale),
            UInt32(frame.height * scale),
            Float(scale)
        )
        log("AppCoreHandle created (\(Int(frame.width * scale))×\(Int(frame.height * scale)) @\(scale)x)")

        installInputForwarding()
        startTicking()
    }

    // MARK: - Events in

    private func installInputForwarding() {
        // A local monitor sees key events before the responder chain; returning nil swallows
        // them (no system beep). The full NSEvent → PbKey adapter (letters, gestures, OS-repeat
        // policy) is NS1 item 4 — this is just enough surface to drive the engine's nav keys.
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] event in
            guard let self, let name = Self.pbKeyName(for: event.keyCode) else { return event }
            if event.type == .keyDown {
                let f = event.modifierFlags
                self.core.key_down(
                    name,
                    f.contains(.control),
                    f.contains(.shift),
                    f.contains(.option),
                    f.contains(.command),
                    event.isARepeat
                )
            } else {
                self.core.key_up(name)
            }
            self.drainEffects()
            return nil
        }

        // The focus-loss release net: held keys are cleared so nothing keeps flying.
        focusObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.core.focus_lost()
                self.drainEffects()
            }
        }
    }

    /// Slice-1 `keyCode` → `PbKey` name map (see `PbKey::from_name`) — nav keys only.
    private static func pbKeyName(for keyCode: UInt16) -> String? {
        switch keyCode {
        case 49: return "Space"
        case 53: return "Escape"
        case 36: return "Return"
        case 76: return "NumpadEnter"
        case 51: return "Backspace"
        case 123: return "Left"
        case 124: return "Right"
        case 125: return "Down"
        case 126: return "Up"
        default: return nil
        }
    }

    private func startTicking() {
        let timer = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.core.tick()
                self.drainEffects()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        tickTimer = timer
    }

    // MARK: - Effects out

    /// Pull the effect queue dry and execute each effect — always on the main actor.
    private func drainEffects() {
        while let effect = core.next_effect() {
            apply(effect)
        }
    }

    private func apply(_ effect: CoreEffectFfi) {
        switch effect {
        case .RequestRender:
            log("RequestRender")
        case .SetTitle(let title):
            log("SetTitle(\"\(title.toString())\")")
        case .SetWakeSoon, .ClearWake:
            break // the fixed timer stands in for the wake seam until item 7
        case .Quit:
            log("Quit → NSApp.terminate")
            NSApp.terminate(nil)
        case .ShellFlowAction(let id):
            // A host-side flow command by stable Action id. Esc arrives HERE (the keymap
            // resolves Escape → Action::Quit → a host-side flow action), not as .Quit.
            let action = id.toString()
            log("ShellFlowAction(\"\(action)\")")
            if action == "quit" {
                NSApp.terminate(nil)
            }
        case .Other:
            log("Other (not yet bridged)")
        }
    }

    private func log(_ text: String) {
        effectLog.append(EffectLine(id: nextLineId, text: text))
        nextLineId += 1
        if effectLog.count > 200 {
            effectLog.removeFirst(effectLog.count - 200)
        }
    }
}
