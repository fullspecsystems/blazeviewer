// swift-tools-version:5.10
// The NS1 SwiftUI/AppKit host (ADR-021, macos-native-ui-plan §NS1) — a SwiftPM executable
// so the whole target is CLI/CI-drivable (scripts/build-swift-host.sh) and still opens in
// Xcode (`open mac/Package.swift`) with the full IDE + debugger. arm64-only, macOS 14+.
//
// Build order matters: the PbMacFfi path dependency is a *generated* package
// (crates/pb-mac-ffi/PbMacFfi — the swift-bridge glue + the Rust staticlib in an
// xcframework), produced by `scripts/build-swift-host.sh` steps 1–2. Run that script
// (or its first two steps) before the first `swift build`.
import PackageDescription

let package = Package(
    name: "PhotoBlazeMac",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../crates/pb-mac-ffi/PbMacFfi"),
        // Sparkle: the Mac-native auto-updater (task #65). Binary-target XCFramework — a
        // *link* dependency only; `swift build` produces a bare executable and does NOT
        // embed Sparkle.framework into the .app (no Xcode "Embed Frameworks" phase), so
        // build-swift-host.sh copies the framework into Contents/Frameworks and the rpath
        // linker flag below lets the bundle resolve it at runtime.
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.9.4"),
    ],
    targets: [
        .executableTarget(
            name: "PhotoBlazeMac",
            dependencies: [
                .product(name: "PbMacFfi", package: "PbMacFfi"),
                .product(name: "Sparkle", package: "Sparkle"),
            ],
            // A staticlib carries no framework references, so the frameworks the Rust
            // graph `#[link]`s (pb-decode's Image I/O + Live Photo paths, the trash crate,
            // …) must be linked by the consumer. Mirror crates/pb-decode/src/{imageio,
            // livephoto}.rs when these change.
            linkerSettings: [
                .linkedFramework("CoreFoundation"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("ImageIO"),
                .linkedFramework("Foundation"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("CoreMedia"),
                // On-device OCR: pb-app-core's Apple Vision text-recognition backend
                // (image_text.rs, task #45) references VNImageRequestHandler /
                // VNRecognizeTextRequest — the class symbols live in Vision.framework.
                .linkedFramework("Vision"),
                .linkedLibrary("objc"),
                // Sparkle.framework is embedded into Contents/Frameworks by
                // build-swift-host.sh; this rpath lets the executable resolve its
                // `@rpath/Sparkle.framework/...` install name at runtime. `-rpath` is a
                // linker flag, so each token needs its own `-Xlinker` (a bare `-rpath` is
                // "unknown argument" to the Swift driver).
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]),
            ]
        )
    ]
)
