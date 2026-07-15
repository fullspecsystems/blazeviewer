// The command-line launch preflight (task #78) — the Mac twin of the winit shell's
// `main()` order-of-ops: parse BEFORE any window exists, render help/version/usage
// errors to the right place, and only let a clean parse proceed to the app proper.
//
// The parse itself is the shared Rust `pb-cli` clap surface, reached through the
// `cli_preflight` FFI (one flag vocabulary across shells). This file owns the two
// Mac-specific policies:
//
// 1. **When**: `BlazeViewerMacApp.init` runs the preflight before the `CoreModel`
//    (and its decode-pool engine) is built, before SwiftUI materializes a window,
//    and before Sparkle starts — a terminal `photoblaze --help` prints and exits
//    without flashing any GUI or doing unrelated startup work.
// 2. **Where the text goes** (the output policy): help/version → stdout, errors →
//    stderr, ALWAYS written when the process has those streams — a pipe
//    (`--help | less`) and a redirect (`--version > f`) are non-TTY but must
//    receive output. The `NSAlert` is reserved for a *real* error on a *GUI*
//    launch (Finder / Dock / `open`), where there is no meaningful sink; help and
//    version on a GUI launch exit silently. Exit codes are clap's own
//    (0 help/version, 2 usage / bad path).
import AppKit
import PbMacFfi

@MainActor
enum Launch {
    /// The preflight outcome the app acts on.
    enum Disposition {
        /// Parse OK — run the app; `CoreModel` applies the overrides and opens the paths.
        case proceed
        /// Render `text` and exit(`exitCode`) — help, version, or a usage error.
        case emit(text: String, useStderr: Bool, exitCode: Int32)
    }

    /// Where CLI output can meaningfully land — the Mac analog of the Windows shell's
    /// `win_console::attach_parent_console()` "have_output" answer.
    ///
    /// Truth table (stdout/stderr at launch):
    /// - either is a TTY                        → `.shell` (a terminal)
    /// - stdout is a pipe (FIFO) or regular file → `.shell` (a deliberate `|` / `>`)
    /// - anything else (the launchd null-device / unified-log plumbing of a Finder,
    ///   Dock, or `open` launch)                 → `.gui`
    enum Sink { case shell, gui }

    static let sink: Sink = {
        if isatty(STDOUT_FILENO) != 0 || isatty(STDERR_FILENO) != 0 { return .shell }
        var st = stat()
        if fstat(STDOUT_FILENO, &st) == 0 {
            let kind = st.st_mode & S_IFMT
            if kind == S_IFIFO || kind == S_IFREG { return .shell }
        }
        return .gui
    }()

    /// The `--version` / help build string: bundle version + build id, the exact string
    /// the About panel shows (`CFBundleShortVersionString` + `PBBuildID`, stamped by
    /// build-swift-host.sh). The Rust side never uses its own crate version — the bundle
    /// is the single source of the packaged version on this platform. A bare
    /// `swift run` (no bundle plist) reads as "dev".
    static var versionString: String {
        let short = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString")
            as? String
        let build = Bundle.main.object(forInfoDictionaryKey: "PBBuildID") as? String
        switch (short, build) {
        case let (v?, b?) where !b.isEmpty: return "\(v) (\(b))"
        case let (v?, _): return v
        default: return "dev"
        }
    }

    /// Host-only dev flags, stripped before the shared parser sees argv — the winit
    /// shell intercepts ITS dev flags (--hud-gallery, --egui-shot, …) pre-clap the same
    /// way, so the shared `Cli` never learns shell-private switches. The features read
    /// `ProcessInfo` directly, so stripping here breaks nothing.
    private static let hostDevFlags: Set<String> = ["--pb-f-smoke"]

