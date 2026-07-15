import AppKit
import SwiftUI

/// A native macOS combo box (`NSComboBox`): one bordered field with a single integrated
/// dropdown arrow, editable *and* pickable from a list. SwiftUI has no built-in
/// equivalent — a `TextField` next to a separate `Menu` button only approximates it (and
/// without `.menuIndicator(.hidden)` the Menu's own default chevron doubles up with a
/// custom one). This wraps the real AppKit control instead. Reach for it anywhere a
/// setting can come from a fetched list OR free-typed text (the AI tab's Model field is
/// the first use; endpoint URLs / similar "pick or type" fields are the likely next one).
struct ComboBox: NSViewRepresentable {
    @Binding var text: String
    var placeholder: String = ""
    /// The literal dropdown row strings, in display order.
    var options: [String] = []
    /// Maps a selected row's literal text to the value written into `text` — e.g.
    /// stripping a cosmetic badge suffix, or mapping a sentinel row to `""`. Identity
    /// makes this a plain combo box where a row's text IS its value.
    var resolve: (String) -> String = { $0 }

    func makeCoordinator() -> Coordinator {
        Coordinator(text: $text, resolve: resolve)
    }

    func makeNSView(context: Context) -> NSComboBox {
        let box = NSComboBox()
        box.completes = false
        box.delegate = context.coordinator
        return box
    }

    func updateNSView(_ box: NSComboBox, context: Context) {
        context.coordinator.resolve = resolve
        box.placeholderString = placeholder
        if box.stringValue != text {
            box.stringValue = text
        }
        if context.coordinator.lastOptions != options {
            box.removeAllItems()
            box.addItems(withObjectValues: options)
            context.coordinator.lastOptions = options
        }
    }

    final class Coordinator: NSObject, NSComboBoxDelegate {
        private let text: Binding<String>
        var resolve: (String) -> String
        var lastOptions: [String] = []

        init(text: Binding<String>, resolve: @escaping (String) -> String) {
            self.text = text
            self.resolve = resolve
        }

        /// Free typing: the field's literal text is the value, no resolve step.
        func controlTextDidChange(_ notification: Notification) {
            guard let box = notification.object as? NSComboBox else { return }
            text.wrappedValue = box.stringValue
        }

        /// Picking a row: resolve it to a value, and reflect that back into the field
        /// itself (not just the binding) so a badge/sentinel row doesn't linger as the
        /// visible text after selection.
        func comboBoxSelectionDidChange(_ notification: Notification) {
            guard let box = notification.object as? NSComboBox else { return }
            let value = resolve(box.stringValue)
            text.wrappedValue = value
            box.stringValue = value
        }
    }
}
