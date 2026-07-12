//! Opening a [`VideoInput`] for FFmpeg — the custom-AVIO + cancellation seam
//! (plan §6, the phase-0 proof gate).
//!
//! Two jobs, both about *control* rather than decoding:
//!
//! 1. **In-RAM archive bytes.** `VideoInput::Bytes` (a ZIP/7z entry, RAM-only per
//!    the privacy guarantee) is served to FFmpeg through a custom `AVIOContext`
//!    whose read/seek callbacks walk an `Arc<Vec<u8>>` cursor — bounds-checked,
//!    panic-free (no Rust panic may cross a C callback), no temp files ever.
//!
//! 2. **Cancellation that reaches *inside* blocking work.** An FFmpeg
//!    `AVIOInterruptCB` is installed on every open (path and bytes alike): it
//!    aborts blocking probe/demux/decode when the caller's cancel flag flips
//!    (the decode pool's poster cancellation), and enforces a per-operation
//!    **watchdog deadline** so corrupt/hostile input can't pin a worker forever
//!    — "we only block on `recv()`" is true only *between* libav calls; this is
//!    the abort lever inside them.
//!
//! Teardown order is load-bearing (proved by the drop/reopen tests): the format
//! context closes first (`avformat_close_input` via the wrapped `Input`'s Drop),
//! then the AVIO buffer + context are freed (close does NOT free a custom `pb` —
//! `AVFMT_FLAG_CUSTOM_IO` is set automatically when we install one), then the
//! cursor and interrupt state boxes.

use std::ffi::{c_int, c_void, CString};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ffmpeg_next as ff;
use ffmpeg_next::ffi;

use crate::video::VideoInput;

/// Size of the AVIO read buffer for in-RAM inputs. FFmpeg's conventional default;
/// the demuxer may grow/replace it (which is why Drop frees `(*avio).buffer`, not
/// our original allocation).
const AVIO_BUF_SIZE: usize = 32 * 1024;

/// Watchdog ceiling for the open + stream-info probe. Local files and RAM bytes
/// resolve in milliseconds; only a hostile/corrupt container hits this.
const OPEN_DEADLINE: Duration = Duration::from_secs(20);

/// Shared state the `AVIOInterruptCB` reads. Heap-pinned for the life of the
/// [`FfInput`]; the callback runs on the calling thread *inside* libav calls.
struct InterruptState {
    /// Borrowed cancel flag (the decode pool's), null when the caller has none.
    /// Validity for the `FfInput`'s lifetime is guaranteed by the `'a` borrow
    /// on [`FfInput`] itself.
    cancel: *const AtomicBool,
    /// Watchdog epoch.
    t0: Instant,
    /// Abort once `t0.elapsed()` exceeds this many milliseconds. `u64::MAX` = no
    /// deadline armed.
    deadline_ms: AtomicU64,
}

/// The `AVIOInterruptCB` trampoline: return nonzero to make the current blocking
/// libav operation fail with `AVERROR_EXIT`. Must never panic (it's called from C):
/// atomic loads + a monotonic-clock read only.
extern "C" fn interrupt_cb(opaque: *mut c_void) -> c_int {
    let st = unsafe { &*(opaque as *const InterruptState) };
    if !st.cancel.is_null() && unsafe { &*st.cancel }.load(Ordering::Relaxed) {
        return 1;
    }
    let deadline = st.deadline_ms.load(Ordering::Relaxed);
    if deadline != u64::MAX && st.t0.elapsed().as_millis() as u64 > deadline {
        return 1;
    }
    0
}

/// The in-RAM read cursor behind the custom AVIO callbacks. Owns (a refcount on)
/// the archive entry's bytes, so the archive can close while playback continues.
struct MemCursor {
    data: std::sync::Arc<Vec<u8>>,
    pos: usize,
}

/// AVIO read callback: copy up to `len` bytes at the cursor. Bounds-checked;
/// `AVERROR_EOF` at the end (FFmpeg 7+ requires the error code, not 0).
unsafe extern "C" fn mem_read(opaque: *mut c_void, buf: *mut u8, len: c_int) -> c_int {
    let c = &mut *(opaque as *mut MemCursor);
    if buf.is_null() || len <= 0 {
        return ffi::AVERROR(ffi::EINVAL);
    }
    let remaining = c.data.len().saturating_sub(c.pos);
    if remaining == 0 {
        return ffi::AVERROR_EOF;
    }
    let n = remaining.min(len as usize);
    std::ptr::copy_nonoverlapping(c.data.as_ptr().add(c.pos), buf, n);
    c.pos += n;
    n as c_int
}

