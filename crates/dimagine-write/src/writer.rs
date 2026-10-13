//! The library handle and durable write primitives (ADR-016 §3, §4).

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};

use crate::error::{conflict_err, Error, Exists, PathReject, Result};
use crate::fingerprint::Fingerprint;
use crate::fs::{self, Renamed};
use crate::lock::WriteLock;
use crate::paths::{self, RelPath, StepPath};

/// An RAII guard to clean up a dot-temp file if an operation fails or panics.
pub(crate) struct TempFileGuard<'a> {
    dir: &'a File,
    name: String,
    active: bool,
}

/// The settings file's directory and name, under the
/// library root (ADR-019 §1). The schema itself lives
/// in dimagine-core; this door only reads and writes
/// the bytes.
pub const SETTINGS_DIR: &str = ".dimagine";
pub const SETTINGS_FILE: &str = "settings.json";

impl<'a> TempFileGuard<'a> {
    pub(crate) fn new(dir: &'a File, name: String) -> Self {
        Self {
            dir,
            name,
            active: true,
        }
    }

    pub(crate) fn defuse(&mut self) {
        self.active = false;
    }
}

impl<'a> Drop for TempFileGuard<'a> {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::unlink(self.dir, &self.name);
        }
    }
}

/// A handle to an open dimagine library.
///
/// Holds the library root descriptor open. All write operations go through
/// this handle, validating paths and ensuring durability.
pub struct Writer {
    pub(crate) root: File,
    pub(crate) root_path: PathBuf,
}

/// What a startup sweep did and found.
///
/// ADR-016 §7: a journal that refuses recovery is *reported*, never silently
/// skipped, so `open_with_report` exists for the CLI and serve layers to show
/// this to the user.
#[derive(Clone, Debug, Default)]
pub struct SweepReport {
    /// Leftover dot-temp files this crate recognises and removed.
    pub temp_files_removed: usize,
    /// Per-operation outcome of journal recovery.
    pub recovery: Vec<crate::journal::RecoveryOutcome>,
}

impl Writer {
    /// Open a library root without running startup sweep or recovery.
    pub fn open_raw(root_path: impl AsRef<Path>) -> Result<Self> {
        let path = root_path.as_ref();
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let root = fs::open_root(&canonical_path)?;
        Ok(Self {
            root,
            root_path: canonical_path,
        })
    }

