import XCTest

@testable import PbSeek

/// T1 (plan §"Testing"): the pure seek-frame classification, the logic that shipped as a
/// comment inside a display-layer feed loop in `97f80bb4` with no test able to catch it.
///
/// ⚠ These validate *classification* (pre-roll? anchor? at what time?). They cannot catch
/// the seek **deadlock** — that is about *progress* over time and only the renderer harness
/// (T2) can see it. A green suite here must not read as "seek works."
final class SeekFramePolicyTests: XCTestCase {
    private let policy = SeekFramePolicy()

    // MARK: decide — the forward-seek pre-roll (the 97f80bb4 bug)

    /// Forward seek to 2s inside a GOP whose keyframe is at 0s: every frame before the
    /// target is decode-only pre-roll; the first frame at/after the target anchors — at the
    /// target, not at the keyframe (which is what sent the film backwards before the fix).
    func testForwardSeekInsideGop() {
        // keyframe (0s) and the frames up to the target: pre-roll, no anchor.
        for pts in [0.0, 0.5, 1.0, 1.5, 1.9] {
            let d = policy.decide(seekTarget: 2.0, ptsSecs: pts, dtsSecs: pts, firstFrameSent: false)
            XCTAssertTrue(d.doNotDisplay, "pts \(pts) < target 2.0 must be pre-roll")
            XCTAssertNil(d.anchorSecs, "pre-roll never anchors")
        }
        // the first frame at the target: displayable, anchors AT THE TARGET.
        let hit = policy.decide(seekTarget: 2.0, ptsSecs: 2.0, dtsSecs: 1.8, firstFrameSent: false)
        XCTAssertFalse(hit.doNotDisplay)
        XCTAssertEqual(hit.anchorSecs, 2.0)
    }

    /// A frame within epsilon of the target counts as *at* it (displayable), not pre-roll —
    /// pins the historical `target - 0.001` boundary.
    func testPrerollEpsilonBoundary() {
        let justBefore = policy.decide(
            seekTarget: 2.0, ptsSecs: 2.0 - SeekFramePolicy.prerollEpsilon - 1e-6,
            dtsSecs: 2.0, firstFrameSent: false)
        XCTAssertTrue(justBefore.doNotDisplay)
        let atEdge = policy.decide(
            seekTarget: 2.0, ptsSecs: 2.0 - SeekFramePolicy.prerollEpsilon + 1e-6,
            dtsSecs: 2.0, firstFrameSent: false)
        XCTAssertFalse(atEdge.doNotDisplay)
        XCTAssertEqual(atEdge.anchorSecs, 2.0)
    }

    /// Once a displayable frame has anchored, later frames never re-anchor — even the exact
    /// target frame — and past-target frames are plain displayable frames, not pre-roll.
    func testNoReanchorAfterFirstFrame() {
        let d = policy.decide(seekTarget: 2.0, ptsSecs: 2.5, dtsSecs: 2.4, firstFrameSent: true)
        XCTAssertFalse(d.doNotDisplay)
        XCTAssertNil(d.anchorSecs)
    }

    // MARK: decide — backward seek

    /// A backward seek always crosses into an earlier keyframe, so the pre-roll runs from
    /// that keyframe up to the (smaller) target. Same rule; different direction.
    func testBackwardSeek() {
        let pre = policy.decide(seekTarget: 5.0, ptsSecs: 4.0, dtsSecs: 4.0, firstFrameSent: false)
        XCTAssertTrue(pre.doNotDisplay)
        XCTAssertNil(pre.anchorSecs)
        let hit = policy.decide(seekTarget: 5.0, ptsSecs: 5.01, dtsSecs: 4.9, firstFrameSent: false)
        XCTAssertFalse(hit.doNotDisplay)
        XCTAssertEqual(hit.anchorSecs, 5.0)
    }

    // MARK: decide — target before the first usable keyframe (the demux i64::MAX fallback)