    /// The full argv, argv[0] included — clap consumes the first element as the program
    /// name, so this must NEVER be `dropFirst()`'d (it would eat the first real flag or
    /// path — the P0 the plan calls out). Host dev flags are stripped (above); the
    /// `-psn_…` process-serial argument older LaunchServices launches inject is too.
    static func argvVec() -> RustVec<RustString> {
        let vec = RustVec<RustString>()
        for (i, a) in ProcessInfo.processInfo.arguments.enumerated() {
            if i > 0 && (hostDevFlags.contains(a) || a.hasPrefix("-psn_")) { continue }
            vec.push(value: RustString(a))
        }
        return vec
    }

    /// Parse the command line through the shared `pb-cli` surface. Pure decision —
    /// [`act(on:)`] performs the emit/exit. The per-stream TTY-ness rides along so the
    /// Rust render styles help/errors with ANSI on a terminal (the color Windows shows)
    /// and stays plain into a pipe or redirect.
    static func preflight() -> Disposition {
        // RustString explicitly: the generated generic ties the vec's element type and
        // the version parameter to one IntoRustString type.
        let r = cli_preflight(
            argvVec(),
            RustString(versionString),
            isatty(STDOUT_FILENO) != 0,
            isatty(STDERR_FILENO) != 0
        )
        if r.proceed { return .proceed }
        return .emit(text: r.text.toString(), useStderr: r.use_stderr, exitCode: r.exit_code)
    }

    // MARK: - The bare-path double-delivery dedup (task #78.10)

    /// The argv launch paths (standardized), consumed as AppKit's document-open echo
    /// arrives. A bare `photoblaze ~/Photos` delivers that path TWICE: once parsed from
    /// argv (opened via `open_launch_paths`), and once as an odoc Apple Event
    /// (`application(_:open:)`) because AppKit treats a bare `argv[1]` path as a
    /// document launch. Without the filter, the echo's second open supersedes the
    /// first scan.
    private static var argvEchoPaths: Set<String> = []
    /// The dedup only applies during launch; a later Finder open of the SAME folder is
    /// a real user action and must pass through.
    private static let launchedAt = Date()

    /// Record the parsed launch paths (CoreModel, right after `apply_launch_args`).
    static func recordArgvPaths(_ paths: [String]) {
        argvEchoPaths = Set(paths.map { URL(fileURLWithPath: $0).standardizedFileURL.path })
    }

    /// Drop the document-open echo of argv paths: each recorded path is consumed at
    /// most once, and only within the launch window. Everything else passes through.
    static func filterLaunchEcho(_ urls: [URL]) -> [URL] {
        guard !argvEchoPaths.isEmpty else { return urls }
        guard Date().timeIntervalSince(launchedAt) < 10 else {
            // Launch is long over — retire the filter entirely.
            argvEchoPaths.removeAll()
            return urls
        }
        return urls.filter { url in
            let p = url.standardizedFileURL.path
            if argvEchoPaths.contains(p) {
                argvEchoPaths.remove(p)
                return false
            }
            return true
        }
    }

    /// Perform an `.emit` disposition: write the text to the chosen stream (always — a
    /// pipe/redirect must receive it; harmless when nobody reads it), alert only for a
    /// real error on a GUI launch, then exit with clap's code. Never returns for
    /// `.emit`; a `.proceed` is a no-op.
    static func act(on disposition: Disposition) {
        guard case let .emit(text, useStderr, exitCode) = disposition else { return }
        let payload = text.hasSuffix("\n") ? text : text + "\n"
        let handle = useStderr ? FileHandle.standardError : FileHandle.standardOutput
        try? handle.write(contentsOf: Data(payload.utf8))
        if exitCode != 0 && sink == .gui {
            // A Finder/`open` launch with a bad flag or missing path must not fail
            // silently — mirror the winit shell's no-console dialog fallback.
            let alert = NSAlert()
            alert.messageText = appName
            alert.informativeText = text
            alert.alertStyle = .warning
            alert.runModal()
        }
        exit(exitCode)
    }
}
