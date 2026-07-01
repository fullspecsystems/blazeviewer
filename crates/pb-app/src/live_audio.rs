//! Live Photo audio (task #38) — the "cheap path": play the companion `.mov`'s audio
//! track via `AVAudioPlayer`, tied to the motion's play/pause state.
//!
//! `AVAudioPlayer` reads the `.mov`'s audio track straight from the file URL and plays
//! it on its own thread, so there's no PCM extraction and no new audio-output plumbing.
//! Sync is "start together, pause together": the video frame pump and the audio are both
//! wall-clock-driven, so over a ~3 s clip they stay together to the ear. It is *not*
//! sample-accurate — that would want the `AVPlayer` streaming rework (a v2 task).
//!
//! Read-only, RAM-only — no trace written (privacy #2). macOS-only (Windows audio is
//! part of task #39); off macOS this is a no-op stub so the call sites stay cfg-free.

use std::path::Path;

pub use imp::LiveAudio;

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Path;

    /// No-op stub off macOS — Live Photos don't play there yet (task #39), so this is
    /// never actually constructed; it just keeps the `pb-app` call sites platform-free.
    pub struct LiveAudio;

    impl LiveAudio {
        pub fn play(_path: &Path, _start_secs: f64) -> Option<LiveAudio> {
            None
        }
        pub fn pause(&self) {}
        pub fn resume(&self) {}
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Path;
    use std::ffi::{c_void, CString};
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;

    type Id = *mut c_void;
    type Class = *mut c_void;
    type Sel = *const c_void;

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Class;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    // Force-link the frameworks that vend the classes we message.
    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {}
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {}

    #[inline]
    unsafe fn class(name: &[u8]) -> Class {
        objc_getClass(name.as_ptr() as *const c_char)
    }
    #[inline]
    unsafe fn sel(name: &[u8]) -> Sel {
        sel_registerName(name.as_ptr() as *const c_char)
    }
    // Typed `objc_msgSend` shims — one per call signature (see `livephoto.rs` for the ABI
    // rationale: the cast makes Rust marshal args/return per the platform C ABI).
    #[inline]
    unsafe fn send(o: Id, s: Sel) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s)
    }
    #[inline]
    unsafe fn send_cstr(o: Id, s: Sel, a: *const c_char) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s, a)
    }
    #[inline]
    unsafe fn send_id(o: Id, s: Sel, a: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s, a)
    }
    #[inline]
    unsafe fn send_init_url(o: Id, s: Sel, url: Id, err: *mut Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id, *mut Id) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s, url, err)
    }
    #[inline]
    unsafe fn send_bool(o: Id, s: Sel) -> bool {
        let f: unsafe extern "C" fn(Id, Sel) -> bool =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s)
    }
    #[inline]
    unsafe fn send_void(o: Id, s: Sel) {
        let f: unsafe extern "C" fn(Id, Sel) =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s)
    }
    #[inline]
    unsafe fn send_f64(o: Id, s: Sel, a: f64) {
        let f: unsafe extern "C" fn(Id, Sel, f64) =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(o, s, a)
    }

    /// A retained `AVAudioPlayer` for a Live Photo's motion `.mov`, playing its audio
    /// track. Owned `+1` from `alloc/init`; released on drop (which also stops playback).
    pub struct LiveAudio {
        player: Id,
    }

    impl LiveAudio {
        /// Open the `.mov`'s audio and start playing from `start_secs` into the clip
        /// (`0.0` = the top, to line up with the motion's first frame; a non-zero offset
        /// keeps a mid-playback unmute in sync). `None` if the file has no playable audio
        /// (a silent Live Photo) or the player can't be made.
        pub fn play(path: &Path, start_secs: f64) -> Option<LiveAudio> {
            let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
            unsafe {
                let pool = objc_autoreleasePoolPush();
                let player = build_player(&cpath);
                objc_autoreleasePoolPop(pool);
                let player = player?;
                send_f64(player, sel(b"setCurrentTime:\0"), start_secs.max(0.0));
                send_bool(player, sel(b"play\0"));
                Some(LiveAudio { player })
            }
        }

        /// Pause (keeps the position) — mirrors pausing the motion.
        pub fn pause(&self) {
            unsafe { send_void(self.player, sel(b"pause\0")) }
        }

        /// Resume from where it paused — mirrors resuming the motion.
        pub fn resume(&self) {
            unsafe {
                send_bool(self.player, sel(b"play\0"));
            }
        }
    }

    impl Drop for LiveAudio {
        fn drop(&mut self) {
            unsafe {
                send_void(self.player, sel(b"stop\0"));
                send_void(self.player, sel(b"release\0")); // balance the alloc/init +1
            }
        }
    }

    /// `[[AVAudioPlayer alloc] initWithContentsOfURL:[NSURL fileURLWithPath:path] error:nil]`,
    /// then `prepareToPlay`. Returns the owned player, or `None` on failure.
    unsafe fn build_player(cpath: &std::ffi::CStr) -> Option<Id> {
        let path_ns = send_cstr(
            class(b"NSString\0"),
            sel(b"stringWithUTF8String:\0"),
            cpath.as_ptr(),
        );
        if path_ns.is_null() {
            return None;
        }
        let url = send_id(class(b"NSURL\0"), sel(b"fileURLWithPath:\0"), path_ns);
        if url.is_null() {
            return None;
        }
        let alloc = send(class(b"AVAudioPlayer\0"), sel(b"alloc\0"));
        let mut err: Id = std::ptr::null_mut();
        let player = send_init_url(alloc, sel(b"initWithContentsOfURL:error:\0"), url, &mut err);
        if player.is_null() {
            return None;
        }
        send_bool(player, sel(b"prepareToPlay\0"));
        Some(player)
    }
}
