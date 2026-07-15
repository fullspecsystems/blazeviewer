import XCTest

@testable import PbSeek

/// H1c (plan §H1c): the identity that lets every callback reject a stale seek before it
/// touches the clock. The scrubber issues up to ~16 seeks/second, so a stale anchor
/// landing after a newer seek is expected traffic — these pin the "is this still current"
/// gate the presenter applies in `onDecodeAnchor` and the audio/EOS/UI callbacks.
final class SeekContextTests: XCTestCase {
    func testCurrentWhenEpochMatches() {
        let ctx = SeekContext(epoch: 7, target: 2.0, desiredRateAfterSeek: 1, source: .scrub)
        XCTAssertTrue(ctx.isCurrent(latestEpoch: 7))
    }

    /// A→B→C rapid scrubbing: only C's epoch is current, so A and B are rejected even
    /// though their anchors may land after C was issued.
    func testStaleEpochRejected() {
        let a = SeekContext(epoch: 5, target: 1.0, desiredRateAfterSeek: 1, source: .scrub)
        let b = SeekContext(epoch: 6, target: 2.0, desiredRateAfterSeek: 1, source: .scrub)
        let latest: UInt64 = 7  // C
        XCTAssertFalse(a.isCurrent(latestEpoch: latest))
        XCTAssertFalse(b.isCurrent(latestEpoch: latest))
    }

    /// The initial feed is epoch 0 with no target; it stays current until a real seek
    /// (epoch ≥ 1) supersedes it.
    func testInitialContext() {
        let ctx = SeekContext.initial(desiredRate: 1)
        XCTAssertEqual(ctx.epoch, 0)
        XCTAssertNil(ctx.target)
        XCTAssertEqual(ctx.source, .initial)
        XCTAssertTrue(ctx.isCurrent(latestEpoch: 0))
        XCTAssertFalse(ctx.isCurrent(latestEpoch: 1))
    }

    /// Desired rate is preserved across the seek independently of any pre-roll drain rate
    /// (the concept v1 conflated): a paused scrub keeps rate 0, a playing seek keeps rate 1.
    func testDesiredRatePreserved() {
        let paused = SeekContext(epoch: 1, target: 3.0, desiredRateAfterSeek: 0, source: .step)
        XCTAssertEqual(paused.desiredRateAfterSeek, 0)
        let playing = SeekContext(epoch: 2, target: 3.0, desiredRateAfterSeek: 1, source: .arrow)
        XCTAssertEqual(playing.desiredRateAfterSeek, 1)
    }
}
