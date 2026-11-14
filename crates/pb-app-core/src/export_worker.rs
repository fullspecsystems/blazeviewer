//! The export **write worker** (export-plan.md § 3.3 / § 3.4, rung 0c).
//!
//! One low-priority thread, FIFO: read the source bytes → `pb_decode` export decode →
//! `pb_encode::prepare` → PNG → a safe write. Jobs survive navigation — the user asked for
//! this file — and never touch the decode pool, so display decodes always find a worker.
//! Results go back over a channel the core drains on its (low-rate) export wake.
//!
//! Safe writes (§ 3.4, reviews C2/F7/F26):
//! - the encoded file is written to a **unique, visible** temp sibling
//!   (`<stem>.blaze-export-<hex>.tmp`, created `create_new`) and `fsync`ed;
//! - then committed **no-clobber** when the target didn't exist (a hard link — atomic on
//!   APFS/NTFS/ext4 — or, where links aren't supported (exFAT, many SMB shares), a
//!   `create_new` placeholder we then rename over), or **replacing** when the save dialog
//!   already confirmed an overwrite;
//! - if the temp can't be created (a TCC-gated folder whose grant covers only the chosen
//!   file), the file is written directly — non-atomically — and removed on failure;
//! - the source file itself is never overwritten (file identity, not path text).
//!
//! Nothing ever scans for or deletes stray temps: that would be a passive write.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_encode::ExportNote;
use pb_source::ItemSource;

/// One file to write.
pub struct ExportJob {
    pub id: u64,
    pub source: Arc<dyn ItemSource>,
    pub item: usize,
    pub name: String,
    pub source_path: Option<PathBuf>,
    pub rotation_cw: u8,
    pub dest: PathBuf,
    pub queued_at: Instant,
}

/// Per-stage wall time, reported when `PB_PERF` is on (review C21: dialog time excluded —
/// the clock starts when the job is queued, i.e. at dialog confirm).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExportTimings {
    pub queue: Duration,
    pub read: Duration,
    pub decode: Duration,
    pub prepare: Duration,
    pub encode: Duration,
    pub write: Duration,
}

/// A finished job.
#[derive(Debug)]
pub struct ExportDone {
    pub id: u64,
    pub dest: PathBuf,
    pub result: Result<ExportOutcome, String>,
    pub timings: ExportTimings,
}

/// A successful export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportOutcome {
    pub notes: Vec<ExportNote>,
    pub bytes: u64,
}

/// Handle to the worker thread.
pub struct ExportWorker {
    jobs: Sender<ExportJob>,
    done: Receiver<ExportDone>,
}

impl ExportWorker {
    /// Spawn the worker (once, on the first export).
    pub fn new() -> ExportWorker {
        let (jobs, job_rx) = std::sync::mpsc::channel::<ExportJob>();
        let (done_tx, done) = std::sync::mpsc::channel::<ExportDone>();
        std::thread::Builder::new()
            .name("pb-export".into())
            .spawn(move || {
                lower_thread_priority();
                for job in job_rx {
                    let done = run_job(job);
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn the export worker");
        ExportWorker { jobs, done }
    }

    /// Queue a job. `false` if the worker is gone.
    pub fn submit(&self, job: ExportJob) -> bool {
        self.jobs.send(job).is_ok()
    }

    /// Finished jobs since the last drain.
    pub fn drain(&self) -> Vec<ExportDone> {
        self.done.try_iter().collect()
    }
}

impl Default for ExportWorker {
    fn default() -> Self {
        Self::new()
    }
}

/// Run on low OS priority so an export never competes with browsing (review F8).
fn lower_thread_priority() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        const QOS_CLASS_UTILITY: u32 = 0x11;
        // SAFETY: plain libSystem call on the current thread; failure just leaves the default.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
        }
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        // SAFETY: the pseudo-handle for the current thread is always valid.
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        extern "C" {
            fn nice(inc: i32) -> i32;
        }
        // SAFETY: on Linux (NPTL) nice values are per-thread; this lowers only this one.
        unsafe {
            nice(10);
        }
    }
}