/// AVIO seek callback: SEEK_SET/CUR/END plus `AVSEEK_SIZE` (report length).
/// Rejects out-of-range/overflowing targets with an error code, never a panic.
unsafe extern "C" fn mem_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let c = &mut *(opaque as *mut MemCursor);
    let len = c.data.len() as i64;
    let mode = whence & !(ffi::AVSEEK_FORCE as c_int);
    if mode == ffi::AVSEEK_SIZE as c_int {
        return len;
    }
    let base = match mode {
        0 => 0,            // SEEK_SET
        1 => c.pos as i64, // SEEK_CUR
        2 => len,          // SEEK_END
        _ => return ffi::AVERROR(ffi::EINVAL) as i64,
    };
    let target = match base.checked_add(offset) {
        Some(t) if (0..=len).contains(&t) => t,
        _ => return ffi::AVERROR(ffi::EINVAL) as i64,
    };
    c.pos = target as usize;
    target
}

/// An opened FFmpeg input over a [`VideoInput`], owning every C-side resource in
/// the right teardown order. `'a` is the borrowed cancel flag's lifetime.
pub struct FfInput<'a> {
    /// `Some` for the whole life of the value; `Option` only so Drop can close
    /// the format context *before* freeing the AVIO pieces below.
    ctx: Option<ff::format::context::Input>,
    avio: *mut ffi::AVIOContext,
    cursor: *mut MemCursor,
    intr: *mut InterruptState,
    _cancel: PhantomData<&'a AtomicBool>,
}

impl<'a> FfInput<'a> {
    /// Open `input` for demuxing, with the interrupt callback armed: `cancel`
    /// (when given) aborts any blocking libav work, and the open itself runs
    /// under a watchdog deadline. Bytes inputs are served entirely from RAM.
    pub fn open(input: &VideoInput, cancel: Option<&'a AtomicBool>) -> Result<Self, String> {
        super::init::ff_init();
        let intr = Box::into_raw(Box::new(InterruptState {
            cancel: cancel.map_or(std::ptr::null(), |c| c as *const AtomicBool),
            t0: Instant::now(),
            deadline_ms: AtomicU64::new(u64::MAX),
        }));
        let result = unsafe { Self::open_raw(input, intr) };
        match result {
            Ok(mut me) => {
                me.set_op_deadline(Some(OPEN_DEADLINE));
                let ok = unsafe {
                    ffi::avformat_find_stream_info(me.ctx_ptr(), std::ptr::null_mut()) >= 0
                };
                me.set_op_deadline(None);
                if ok {
                    Ok(me)
                } else {
                    Err(format!(
                        "unreadable video container: {}",
                        input.display_name()
                    ))
                    // `me` drops here, tearing down in order.
                }
            }
            Err(e) => {
                // Nothing else was allocated (open_raw cleaned up); free the state.
                unsafe { drop(Box::from_raw(intr)) };
                Err(e)
            }
        }
    }

    /// Allocate + open the format context (no stream-info probe yet). On error,
    /// every C allocation made here is already freed.
    unsafe fn open_raw(input: &VideoInput, intr: *mut InterruptState) -> Result<Self, String> {
        let ps = ffi::avformat_alloc_context();
        if ps.is_null() {
            return Err("out of memory (AVFormatContext)".into());
        }
        (*ps).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(interrupt_cb),
            opaque: intr as *mut c_void,
        };
        // Arm the open watchdog by hand (the FfInput doesn't exist yet).
        (*intr).deadline_ms.store(
            (*intr).t0.elapsed().as_millis() as u64 + OPEN_DEADLINE.as_millis() as u64,
            Ordering::Relaxed,
        );

