import Foundation

/// One compressed frame's fate during a seek pre-roll (plan §T1). Produced by
/// `SeekFramePolicy.decide`, consumed by `DemuxReader` which turns it into an
/// `AVSampleBufferDisplayLayer` enqueue: `doNotDisplay` → the
/// `kCMSampleAttachmentKey_DoNotDisplay` attachment; a non-nil `anchorSecs` → the first
/// displayable frame, at which the synchronizer anchors and the clock (re)starts.
public struct SeekFrameDecision: Equatable, Sendable {
    /// Feed the frame decode-only: VideoToolbox decodes it (later frames reference it),
    /// the layer never shows it. True for the pre-roll span keyframe → target.
    public let doNotDisplay: Bool
    /// If non-nil, this is the first *displayable* frame of the feed/seek — anchor the
    /// synchronizer here and apply the desired rate. `nil` means "keep feeding, do not
    /// anchor on this frame."
    public let anchorSecs: Double?

    public init(doNotDisplay: Bool, anchorSecs: Double?) {
        self.doNotDisplay = doNotDisplay
        self.anchorSecs = anchorSecs
    }
}

/// The **pure** seek-frame decision logic for the sample-buffer route, lifted out of
/// `DemuxReader.provide` so it is unit-testable without a live display layer (plan §T1).
///
/// It answers, per frame: is this pre-roll (decode-only)? and is this the frame that
/// anchors the clock, at what time? The two rules that make forward seeking work at all
/// (and that shipped as a comment, untested, in `97f80bb4`):
///
///  1. **Pre-roll.** `demux_seek` lands on the keyframe *at or before* the target, so the
///     frames from that keyframe up to the target must be decoded but not shown. A frame
///     whose PTS is (strictly, by an epsilon) before the target is pre-roll.
///  2. **Anchor.** The clock anchors on the first frame that will actually be seen. For a
///     seek that is the **target** (the displayable frame's PTS is at/after it, so it is
///     on time and the pre-roll's DoNotDisplay keeps the replay invisible). For the
///     **initial** feed (no seek) it is the first **DTS**, not PTS — a B-frame stream's
///     opening IDR decodes before it presents (a negative synthesized DTS here), and
///     anchoring at the PTS would make it "late" and the layer would drop the IDR.
///
/// Every input is seconds in the 0-based presentation timeline; the caller does the
/// CMTime conversion. Non-finite timestamps (a missing PTS/DTS sentinel became NaN/∞ at
/// the edge) are handled explicitly rather than producing a silently-wrong comparison.
public struct SeekFramePolicy: Sendable {
    /// A frame is pre-roll only if its PTS is at least this far *before* the target.
    /// Matches the historical `target - 0.001` so extraction is behaviour-preserving; a
    /// frame within epsilon of the target counts as *at* the target (displayable).
    public static let prerollEpsilon: Double = 0.001

    public init() {}

    /// Decide one compressed frame.
    ///
    /// - Parameters:
    ///   - seekTarget: the requested target (seconds), or `nil` for the initial feed.
    ///   - ptsSecs: this frame's PTS (seconds); non-finite ⇒ treated as *not* pre-roll
    ///     (we cannot prove it precedes the target, so we never hide it).
    ///   - dtsSecs: this frame's DTS (seconds) — the initial-feed anchor time.
    ///   - firstFrameSent: has a displayable frame already anchored this feed/seek?
    public func decide(
        seekTarget: Double?, ptsSecs: Double, dtsSecs: Double, firstFrameSent: Bool
    ) -> SeekFrameDecision {
        // Pre-roll: meaningful only during a seek, and only for a finite PTS strictly
        // before the target. A non-finite PTS is never pre-roll.
        let preroll: Bool
        if let target = seekTarget, ptsSecs.isFinite {
            preroll = ptsSecs < target - Self.prerollEpsilon
        } else {
            preroll = false
        }
        // Anchor on the first displayable frame only. Once `firstFrameSent`, never again.
        guard !firstFrameSent, !preroll else {
            return SeekFrameDecision(doNotDisplay: preroll, anchorSecs: nil)
        }
        let anchor: Double
        if let target = seekTarget {
            anchor = target  // a seek anchors at the target (see rule 2)
        } else {
            anchor = dtsSecs  // the initial feed anchors at the first DTS
        }
        return SeekFrameDecision(doNotDisplay: false, anchorSecs: anchor)
    }

    /// The forced anchor when a seek reaches EOS **before** ever hitting its target (near
    /// the end of a clip, or a stream whose last frames precede it). Without it the clock
    /// stays held at rate 0 forever — a frozen picture with no way out. Returns the target
    /// to anchor at anyway, or `nil` when there is nothing to force (no seek in flight, or
    /// a displayable frame already anchored).
    public func eosAnchor(seekTarget: Double?, firstFrameSent: Bool) -> Double? {
        guard !firstFrameSent, let target = seekTarget else { return nil }
        return target
    }

    /// Resolve the effective anchor target once the landed keyframe's PTS is known
    /// (plan §H1b **S3** — bound the pre-roll). If the keyframe → target span exceeds
    /// `budgetSecs`, decoding the whole pre-roll before the clock can start risks a stall,
    /// and sub-GOP accuracy is not worth it: anchor at the keyframe and show immediately.
    /// `budgetSecs == nil` (the default until the corpus measurement picks a number —
    /// 320×240 fixtures can validate correctness but cannot choose the budget) keeps the
    /// exact requested target, i.e. today's behaviour.
    ///
    /// - Parameters:
    ///   - requested: the requested target (seconds).
    ///   - keyframeSecs: the real PTS of the first post-seek packet (seconds). Non-finite
    ///     ⇒ no clamp (we cannot measure the span).
    ///   - budgetSecs: the maximum pre-roll span to pay for, or `nil` for no bound.
    public func effectiveTarget(
        requested: Double, keyframeSecs: Double, budgetSecs: Double?
    ) -> Double {
        guard let budget = budgetSecs, keyframeSecs.isFinite, budget >= 0 else { return requested }
        if requested - keyframeSecs > budget {
            return keyframeSecs  // too far to pre-roll in time — land on the keyframe
        }
        return requested
    }
}
