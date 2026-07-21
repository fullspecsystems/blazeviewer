// The pure folder-breadcrumb model (task #129) — path → ancestry crumbs + the interactivity rule.
// No SwiftUI/AppKit/FFI, so it is unit-tested in isolation (`swift test`) with no native deps.
import Foundation

/// One folder in the breadcrumb: a display name + the absolute path a click opens.
public struct Crumb: Equatable {
    public let name: String
    /// Absolute path. The filesystem root is `"/"`, rendered with the name `"/"`.
    public let path: String
    public init(name: String, path: String) {
        self.name = name
        self.path = path
    }
}

public enum FolderBreadcrumbModel {
    /// Derive the folder ancestry from an absolute path, ordered root-first … current-last.
    ///
    /// LEXICAL only — no lowercasing, canonicalizing, or symlink resolution — so the paths agree
    /// with the core's `strip_prefix`/`starts_with` tree containment (a "cleaned" path can
    /// disagree with the resident tree, and case-insensitive volumes don't make those comparisons
    /// case-insensitive). An empty or relative path yields no crumbs (the bar hides). A trailing
    /// slash is ignored (`/a/b/` ≡ `/a/b`).
    public static func crumbs(for path: String) -> [Crumb] {
        guard path.hasPrefix("/") else { return [] }
        let parts = path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
        var out: [Crumb] = [Crumb(name: "/", path: "/")]
        var acc = ""
        for part in parts {
            acc += "/" + part
            out.append(Crumb(name: part, path: acc))
        }
        return out
    }

    /// Whether clicking `path` should open (recursively scan) that folder. The current folder and
    /// broad roots are context-only: `/`, a single-segment system root (`/Users`, `/Applications`,
    /// `/Volumes`, …), and a `/Volumes/<name>` volume/share root — one-click-scanning any of those
    /// walks an entire boot/external/SMB volume, which is exactly the cost the breadcrumb must not
    /// hide behind a casual click. Everything deeper is fair game.
    public static func isInteractive(_ path: String) -> Bool {
        let parts = path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
        if parts.count < 2 { return false } // "/" and single-segment roots (/Users, /Volumes, …)
        if parts.first == "Volumes" && parts.count == 2 { return false } // a volume/share root
        return true
    }
}