        let (mut avio, mut cursor): (*mut ffi::AVIOContext, *mut MemCursor) =
            (std::ptr::null_mut(), std::ptr::null_mut());
        let url = match input {
            VideoInput::Path(p) => path_cstring(p),
            VideoInput::Bytes { data, name } => {
                cursor = Box::into_raw(Box::new(MemCursor {
                    data: data.clone(),
                    pos: 0,
                }));
                let buf = ffi::av_malloc(AVIO_BUF_SIZE) as *mut u8;
                if buf.is_null() {
                    drop(Box::from_raw(cursor));
                    ffi::avformat_free_context(ps);
                    return Err("out of memory (AVIO buffer)".into());
                }
                avio = ffi::avio_alloc_context(
                    buf,
                    AVIO_BUF_SIZE as c_int,
                    0,
                    cursor as *mut c_void,
                    Some(mem_read),
                    None,
                    Some(mem_seek),
                );
                if avio.is_null() {
                    ffi::av_free(buf as *mut c_void);
                    drop(Box::from_raw(cursor));
                    ffi::avformat_free_context(ps);
                    return Err("out of memory (AVIOContext)".into());
                }
                // Installing a custom pb makes avformat_open_input set
                // AVFMT_FLAG_CUSTOM_IO, so close_input won't touch it — Drop does.
                (*ps).pb = avio;
                // The entry NAME rides as the URL: FFmpeg uses its extension as a
                // format-probe hint (byte streams have nothing else to sniff).
                name_cstring(name)
            }
        };

        let mut ps_mut = ps;
        let rc = ffi::avformat_open_input(
            &mut ps_mut,
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if rc < 0 {
            // On failure avformat_open_input has already freed the context;
            // the custom AVIO pieces are ours to free.
            if !avio.is_null() {
                ffi::av_freep(&mut (*avio).buffer as *mut _ as *mut c_void);
                ffi::avio_context_free(&mut avio);
            }
            if !cursor.is_null() {
                drop(Box::from_raw(cursor));
            }
            return Err(format!(
                "couldn't open the video ({}): {}",
                input.display_name(),
                ff::Error::from(rc)
            ));
        }
        Ok(FfInput {
            ctx: Some(ff::format::context::Input::wrap(ps_mut)),
            avio,
            cursor,
            intr,
            _cancel: PhantomData,
        })
    }

    /// The wrapped demuxer context.
    pub fn ctx(&mut self) -> &mut ff::format::context::Input {
        self.ctx.as_mut().expect("live until Drop")
    }

    fn ctx_ptr(&mut self) -> *mut ffi::AVFormatContext {
        unsafe { self.ctx().as_mut_ptr() }
    }

    /// Arm (or clear, with `None`) the watchdog for the next blocking libav
    /// operation: the interrupt callback aborts once the budget elapses. Callers
    /// wrap each read/decode/seek burst so hostile input can't pin the thread.
    pub fn set_op_deadline(&self, budget: Option<Duration>) {
        let st = unsafe { &*self.intr };
        let v = match budget {
            Some(d) => st.t0.elapsed().as_millis() as u64 + d.as_millis() as u64,
            None => u64::MAX,
        };
        st.deadline_ms.store(v, Ordering::Relaxed);
    }

    /// Whether the caller's cancel flag is set (poster walk early-outs on it
    /// between frames, without waiting for the next blocking call to abort).
    pub fn cancelled(&self) -> bool {
        let st = unsafe { &*self.intr };
        !st.cancel.is_null() && unsafe { &*st.cancel }.load(Ordering::Relaxed)
    }
}

impl Drop for FfInput<'_> {
    fn drop(&mut self) {
        // 1. Close the demuxer (flushes/frees everything EXCEPT a custom pb).
        self.ctx.take();
        unsafe {
            // 2. Free the custom AVIO buffer (possibly reallocated by FFmpeg —
            //    free what the context points at now) and the context itself.
            if !self.avio.is_null() {
                ffi::av_freep(&mut (*self.avio).buffer as *mut _ as *mut c_void);
                ffi::avio_context_free(&mut self.avio);
            }
            // 3. The cursor (drops its Arc refcount) and interrupt state.
            if !self.cursor.is_null() {
                drop(Box::from_raw(self.cursor));
            }
            drop(Box::from_raw(self.intr));
        }
    }
}

