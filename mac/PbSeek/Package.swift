// swift-tools-version:5.10
// PbSeek — the *pure*, platform-free seek-decision logic for the macOS sample-buffer
// video route (seek-robustness plan, T1). It holds no AVFoundation, no FFI, no CoreMedia:
// every input and output is a primitive number or a plain value type, so it builds and
// `swift test`s in isolation with zero native dependencies — including on a hosted CI
// runner that has neither the generated PbMacFfi xcframework nor an ffvideo build.
//
// The point (plan §"Testing — the actual deliverable"): the seek regression shipped because
// the decision logic lived inside a Swift feed loop entangled with `AVSampleBufferDisplayLayer`
// and there was no Swift test target. This package IS that test target. AVFoundation
// conversion (CMTime ⇄ seconds, the DoNotDisplay attachment) stays at the edge in
// `DemuxReader`/`SampleBufferPresenter`; the classification lives here where a test can see it.
import PackageDescription

let package = Package(
    name: "PbSeek",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "PbSeek", targets: ["PbSeek"])
    ],
    targets: [
        .target(name: "PbSeek"),
        .testTarget(name: "PbSeekTests", dependencies: ["PbSeek"]),
    ]
)
