//! The one lock (ADR-016 §6).
//!
//! Two layers, both held across "journal → steps → journal delete":
//!
//! - a process-wide mutex, so threads of one process serialise as one
//!   writer;
//! - an exclusive `flock` on `<library>/.dimagine/write.lock`, so all
//!   dimagine writers (serve, CLI, importer) serialise across processes.
//!
//! There are deliberately no per-path locks: a multi-file operation
//! taking them would deadlock against itself. The lock does not constrain
//! people, Obsidian or agents — protection against their edits is the
//! compare-and-swap, not this lock.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{io_err, Result};
use crate::fs;

/// The single process-wide write mutex. Every [`Writer`](crate::Writer)
/// in the process shares it, however many library roots are open.
static WRITE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn write_mutex() -> &'static Mutex<()> {
    WRITE_MUTEX.get_or_init(|| Mutex::new(()))
}

/// Where the cross-process lock lives, relative to the library root.
pub(crate) const LOCK_FILE: &str = ".dimagine/write.lock";

/// A held write lock. Dropping it releases the `flock` (closing the
/// lock file's descriptor) and then the process mutex.
///
/// Obtained from [`Writer::lock_library`](crate::Writer::lock_library): a
/// second dimagine writer that does not go through this crate's primitives —
/// the Eagle importer, ADR-016 §1 — holds this across the whole of its write.
pub struct WriteLock {
    _mutex: MutexGuard<'static, ()>,
    _lock_file: File,
}

impl WriteLock {
    /// Take both layers, blocking until every other dimagine writer in
    /// this process and on this library has finished its operation.
    /// `root` is the already-open library root; the lock file is created
    /// on first use. A symlink at the lock file's name is refused.
    pub(crate) fn acquire(root: &File, root_path: &Path) -> Result<Self> {
        // Poison recovery: a panic inside a critical section leaves no
        // inconsistent on-disk state (the journal carries the outcome),
        // so the mutex is always taken back.
        let mutex = write_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let dimagine_dir = fs::resolve_dir(root, &[".dimagine".to_string()], true)?;
        let lock_file = fs::open_leaf(
            &dimagine_dir,
            "write.lock",
            libc::O_RDWR | libc::O_CREAT,
            0o644,
        )?;

        // A signal can interrupt the wait; `flock` is then retried.
        loop {
            let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
            if rc == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(io_err("flock write lock", root_path.join(LOCK_FILE), e));
            }
        }
        Ok(Self {
            _mutex: mutex,
            _lock_file: lock_file,
        })
    }

    /// Try to take both layers without blocking.
    /// Returns `Ok(None)` if either the process mutex or the flock is held.
    #[allow(dead_code)]
    pub(crate) fn try_acquire(root: &File, root_path: &Path) -> Result<Option<Self>> {
        let Ok(mutex) = write_mutex().try_lock() else {
            return Ok(None);
        };
        let dimagine_dir = fs::resolve_dir(root, &[".dimagine".to_string()], true)?;
        let lock_file = fs::open_leaf(
            &dimagine_dir,
            "write.lock",
            libc::O_RDWR | libc::O_CREAT,
            0o644,
        )?;

        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) || e.raw_os_error() == Some(libc::EAGAIN)
            {
                return Ok(None);
            }
            return Err(io_err(
                "flock write lock non-blocking",
                root_path.join(LOCK_FILE),
                e,
            ));
        }
        Ok(Some(Self {
            _mutex: mutex,
            _lock_file: lock_file,
        }))
    }
}

/// Run `f` holding the one lock for the library rooted at `root`.
#[allow(dead_code)]
pub(crate) fn with_write_lock<R>(
    root: &File,
    root_path: &Path,
    f: impl FnOnce() -> Result<R>,
) -> Result<R> {
    let _lock = WriteLock::acquire(root, root_path)?;
    f()
}
