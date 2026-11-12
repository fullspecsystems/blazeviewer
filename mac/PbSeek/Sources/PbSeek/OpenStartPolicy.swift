/// Where a freshly opened native (`AVPlayer`) item goes before it is revealed and
/// played (task #139) — decided from the resume position the core asked for and the
/// position AVFoundation actually readied the item at.
///
/// **Why the ready position can't be trusted.** Attaching an `AVPlayerLayer` makes
/// AVFoundation prepare a first frame for display, and on some clips it does that at
/// the first *in-edit* keyframe rather than at time zero. The case that found it: an
/// iPhone 240 fps capture whose only early I-frame sits in the edit list's trimmed
/// lead-in (pts −0.054, flagged discard) and whose next keyframe is at 0.929 s — so
/// the item readies at 0.933 of a 1.282 s clip and three quarters of it is skipped.
/// Measured in a bare `AVPlayer`, no app code involved: without a layer the item
/// readies at 0.000, with one at 0.933; a default-tolerance `seek(to: .zero)` lands
/// back on 0.933, and only an exact (zero-tolerance) seek reaches 0.000 (~4 ms).
public enum OpenStartPolicy {
    /// Resume positions at or below this play from the start (task #94.2).
    public static let resumeFloor: Double = 0.5
    /// How far past zero a ready item may sit and still count as "at the start":
    /// half a frame at 240 fps. Anything further is a real skip, worth a seek.
    public static let startTolerance: Double = 0.002

    public enum Action: Equatable, Sendable {
        /// Already where it should be — reveal and play at once (the common case; no
        /// added latency).
        case revealNow
        /// Readied past its start: seek *exactly* (zero tolerance) to zero first, then
        /// reveal on the seek's completion so the first frame shown is the real one.
        case seekExactlyToStart
        /// A remembered position: seek there first (tolerant — it is a resume, not a
        /// frame-accurate request), then reveal.
        case seekToResume(Double)
    }

    public static func action(startSecs: Double, readyAt: Double) -> Action {
        if startSecs > resumeFloor { return .seekToResume(startSecs) }
        // A NaN/infinite playhead (an invalid CMTime) is garbage, not a position to fix.
        guard readyAt.isFinite else { return .revealNow }
        return readyAt > startTolerance ? .seekExactlyToStart : .revealNow
    }
}
