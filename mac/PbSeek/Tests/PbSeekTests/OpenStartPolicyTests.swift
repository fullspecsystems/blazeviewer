import XCTest

@testable import PbSeek

/// Pins where a freshly opened native (AVPlayer) item is put before it is revealed
/// (task #139). The failure this exists for is silent: AVFoundation readies some
/// clips *away* from their start — an iPhone 240 fps capture whose only early
/// keyframe sits in the edit list's trimmed lead-in readies at 0.933 s of 1.282 s —
/// and the player then plays from there, so the user sees the clip "skip its start"
/// with no error anywhere.
final class OpenStartPolicyTests: XCTestCase {

    func testAClipReadiedAtItsStartRevealsWithNoSeek() {
        // The common case must cost nothing: no seek, no added latency.
        XCTAssertEqual(OpenStartPolicy.action(startSecs: 0, readyAt: 0), .revealNow)
    }

    func testAClipReadiedPastItsStartIsSeekedExactlyBack() {
        // The #139 clip: AVPlayerLayer readies it at the first in-edit keyframe.
        XCTAssertEqual(
            OpenStartPolicy.action(startSecs: 0, readyAt: 0.933), .seekExactlyToStart)
    }

    func testSubFrameDriftIsNotCorrected() {
        // Within half a frame at 240 fps is "at the start" — don't pay a seek for it.
        XCTAssertEqual(OpenStartPolicy.action(startSecs: 0, readyAt: 0.001), .revealNow)
        XCTAssertEqual(
            OpenStartPolicy.action(startSecs: 0, readyAt: 0.01), .seekExactlyToStart)
    }

    func testAResumePositionSeeksThereWhateverTheReadyPosition() {
        XCTAssertEqual(
            OpenStartPolicy.action(startSecs: 12, readyAt: 0), .seekToResume(12))
        XCTAssertEqual(
            OpenStartPolicy.action(startSecs: 12, readyAt: 0.933), .seekToResume(12))
    }

    func testAResumeAtOrBelowTheFloorPlaysFromTheStart() {
        // `<= 0.5` was always "from the start" (task #94.2) — and that start must now be
        // the real one, not wherever AVFoundation readied the item.
        XCTAssertEqual(
            OpenStartPolicy.action(startSecs: 0.5, readyAt: 0.933), .seekExactlyToStart)
        XCTAssertEqual(OpenStartPolicy.action(startSecs: 0.4, readyAt: 0), .revealNow)
    }

    func testAnUnreadablePlayheadRevealsRatherThanSeeking() {
        // CMTimeGetSeconds of an invalid time is NaN; never seek on garbage.
        XCTAssertEqual(OpenStartPolicy.action(startSecs: 0, readyAt: .nan), .revealNow)
        XCTAssertEqual(OpenStartPolicy.action(startSecs: 0, readyAt: .infinity), .revealNow)
    }
}
