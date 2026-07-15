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

            TrackPickerButton(model: model)
        }
        // Width is governed by the info-line pill (min/max); the scrubber fills what's left.
    }
}

/// One row of the subtitle track picker (task #99). `id` is the row's index in the core's
/// list — the currency `selectSubtitleTrack` takes, not a display detail.
struct SubtitleTrackRow: Identifiable {
    let id: Int
    let label: String
    let active: Bool
}

/// The track picker (task #99, owner placement 2026-07-15): a button to the **right of the
/// total runtime**, complementing the play button on the left, opening the subtitle track
/// list.
///
/// The list is pulled on open and never cached — it is per-file, and a cached one is stale
/// exactly when the user navigates.
struct TrackPickerButton: View {
    let model: CoreModel

    @State private var open = false
    @State private var rows: [SubtitleTrackRow] = []
    @State private var known = false

    var body: some View {
        // The rows are loaded HERE, before `open` flips — not in `.onChange`, which fires
        // after the popover has begun presenting and would show a frame of "Reading tracks…"
        // over a list we already had.
        Button(action: {
            if !open { reload() }
            open.toggle()
        }) {
            Image(systemName: "captions.bubble")
                .font(.system(size: 14))
                .foregroundStyle(.primary)
                .frame(width: 20, height: 20)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Subtitles")
        .accessibilityLabel("Subtitle track")
        // `.top`: the playback bar lives at the *bottom* of the window, so the list opens
        // upward, over the picture. (AppKit only treats the edge as a preference and would
        // flip it anyway for want of room — but the preference should still say what we mean.)
        .popover(isPresented: $open, arrowEdge: .top) {
            TrackPickerPopover(rows: rows, known: known) { row in
                model.selectSubtitleTrack(row)
                reload()
            }
        }
        .onChange(of: open) { _, isOpen in
            // Pin the controls up for as long as the popover is anchored to them, and
            // re-arm the fade on close. Without this the bar decays out from under an open
            // popover on its 1.8s timer. (Also catches a dismiss-by-clicking-outside, which
            // never runs the button's action.)
            model.pickerOpenChanged(isOpen)
        }
    }

    private func reload() {
        rows = model.subtitleTrackRows()
        known = model.subtitleTracksKnown
    }
}

/// The picker's list. Deliberately dumb — it draws the rows it is handed and reports a
/// click; every decision about *what* the rows are lives in the core.
struct TrackPickerPopover: View {
    let rows: [SubtitleTrackRow]
    let known: Bool
    let select: (Int) -> Void

    /// Films carry mind-boggling subtitle counts (a 30-track Blu-ray rip is unremarkable),
    /// so the list scrolls past this rather than growing a popover taller than the display.
    private let maxHeight: CGFloat = 320

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Subtitles")
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 12)
                .padding(.top, 10)

            if rows.isEmpty {
                // 0 rows never means "no tracks" — the Off row would be there if we knew.
                // It means the probe hasn't landed, so say *that*.
                Text(known ? "No subtitle tracks" : "Reading tracks…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
                    .padding(.bottom, 10)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(rows) { row in
                            TrackPickerRow(row: row) { select(row.id) }
                        }
                    }
                    .padding(.horizontal, 6)
                }
                .frame(maxHeight: maxHeight)
                .scrollBounceBehavior(.basedOnSize)
                .padding(.bottom, 6)
            }
        }
        .frame(minWidth: 220)
    }
}

/// A single selectable row: a checkmark gutter (always reserved, so labels don't shift as
/// the tick moves) and the track's shared `track_summary` line.
private struct TrackPickerRow: View {
    let row: SubtitleTrackRow
    let action: () -> Void

    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Image(systemName: "checkmark")
                    .font(.system(size: 11, weight: .semibold))
                    .opacity(row.active ? 1 : 0)
                    .frame(width: 12)
                Text(row.label)
                    .font(.callout)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 6)
            .padding(.vertical, 4)
            .contentShape(Rectangle())
            .background(
                RoundedRectangle(cornerRadius: 4)
                    .fill(hovering ? Color.primary.opacity(0.1) : .clear)
            )
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .accessibilityAddTraits(row.active ? [.isSelected] : [])
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
