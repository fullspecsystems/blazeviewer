//! Live Photo audio (tasks #38/#39) — the "cheap path": play the companion `.mov`'s
//! audio track via the platform's media player (`AVAudioPlayer` on macOS, the WinRT
//! `MediaPlayer` on Windows), tied to the motion's play/pause state.
//!
//! Both players read the `.mov`'s audio track straight from the file URL and play it
//! on their own thread, so there's no PCM extraction and no new audio-output plumbing.
//! Sync is "start together, pause together": the video frame pump and the audio are both
//! wall-clock-driven, so over a ~3 s clip they stay together to the ear. It is *not*
//! sample-accurate — that would want the `AVPlayer` streaming rework (a v2 task).
//!
//! On **Linux** (feature `livephoto`) there's no such all-in-one player, so we extract the
//! PCM (FFmpeg via `pb_decode::decode_motion_audio`) and pipe it to PipeWire's `pw-cat` —
//! see the Linux `imp` below for why that beats cpal/rodio on a PipeWire desktop. Without the
//! feature (or on any other platform) it's a no-op stub, so the call sites stay cfg-free.
//!
//! Read-only, RAM-only — no trace written (privacy #2); the PCM is streamed through a pipe,
//! never staged to disk.

use std::path::Path;

pub use imp::LiveAudio;

#[cfg(not(any(
    target_os = "macos",
    windows,
    all(unix, not(target_os = "macos"), feature = "livephoto")
)))]
mod imp {
    use super::Path;

    /// No-op stub where there's no Live Photo audio backend — the call sites stay
    /// platform-free and this is never actually constructed.
    pub struct LiveAudio;

    impl LiveAudio {
        pub fn play(_path: &Path, _start_secs: f64) -> Option<LiveAudio> {
            None
        }
        pub fn pause(&self) {}
        pub fn resume(&self) {}
    }
}

#[cfg(all(unix, not(target_os = "macos"), feature = "livephoto"))]
mod imp {
    use super::Path;
    use std::io::Write;
    use std::process::{Child, Command, Stdio};
    use std::thread::JoinHandle;

    /// Linux Live Photo audio: FFmpeg (`pb_decode::decode_motion_audio`) decodes the `.mov`'s
    /// audio track to interleaved f32 PCM, and we **pipe it to PipeWire's `pw-cat`** (raw f32
    /// on stdin) — the Linux analog of the macOS `AVAudioPlayer` / Windows `MediaPlayer` paths.
    ///
    /// Why `pw-cat`, not cpal/rodio: on a PipeWire desktop cpal's ALSA PCM advertises
    /// `buffer_size min: 1` and underruns nondeterministically (audible machine-gun clicks) no
    /// matter the buffer/config strategy tried; talking to PipeWire directly — as `pw-cat` does
    /// — plays clean. PipeWire also resamples/upmixes robustly, so we hand it the source
    /// rate/channels verbatim and let it convert to the device. When the PCM is exhausted the
    /// writer closes stdin, `pw-cat` hits EOF and exits on its own, so there is no idle stream
    /// left open to underrun. `None` (graceful silent motion) if `pw-cat` isn't on `PATH`.
    pub struct LiveAudio {
        child: Child,
        // Feeds stdin, paced by pw-cat's consumption (pipe backpressure). Joined on drop.
        writer: Option<JoinHandle<()>>,
    }

