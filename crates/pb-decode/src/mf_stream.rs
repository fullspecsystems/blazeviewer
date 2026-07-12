//! An in-RAM `IStream`/`IMFByteStream` over shared bytes — how archived videos reach
//! Media Foundation (task: archive video playback).
//!
//! Archive entries have no filesystem path (and must never get one: the no-trace
//! guarantee forbids extracting to disk), so the MF readers can't use
//! `MFCreateSourceReaderFromURL`. Instead the entry's decompressed bytes — already in
//! RAM — back a minimal read-only COM `IStream`, wrapped by the OS
//! (`MFCreateMFByteStreamOnStream`) into the `IMFByteStream` the source resolver
//! consumes. **Zero-copy by design:** the stream holds an `Arc` to the one buffer, so
//! the playback producer, each seek-reopen (a fresh reader per seek — spike E), the
//! poster/probe, and the WinRT audio player all read the same copy; each consumer gets
//! its *own* stream instance (independent seek position) over the shared bytes.
//!
//! A byte stream has no URL to sniff, so the resolver is told what the container is:
//! `MF_BYTESTREAM_ORIGIN_NAME` (extension-based handler lookup) and, when known,
//! `MF_BYTESTREAM_CONTENT_TYPE` are set from the entry name.

use std::sync::{Arc, Mutex};

use windows::core::{implement, HSTRING};
use windows::Win32::Foundation::{
    E_NOTIMPL, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, S_FALSE, S_OK,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFByteStream, MFCreateMFByteStreamOnStream, MF_BYTESTREAM_CONTENT_TYPE,
    MF_BYTESTREAM_ORIGIN_NAME,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};

use crate::video::{video_content_type, VideoInput};

/// A read-only, thread-safe COM stream over `Arc`-shared bytes. `Clone` (the COM
/// method) hands out a new instance over the same buffer with an independent
/// position, which is also why the position is its own lock rather than `&mut`.
#[implement(IStream)]
struct MemStream {
    data: Arc<Vec<u8>>,
    pos: Mutex<u64>,
}

impl MemStream {
    fn new(data: Arc<Vec<u8>>) -> Self {
        MemStream {
            data,
            pos: Mutex::new(0),
        }
    }

    fn pos(&self) -> u64 {
        *self.pos.lock().unwrap()
    }

    fn set_pos(&self, p: u64) {
        *self.pos.lock().unwrap() = p;
    }
}

impl ISequentialStream_Impl for MemStream_Impl {
    fn Read(
        &self,
        pv: *mut core::ffi::c_void,
        cb: u32,
        pcbread: *mut u32,
    ) -> windows::core::HRESULT {
        let len = self.data.len() as u64;
        // Hold the lock across the copy so two same-instance readers can't serve
        // overlapping ranges (MF serializes per stream, but stay correct regardless).
        let mut pos = self.pos.lock().unwrap();
        let start = (*pos).min(len);
        let n = (u64::from(cb)).min(len - start) as usize;
        if n > 0 && !pv.is_null() {
            // SAFETY: COM contract — `pv` points at a caller buffer of at least `cb`
            // bytes; `start + n` is clamped to the source length above.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data.as_ptr().add(start as usize),
                    pv as *mut u8,
                    n,
                );
            }
        }
        *pos = start + n as u64;
        if !pcbread.is_null() {
            // SAFETY: out-param the caller asked for.
            unsafe { *pcbread = n as u32 };
        }
        // Short read (EOF) is S_FALSE per the ISequentialStream contract.
        if n as u32 == cb {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Write(
        &self,
        _pv: *const core::ffi::c_void,
        _cb: u32,
        _pcbwritten: *mut u32,
    ) -> windows::core::HRESULT {
        STG_E_ACCESSDENIED // read-only by design (privacy: viewing never writes)
    }
}

impl IStream_Impl for MemStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let len = self.data.len() as i64;
        let base = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.pos() as i64,
            STREAM_SEEK_END => len,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let target = base.checked_add(dlibmove).ok_or(STG_E_INVALIDFUNCTION)?;
        if target < 0 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        // Seeking past the end is legal for streams; reads there return 0 bytes.
        self.set_pos(target as u64);
        if !plibnewposition.is_null() {
            // SAFETY: optional out-param per the IStream contract.
            unsafe { *plibnewposition = target as u64 };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into()) // MF never uses it; readers Read/Seek
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(()) // no-op on a read-only stream
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into()) // locking unsupported, per the docs' escape hatch
    }

    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        // SAFETY: caller-provided out-param; zero it, then fill what a stream has.
        // The size is what MF actually consumes (duration math, EOF detection).
        unsafe {
            *pstatstg = STATSTG::default();
            (*pstatstg).r#type = STGTY_STREAM.0 as u32;
            (*pstatstg).cbSize = self.data.len() as u64;
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        // Same shared buffer, fresh independent position (COM clone semantics say
        // "same seek pointer", but every in-tree consumer re-seeks; MF clones to
        // read elsewhere, so independence is the point).
        let s = MemStream::new(self.data.clone());
        s.set_pos(self.pos());
        Ok(s.into())
    }
}

