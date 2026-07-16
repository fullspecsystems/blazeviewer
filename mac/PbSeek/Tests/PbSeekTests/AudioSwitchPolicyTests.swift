import XCTest

@testable import PbSeek

/// Pins the switch-as-rebuffer decision table (macos-video-smoothness §2). Each
/// rule here is one that, if regressed, fails silently in the app: a moved tick,
/// garbled audio, or a clobbered user seek — never an error.
final class AudioSwitchPolicyTests: XCTestCase {

    // MARK: - Entry

    func testEntryRefusesWhenNotPlayable() {
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: false, failed: false, switching: false, activeStream: nil, requested: 2),
            .refuse, "before open")
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: true, failed: true, switching: false, activeStream: 1, requested: 2),
            .refuse, "after a latched failure")
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: true, failed: false, switching: true, activeStream: 1, requested: 2),
            .refuse, "one switch at a time — a second request mid-flight is refused")
    }

    func testEntryConfirmsWithoutChurnWhenAlreadyPlayingTheRequest() {
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: true, failed: false, switching: false, activeStream: 2, requested: 2),
            .confirmNoOp)
    }

    func testEntryProceedsForADifferentStream() {
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: true, failed: false, switching: false, activeStream: 1, requested: 2),
            .proceed)
        // Unknown active stream (nil) still proceeds — the decoder reports the
        // truth at commit.
        XCTAssertEqual(
            AudioSwitchPolicy.entry(
                opened: true, failed: false, switching: false, activeStream: nil, requested: 2),
            .proceed)
    }

    // MARK: - Commit

    func testRefusedSwitchReprimesTheOldTrackAtThePlayhead() {
        let d = AudioSwitchPolicy.commit(
            setTrackOk: false, reconciled: false, genCurrent: true, actualStream: 1)
        XCTAssertTrue(d.seekToPlayhead, "the old track's lookahead was flushed — re-prime it")
        XCTAssertNil(d.newActiveStream, "the cache already names the old track")
        XCTAssertFalse(d.latchFailed)
        XCTAssertFalse(d.rollBack)
        XCTAssertEqual(d.reportOk, false, "the tick must not move")
    }

    func testRefusedSwitchYieldsThePositionToANewerSeek() {
        let d = AudioSwitchPolicy.commit(
            setTrackOk: false, reconciled: false, genCurrent: false, actualStream: 1)
        XCTAssertFalse(d.seekToPlayhead, "a superseding user seek owns the position")
        XCTAssertEqual(d.reportOk, false)
    }

    func testCommittedSwitchCachesTheActualStreamAndSeeks() {
        let d = AudioSwitchPolicy.commit(
            setTrackOk: true, reconciled: true, genCurrent: true, actualStream: 2)
        XCTAssertTrue(d.seekToPlayhead, "a fresh decoder starts at zero — the seek is mandatory")
        XCTAssertEqual(d.newActiveStream, 2, "cache what is playing, never the request")
        XCTAssertEqual(d.reportOk, true)
        XCTAssertFalse(d.rollBack)
        XCTAssertFalse(d.latchFailed)
    }

    func testSupersededCommitStillCompletesTheSwitchButNotThePosition() {
        // The decoder WAS replaced; a user seek arrived mid-switch. The
        // transaction must still cache + report (a dropped half-switch garbles
        // audio) — only the playhead seek is withheld.
        let d = AudioSwitchPolicy.commit(
            setTrackOk: true, reconciled: true, genCurrent: false, actualStream: 2)
        XCTAssertFalse(d.seekToPlayhead, "the newer seek owns the position")
        XCTAssertEqual(d.newActiveStream, 2, "the switch still owns the track")
        XCTAssertEqual(d.reportOk, true)
    }

    func testReconcileFailureRollsBackBeforeReporting() {
        let d = AudioSwitchPolicy.commit(
            setTrackOk: true, reconciled: false, genCurrent: true, actualStream: 2)
        XCTAssertTrue(d.rollBack)
        XCTAssertNil(d.reportOk, "the outcome isn't known until rollback resolves")
        XCTAssertNil(d.newActiveStream, "a mismatched graph must not be cached as playing")
        XCTAssertFalse(d.seekToPlayhead)
        XCTAssertFalse(d.latchFailed)
    }

    // MARK: - Rollback

    func testSuccessfulRollbackReprimesTheOldStreamAndReportsFailure() {
        let d = AudioSwitchPolicy.rollback(
            canRollBack: true, setTrackOk: true, reconciled: true, genCurrent: true,
            actualStream: 1)
        XCTAssertTrue(d.seekToPlayhead)
        XCTAssertEqual(d.newActiveStream, 1, "the old stream is what plays now")
        XCTAssertFalse(d.latchFailed)
        XCTAssertEqual(d.reportOk, false, "the SWITCH failed even though audio survived")
    }

    func testSupersededRollbackYieldsThePositionButStillRecovers() {
        let d = AudioSwitchPolicy.rollback(
            canRollBack: true, setTrackOk: true, reconciled: true, genCurrent: false,
            actualStream: 1)
        XCTAssertFalse(d.seekToPlayhead)
        XCTAssertEqual(d.newActiveStream, 1)
        XCTAssertEqual(d.reportOk, false)
    }

    func testFailedRollbackLatchesFailed() {
        for (canRollBack, setTrackOk, reconciled) in [
            (false, false, false),  // no rollback target was ever known
            (true, false, false),  // the old stream refused to re-open
            (true, true, false),  // re-opened but the graph would not rebuild
        ] {
            let d = AudioSwitchPolicy.rollback(
                canRollBack: canRollBack, setTrackOk: setTrackOk, reconciled: reconciled,
                genCurrent: true, actualStream: nil)
            XCTAssertTrue(d.latchFailed, "\(canRollBack)/\(setTrackOk)/\(reconciled)")
            XCTAssertFalse(d.seekToPlayhead, "a dead pipeline must not seek")
            XCTAssertEqual(d.reportOk, false)
            XCTAssertNil(d.newActiveStream)
        }
    }
}