/// Decode → prepare → encode → write one job. Never panics (the codec stages are wrapped).
pub fn run_job(job: ExportJob) -> ExportDone {
    let mut t = ExportTimings {
        queue: job.queued_at.elapsed(),
        ..ExportTimings::default()
    };
    let id = job.id;
    let dest = job.dest.clone();
    let result = run_stages(&job, &mut t);
    ExportDone {
        id,
        dest,
        result,
        timings: t,
    }
}

fn run_stages(job: &ExportJob, t: &mut ExportTimings) -> Result<ExportOutcome, String> {
    let s = Instant::now();
    let bytes = job
        .source
        .bytes(job.item)
        .map_err(|e| format!("couldn't read the image ({e})"))?;
    t.read = s.elapsed();

    let s = Instant::now();
    let src = pb_decode::decode_named_bytes_for_export(&job.name, &bytes).map_err(|e| match e {
        pb_decode::ExportDecodeError::Refused(why) => why.to_string(),
        pb_decode::ExportDecodeError::Decode(e) => format!("couldn't decode the image ({e})"),
    })?;
    drop(bytes);
    t.decode = s.elapsed();

    let s = Instant::now();
    // Rung 1: PNG at full resolution (PNG's caps are everything `prepare` produces).
    let spec = pb_encode::PrepareSpec::full(job.rotation_cw, true);
    let image = pb_encode::catch_panics(|| {
        pb_encode::prepare(src, &spec).map_err(|e| pb_encode::EncodeError::Codec(e.to_string()))
    })
    .map_err(|e| e.to_string())?;
    t.prepare = s.elapsed();

    let s = Instant::now();
    let png =
        pb_encode::catch_panics(|| pb_encode::encode_png(&image, pb_encode::PngEffort::Balanced))
            .map_err(|e| format!("couldn't encode the PNG ({e})"))?;
    t.encode = s.elapsed();

    let s = Instant::now();
    commit_file(&job.dest, &png, job.source_path.as_deref()).map_err(|e| e.to_string())?;
    t.write = s.elapsed();

    Ok(ExportOutcome {
        notes: image.notes,
        bytes: png.len() as u64,
    })
}

/// Why a write failed — `Display` is the user-facing reason.
#[derive(Debug)]
pub enum WriteError {
    SameAsSource,
    Collision,
    NoFolder,
    Io(std::io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::SameAsSource => {
                write!(f, "that's the original file — choose a different name")
            }
            WriteError::Collision => write!(f, "a file with that name appeared — try again"),
            WriteError::NoFolder => write!(f, "the destination folder doesn't exist"),
            WriteError::Io(e) => write!(f, "couldn't write the file ({e})"),
        }
    }
}

/// Whether `a` and `b` are the same file on disk — by device + inode on Unix (catches hard
/// links and differently-spelled paths, review F26), by canonical path elsewhere.
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x
                .to_string_lossy()
                .eq_ignore_ascii_case(&y.to_string_lossy()),
            _ => false,
        }
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique, visible temp name beside `dest` (no leading dot — a crash leftover stays
/// visible and recognizable, review F7).
fn temp_path(dest: &Path) -> PathBuf {
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tag = nanos ^ ((std::process::id() as u64) << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    dest.with_file_name(format!("{stem}.blaze-export-{tag:016x}.tmp"))
}

/// Write `data` to `dest` safely (see the module docs). `source` is the exported item's own
/// file — never overwritten.
pub fn commit_file(dest: &Path, data: &[u8], source: Option<&Path>) -> Result<(), WriteError> {
    if source.is_some_and(|s| same_file(s, dest)) {
        return Err(WriteError::SameAsSource);
    }
    let dir = dest
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.is_dir() {
        return Err(WriteError::NoFolder);
    }
    // The save dialog confirmed an overwrite iff the target existed when it closed; nothing
    // else can have created it since (pending destinations are refused before queuing).
    let replace = dest.exists();

    let temp = temp_path(dest);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp);
    let mut file = match file {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return write_direct(dest, data, replace);
        }
        Err(e) => return Err(WriteError::Io(e)),
    };
    let written = file.write_all(data).and_then(|_| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(WriteError::Io(e));
    }
    let committed = if replace {
        std::fs::rename(&temp, dest).map_err(WriteError::Io)
    } else {
        commit_no_clobber(&temp, dest)
    };
    if committed.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    committed
}