/// A fresh COM `IStream` over `data` (position 0). Public: the shell's WinRT audio
/// player wraps it via `CreateRandomAccessStreamOverStream` to play the same bytes.
pub fn mem_istream(data: Arc<Vec<u8>>) -> IStream {
    MemStream::new(data).into()
}

/// A fresh `IMFByteStream` over an in-RAM container, tagged with the origin name +
/// content type so the source resolver picks the right container handler. One per
/// consumer — a used stream has a position and belongs to its media source.
pub(crate) unsafe fn mf_byte_stream(
    data: Arc<Vec<u8>>,
    name: &str,
) -> windows::core::Result<IMFByteStream> {
    let stream = mem_istream(data);
    let bs = MFCreateMFByteStreamOnStream(&stream)?;
    // Best-effort hints (the wrapper object implements IMFAttributes): the origin
    // name drives the extension-based handler lookup, the content type the direct
    // one. A failure here just means the resolver sniffs content instead.
    if let Ok(attrs) = windows::core::Interface::cast::<IMFAttributes>(&bs) {
        let _ = attrs.SetString(&MF_BYTESTREAM_ORIGIN_NAME, &HSTRING::from(name));
        if let Some(ct) = video_content_type(name) {
            let _ = attrs.SetString(&MF_BYTESTREAM_CONTENT_TYPE, &HSTRING::from(ct));
        }
    }
    Ok(bs)
}

/// A source-reader input for `input`: `Path` resolves by URL, `Bytes` through a
/// fresh tagged byte stream. Shared by every reader-opening site (poster/probe,
/// software producer, seek reopen, NVDEC) so path and archive playback stay
/// configuration-identical by construction.
pub(crate) enum ReaderSource {
    Url(HSTRING),
    Stream(IMFByteStream),
}

impl ReaderSource {
    pub(crate) unsafe fn new(input: &VideoInput) -> windows::core::Result<ReaderSource> {
        Ok(match input {
            VideoInput::Path(p) => ReaderSource::Url(HSTRING::from(p.as_os_str())),
            VideoInput::Bytes { data, name } => {
                ReaderSource::Stream(mf_byte_stream(data.clone(), name)?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_stream_reads_seeks_and_stats_like_a_file() {
        unsafe {
            let data = Arc::new((0u8..=99).collect::<Vec<u8>>());
            let s = mem_istream(data);
            // Stat reports the size.
            let mut st = STATSTG::default();
            s.Stat(&mut st, STATFLAG(0)).unwrap();
            assert_eq!(st.cbSize, 100);
            // Read from the front.
            let mut buf = [0u8; 10];
            let mut got = 0u32;
            s.Read(buf.as_mut_ptr() as *mut _, 10, Some(&mut got))
                .ok()
                .unwrap();
            assert_eq!((got, buf[0], buf[9]), (10, 0, 9));
            // Seek relative to end, read the tail (short read = S_FALSE, not an error).
            let mut newpos = 0u64;
            s.Seek(-5, STREAM_SEEK_END, Some(&mut newpos)).unwrap();
            assert_eq!(newpos, 95);
            let hr = s.Read(buf.as_mut_ptr() as *mut _, 10, Some(&mut got));
            assert_eq!(got, 5);
            assert_eq!(buf[..5], [95, 96, 97, 98, 99]);
            assert_eq!(hr, S_FALSE);
        }
    }

    #[test]
    fn mem_stream_is_read_only() {
        unsafe {
            let s = mem_istream(Arc::new(vec![1, 2, 3]));
            let mut wrote = 0u32;
            let hr = s.Write([9u8].as_ptr() as *const _, 1, Some(&mut wrote));
            assert!(
                hr.is_err(),
                "writes must be refused (privacy: RAM-only, read-only)"
            );
        }
    }

    #[test]
    fn byte_stream_wraps_and_reports_length() {
        crate::mf_video::ensure_mf();
        unsafe {
            let bs = mf_byte_stream(Arc::new(vec![0u8; 4096]), "clip.mp4").unwrap();
            assert_eq!(bs.GetLength().unwrap(), 4096);
        }
    }
}
