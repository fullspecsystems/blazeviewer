// The "can't display this image" placeholder (task #127) — shown centered over the
// canvas when the item on screen is a genuinely undecodable file (every rung of the
// decode recovery ladder failed). It stands in for the photo that can't be shown, the
// same way `DoorCardView` stands in for an archive door: content chrome, not a nag, so
// it has no fade timer and no hover-to-hold.
//
// Deliberately built in the SAME panel language as the door card (header + divider +
// a large glyph over `.panelBackground`), so a broken file and an archive read as
// siblings rather than two unrelated stand-ins. The core blanks the canvas behind it
// (`present_failed`) so nothing of the previous photo shows through. It never intercepts
// input (`allowsHitTesting(false)`) — nav must still fall through to move off the file.

import SwiftUI

struct DecodeErrorView: View {
    /// The file name (e.g. "IMG_1340.JPG"), named like the door card names its archive.
    let name: String
    /// The cleaned decoder reason (e.g. "No more bytes"); empty renders just the glyph.
    let reason: String
    let opacity: Double

    /// Matches the door card's folder-art footprint (`DoorCardView.artCap` ≈ 148 pt of
    /// folder) so the two placeholders feel the same size.
    private static let iconSize: CGFloat = 140
    private static let bodyPad: CGFloat = 16
    private static let gap: CGFloat = 12
    private static let width: CGFloat = 264

    init(name: String, reason: String, opacity: Double = 1) {
        self.name = name
        self.reason = reason
        self.opacity = opacity
    }

    var body: some View {
        VStack(spacing: 0) {
            Text("Error Displaying Image")
                .font(.headline)
                .frame(height: PanelMetrics.headerHeight)
            PanelDivider()

            VStack(spacing: Self.gap) {
                Image(systemName: "photo.badge.exclamationmark")
                    .resizable()
                    .scaledToFit()
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(.secondary)
                    .frame(width: Self.iconSize, height: Self.iconSize)
                // Name over reason — the door card's name-line role, and the one fact the
                // titlebar/details also carry, so a user reading the card knows *which* file.
                VStack(spacing: 4) {
                    if !name.isEmpty {
                        Text(name)
                            .font(.body)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                    if !reason.isEmpty {
                        Text(reason)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .multilineTextAlignment(.center)
                    }
                }
            }
            .padding(Self.bodyPad)
        }
        .frame(width: Self.width)
        .panelBackground(opacity: opacity)
        .help(name)
        .allowsHitTesting(false)
    }
}