/// Commit without ever replacing an existing file: a hard link (atomic), else a
/// `create_new` placeholder we rename over (filesystems without links).
fn commit_no_clobber(temp: &Path, dest: &Path) -> Result<(), WriteError> {
    match std::fs::hard_link(temp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(temp);
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(WriteError::Collision)
        }
        Err(_) => {} // links unsupported here — fall through to the placeholder
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
    {
        Ok(placeholder) => drop(placeholder),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(WriteError::Collision)
        }
        Err(e) => return Err(WriteError::Io(e)),
    }
    std::fs::rename(temp, dest).map_err(|e| {
        let _ = std::fs::remove_file(dest); // our own empty placeholder
        WriteError::Io(e)
    })
}

/// No temp allowed beside the target: write it directly (not atomic), removing a file we
/// created if the write fails.
fn write_direct(dest: &Path, data: &[u8], replace: bool) -> Result<(), WriteError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if replace {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    let mut f = opts.open(dest).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => WriteError::Collision,
        _ => WriteError::Io(e),
    })?;
    let r = f.write_all(data).and_then(|_| f.sync_all());
    drop(f);
    r.map_err(|e| {
        if !replace {
            let _ = std::fs::remove_file(dest);
        }
        WriteError::Io(e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-export-write-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entries(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_new_file_is_written_and_no_temp_is_left() {
        let d = tmpdir("new");
        let dest = d.join("out.png");
        commit_file(&dest, b"hello", None).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert_eq!(entries(&d), vec!["out.png"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_confirmed_overwrite_replaces_the_file() {
        let d = tmpdir("replace");
        let dest = d.join("out.png");
        std::fs::write(&dest, b"old").unwrap();
        commit_file(&dest, b"new", None).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(entries(&d), vec!["out.png"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_source_file_is_never_overwritten_even_via_a_hard_link() {
        let d = tmpdir("source");
        let src = d.join("photo.png");
        std::fs::write(&src, b"original").unwrap();
        assert!(matches!(
            commit_file(&src, b"x", Some(&src)),
            Err(WriteError::SameAsSource)
        ));
        #[cfg(unix)]
        {
            let alias = d.join("alias.png");
            std::fs::hard_link(&src, &alias).unwrap();
            assert!(matches!(
                commit_file(&alias, b"x", Some(&src)),
                Err(WriteError::SameAsSource)
            ));
        }
        assert_eq!(std::fs::read(&src).unwrap(), b"original");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_clobber_commit_refuses_a_file_that_appeared() {
        let d = tmpdir("clobber");
        let temp = d.join("t.tmp");
        let dest = d.join("out.png");
        std::fs::write(&temp, b"new").unwrap();
        std::fs::write(&dest, b"someone else's").unwrap();
        assert!(matches!(
            commit_no_clobber(&temp, &dest),
            Err(WriteError::Collision)
        ));
        assert_eq!(std::fs::read(&dest).unwrap(), b"someone else's");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_folder_is_reported() {
        let d = tmpdir("nofolder");
        assert!(matches!(
            commit_file(&d.join("nope/out.png"), b"x", None),
            Err(WriteError::NoFolder)
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn temp_names_are_unique_and_visible() {
        let a = temp_path(Path::new("/o/beach.png"));
        let b = temp_path(Path::new("/o/beach.png"));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("beach.blaze-export-") && name.ends_with(".tmp"),
            "{name}"
        );
        assert!(!name.starts_with('.'));
    }
}
