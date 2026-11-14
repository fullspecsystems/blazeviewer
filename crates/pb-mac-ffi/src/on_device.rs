//! Apple's on-device Foundation Model as the core's
//! [`HostDescriber`](pb_app_core::describe::HostDescriber) (task #44.5, macOS 27+).
//!
//! Foundation Models is a Swift-only framework, so the Swift host carries a tiny `@objc`
//! shim — `PBOnDeviceDescriber`, `mac/Sources/BlazeViewerMac/OnDeviceDescriber.swift` — and
//! this calls it **by class name through the Objective-C runtime**, the way `pb-app-core`'s
//! OCR calls Vision. No swift-bridge surface, no effect round trip: the core's describe
//! worker calls [`AppleOnDevice::describe`] exactly as it calls the LM Studio backend, with
//! the same prepared image.
//!
//! The class is looked up on every call, so its absence (a binary without the Swift host,
//! such as a test) reads as "unavailable", never a crash. ⚠ The class name and the two
//! selectors must match the Swift file.

use objc2::msg_send;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyClass;
use objc2_foundation::{NSData, NSError, NSString};
use pb_app_core::describe::{DescribeError, HostDescriber, PreparedImage};

/// `@objc(PBOnDeviceDescriber)` in the Swift host.
const SHIM_CLASS: &std::ffi::CStr = c"PBOnDeviceDescriber";

const NOT_IN_BUILD: &str = "Apple's on-device model isn't available in this build.";

/// Apple's on-device model, via the Swift shim. Stateless: everything is per call.
pub struct AppleOnDevice;

fn shim() -> Option<&'static AnyClass> {
    AnyClass::get(SHIM_CLASS)
}

impl HostDescriber for AppleOnDevice {
    fn availability(&self) -> Result<(), String> {
        let Some(cls) = shim() else {
            return Err(NOT_IN_BUILD.to_string());
        };
        autoreleasepool(|_| {
            // SAFETY: `+[PBOnDeviceDescriber unavailableReason]` takes no arguments and
            // returns `NSString * _Nullable` (Swift `String?`).
            let why: Option<Retained<NSString>> = unsafe { msg_send![cls, unavailableReason] };
            match why {
                None => Ok(()),
                Some(why) => Err(why.to_string()),
            }
        })
    }

    fn describe(
        &self,
        image: &PreparedImage,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String, DescribeError> {
        let Some(cls) = shim() else {
            return Err(DescribeError::Backend(NOT_IN_BUILD.to_string()));
        };
        // A Rust worker thread has no autorelease pool of its own.
        autoreleasepool(|_| {
            let jpeg = NSData::with_bytes(&image.jpeg);
            let prompt = NSString::from_str(prompt);
            let max_tokens = isize::try_from(max_tokens).unwrap_or(isize::MAX);
            // SAFETY: matches Swift's
            // `@objc static func describeJPEG(_: Data, prompt: String, maxTokens: Int) throws -> String`,
            // i.e. `+ (NSString *)describeJPEG:(NSData *)prompt:(NSString *)maxTokens:(NSInteger)
            // error:(NSError **)` — nil + an NSError on failure. It blocks this (worker)
            // thread until the model answers; the core never calls it on the main thread.
            let answer: Result<Retained<NSString>, Retained<NSError>> = unsafe {
                msg_send![
                    cls,
                    describeJPEG: &*jpeg,
                    prompt: &*prompt,
                    maxTokens: max_tokens,
                    error: _
                ]
            };
            answer
                .map(|text| text.to_string())
                // The shim words its errors for the panel; show them verbatim.
                .map_err(|e| DescribeError::Backend(e.localizedDescription().to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test binary has no Swift host, so the shim class is absent: the backend reports
    /// itself unavailable (the core's Auto then falls back to the endpoint) rather than
    /// crashing on a missing class.
    #[test]
    fn without_the_swift_host_it_is_unavailable_not_a_crash() {
        assert_eq!(AppleOnDevice.availability(), Err(NOT_IN_BUILD.to_string()));
        let image = PreparedImage { jpeg: Vec::new() };
        assert_eq!(
            AppleOnDevice.describe(&image, "Describe it.", 512),
            Err(DescribeError::Backend(NOT_IN_BUILD.to_string()))
        );
    }
}
