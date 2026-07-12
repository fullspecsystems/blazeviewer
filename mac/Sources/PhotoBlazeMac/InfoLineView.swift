// The one-line info readout (`i`) — the last HUD element converted to native SwiftUI. A small
// bottom-corner pill showing `folder/name · W×H · CODEC[· Live]`, reusing the shared
// panelBackground so it matches the rest of the chrome. Placement follows the Settings
// "info line alignment" (left / center / right, default right). For a video it grows a second,
// interactive **playback row** (play/pause + scrubber + elapsed/total — task 79.9 phase 5).

import SwiftUI

struct InfoLineView: View {
    let model: CoreModel

    // The pill's geometry. The codec badge is inset from the pill by ~`inset` on the top,
    // right, and bottom (its own vertical padding makes it about the row height, and the pill's
    // trailing padding drops to match the vertical one), so a badge radius of `pillRadius −
    // inset` makes its corners run roughly concentric with the pill's — parallel curves.
    private let pillRadius = PanelMetrics.cornerRadius
    private let inset: CGFloat = 6

    /// The SF Symbol marking an animated (non-Live-Photo) image beside the codec. Placeholder
    /// until the final glyph is chosen — one place to swap it.
    static let animationMark = "circle.dotted.and.circle"

    var body: some View {
        if model.videoControlsVisible {
            // Two-row pill: the summary on top, the interactive playback row below. A
            // reasonable min width so the scrubber is usable on short configs, capped so a
            // long filename truncates rather than spanning the screen; the summary centers
            // when it's narrower than the scrubber row. Uniform padding (the badge-concentric
            // trailing trick is a single-row nicety).
            VStack(alignment: .center, spacing: 6) {
                summaryRow
                VideoPlaybackRow(model: model)
            }
            .frame(minWidth: 300, maxWidth: 560)
            .padding(.horizontal, 11)
            .padding(.vertical, inset)
            .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
        } else {
            // Single row (stills, and video with `i` off) — unchanged from before.
            let hasCodec = !model.infoLineCodec.isEmpty
            summaryRow
                .padding(.leading, 11)
                // Trailing padding collapses to the vertical inset when the badge is present,
                // so it's spaced equally on the exposed sides and the concentric corners align.
                .padding(.trailing, hasCodec ? inset : 11)
                .padding(.vertical, inset)
                .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
        }
    }

    /// The `folder/name · W×H`, motion mark, and codec badge — the always-present summary.
    @ViewBuilder private var summaryRow: some View {
        let hasCodec = !model.infoLineCodec.isEmpty
        HStack(spacing: 8) {
            Text(model.infoLineText)
                .font(.callout)
                .foregroundStyle(.primary)
                .lineLimit(1)
                .truncationMode(.middle)
            // A Live Photo shows the livephoto mark by the codec (instead of the word "Live");
            // a video shows a film mark; any other animated image (GIF/APNG/…) shows a motion
            // mark. Swap `animationMark` for the final SF Symbol once picked.
            if model.infoLineIsLive {
                Image(systemName: "livephoto")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else if model.infoLineIsVideo {
                Image(systemName: "film")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else if model.infoLineIsAnimated {
                Image(systemName: Self.animationMark)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if hasCodec {
                Text(model.infoLineCodec)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    // Deliberately shorter than the callout row so the badge sits inside it and
                    // never drives the pill's height (do NOT use frame(maxHeight: .infinity) —
                    // it makes the pill fill the viewport).
                    .padding(.vertical, 2)
                    .background(
                        .quaternary,
                        in: RoundedRectangle(cornerRadius: pillRadius - inset)
                    )
            }
        }
    }
}
