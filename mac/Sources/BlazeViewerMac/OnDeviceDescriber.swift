import CoreGraphics
import Foundation
import FoundationModels
import ImageIO

/// Apple's on-device Foundation Model for AI descriptions and Ask (task #44.5) — the Swift
/// half of `pb-mac-ffi`'s `on_device::AppleOnDevice`, the core's `HostDescriber`.
///
/// Foundation Models is Swift-only, so this `@objc` shim is what the Rust describe worker
/// calls, **by class name through the Objective-C runtime** (the way the core's OCR calls
/// Vision). ⚠ Keep the class name and both selectors in step with
/// `crates/pb-mac-ffi/src/on_device.rs`: `unavailableReason` and
/// `describeJPEG:prompt:maxTokens:error:`.
///
/// The core hands over the same prepared image the LM Studio path gets (the frame on screen,
/// rotated, HDR tone-mapped, ≤1024 px, JPEG), so stills, video and animation frames and
/// photos inside archives all work alike.
///
/// Privacy (ADR-018): `SystemLanguageModel.default` runs **on this Mac**. Apple's Private
/// Cloud Compute model is deliberately not used here — if it is ever offered, it must be its
/// own opt-in backend behind a warning, never a fallback.
@objc(PBOnDeviceDescriber)
final class OnDeviceDescriber: NSObject {
    /// `nil` when the on-device model can describe images right now; otherwise why not, as
    /// one user-facing sentence. Cheap — the core asks each time a describe starts, so turning
    /// Apple Intelligence on (or a model download finishing) takes effect without a relaunch.
    @objc static func unavailableReason() -> String? {
        guard #available(macOS 27, *) else {
            return "Apple's on-device model needs macOS 27 or later."
        }
        let model = SystemLanguageModel.default
        switch model.availability {
        case .available:
            return model.capabilities.contains(.vision)
                ? nil : "Apple's on-device model on this Mac can't read images."
        case .unavailable(.deviceNotEligible):
            return "This Mac can't run Apple Intelligence."
        case .unavailable(.appleIntelligenceNotEnabled):
            return "Turn on Apple Intelligence in System Settings to use Apple's on-device model."
        case .unavailable(.modelNotReady):
            return "Apple's on-device model is still downloading. Try again in a little while."
        case .unavailable:
            return "Apple's on-device model isn't available right now."
        }
    }

    /// Describe a JPEG per `prompt`, answering in at most `maxTokens`. Called on a Rust
    /// describe worker thread — never the main thread — and blocks it until the model answers
    /// (typically 2–3 s). A superseded describe just finishes and is dropped by the core.
    @objc static func describeJPEG(_ jpeg: Data, prompt: String, maxTokens: Int) throws -> String {
        guard #available(macOS 27, *) else {
            throw failure(unavailableReason() ?? "Apple's on-device model needs macOS 27 or later.")
        }
        guard let source = CGImageSourceCreateWithData(jpeg as CFData, nil),
            let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else {
            throw failure("Couldn't prepare the image for Apple's on-device model.")
        }
        let box = ResultBox()
        let done = DispatchSemaphore(value: 0)
        Task.detached(priority: .userInitiated) {
            box.result = await respond(image: image, prompt: prompt, maxTokens: maxTokens)
            done.signal()
        }
        done.wait()
        return try box.result.get()
    }

    @available(macOS 27, *)
    private static func respond(image: CGImage, prompt: String, maxTokens: Int) async
        -> Result<String, Error>
    {
        do {
            // A fresh session per request: each describe stands alone (no chat history to
            // carry between photos), and sessions don't serialize against each other.
            let session = LanguageModelSession(model: SystemLanguageModel.default)
            let options = GenerationOptions(maximumResponseTokens: maxTokens)
            let response = try await session.respond(options: options) {
                prompt
                Attachment(image)
            }
            let text = response.content.trimmingCharacters(in: .whitespacesAndNewlines)
            return text.isEmpty
                ? .failure(failure("Apple's on-device model returned an empty description."))
                : .success(text)
        } catch {
            return .failure(failure(message(for: error)))
        }
    }

    /// One user-facing sentence for a model error — the panel shows it verbatim.
    @available(macOS 27, *)
    private static func message(for error: Error) -> String {
        if let error = error as? LanguageModelError {
            switch error {
            case .guardrailViolation, .refusal:
                return "Apple's on-device model declined to describe this image."
            case .contextSizeExceeded:
                return "The prompt is too long for Apple's on-device model. Shorten the custom "
                    + "prompt in Settings ▸ AI."
            case .rateLimited:
                return "Apple's on-device model is busy. Try again in a moment."
            case .unsupportedLanguageOrLocale:
                return "Apple's on-device model doesn't support this language yet."
            case .timeout:
                return "Apple's on-device model took too long to answer."
            default:
                break
            }
        }
        return "Apple's on-device model couldn't describe this image (\(error.localizedDescription))."
    }

    private static func failure(_ message: String) -> NSError {
        NSError(
            domain: "ca.fullspec.BlazeViewer.OnDeviceDescriber", code: 1,
            userInfo: [NSLocalizedDescriptionKey: message])
    }
}

/// Carries the async answer back to the blocked worker thread. The semaphore orders the
/// write before the read, so the unchecked `Sendable` is sound.
private final class ResultBox: @unchecked Sendable {
    var result: Result<String, Error> = .failure(
        NSError(domain: "ca.fullspec.BlazeViewer.OnDeviceDescriber", code: 0))
}
