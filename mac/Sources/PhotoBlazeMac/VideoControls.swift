import AppKit
import SwiftUI

/// The info-line **playback row** for a video (task 79.9 phase 5): a play/pause button,
/// elapsed time, a click/drag scrubber, and the total — the macOS twin of the winit/egui
/// row. Play/pause routes through the core action (so it matches `P`/the toolbar); the
/// scrubber seeks the native `AVPlayer` (which owns the clock). Position/duration come
/// from the player's periodic observer via `CoreModel`.
struct VideoPlaybackRow: View {
    let model: CoreModel

    var body: some View {
        HStack(spacing: 11) {
            Button(action: { model.toggleVideoPlay() }) {
                Image(systemName: model.videoPlaying ? "pause.fill" : "play.fill")
                    .font(.system(size: 15))
                    .foregroundStyle(.primary)
                    .frame(width: 20, height: 20)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(model.videoPlaying ? "Pause" : "Play")
            .accessibilityLabel(model.videoPlaying ? "Pause" : "Play")

            Text(model.videoElapsed)
                .font(.callout)
                .monospacedDigit()
                .foregroundStyle(.secondary)

            VideoScrubber(model: model)

            if !model.videoTotal.isEmpty {
                Text(model.videoTotal)
                    .font(.callout)
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
        }
        // Width is governed by the info-line pill (min/max); the scrubber fills what's left.
    }
}

/// A click/drag seek bar. While dragging, the knob follows the pointer locally (a periodic
/// AVPlayer update must not yank it away); seeks are throttled by time and the final
/// position is always sent on release. The bar's displayed fraction is the player's when
/// idle, the pointer's while dragging. The track is inset by the knob radius so the knob
/// stays within the bar's bounds at 0 % / 100 % (never overhanging the time labels).
struct VideoScrubber: View {
    let model: CoreModel

    @State private var dragging = false
    @State private var dragFraction = 0.0
    @State private var lastSeek = Date.distantPast

    private let trackHeight: CGFloat = 4
    private let knobRadius: CGFloat = 7
    /// At most ~16 live seeks/s while scrubbing (AVPlayer also coalesces); the release
    /// always sends a final, unthrottled seek.
    private let seekInterval: TimeInterval = 0.06

    var body: some View {
        GeometryReader { geo in
            let width = max(1, geo.size.width)
            // The knob center travels [knobRadius, width − knobRadius], so it never overhangs.
            let usable = max(1, width - knobRadius * 2)
            let fraction = dragging ? dragFraction : model.videoFraction
            let knobX = knobRadius + usable * CGFloat(fraction)

            ZStack(alignment: .leading) {
                Capsule()
                    .fill(Color.secondary.opacity(0.35))
                    .frame(width: usable, height: trackHeight)
                    .offset(x: knobRadius)
                Capsule()
                    .fill(Color.accentColor)
                    .frame(width: max(trackHeight, knobX - knobRadius), height: trackHeight)
                    .offset(x: knobRadius)
                Circle()
                    .fill(dragging ? Color.accentColor : Color.primary)
                    .frame(width: knobRadius * 2, height: knobRadius * 2)
                    .offset(x: knobX - knobRadius)
            }
            .frame(maxHeight: .infinity)
            // The glide comes from the model (it animates only normal forward playback steps
            // via withAnimation; jumps snap). No implicit animation here — that would glide the
            // jump-to-0 on a new video/replay too.
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { value in
                        if !dragging { model.scrubbingChanged(true) } // pin the controls up
                        dragging = true
                        dragFraction = clamp((value.location.x - knobRadius) / usable)
                        let now = Date()
                        if now.timeIntervalSince(lastSeek) >= seekInterval {
                            lastSeek = now
                            model.seekVideoFraction(dragFraction)
                        }
                    }
                    .onEnded { value in
                        model.seekVideoFraction(clamp((value.location.x - knobRadius) / usable))
                        dragging = false
                        model.scrubbingChanged(false) // let the reveal fade on its normal timer
                    }
            )
            .onHover { $0 ? NSCursor.pointingHand.set() : NSCursor.arrow.set() }
            .accessibilityElement()
            .accessibilityLabel("Playback position")
            .accessibilityValue("\(model.videoElapsed) of \(model.videoTotal)")
        }
        .frame(height: knobRadius * 2)
        .frame(minWidth: 120)
    }

    private func clamp(_ x: CGFloat) -> Double {
        Double(min(1, max(0, x)))
    }
}
