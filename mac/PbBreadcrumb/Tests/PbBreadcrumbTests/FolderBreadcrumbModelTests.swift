import XCTest

@testable import PbBreadcrumb

final class FolderBreadcrumbModelTests: XCTestCase {
    // A normal nested path yields root-first … current-last, each crumb carrying its absolute path.
    func testNestedPathDerivesAncestryRootFirst() {
        let crumbs = FolderBreadcrumbModel.crumbs(for: "/Users/jd/Photos/2026")
        XCTAssertEqual(
            crumbs,
            [
                Crumb(name: "/", path: "/"),
                Crumb(name: "Users", path: "/Users"),
                Crumb(name: "jd", path: "/Users/jd"),
                Crumb(name: "Photos", path: "/Users/jd/Photos"),
                Crumb(name: "2026", path: "/Users/jd/Photos/2026"),
            ])
        // Current-last, and every crumb's path is the exact lexical prefix (tree containment).
        XCTAssertEqual(crumbs.last?.path, "/Users/jd/Photos/2026")
        XCTAssertTrue("/Users/jd/Photos/2026".hasPrefix(crumbs[2].path)) // under /Users/jd
    }

    // The filesystem root is a single "/" crumb.
    func testRootPath() {
        XCTAssertEqual(FolderBreadcrumbModel.crumbs(for: "/"), [Crumb(name: "/", path: "/")])
    }

    // A trailing slash is ignored — /a/b/ ≡ /a/b (no phantom empty crumb).
    func testTrailingSlashIgnored() {
        XCTAssertEqual(
            FolderBreadcrumbModel.crumbs(for: "/a/b/"),
            FolderBreadcrumbModel.crumbs(for: "/a/b"))
    }

    // Empty / relative paths yield nothing (the bar hides on a non-fs/empty deck).
    func testEmptyAndRelativeYieldNoCrumbs() {
        XCTAssertTrue(FolderBreadcrumbModel.crumbs(for: "").isEmpty)
        XCTAssertTrue(FolderBreadcrumbModel.crumbs(for: "relative/path").isEmpty)
    }

    // Lexical fidelity: casing/Unicode is preserved verbatim (must match the core's byte-wise tree).
    func testCasePreservedVerbatim() {
        let crumbs = FolderBreadcrumbModel.crumbs(for: "/Users/JD/MyPhotos")
        XCTAssertEqual(crumbs.map(\.name), ["/", "Users", "JD", "MyPhotos"])
    }

    // The boundary rule: `/`, single-segment roots, and volume roots are context-only; deeper is
    // openable.
    func testInteractivityBoundary() {
        XCTAssertFalse(FolderBreadcrumbModel.isInteractive("/"))
        XCTAssertFalse(FolderBreadcrumbModel.isInteractive("/Users")) // single-segment system root
        XCTAssertFalse(FolderBreadcrumbModel.isInteractive("/Volumes")) // single-segment
        XCTAssertFalse(FolderBreadcrumbModel.isInteractive("/Volumes/BigDrive")) // volume/share root
        XCTAssertTrue(FolderBreadcrumbModel.isInteractive("/Users/jd")) // a user's home subtree
        XCTAssertTrue(FolderBreadcrumbModel.isInteractive("/Users/jd/Photos"))
        XCTAssertTrue(FolderBreadcrumbModel.isInteractive("/Volumes/BigDrive/Photos")) // under the volume
    }
}