    /// Open a library root, sweeping leftover temporary files and running
    /// journal recovery under the write lock.
    pub fn open(root_path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::open_with_report(root_path)?.0)
    }

    /// As [`Writer::open`], returning what the sweep found so refusals and
    /// third-state reports reach the user.
    pub fn open_with_report(root_path: impl AsRef<Path>) -> Result<(Self, SweepReport)> {
        let writer = Self::open_raw(root_path)?;
        let report = writer.startup_sweep()?;
        Ok((writer, report))
    }

    /// The filesystem path of the library root.
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// The root directory file descriptor.
    pub(crate) fn root_file(&self) -> &File {
        &self.root
    }

    /// Take the one write lock for this library and hold it, without
    /// performing a write.
    ///
    /// For a dimagine writer that cannot express its work as a
    /// [`Writer`] primitive — the Eagle importer (ADR-016 §1) — the contract is
    /// identical: hold the guard across "journal → steps → journal delete", so
    /// every writer in the process and on the library serialises on it.
    ///
    /// The guard is not reentrant: the crate's own primitives take the lock
    /// themselves, so holding this while calling one deadlocks.
    pub fn lock_library(&self) -> Result<WriteLock> {
        WriteLock::acquire(&self.root, &self.root_path)
    }

    /// Read a regular file under the library, computing its fingerprint.
    /// Symlinks are refused.
    pub fn read(&self, path: &RelPath) -> Result<(Vec<u8>, Fingerprint)> {
        let dir = fs::resolve_dir(&self.root, path.parent_components(), false)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;
        let (bytes, meta) = fs::read_leaf(&dir, path.name())?;
        let fp = Fingerprint::of(&bytes, &meta);
        Ok((bytes, fp))
    }

    /// Read a regular file the way a journal step names it: under
    /// the library or inside `.trash/` — the only two doors a
    /// step may touch (ADR-016 §0, ADR-017 §7). Symlinks are
    /// refused.
    pub fn read_step(&self, path: &StepPath) -> Result<(Vec<u8>, Fingerprint)> {
        let dir = fs::resolve_dir(&self.root, path.parent_components(), false)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;
        let (bytes, meta) = fs::read_leaf(&dir, path.name())?;
        let fp = Fingerprint::of(&bytes, &meta);
        Ok((bytes, fp))
    }

    /// Update an existing note (.md or .canvas) with compare-and-swap durability.
    ///
    /// Never touches images or raw metadata sidecars (enforced by `path.note_kind()`).
    /// Writes to a dot-prefixed temporary file in the same directory, copies mode bits,
    /// fsyncs the file, re-checks the fingerprint under the lock, renames over the original,
    /// and fsyncs the directory.
    pub fn update_existing(
        &self,
        path: &RelPath,
        expected: &Fingerprint,
        new_bytes: &[u8],
    ) -> Result<Fingerprint> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.update_existing_locked(&lock, &StepPath::Library(path.clone()), expected, new_bytes)
    }

    pub(crate) fn update_existing_locked(
        &self,
        _lock: &WriteLock,
        path: &StepPath,
        expected: &Fingerprint,
        new_bytes: &[u8],
    ) -> Result<Fingerprint> {
        if path.note_kind().is_none() {
            return Err(Error::BadPath {
                path: path.text(),
                reject: PathReject::NotANote,
            });
        }
        let dir = fs::resolve_dir(&self.root, path.parent_components(), false)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;

        // 1. Initial stat and check
        let (cur_bytes, cur_meta) = fs::read_leaf(&dir, path.name())?;
        let cur_fp = Fingerprint::of(&cur_bytes, &cur_meta);
        if !expected.matches(&cur_fp) {
            return Err(conflict_err(crate::error::Conflict {
                path: path.to_path(),
                expected: expected.clone(),
                current: cur_fp,
                current_bytes: cur_bytes,
            }));
        }

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            cur_meta.mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = 0o644;

        let nonce = ulid::Ulid::new().to_string();
        let temp_name = fs::temp_name(&nonce);

        let temp_file = fs::open_leaf(
            &dir,
            &temp_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            mode as libc::mode_t,
        )?;

        let mut temp_guard = TempFileGuard::new(&dir, temp_name.clone());

        fs::fchmod(&temp_file, mode)?;

        let mut temp_writer = temp_file;
        temp_writer
            .write_all(new_bytes)
            .map_err(|e| fs::io_failed("write temp file", &temp_name, e))?;
        fs::fsync_file(&temp_writer)?;

        // 2. Re-check fingerprint under the lock before rename
        let (verify_bytes, verify_meta) = fs::read_leaf(&dir, path.name())?;
        let verify_fp = Fingerprint::of(&verify_bytes, &verify_meta);
        if !expected.matches(&verify_fp) {
            return Err(conflict_err(crate::error::Conflict {
                path: path.to_path(),
                expected: expected.clone(),
                current: verify_fp,
                current_bytes: verify_bytes,
            }));
        }

        fs::rename_replace(&dir, &temp_name, path.name())?;
        temp_guard.defuse();

        fs::fsync_dir(&dir)?;

        let (final_bytes, final_meta) = fs::read_leaf(&dir, path.name())?;
        Ok(Fingerprint::of(&final_bytes, &final_meta))
    }

    /// Create a new file exclusively, failing with `Exists` if the target exists.
    pub fn create_exclusive(&self, path: &RelPath, bytes: &[u8]) -> Result<Fingerprint> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.create_step_exclusive_locked(&lock, &StepPath::Library(path.clone()), bytes)
    }

    pub(crate) fn create_step_exclusive_locked(
        &self,
        _lock: &WriteLock,
        path: &StepPath,
        bytes: &[u8],
    ) -> Result<Fingerprint> {
        let dir = fs::resolve_dir(&self.root, path.parent_components(), true)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;

        let nonce = ulid::Ulid::new().to_string();
        let temp_name = fs::temp_name(&nonce);

        let temp_file = fs::open_leaf(
            &dir,
            &temp_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o644,
        )?;

        let mut temp_guard = TempFileGuard::new(&dir, temp_name.clone());

        let mut temp_writer = temp_file;
        temp_writer
            .write_all(bytes)
            .map_err(|e| fs::io_failed("write temp file", &temp_name, e))?;
        fs::fsync_file(&temp_writer)?;

        let renamed = fs::rename_noreplace(&dir, &temp_name, path.name());
        match renamed {
            Ok(Renamed::Done) => {
                temp_guard.defuse();
                fs::fsync_dir(&dir)?;
                let (final_bytes, final_meta) = fs::read_leaf(&dir, path.name())?;
                Ok(Fingerprint::of(&final_bytes, &final_meta))
            }
            Ok(Renamed::Occupied) => Err(Error::Exists(Exists {
                path: path.to_path(),
            })),
            Ok(Renamed::Unsupported) => {
                // Fallback to direct O_EXCL create (this platform has no
                // noreplace primitive here). The guard removes the partial
                // file if the write fails midway (ENOSPC), so no temp
                // survives the error and the next create gets a clean retry.
                let direct_file = fs::open_leaf(
                    &dir,
                    path.name(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    0o644,
                )?;
                let mut direct_guard = TempFileGuard::new(&dir, path.name().to_string());
                let mut direct_writer = direct_file;
                direct_writer
                    .write_all(bytes)
                    .map_err(|e| fs::io_failed("write file", path.name(), e))?;
                fs::fsync_file(&direct_writer)?;
                direct_guard.defuse();
                fs::fsync_dir(&dir)?;
                let (final_bytes, final_meta) = fs::read_leaf(&dir, path.name())?;
                Ok(Fingerprint::of(&final_bytes, &final_meta))
            }
            Err(Error::Exists(_)) => Err(Error::Exists(Exists {
                path: path.to_path(),
            })),
            Err(e) => Err(e),
        }
    }

    /// Move a file, refusing to replace an existing target.
    pub fn move_noreplace(&self, from: &RelPath, to: &RelPath) -> Result<()> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.move_step_noreplace_locked(
            &lock,
            &StepPath::Library(from.clone()),
            &StepPath::Library(to.clone()),
        )
    }

    pub(crate) fn move_step_noreplace_locked(
        &self,
        _lock: &WriteLock,
        from: &StepPath,
        to: &StepPath,
    ) -> Result<()> {
        let from_dir = fs::resolve_dir(&self.root, from.parent_components(), false)?;
        fs::under_root(&from_dir, &self.root_path, &from.text())?;

        let to_dir = fs::resolve_dir(&self.root, to.parent_components(), true)?;
        fs::under_root(&to_dir, &self.root_path, &to.text())?;

        let renamed = fs::rename_between(&from_dir, from.name(), &to_dir, to.name(), true);
        match renamed {
            Ok(Renamed::Done) => {
                fs::fsync_dir(&from_dir)?;
                fs::fsync_dir(&to_dir)?;
                Ok(())
            }
            Ok(Renamed::Occupied) => Err(Error::Exists(Exists { path: to.to_path() })),
            Ok(Renamed::Unsupported) => Err(Error::Io {
                context: "rename noreplace unsupported".to_string(),
                path: to.to_path(),
                source: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "atomic noreplace across directories unsupported",
                ),
            }),
            Err(Error::Exists(_)) => Err(Error::Exists(Exists { path: to.to_path() })),
            Err(e) => Err(e),
        }
    }

    /// Permanently unlink a file inside `.trash/` — the only
    /// place this crate hard-deletes (ADR-016 §0, ADR-017 §7):
    /// library content is never unlinked, only moved to the
    /// trash. A journal's `delete` step applies through here.
    pub(crate) fn unlink_step_locked(&self, _lock: &WriteLock, path: &StepPath) -> Result<()> {
        let StepPath::Trash(_) = path else {
            return Err(Error::BadPath {
                path: path.text(),
                reject: PathReject::NotInTrash,
            });
        };
        let dir = fs::resolve_dir(&self.root, path.parent_components(), false)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;
        fs::unlink(&dir, path.name())?;
        fs::fsync_dir(&dir)?;
        Ok(())
    }

    /// Fold keys of every name anywhere in the library (ADR-017 §4).
    ///
    /// The bare-name rule — a name colliding with any file in the library
    /// (NFC + case-insensitive) gets ` 2` — is checked against leaf names
    /// library-wide, so a new file never makes an existing bare link
    /// ambiguous. `.dimagine/` holds write state, not library content, and is
    /// skipped; everything else (including `.trash/` groups) counts.
    /// Symlinked entries count by name but are not descended into.
    ///
    /// Cost: one walk per call. Correctness first — W63 (uploads, new
    /// collections) may serve this from the index instead, but the rule
    /// itself stays in this one helper.
    pub(crate) fn library_fold_set(&self) -> Result<std::collections::HashSet<String>> {
        let mut set = std::collections::HashSet::new();
        let mut stack = vec![(fs::dup_dir(&self.root), true)];
        while let Some((dir, is_top)) = stack.pop() {
            for name in fs::list_names(&dir)? {
                if is_top && name == ".dimagine" {
                    continue;
                }
                set.insert(paths::fold_key(&name));
                let Ok(c) = fs::cname(&name) else {
                    continue;
                };
                let raw = unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if raw >= 0 {
                    stack.push((unsafe { File::from_raw_fd(raw) }, false));
                }
            }
        }
        Ok(set)
    }

    /// Find the next free name, avoiding NFC + case-fold collisions
    /// library-wide (ADR-017 §4), not just among siblings.
    ///
    /// A pure query: no directories are created and nothing is reserved. The
    /// exclusive create underneath (or its EEXIST retry) is what decides
    /// under concurrency.
    pub fn next_free_name(
        &self,
        parent: Option<&RelPath>,
        stem: &str,
        ext: &str,
    ) -> Result<RelPath> {
        let folded_lib = self.library_fold_set()?;

        let clean_ext = ext.strip_prefix('.').unwrap_or(ext);
        let dotted = if clean_ext.is_empty() {
            String::new()
        } else {
            format!(".{clean_ext}")
        };

        let mut n = 1;
        loop {
            let candidate_name = if n == 1 {
                format!("{stem}{dotted}")
            } else {
                format!("{stem} {n}{dotted}")
            };
            let nfc_name = paths::to_nfc(&candidate_name);
            if !folded_lib.contains(&paths::fold_key(&nfc_name)) {
                let candidate_path = match parent {
                    Some(p) => p.join(&nfc_name)?,
                    None => RelPath::parse(&nfc_name)?,
                };
                return Ok(candidate_path);
            }
            n += 1;
        }
    }

    /// Create a file picking the next free name (" 2", " 3", ...) and retrying on EEXIST.
    pub fn create_with_next_free_name(
        &self,
        parent: Option<&RelPath>,
        stem: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(RelPath, Fingerprint)> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.create_with_next_free_name_locked(&lock, parent, stem, ext, bytes)
    }

    pub(crate) fn create_with_next_free_name_locked(
        &self,
        lock: &WriteLock,
        parent: Option<&RelPath>,
        stem: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(RelPath, Fingerprint)> {
        let clean_ext = ext.strip_prefix('.').unwrap_or(ext);
        let dotted = if clean_ext.is_empty() {
            String::new()
        } else {
            format!(".{clean_ext}")
        };
        let mut n = 1;
        loop {
            // Re-derive the candidate from the library-wide fold set every
            // round (ADR-017 §4): on a case-sensitive filesystem only this
            // in-memory fold — not the kernel's exact-name EEXIST — sees a
            // case-only or NFC collision, and a sibling may have appeared
            // since the last try.
            let folded_lib = self.library_fold_set()?;
            let candidate_name = loop {
                let name = if n == 1 {
                    format!("{stem}{dotted}")
                } else {
                    format!("{stem} {n}{dotted}")
                };
                let nfc_name = paths::to_nfc(&name);
                if !folded_lib.contains(&paths::fold_key(&nfc_name)) {
                    break nfc_name;
                }
                n += 1;
            };
            let candidate_path = match parent {
                Some(p) => p.join(&candidate_name)?,
                None => RelPath::parse(&candidate_name)?,
            };
            match self.create_step_exclusive_locked(
                lock,
                &StepPath::Library(candidate_path.clone()),
                bytes,
            ) {
                Ok(fp) => return Ok((candidate_path, fp)),
                Err(Error::Exists(_)) => {
                    n += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Apply an intent function to a note, retrying up to 3 times on CAS conflict.
    ///
    /// If `f` returns `Ok(None)`, no modification is made and `Ok(None)` is returned.
    /// If CAS conflicts more than 3 times, returns the conflict error.
    pub fn apply_intent<F>(&self, path: &RelPath, mut f: F) -> Result<Option<Fingerprint>>
    where
        F: FnMut(&[u8]) -> Result<Option<Vec<u8>>>,
    {
        if path.note_kind().is_none() {
            return Err(Error::BadPath {
                path: path.text(),
                reject: PathReject::NotANote,
            });
        }
        let (mut bytes, mut fp) = self.read(path)?;
        let mut last_conflict = None;
        for _ in 0..=3 {
            let maybe_new_bytes = f(&bytes)?;
            let Some(new_bytes) = maybe_new_bytes else {
                return Ok(None);
            };
            match self.update_existing(path, &fp, &new_bytes) {
                Ok(new_fp) => return Ok(Some(new_fp)),
                Err(Error::Conflict(conflict)) => {
                    bytes = conflict.current_bytes.clone();
                    fp = conflict.current.clone();
                    last_conflict = Some(*conflict);
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Conflict(Box::new(last_conflict.unwrap())))
    }

    /// Force update a note: requires the hash the caller saw in the 409.
    /// Refuses if the note has moved to a third state.
    pub fn apply_force(
        &self,
        path: &RelPath,
        expected_sha256: &[u8; 32],
        new_bytes: &[u8],
    ) -> Result<Fingerprint> {
        if path.note_kind().is_none() {
            return Err(Error::BadPath {
                path: path.text(),
                reject: PathReject::NotANote,
            });
        }
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        let (current_bytes, current_fp) = self.read(path)?;
        if current_fp.sha256 != *expected_sha256 {
            return Err(conflict_err(crate::error::Conflict {
                path: path.to_path(),
                expected: Fingerprint {
                    sha256: *expected_sha256,
                    size: 0,
                    mtime_ns: 0,
                    inode: 0,
                },
                current: current_fp,
                current_bytes,
            }));
        }
        self.update_existing_locked(
            &lock,
            &StepPath::Library(path.clone()),
            &current_fp,
            new_bytes,
        )
    }

    /// Sweep leftover dot-temp files and run journal recovery.
    pub fn startup_sweep(&self) -> Result<SweepReport> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.startup_sweep_locked(&lock)
    }

    pub(crate) fn startup_sweep_locked(&self, lock: &WriteLock) -> Result<SweepReport> {
        // 1. Delete leftover dot-temp files
        let temp_files_removed = self.sweep_temp_files(&self.root)?;

        // 2. Run journal recovery
        let recovery = crate::journal::recover_journals(self, lock)?;

        // 3. Compact op log on open
        crate::op_log::compact_op_log(&self.root, lock)?;

        Ok(SweepReport {
            temp_files_removed,
            recovery,
        })
    }

    pub(crate) fn sweep_temp_files(&self, dir: &File) -> Result<usize> {
        let mut removed = 0;
        let names = fs::list_names(dir)?;
        for name in names {
            if name.starts_with(fs::TMP_PREFIX) {
                if fs::unlink(dir, &name).is_ok() {
                    removed += 1;
                }
            } else if name.starts_with('.') {
                if name == ".dimagine" {
                    if let Ok(child) = fs::resolve_dir(dir, &[name], false) {
                        removed += self.sweep_temp_files(&child)?;
                    }
                }
            } else {
                // Check if directory to recurse
                let c = match fs::cname(&name) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let raw = unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if raw >= 0 {
                    let child = unsafe { File::from_raw_fd(raw) };
                    removed += self.sweep_temp_files(&child)?;
                }
            }
        }
        Ok(removed)
    }

    /// Execute an operation with multiple steps, fully journaled and op-logged.
    pub fn execute_op(
        &self,
        op_id: &str,
        source: &str,
        client_addr: Option<&str>,
        intent: serde_json::Value,
        steps: Vec<crate::journal::StepRecord>,
    ) -> Result<()> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.execute_op_locked(&lock, op_id, source, client_addr, intent, steps, None)
    }

    /// Execute an operation, with optional failure injection for testing crash recovery.
    ///
    /// Test hook only (integration tests simulate a crash between steps);
    /// production callers use [`Writer::execute_op`].
    #[doc(hidden)]
    pub fn execute_op_with_crash_injection(
        &self,
        op_id: &str,
        source: &str,
        client_addr: Option<&str>,
        intent: serde_json::Value,
        steps: Vec<crate::journal::StepRecord>,
        fail_after_step: Option<usize>,
    ) -> Result<()> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        self.execute_op_locked(
            &lock,
            op_id,
            source,
            client_addr,
            intent,
            steps,
            fail_after_step,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_op_locked(
        &self,
        lock: &WriteLock,
        op_id: &str,
        source: &str,
        client_addr: Option<&str>,
        intent: serde_json::Value,
        steps: Vec<crate::journal::StepRecord>,
        fail_after_step: Option<usize>,
    ) -> Result<()> {
        // A caller describes an intent; the journal must record fact. Read the
        // state each step is about to change before any file is touched, so
        // recovery and undo have hashes that are really the before and after.
        // A step that cannot even be described (e.g. modifying a note that
        // does not exist) refuses the whole operation here, before any file
        // moves.
        let mut steps = steps;
        for step in &mut steps {
            crate::journal::enrich_step(self, step)?;
        }

        let journal =
            crate::journal::JournalOp::create(&self.root, &self.root_path, op_id, steps.clone())?;

        let now = chrono::Utc::now().to_rfc3339();
        let mut failures: Vec<crate::error::BatchStepError> = Vec::new();

        for (idx, step) in steps.iter_mut().enumerate() {
            if let Err(e) = self.apply_journaled_step(
                lock,
                &journal,
                op_id,
                source,
                client_addr,
                &intent,
                &now,
                step,
            ) {
                failures.push(crate::error::BatchStepError {
                    step_idx: idx,
                    path: PathBuf::from(step.dst.clone()),
                    message: e.to_string(),
                    landed_unaudited: matches!(e, Error::LandedButUnaudited { .. }),
                });
            }

            if fail_after_step == Some(idx) {
                return Err(Error::Io {
                    context: "simulated crash between steps".to_string(),
                    path: PathBuf::from(op_id),
                    source: std::io::Error::other("simulated crash"),
                });
            }
        }

        if failures.is_empty() {
            journal.mark_committed()?;

            if fail_after_step == Some(steps.len()) {
                return Err(Error::Io {
                    context: "simulated crash after committed marker".to_string(),
                    path: PathBuf::from(op_id),
                    source: std::io::Error::other("simulated crash"),
                });
            }

            journal.cleanup(&self.root)?;

            Ok(())
        } else {
            // ADR-016 §7: files that reached the disk count as success — their
            // op-log lines are already fsynced — and the rest come back as
            // per-file errors. The operation must reach a terminal state
            // before returning: the `partial` marker is written first, so a
            // later recovery reads "subset landed — do nothing" even when the
            // directory removal below fails (RW60b F2).
            journal.mark_partial()?;
            // The marker is the terminal state; a failed removal no
            // longer decides it — recovery reads the marker and
            // removes the directory itself.
            let _ = journal.cleanup(&self.root);
            Err(Error::BatchPartial {
                op: op_id.to_string(),
                errors: failures,
            })
        }
    }

    /// Apply one journaled step: re-record the on-disk state if it moved since
    /// the journal was written, perform the write, and fsync its audit line.
    #[allow(clippy::too_many_arguments)]
    fn apply_journaled_step(
        &self,
        lock: &WriteLock,
        journal: &crate::journal::JournalOp,
        op_id: &str,
        source: &str,
        client_addr: Option<&str>,
        intent: &serde_json::Value,
        now: &str,
        step: &mut crate::journal::StepRecord,
    ) -> Result<()> {
        // Something outside this process may have edited the file since the
        // journal was written. Re-record it, so a rollback restores what
        // was actually there rather than what was expected there.
        let recorded_before = step.before_hash.clone();
        let recorded_bytes = step.before_bytes.clone();
        crate::journal::enrich_step(self, step)?;
        if step.before_hash != recorded_before || step.before_bytes != recorded_bytes {
            journal.record_step(step)?;
        }

        let mut before_bytes_recorded = None;

        let after_hash_recorded = match step.kind {
            crate::journal::StepKind::Create => {
                let dst = StepPath::parse(&step.dst)?;
                let bytes = step
                    .after_bytes
                    .as_deref()
                    .ok_or_else(|| Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!(
                            "create step {} has no after_bytes to write",
                            step.step_idx
                        ),
                    })?;
                let new_fp = self.create_step_exclusive_locked(lock, &dst, bytes)?;
                new_fp.hex()
            }
            crate::journal::StepKind::Modify => {
                let dst = StepPath::parse(&step.dst)?;
                let (cur_bytes, cur_fp) = self.read_step(&dst)?;
                before_bytes_recorded = Some(cur_bytes);
                let bytes = step
                    .after_bytes
                    .as_deref()
                    .ok_or_else(|| Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!(
                            "modify step {} has no after_bytes to write",
                            step.step_idx
                        ),
                    })?;
                let new_fp = self.update_existing_locked(lock, &dst, &cur_fp, bytes)?;
                new_fp.hex()
            }
            crate::journal::StepKind::Move => {
                let src = StepPath::parse(step.src.as_ref().unwrap())?;
                let dst = StepPath::parse(&step.dst)?;
                self.move_step_noreplace_locked(lock, &src, &dst)?;
                step.after_hash.clone()
            }
            crate::journal::StepKind::Delete => {
                let dst = StepPath::parse(&step.dst)?;
                self.unlink_step_locked(lock, &dst)?;
                // The recorded bytes ride the audit line, so an
                // undo can put a small trash record back
                // (ADR-017 §7). A purged image records none and
                // is permanent by design (ADR-016 §8).
                before_bytes_recorded = step.before_bytes.clone();
                // The after-state of a delete is absence.
                String::new()
            }
        };

        // Right after rename, append to ops.jsonl and fsync. A write
        // that landed here but whose audit line failed is classified
        // distinctly (RW60b F3): the change is real, so a caller must
        // never retry the step as a fresh write.
        let log_entry = crate::op_log::OpEntry {
            op_id: op_id.to_string(),
            time: now.to_string(),
            source: source.to_string(),
            client_addr: client_addr.map(|s| s.to_string()),
            intent: intent.clone(),
            step_kind: step.kind,
            path: step.dst.clone(),
            src_path: step.src.clone(),
            before_hash: step.before_hash.clone(),
            after_hash: after_hash_recorded,
            before_bytes: before_bytes_recorded,
        };
        crate::op_log::append_op_entry(&self.root, &log_entry).map_err(|e| {
            Error::LandedButUnaudited {
                path: step.dst.clone(),
                message: e.to_string(),
            }
        })?;
        Ok(())
    }

    /// Undo an operation by op_id.
    pub fn undo(&self, op_id: &str) -> Result<crate::op_log::UndoOutcome> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        crate::op_log::undo(self, &lock, op_id, false)
    }

    /// Read all audit log entries currently recorded.
    pub fn op_log_entries(&self) -> Result<Vec<crate::op_log::OpEntry>> {
        crate::op_log::read_all_entries(&self.root)
    }

    /// Force undo an operation by op_id, reverting the subset that still matches.
    pub fn undo_force(&self, op_id: &str) -> Result<crate::op_log::UndoOutcome> {
        let lock = WriteLock::acquire(&self.root, &self.root_path)?;
        crate::op_log::undo(self, &lock, op_id, true)
    }

    /// Execute an already-planned operation while holding the
    /// write lock (ADR-016 §6).
    ///
    /// For a caller that must plan steps from facts only the
    /// filesystem knows — the collision renaming of a trash
    /// group is the example — the contract is the same as the
    /// crate's own: hold [`Writer::lock_library`] across the
    /// planning reads and this one call. The guard is not
    /// reentrant, so no primitive may be called in between.
    pub fn execute_op_with_lock(
        &self,
        lock: &WriteLock,
        op_id: &str,
        source: &str,
        intent: serde_json::Value,
        steps: Vec<crate::journal::StepRecord>,
    ) -> Result<()> {
        self.execute_op_locked(lock, op_id, source, None, intent, steps, None)
    }

    /// Read `<library>/.dimagine/settings.json` (ADR-019 §1).
    ///
    /// `Ok(None)` when the file or its directory does not exist
    /// yet: every setting then defaults. The bytes are returned
    /// unparsed — the schema lives in dimagine-core, and this
    /// crate stays the door, not the interpreter.
    pub fn read_settings(&self) -> Result<Option<Vec<u8>>> {
        let dir = match fs::resolve_dir(&self.root, &[SETTINGS_DIR.to_string()], false) {
            Ok(dir) => dir,
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        match fs::read_leaf(&dir, SETTINGS_FILE) {
            Ok((bytes, _)) => Ok(Some(bytes)),
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Atomically write `<library>/.dimagine/settings.json`
    /// (ADR-019 §2): dot-temp file, fsync, rename, directory
    /// fsync (ADR-016 §4). The caller decides the content —
    /// the schema, the non-default-key rule and the unknown
    /// keys are dimagine-core's settings module.
    pub fn write_settings(&self, bytes: &[u8]) -> Result<()> {
        let _lock = WriteLock::acquire(&self.root, &self.root_path)?;
        let dir = fs::resolve_dir(&self.root, &[SETTINGS_DIR.to_string()], true)?;
        fs::under_root(
            &dir,
            &self.root_path,
            &format!("{SETTINGS_DIR}/{SETTINGS_FILE}"),
        )?;

        let nonce = ulid::Ulid::new().to_string();
        let temp_name = fs::temp_name(&nonce);
        let temp_file = fs::open_leaf(
            &dir,
            &temp_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o644,
        )?;
        let mut temp_guard = TempFileGuard::new(&dir, temp_name.clone());
        let mut temp_writer = temp_file;
        temp_writer
            .write_all(bytes)
            .map_err(|e| fs::io_failed("write settings file", &temp_name, e))?;
        fs::fsync_file(&temp_writer)?;
        fs::rename_replace(&dir, &temp_name, SETTINGS_FILE)?;
        temp_guard.defuse();
        fs::fsync_dir(&dir)?;
        Ok(())
    }

    /// Set the modification time of a file under the library or
    /// inside `.trash/`, without following symlinks. A restore
    /// puts the original mtime back (ADR-017 §7); a rename
    /// already preserves it, so this is the belt-and-braces
    /// half of that promise.
    pub fn set_mtime(&self, path: &StepPath, mtime_ns: i64) -> Result<()> {
        let dir = fs::resolve_dir(&self.root, path.parent_components(), false)?;
        fs::under_root(&dir, &self.root_path, &path.text())?;
        fs::set_mtime(&dir, path.name(), mtime_ns)
    }

    /// Every trash record in the library (ADR-017 §7), with
    /// the path each was read from. Records are the
    /// `<group>.trash.json` files under `.trash/`; a record
    /// that does not parse is reported as an error item
    /// rather than dropped, so `check` can flag it.
    pub fn trash_records(&self) -> Result<Vec<crate::trash::ScannedRecord>> {
        crate::trash::scan_records(self)
    }
}
