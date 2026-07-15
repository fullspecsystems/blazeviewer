import Foundation

/// What triggered a seek — carried through so diagnostics (plan §0) and any
/// source-specific behaviour can tell an arrow-key hop from a scrubber drag from a
/// resume-at-open. `initial` is *not* a seek: it is the first feed of a freshly opened
/// clip (`seekTarget == nil`, epoch 0), threaded through the same context so the anchor
/// path has one shape.
public enum SeekSource: String, Equatable, Sendable {
    case initial  // first playback, no seek in flight
    case arrow    // ←/→ signed-millisecond delta
    case scrub    // scrubber fraction (up to ~16/s)
    case step     // ,/. frame-step while paused
    case resume   // startSecs resume-at-open
}

/// The identity + intent of one seek, carried unchanged through every callback it can
/// touch — the video anchor, the audio completion, EOS, failure, and the UI landing
/// (plan §H1c). The whole reason it exists: `onDecodeAnchor` (the only thing that
/// restarts the clock) carried **no epoch** and unconditionally set the rate, so a stale
/// anchor from an older seek could overwrite a newer one. The scrubber issues up to ~16
/// seeks/second, so a stale anchor landing last is expected traffic, not a corner case.
///
/// Every callback compares `epoch` against the presenter's current epoch **before**
/// touching the clock, the renderers, or the scrubber, and drops the stale one.
public struct SeekContext: Equatable, Sendable {
    /// Monotonic generation. Bumped once per issued seek; `0` is the initial feed.
    public let epoch: UInt64
    /// The requested target in the 0-based presentation timeline (seconds), or `nil`
    /// for the initial feed (no seek — anchor at the first DTS instead).
    public let target: Double?
    /// The rate the user intended *after* the seek settles: `1` playing, `0` paused.
    /// Preserved across the seek so a paused scrub re-parks at rate 0 and a playing
    /// seek resumes at rate 1 (plan §H1b — the concept v1 conflated with the drain rate).
    public let desiredRateAfterSeek: Float
    /// What issued the seek (diagnostics + source-specific handling).
    public let source: SeekSource

    public init(epoch: UInt64, target: Double?, desiredRateAfterSeek: Float, source: SeekSource) {
        self.epoch = epoch
        self.target = target
        self.desiredRateAfterSeek = desiredRateAfterSeek
        self.source = source
    }

    /// The initial-feed context: epoch 0, no target, rate carried from the caller.
    public static func initial(desiredRate: Float) -> SeekContext {
        SeekContext(epoch: 0, target: nil, desiredRateAfterSeek: desiredRate, source: .initial)
    }

    /// Is `self` still current, given the presenter's latest epoch? A callback rejects a
    /// stale context before it can move the clock.
    public func isCurrent(latestEpoch: UInt64) -> Bool {
        epoch == latestEpoch
    }
}