/// A filesystem path as the C string FFmpeg opens. Non-UTF-8 paths pass through
/// byte-preserving on Unix (FFmpeg opens raw bytes); interior NULs (impossible in
/// real paths) fall back to an empty URL, which opens nothing and errors cleanly.
fn path_cstring(p: &std::path::Path) -> CString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        CString::new(p.to_string_lossy().as_bytes().to_vec()).unwrap_or_default()
    }
}

/// An archive entry name as the probe-hint URL (extension is what matters).
/// Interior NULs are stripped rather than erroring — the name is only a hint.
fn name_cstring(name: &str) -> CString {
    CString::new(name.replace('\0', "")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture_bytes() -> Arc<Vec<u8>> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/black_then_color.mp4");
        Arc::new(std::fs::read(p).expect("fixture"))
    }

    /// The custom AVIO path end-to-end: open in-RAM bytes, see real streams,
    /// drop cleanly. Repeated open/drop exercises the teardown-order contract
    /// (buffer/context/cursor frees) — a double-free or leak crashes under the
    /// allocator long before miri would.
    #[test]
    fn bytes_open_probes_streams_and_drops_cleanly_repeatedly() {
        let data = fixture_bytes();
        for _ in 0..8 {
            let mut input = FfInput::open(
                &VideoInput::Bytes {
                    data: data.clone(),
                    name: "clip.mp4".into(),
                },
                None,
            )
            .expect("bytes open");
            let n = input.ctx().streams().count();
            assert!(n >= 1, "expected at least the video stream, got {n}");
        }
        // The bytes are still alive and untouched (Arc refcount back to ours).
        assert_eq!(Arc::strong_count(&data), 1);
    }

    #[test]
    fn path_open_works_and_matches_bytes_open() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/black_then_color.mp4");
        let mut by_path = FfInput::open(&VideoInput::Path(p), None).expect("path open");
        let data = fixture_bytes();
        let mut by_bytes = FfInput::open(
            &VideoInput::Bytes {
                data,
                name: "clip.mp4".into(),
            },
            None,
        )
        .expect("bytes open");
        assert_eq!(
            by_path.ctx().streams().count(),
            by_bytes.ctx().streams().count()
        );
    }

    /// Garbage bytes must fail with an error — not hang (the watchdog covers the
    /// probe), not crash, and not leak the AVIO pieces (the error path frees them).
    #[test]
    fn garbage_bytes_fail_cleanly() {
        let data = Arc::new(vec![0x55u8; 4096]);
        let r = FfInput::open(
            &VideoInput::Bytes {
                data: data.clone(),
                name: "junk.mp4".into(),
            },
            None,
        );
        assert!(r.is_err(), "garbage must not open");
        assert_eq!(Arc::strong_count(&data), 1, "error path released the bytes");
    }

    /// The cancellation contract is layered: the interrupt callback aborts
    /// *blocking* libav work (FFmpeg only consults it where it can actually
    /// wait — a tiny in-RAM open completes without ever asking), and the
    /// [`FfInput::cancelled`] accessor is what the fast paths (the poster's
    /// per-frame walk) check explicitly. This locks the accessor half; the
    /// poster's `cancel_aborts_the_poster` test proves the end-to-end abort.
    #[test]
    fn cancel_flag_is_visible_through_the_opened_input() {
        let cancel = AtomicBool::new(false);
        let input = FfInput::open(
            &VideoInput::Bytes {
                data: fixture_bytes(),
                name: "clip.mp4".into(),
            },
            Some(&cancel),
        )
        .expect("open");
        assert!(!input.cancelled());
        cancel.store(true, Ordering::Relaxed);
        assert!(input.cancelled(), "the live flag reads through");
        // And the interrupt callback agrees (this is what libav polls inside
        // blocking work).
        assert_eq!(interrupt_cb(input.intr as *mut c_void), 1);
    }

    /// A truncated container opens-or-fails but never hangs: the stream-info
    /// probe runs under the watchdog deadline.
    #[test]
    fn truncated_bytes_never_hang() {
        let full = fixture_bytes();
        let cut = Arc::new(full[..full.len() / 3].to_vec());
        let t0 = Instant::now();
        let _ = FfInput::open(
            &VideoInput::Bytes {
                data: cut,
                name: "cut.mp4".into(),
            },
            None,
        );
        assert!(t0.elapsed() < OPEN_DEADLINE + Duration::from_secs(5));
    }
}
