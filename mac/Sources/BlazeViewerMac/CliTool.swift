// The opt-in `blaze` command-line tool (task #78.14) — the VS Code / iTerm
// pattern: an explicit menu action ("Install Command-Line Tool…") that symlinks
// /usr/local/bin/blaze → this app's embedded binary. NEVER automatic: a silent
// PATH write on launch is bad citizenship (and usually needs admin rights). An
// explicit, user-initiated write is the ADR-018-sanctioned category (same as saving
// a rotation) — the no-trace guarantee is about involuntary traces of *viewing*.
//
// Mechanics: /usr/local/bin is in macOS's default PATH (/etc/paths) but root-owned
// on stock systems, so the flow is try-plain-first (covers Homebrew-style setups
// where the dir is user-writable), then escalate once through the standard
// osascript administrator prompt. The link targets the running bundle's executable
// at its current location — Sparkle updates replace bundle *contents*, not the
// path, so the link survives updates; a moved/reinstalled app shows as "stale" and
// the same menu action repairs it.

import AppKit

@MainActor
final class CliTool: NSObject {
    static let shared = CliTool()

    private static let linkPath = "/usr/local/bin/blaze"

    /// The install target: the real embedded binary of the running app. For a bare
    /// `swift run` this is the debug executable — installing from a dev run is allowed
    /// (a dev convenience), and the status shows stale once a real .app replaces it.
    private static var targetPath: String? { Bundle.main.executableURL?.path }

    /// What the symlink currently is, resolved fresh on every menu fire.
    private enum Status {
        /// No file at the link path.
        case notInstalled
        /// A symlink pointing at this running binary.
        case installed
        /// A symlink pointing somewhere else (an old app location, or not ours).
        case elsewhere(String)
        /// A non-symlink file occupies the path — never touch it silently.
        case foreign
    }

    private static func status() -> Status {
        let fm = FileManager.default
        guard
            let attrs = try? fm.attributesOfItem(atPath: linkPath),
            let type = attrs[.type] as? FileAttributeType
        else { return .notInstalled }
        guard type == .typeSymbolicLink,
            let dest = try? fm.destinationOfSymbolicLink(atPath: linkPath)
        else { return .foreign }
        return dest == targetPath ? .installed : .elsewhere(dest)
    }

    /// The menu item for the app menu — static title; the dialog is state-aware.
    func menuItem() -> NSMenuItem {
        let item = NSMenuItem(
            title: "Install Command-Line Tool…",
            action: #selector(fire(_:)),
            keyEquivalent: ""
        )
        item.target = self
        return item
    }

    @objc private func fire(_ sender: NSMenuItem) {
        guard let target = Self.targetPath else { return }
        switch Self.status() {
        case .notInstalled:
            confirm(
                "Install the blaze command?",
                info: "Creates \(Self.linkPath) → this app, so you can run blaze "
                    + "from the Terminal (try blaze --help). You may be asked for "
                    + "an administrator password.",
                button: "Install"
            ) { self.install(target: target) }
        case .installed:
            confirm(
                "The blaze command is installed.",
                info: "\(Self.linkPath) points at this app. Remove it?",
                button: "Remove"
            ) { self.remove() }
        case .elsewhere(let dest):
            confirm(
                "Repair the blaze command?",
                info: "\(Self.linkPath) points at\n\(dest)\nwhich isn't this copy of "
                    + "Blaze Viewer (moved or reinstalled?). Point it here instead?",
                button: "Repair"
            ) { self.install(target: target) }
        case .foreign:
            let a = NSAlert()
            a.messageText = "Can't install the blaze command"
            a.informativeText =
                "\(Self.linkPath) already exists and isn't a Blaze Viewer link. "
                + "Remove it manually first."
            a.alertStyle = .warning
            a.runModal()
        }
    }

    private func confirm(
        _ message: String, info: String, button: String, then action: @escaping () -> Void
    ) {
        let a = NSAlert()
        a.messageText = message
        a.informativeText = info
        a.addButton(withTitle: button)
        a.addButton(withTitle: "Cancel")
        if a.runModal() == .alertFirstButtonReturn { action() }
    }

    /// `ln -sfn` semantics: replace whatever symlink is there atomically-enough for a
    /// dev tool. Plain attempt first (a user-writable /usr/local/bin — common on
    /// Homebrew machines), then one administrator escalation.
    private func install(target: String) {
        let fm = FileManager.default
        // The plain attempt: remove a stale link, then create.
        if (try? fm.destinationOfSymbolicLink(atPath: Self.linkPath)) != nil {
            try? fm.removeItem(atPath: Self.linkPath)
        }
        if (try? fm.createSymbolicLink(
            atPath: Self.linkPath, withDestinationPath: target)) != nil
        {
            done("The blaze command is ready — try blaze --help in a Terminal.")
            return
        }
        // Escalate: mkdir + ln through the standard admin prompt.
        let cmd = "mkdir -p /usr/local/bin && ln -sfn \(shQuote(target)) \(shQuote(Self.linkPath))"
        if runPrivileged(cmd) {
            done("The blaze command is ready — try blaze --help in a Terminal.")
        } else {
            failed(
                "Couldn't create \(Self.linkPath). You can create it yourself:\n"
                    + "sudo ln -sfn \(shQuote(target)) \(Self.linkPath)")
        }
    }

    private func remove() {
        if (try? FileManager.default.removeItem(atPath: Self.linkPath)) != nil {
            done("Removed \(Self.linkPath).")
            return
        }
        if runPrivileged("rm -f \(shQuote(Self.linkPath))") {
            done("Removed \(Self.linkPath).")
        } else {
            failed("Couldn't remove \(Self.linkPath).")
        }
    }

    /// One shell command through the osascript administrator prompt. Returns success.
    private func runPrivileged(_ command: String) -> Bool {
        // AppleScript string literal: escape backslashes then quotes.
        let esc = command
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        let source = "do shell script \"\(esc)\" with administrator privileges"
        var error: NSDictionary?
        NSAppleScript(source: source)?.executeAndReturnError(&error)
        return error == nil
    }

    /// POSIX single-quote for the shell: ' → '\'' inside single quotes.
    private func shQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private func done(_ text: String) {
        let a = NSAlert()
        a.messageText = "Blaze Viewer"
        a.informativeText = text
        a.runModal()
    }

    private func failed(_ text: String) {
        let a = NSAlert()
        a.messageText = "Blaze Viewer"
        a.informativeText = text
        a.alertStyle = .warning
        a.runModal()
    }
}