    impl LiveAudio {
        /// Decode the `.mov`'s audio and start playing from `start_secs` in (0.0 = the top, to
        /// line up with the motion's first frame; a non-zero offset keeps a mid-playback unmute
        /// in sync). `None` if there's no audio track or `pw-cat` can't be spawned — both stay
        /// graceful (silent motion), never a crash.
        pub fn play(path: &Path, start_secs: f64) -> Option<LiveAudio> {
            let audio = pb_decode::decode_motion_audio(path).ok()?;
            let ch = audio.channels.max(1);
            let rate = audio.sample_rate.max(1);
            // Skip whole interleaved frames for the start offset (mid-clip unmute stays in sync).
            let skip = ((start_secs.max(0.0) * rate as f64) as usize * ch as usize)
                .min(audio.samples.len());

            let mut child = Command::new("pw-cat")
                .args([
                    "--playback",
                    "--raw",
                    "--format",
                    "f32",
                    "--rate",
                    &rate.to_string(),
                    "--channels",
                    &ch.to_string(),
                    "-",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?; // pw-cat absent → None → silent motion (graceful)

            let mut stdin = child.stdin.take()?;
            let samples = audio.samples;
            // Write raw little-endian f32; pw-cat consuming at playback rate paces us through
            // pipe backpressure, so the kernel never holds the whole clip at once. A closed pipe
            // (drop/kill) errors the write and ends the thread.
            let writer = std::thread::spawn(move || {
                let mut w = std::io::BufWriter::with_capacity(1 << 15, &mut stdin);
                for &s in &samples[skip..] {
                    if w.write_all(&s.to_le_bytes()).is_err() {
                        return;
                    }
                }
                let _ = w.flush();
            });

            Some(LiveAudio {
                child,
                writer: Some(writer),
            })
        }

        /// Pause — SIGSTOP the `pw-cat` child (freezes playback, keeps position). Idempotent.
        pub fn pause(&self) {
            self.signal(libc::SIGSTOP);
        }

        /// Resume — SIGCONT the `pw-cat` child. Idempotent (a no-op if already running).
        pub fn resume(&self) {
            self.signal(libc::SIGCONT);
        }

        fn signal(&self, sig: libc::c_int) {
            // SIGSTOP/SIGCONT need no state tracking: SIGCONT to a running process (or SIGSTOP
            // to a stopped one) is a no-op. Safe: `kill` just delivers a signal by pid.
            unsafe {
                libc::kill(self.child.id() as libc::pid_t, sig);
            }
        }
    }

    impl Drop for LiveAudio {
        fn drop(&mut self) {
            // Resume a possibly-paused child so it can die promptly, then terminate + reap it;
            // closing its stdin ends the writer thread, which we join to avoid a stray thread.
            self.signal(libc::SIGCONT);
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(w) = self.writer.take() {
                let _ = w.join();
            }
        }
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

#[cfg(windows)]
mod imp {
    use super::Path;
    use windows::core::HSTRING;
    use windows::Foundation::{TimeSpan, Uri};
    use windows::Media::Core::MediaSource;
    use windows::Media::Playback::MediaPlayer;

    /// A WinRT `MediaPlayer` for a Live Photo's motion `.mov`, playing its audio track —
    /// the Windows analog of the macOS `AVAudioPlayer` path. Closed (`IClosable`) on
    /// drop, which also stops playback and releases the media pipeline.
    pub struct LiveAudio {
        player: MediaPlayer,
    }

    impl LiveAudio {
        /// Open the `.mov`'s audio and start playing from `start_secs` into the clip
        /// (`0.0` = the top, to line up with the motion's first frame; a non-zero offset
        /// keeps a mid-playback unmute in sync). `None` if the player can't be made.
        ///
        /// Never blocks this thread: the source opens async on Media Foundation's own
        /// threads, but the seek and `Play()` issued *before* `MediaOpened` stick — the
        /// session queues them and playback starts at the requested offset once the
        /// source is ready (verified empirically; no `MediaOpened` handler needed).
        pub fn play(path: &Path, start_secs: f64) -> Option<LiveAudio> {
            let uri = Uri::CreateUri(&HSTRING::from(file_uri(path)?)).ok()?;
            let source = MediaSource::CreateFromUri(&uri).ok()?;
            let player = MediaPlayer::new().ok()?;
            player.SetAutoPlay(false).ok()?;
            player.SetSource(&source).ok()?;
            let start = TimeSpan {
                Duration: (start_secs.max(0.0) * 1e7) as i64, // seconds → 100 ns ticks
            };
            player.PlaybackSession().ok()?.SetPosition(start).ok()?;
            player.Play().ok()?;
            Some(LiveAudio { player })
        }

        /// Pause (keeps the position) — mirrors pausing the motion.
        pub fn pause(&self) {
            let _ = self.player.Pause();
        }

        /// Resume from where it paused — mirrors resuming the motion.
        pub fn resume(&self) {
            let _ = self.player.Play();
        }
    }

    impl Drop for LiveAudio {
        fn drop(&mut self) {
            let _ = self.player.Pause();
            let _ = self.player.Close(); // IClosable — tears down the media pipeline
        }
    }

    /// `file:///` URI for a local path, percent-encoded byte-wise. `Uri::CreateUri`
    /// tolerates raw spaces (it encodes them itself) but *misparses* a literal `#`,
    /// `%`, or `?` in a filename as fragment/escape/query — and it leaves existing
    /// `%XX` escapes alone, so pre-encoding everything outside the unreserved set
    /// (plus the separators we mean: `/` and the drive `:`) is both safe and exact.
    fn file_uri(path: &Path) -> Option<String> {
        let s = path.to_str()?;
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        // UNC `\\server\share\…` → `file://server/share/…`; drive paths get the
        // empty-authority `file:///C:/…` form.
        let (prefix, rest) = match s.strip_prefix(r"\\") {
            Some(unc) => ("file://", unc),
            None => ("file:///", s),
        };
        let mut uri = String::with_capacity(prefix.len() + rest.len() * 3);
        uri.push_str(prefix);
        for b in rest.bytes() {
            match b {
                b'\\' => uri.push('/'),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => uri.push(b as char),
                b'-' | b'.' | b'_' | b'~' | b'/' | b':' => uri.push(b as char),
                _ => {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    uri.push('%');
                    uri.push(HEX[(b >> 4) as usize] as char);
                    uri.push(HEX[(b & 0xF) as usize] as char);
                }
            }
        }
        Some(uri)
    }

    #[cfg(test)]
    mod tests {
        use super::file_uri;
        use std::path::Path;

        #[test]
        fn drive_path_forward_slashed() {
            assert_eq!(
                file_uri(Path::new(r"D:\Photos\IMG_0001.mov")).unwrap(),
                "file:///D:/Photos/IMG_0001.mov"
            );
        }

        #[test]
        fn reserved_and_space_bytes_are_escaped() {
            assert_eq!(
                file_uri(Path::new(r"C:\a b\100% #1?.mov")).unwrap(),
                "file:///C:/a%20b/100%25%20%231%3F.mov"
            );
        }

        #[test]
        fn verbatim_prefix_stripped_and_unc_kept_as_authority() {
            assert_eq!(
                file_uri(Path::new(r"\\?\C:\x.mov")).unwrap(),
                "file:///C:/x.mov"
            );
            assert_eq!(
                file_uri(Path::new(r"\\nas\share\x.mov")).unwrap(),
                "file://nas/share/x.mov"
            );
        }

        #[test]
        fn non_ascii_is_utf8_percent_encoded() {
            assert_eq!(
                file_uri(Path::new(r"C:\Fotoğraf\été.mov")).unwrap(),
                "file:///C:/Foto%C4%9Fraf/%C3%A9t%C3%A9.mov"
            );
        }
    }
}
