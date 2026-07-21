// swift-tools-version:5.10
// PbBreadcrumb — the *pure*, platform-free folder-breadcrumb model (task #129): an absolute
// path → its ancestry crumbs, and the rule for which crumbs are one-click-openable vs. context-
// only (`/`, single-segment system roots, a `/Volumes/<name>` volume/share root). No SwiftUI, no
// AppKit, no FFI: every input and output is a String or a plain value type, so it builds and
// `swift test`s in isolation with zero native dependencies — the PbSeek pattern.
//
// The point: the fitting/measurement and the SwiftUI presentation live in the app target
// (`FolderBreadcrumb.swift`, which needs AppKit text metrics), but the path→crumbs derivation and
// the boundary rule are pure decisions a test can see here — the place a "breadcrumb path
// disagrees with the resident tree" bug would be caught.
import PackageDescription

let package = Package(
    name: "PbBreadcrumb",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "PbBreadcrumb", targets: ["PbBreadcrumb"])
    ],
    targets: [
        .target(name: "PbBreadcrumb"),
        .testTarget(name: "PbBreadcrumbTests", dependencies: ["PbBreadcrumb"]),
    ]
)
