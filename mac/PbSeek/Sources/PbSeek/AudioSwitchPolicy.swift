import Foundation

/// One decision of the Session route's audio-track **switch-as-rebuffer
/// transaction** (macos-video-smoothness §2) — produced by [`AudioSwitchPolicy`],
/// consumed by `SessionAudioPlayer`, which turns it into engine work (decoder
/// calls, graph reconnects, seeks, the outcome report).
public struct AudioSwitchDecision: Equatable, Sendable {
    /// Seek the (old or new) decoder to the captured playhead and re-arm feeding.
    /// Only ever true while this transaction's generation is still current — a
    /// superseding user seek owns the *position*; the switch owns the *track*.
    public let seekToPlayhead: Bool
    /// Cache this as the active stream (what is actually playing — never the
    /// request). `nil` = leave the cache unchanged.
    public let newActiveStream: Int?
    /// Latch the permanent Failed state (the core's silent fallback — playback
    /// never dies with the audio).
    public let latchFailed: Bool
    /// Run the rollback path (re-open the old stream) before anything else; the
    /// transaction stays open (`switching` stays set) until rollback resolves.
    public let rollBack: Bool
    /// Report the switch outcome now; `nil` = not yet (a rollback is pending).
    public let reportOk: Bool?

    public init(
        seekToPlayhead: Bool, newActiveStream: Int?, latchFailed: Bool,
        rollBack: Bool, reportOk: Bool?
    ) {
        self.seekToPlayhead = seekToPlayhead
        self.newActiveStream = newActiveStream
        self.latchFailed = latchFailed
        self.rollBack = rollBack
        self.reportOk = reportOk
    }
}

/// The pure decision table of the audio-track switch transaction, factored
/// PbSeek-style so the rules that make a switch safe are pinned by unit tests
/// rather than buried in async callbacks. The rules:
///
/// - a REFUSED switch re-primes the OLD track at the playhead (its engine
///   lookahead was already flushed) — unless a newer seek owns the position;
/// - once the decoder HAS been replaced, the transaction must complete (cache +
///   report) even when a newer seek superseded it — only the playhead seek is
///   withheld. A dropped half-switch would leave the engine format mismatched
///   with the decoder: mis-strided, garbled audio with no error anywhere;
/// - a format-reconcile failure rolls back to the old stream; a rollback failure
///   latches Failed;
/// - the reported outcome and the cached stream always describe what is actually
///   playing, never what was requested.
public enum AudioSwitchPolicy {
    /// Whether a switch request may start at all.
    public enum Entry: Equatable, Sendable {
        /// Not playable audio right now (unopened / failed / mid-switch) — report
        /// `ok = false`; the tick must not move.
        case refuse
        /// Already playing the requested stream — confirm without touching the
        /// graph (`ok = true`; the toast re-states the unchanged track).
        case confirmNoOp
        /// Run the transaction.
        case proceed
    }

    public static func entry(
        opened: Bool, failed: Bool, switching: Bool, activeStream: Int?, requested: Int
    ) -> Entry {
        if !opened || failed || switching { return .refuse }
        if activeStream == requested { return .confirmNoOp }
        return .proceed
    }

    /// The commit decision, after `session_audio_set_track` returned and (on
    /// success) the engine format reconcile was attempted. `actualStream` is what
    /// the decoder reports playing *now* — on a refusal that is the old track.
    public static func commit(
        setTrackOk: Bool, reconciled: Bool, genCurrent: Bool, actualStream: Int?
    ) -> AudioSwitchDecision {
        if !setTrackOk {
            // Old decoder untouched, lookahead flushed: re-prime it (unless a
            // newer seek owns the position). The cache already names the old
            // stream — leave it.
            return AudioSwitchDecision(
                seekToPlayhead: genCurrent, newActiveStream: nil, latchFailed: false,
                rollBack: false, reportOk: false)
        }
        if reconciled {
            return AudioSwitchDecision(
                seekToPlayhead: genCurrent, newActiveStream: actualStream,
                latchFailed: false, rollBack: false, reportOk: true)
        }
        // Replaced decoder, mismatched engine — roll back before reporting.
        return AudioSwitchDecision(
            seekToPlayhead: false, newActiveStream: nil, latchFailed: false,
            rollBack: true, reportOk: nil)
    }

    /// The rollback decision. `canRollBack` is false when the old stream was
    /// never known (no rollback target exists). However it resolves, the SWITCH
    /// failed — `reportOk` is always `false` here.
    public static func rollback(
        canRollBack: Bool, setTrackOk: Bool, reconciled: Bool, genCurrent: Bool,
        actualStream: Int?
    ) -> AudioSwitchDecision {
        if canRollBack && setTrackOk && reconciled {
            // The old stream plays again — re-prime it at the playhead (same
            // supersession rule) and re-cache what is actually playing.
            return AudioSwitchDecision(
                seekToPlayhead: genCurrent, newActiveStream: actualStream,
                latchFailed: false, rollBack: false, reportOk: false)
        }
        // No way back: latch Failed; the session degrades to silent playback.
        return AudioSwitchDecision(
            seekToPlayhead: false, newActiveStream: nil, latchFailed: true,
            rollBack: false, reportOk: false)
    }
}