    /// When no keyframe exists at/before the target, the demuxer lands on the first usable
    /// keyframe *after* it (a documented branch — demux.rs retries with i64::MAX). That
    /// first frame's PTS is > target, so it is displayable immediately and anchors AT THE
    /// TARGET (matching the presenter, which then holds the brief [target, pts) gap).
    func testTargetBeforeFirstKeyframe() {
        let d = policy.decide(seekTarget: 0.5, ptsSecs: 1.2, dtsSecs: 1.1, firstFrameSent: false)
        XCTAssertFalse(d.doNotDisplay, "a frame after the target is not pre-roll")
        XCTAssertEqual(d.anchorSecs, 0.5, "anchor at the requested target, not the landed PTS")
    }

    // MARK: decide — initial playback anchors at DTS, not PTS

    /// No seek in flight (`seekTarget == nil`): nothing is pre-roll, and the first frame
    /// anchors at its **DTS** — the negative-DTS/B-frame IDR rule (currently only a comment
    /// in the shipping code). Anchoring at the PTS would drop the opening IDR.
    func testInitialPlaybackAnchorsAtDts() {
        let d = policy.decide(
            seekTarget: nil, ptsSecs: 0.0, dtsSecs: -0.08, firstFrameSent: false)
        XCTAssertFalse(d.doNotDisplay)
        XCTAssertEqual(d.anchorSecs, -0.08, "initial anchor is the DTS, not the PTS")
    }

    func testInitialPlaybackNeverPreroll() {
        let d = policy.decide(seekTarget: nil, ptsSecs: 3.0, dtsSecs: 2.9, firstFrameSent: true)
        XCTAssertFalse(d.doNotDisplay)
        XCTAssertNil(d.anchorSecs)
    }

    // MARK: decide — non-finite timestamps

    /// A non-finite PTS (missing-timestamp sentinel that became NaN/∞ at the CMTime edge) is
    /// never classified as pre-roll — we cannot prove it precedes the target, so we must not
    /// hide it. It is treated as displayable and can anchor.
    func testNonFinitePtsIsNotPreroll() {
        let nan = policy.decide(
            seekTarget: 2.0, ptsSecs: .nan, dtsSecs: 1.9, firstFrameSent: false)
        XCTAssertFalse(nan.doNotDisplay)
        XCTAssertEqual(nan.anchorSecs, 2.0)

        let inf = policy.decide(
            seekTarget: 2.0, ptsSecs: .infinity, dtsSecs: 1.9, firstFrameSent: false)
        XCTAssertFalse(inf.doNotDisplay)
    }

    // MARK: eosAnchor — seek past EOS must anchor anyway

    func testEosAnchorForcesTargetWhenNeverReached() {
        XCTAssertEqual(policy.eosAnchor(seekTarget: 30.0, firstFrameSent: false), 30.0)
    }

    func testEosAnchorNilWhenAlreadyAnchored() {
        XCTAssertNil(policy.eosAnchor(seekTarget: 30.0, firstFrameSent: true))
    }

    func testEosAnchorNilWhenNotSeeking() {
        XCTAssertNil(policy.eosAnchor(seekTarget: nil, firstFrameSent: false))
    }

    // MARK: effectiveTarget — bound the pre-roll (S3)

    /// No budget (the default) keeps the exact requested target — today's behaviour.
    func testEffectiveTargetNoBudgetIsRequested() {
        XCTAssertEqual(
            policy.effectiveTarget(requested: 10.0, keyframeSecs: 2.0, budgetSecs: nil), 10.0)
    }

    /// A pre-roll span within budget is honoured exactly.
    func testEffectiveTargetWithinBudget() {
        XCTAssertEqual(
            policy.effectiveTarget(requested: 3.0, keyframeSecs: 2.0, budgetSecs: 2.0), 3.0)
    }

    /// A pre-roll span beyond budget snaps the anchor to the keyframe: show immediately
    /// rather than stall decoding a GOP we cannot afford.
    func testEffectiveTargetBeyondBudgetSnapsToKeyframe() {
        XCTAssertEqual(
            policy.effectiveTarget(requested: 10.0, keyframeSecs: 2.0, budgetSecs: 1.0), 2.0)
    }

    /// A non-finite keyframe PTS means we cannot measure the span — no clamp.
    func testEffectiveTargetNonFiniteKeyframeNoClamp() {
        XCTAssertEqual(
            policy.effectiveTarget(requested: 10.0, keyframeSecs: .nan, budgetSecs: 1.0), 10.0)
    }
}
