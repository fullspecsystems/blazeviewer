import SwiftUI

/// The NS2 dialog sheets — Password / Loading / Scanning — presented over the canvas by
/// `ContentView`'s `.sheet(item:)` and bound to `CoreModel`'s dialog state (the drain
/// mutates it; these views are pure bindings + the explicit resolve buttons).
///
/// Buttons carry the resolution: primary = `.defaultAction` (Return), Cancel =
/// `.cancelAction` (Esc) — so every keyboard path is an *explicit* DialogResolved entry,
/// never an ambiguous sheet dismissal. (Confirm/Message are NSAlert sheets in CoreModel;
/// About is the standard NSApp panel; Settings has its own window.)

/// The archive password prompt: lock + the two-line prompt (file name on its own line), a
/// secure field, an inline wrong-password error, and a "Checking…" state while a submitted
/// entry re-opens the archive. Mirrors the egui `password_dialog`.
struct PasswordSheetView: View {
    @Bindable var model: CoreModel
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "lock.fill")
                    .font(.title)
                    .foregroundStyle(.tint)
                Text(model.dialogMessage)
                    .fixedSize(horizontal: false, vertical: true)
            }
            SecureField("Password", text: $model.passwordEntry)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { model.passwordSubmit() }
                .disabled(model.dialogChecking)
            if !model.passwordError.isEmpty {
                Text(model.passwordError)
                    .font(.callout)
                    .foregroundStyle(.red)
            }
            HStack(spacing: 8) {
                if model.dialogChecking {
                    ProgressView().controlSize(.small)
                    Text("Checking…").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { model.passwordCancel() }
                    .keyboardShortcut(.cancelAction)
                Button("Unlock") { model.passwordSubmit() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.dialogChecking || model.passwordEntry.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 380)
        .onAppear { focused = true }
    }
}

/// The "Ask about image" sheet (task #44): a sparkles icon + a **multi-line** `TextEditor`
/// for a question about the current photo, and an Ask/Cancel bar. Plain Return inserts a
/// newline (real textarea); ⌘Return or the Ask button submits. Mirrors the egui `ask_dialog`.
struct AskSheetView: View {
    @Bindable var model: CoreModel
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "sparkles")
                    .font(.title)
                    .foregroundStyle(.tint)
                Text("Ask a question about this image:")
                    .fixedSize(horizontal: false, vertical: true)
            }
            TextEditor(text: $model.askEntry)
                .font(.body)
                .frame(minHeight: 90)
                .overlay(
                    RoundedRectangle(cornerRadius: 6).stroke(.secondary.opacity(0.3))
                )
                .focused($focused)
            Text("⌘⏎ to ask")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack(spacing: 8) {
                Spacer()
                Button("Cancel") { model.askCancel() }
                    .keyboardShortcut(.cancelAction)
                Button("Ask") { model.askSubmit() }
                    .keyboardShortcut(.return, modifiers: .command)
                    .buttonStyle(.borderedProminent)  // the accent (blue) default action
                    .disabled(model.askEntry.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420)
        .onAppear { focused = true }
    }
}

/// The archive "Opening…" sheet: determinate once the header publishes a total (a spinner
/// until then) + Cancel. The fraction refreshes each pump from the Rust-side handle.
struct LoadingSheetView: View {
    let model: CoreModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(model.dialogMessage)
                .fixedSize(horizontal: false, vertical: true)
            if model.progressFraction > 0 {
                ProgressView(value: model.progressFraction)
            } else {
                ProgressView()
                    .frame(maxWidth: .infinity)
            }
            HStack {
                Spacer()
                Button("Cancel") { model.loadingCancel() }
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}

/// The folder "Scanning…" sheet (revealed only for a genuinely slow walk): live image
/// count + the folder currently being walked + Cancel (keeps the current view).
struct ScanningSheetView: View {
    let model: CoreModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(model.dialogMessage)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("\(model.scanFound) images found")
                    .monospacedDigit()
            }
            Text(model.scanCurrentDir.isEmpty ? " " : model.scanCurrentDir)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            HStack {
                Spacer()
                Button("Cancel") { model.scanningCancel() }
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 420)
    }
}
