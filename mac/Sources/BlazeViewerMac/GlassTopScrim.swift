// The top legibility scrim for the transparent-toolbar mode (task #59). When the canvas runs
// under a translucent glass toolbar, a bright photo (in dark mode) — or a dark one (in light
// mode) — leaves the title bar's text with no contrast. The standard macOS fix, used by Preview
// and friends, is a soft material-blur-plus-tint fading down from the top edge; this is our take.
//
// Rendered in the SwiftUI content layer (below the AppKit titlebar), so it shows *through* the
// transparent titlebar behind the title text. Deliberately restrained: it covers only the top
// ~half of the bar and fades to nothing well before the bar's bottom (never spilling onto the
// photo below it), which keeps the mostly-transparent look while still backing the title. It's
// invisible in fit mode (the top strip is the letterbox there) — it only bites when the photo
// extends under the bar (zoom / fill), which is exactly when contrast is lost.

import SwiftUI

struct GlassTopScrim: View {
    /// The glass bar's height in points (`CoreModel.glassTopInsetPoints`).
    let height: CGFloat

    @Environment(\.colorScheme) private var scheme

    var body: some View {
        // Dark tint in dark mode, light in light mode — matching the titlebar text it backs.
        let base: Color = scheme == .dark ? .black : .white
        // Ease-out stops: steep near the top, then a long gentle tail so the bottom fades to
        // nothing gradually — no perceptible edge — without making the band any taller.
        LinearGradient(
            stops: [
                .init(color: base.opacity(0.54), location: 0.0), // a touch darker at the very top
                .init(color: base.opacity(0.34), location: 0.28),
                .init(color: base.opacity(0.15), location: 0.55),
                .init(color: base.opacity(0.05), location: 0.80),
                .init(color: base.opacity(0.0), location: 1.0),
            ],
            startPoint: .top,
            endPoint: .bottom
        )
        .frame(maxWidth: .infinity)
        // ~80% of the original height, fading out a little past the bar for a soft edge.
        .frame(height: max(0, height) * 1.08)
        .ignoresSafeArea(edges: .top)
        .allowsHitTesting(false)
    }
}
