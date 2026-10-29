//! Multi-step journal and crash recovery (ADR-016 §7).
//!
//! Scope: multi-file operations write an atomically-fsynced step collection
//! into `.dimagine/journal/<op-id>/`, then touch user files, then write a
//! fsynced `committed` marker, and finally remove the journal directory. A
//! partial batch (a step failed, the rest landed) writes a fsynced `partial`
//! marker *before* attempting that removal, so the terminal state — "a subset
//! landed, the caller was told which" — survives even a failed directory
//! removal: recovery reads the marker, touches nothing, and removes the
//! directory itself (RW60b F2).
//!
//! Recovery runs only when the write lock is obtainable and the recorded writer
//! pid/boot id is dead. Files in a third state are never touched.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, PathReject, Result};
use crate::fingerprint::Fingerprint;
use crate::fs;
use crate::lock::WriteLock;
use crate::paths::StepPath;
use crate::writer::Writer;

pub const JOURNAL_DIR: &str = ".dimagine/journal";
pub const COMMITTED_MARKER: &str = "committed";
/// Terminal-state marker of a partial batch: a subset of the
/// steps landed and the caller was told which ones. Recovery
/// interprets it as "do nothing to user files" (RW60b F2).
pub const PARTIAL_MARKER: &str = "partial";
pub const META_FILE: &str = "meta.json";

/// The kind of change a journal step performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Create,
    Modify,
    Move,
    /// A permanent unlink, only ever of a file inside
    /// `.trash/` (ADR-017 §7): a purged group, or the
    /// trash record a restore consumes. Unlike the other
    /// kinds there is no inverse: the step is either
    /// permanent (no bytes recorded — a purged image) or
    /// reversible from its recorded bytes (a small trash
    /// record), and recovery and undo treat the two
    /// exactly so.
    Delete,
}

/// A recorded step in an operation journal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_idx: usize,
    pub kind: StepKind,
    pub src: Option<String>,
    pub dst: String,
    pub before_hash: Option<String>,
    pub after_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_bytes: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_bytes: Option<Vec<u8>>,
}

/// Metadata about the operation and the writer process that initiated it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalMeta {
    pub op_id: String,
    pub pid: u32,
    pub boot_id: Option<String>,
    pub created_at: String,
}

/// The boot identifier of the running operating system, if available.
pub fn current_boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|s| s.trim().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        use std::mem;
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut boottime: libc::timeval = unsafe { mem::zeroed() };
        let mut size = mem::size_of::<libc::timeval>();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut boottime as *mut _ as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            Some(format!("{}.{}", boottime.tv_sec, boottime.tv_usec))
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Check if the recorded writer process is definitely dead.
pub fn is_process_dead(pid: u32, recorded_boot_id: Option<&str>) -> bool {
    if let Some(recorded_boot) = recorded_boot_id {
        if let Some(current_boot) = current_boot_id() {
            if recorded_boot != current_boot {
                return true;
            }
        }
    }
    if pid == 0 {
        return true;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
    }
    false
}

/// The classification of one step during recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    /// Step outcome is present on disk (destination has after-hash; for move, source absent).
    Executed,
    /// Pre-step state is present on disk (destination absent or at before-hash).
    NotExecuted,
    /// Neither expected state: file modified externally or partially corrupted.
    ThirdState(String),
}

/// Result of recovering an operation journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    RolledForward {
        op_id: String,
        steps: usize,
    },
    RolledBack {
        op_id: String,
        steps: usize,
    },
    /// The operation had already reached its terminal state as a
    /// partial batch: a subset of its steps landed and the caller
    /// was told which files those were. Recovery touches no user
    /// file and only removes the journal (RW60b F2).
    PartialLanded {
        op_id: String,
    },
    ThirdStateRefused {
        op_id: String,
        reason: String,
        /// Known-state steps already applied before the refusal: rolled
        /// forward when committed, rolled back when not. The third-state
        /// files were left alone and the journal is kept as the incomplete
        /// marker, so the next open re-reports them.
        applied_steps: usize,
    },
    ReportedCorrupt {
        op_id: String,
        error: String,
    },
    SkippedLiveWriter {
        op_id: String,
        pid: u32,
    },
}

/// An operation id becomes a directory name under `.dimagine/journal/`, and
/// cleanup unlinks everything inside the directory it resolves to. It must
/// therefore be one safe component: no separator, no `.` or `..`, no leading
/// dot, no NUL. `..` alone would resolve the "operation directory" to
/// `.dimagine` itself.
pub(crate) fn check_op_id(op_id: &str) -> Result<String> {
    let reject = if op_id.is_empty() {
        PathReject::Empty
    } else if op_id == "." || op_id == ".." || op_id.contains('/') || op_id.contains('\0') {
        PathReject::Traversal
    } else if op_id.starts_with('.') {
        PathReject::Hidden
    } else {
        return Ok(op_id.to_string());
    };
    Err(Error::BadPath {
        path: op_id.to_string(),
        reject,
    })
}

/// Create a journal directory and write step files atomically before touching files.
pub(crate) struct JournalOp {
    op_id: String,
    op_dir: File,
    _meta: JournalMeta,
    _steps: Vec<StepRecord>,
}

impl JournalOp {
    pub(crate) fn create(
        root: &File,
        root_path: &Path,
        op_id: &str,
        steps: Vec<StepRecord>,
    ) -> Result<Self> {
        let op_id = check_op_id(op_id)?;
        let journal_dir = fs::resolve_dir(
            root,
            &[".dimagine".to_string(), "journal".to_string()],
            true,
        )?;
        let op_dir = fs::resolve_dir(&journal_dir, std::slice::from_ref(&op_id), true)?;
        fs::under_root(&op_dir, root_path, &format!("{JOURNAL_DIR}/{op_id}"))?;

        let meta = JournalMeta {
            op_id: op_id.to_string(),
            pid: std::process::id(),
            boot_id: current_boot_id(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        // Write meta.json
        write_atomic_json(&op_dir, META_FILE, &meta)?;

        // Persist every before/after state before touching any user file.
        write_atomic_json(&op_dir, "steps.json", &steps)?;
        fs::fsync_dir(&op_dir)?;

        Ok(Self {
            op_id: op_id.to_string(),
            op_dir,
            _meta: meta,
            _steps: steps,
        })
    }

    pub(crate) fn mark_committed(&self) -> Result<()> {
        let file = fs::open_leaf(
            &self.op_dir,
            COMMITTED_MARKER,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )?;
        fs::fsync_file(&file)?;
        fs::fsync_dir(&self.op_dir)?;
        Ok(())
    }

    /// Write the terminal-state marker of a partial batch: a subset
    /// of the steps landed and the caller was told which. Written
    /// *before* the journal directory is removed, so the terminal
    /// state does not depend on that removal succeeding — recovery
    /// reads the marker, touches no user file, and removes the
    /// directory itself (RW60b F2).
    pub(crate) fn mark_partial(&self) -> Result<()> {
        let file = fs::open_leaf(
            &self.op_dir,
            PARTIAL_MARKER,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )?;
        fs::fsync_file(&file)?;
        fs::fsync_dir(&self.op_dir)?;
        Ok(())
    }

    /// Rewrite one step file: used when the filesystem knows something about
    /// the step that the caller's intent did not (ADR-016 §7).
    pub(crate) fn record_step(&self, step: &StepRecord) -> Result<()> {
        let step_file_name = format!("step_{:04}.json", step.step_idx);
        write_atomic_json(&self.op_dir, &step_file_name, step)?;
        fs::fsync_dir(&self.op_dir)
    }

    pub(crate) fn cleanup(self, root: &File) -> Result<()> {
        cleanup_op_dir(root, &self.op_id)
    }
}

fn write_atomic_json<T: Serialize>(dir: &File, name: &str, val: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(val).map_err(|e| Error::BadJournal {
        op: name.to_string(),
        message: e.to_string(),
    })?;
    let nonce = ulid::Ulid::new().to_string();
    let temp_name = fs::temp_name(&nonce);
    let temp_file = fs::open_leaf(
        dir,
        &temp_name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o644,
    )?;
    // A failed write must not leave the temp behind (the sweep would remove
    // it eventually, but the ENOSPC contract is immediate cleanup).
    let mut temp_guard = crate::writer::TempFileGuard::new(dir, temp_name.clone());
    let mut writer = temp_file;
    writer
        .write_all(&bytes)
        .map_err(|e| fs::io_failed("write journal file", name, e))?;
    fs::fsync_file(&writer)?;
    fs::rename_replace(dir, &temp_name, name)?;
    temp_guard.defuse();
    fs::fsync_dir(dir)?;
    Ok(())
}

pub(crate) fn cleanup_op_dir(root: &File, op_id: &str) -> Result<()> {
    let Ok(op_id) = check_op_id(op_id) else {
        return Ok(());
    };
    let journal_dir = fs::resolve_dir(
        root,
        &[".dimagine".to_string(), "journal".to_string()],
        false,
    );
    let Ok(journal_dir) = journal_dir else {
        return Ok(());
    };
    let op_dir = fs::resolve_dir(&journal_dir, std::slice::from_ref(&op_id), false);
    let Ok(op_dir) = op_dir else {
        return Ok(());
    };
    let names = fs::list_names(&op_dir)?;
    for name in names {
        let _ = fs::unlink(&op_dir, &name);
    }
    let _ = fs::remove_dir_at(&journal_dir, &op_id);
    fs::fsync_dir(&journal_dir)?;
    Ok(())
}

/// Fill in what only the filesystem can answer for a step: the state it is
/// about to change, and the state its write will produce (ADR-016 §7).
///
/// A caller describes an intent and cannot know a note's current hash, so the
/// journal is written from fact read here, not from a guess passed in. Without
/// this, recovery after a crash would misclassify steps: a move with an empty
/// `after_hash` looks like third state forever, and a modify without
/// before-bytes "rolls back" as a no-op while reporting success.
pub(crate) fn enrich_step(writer: &Writer, step: &mut StepRecord) -> Result<()> {
    match step.kind {
        StepKind::Create => {
            step.before_hash = None;
            step.before_bytes = None;
            if step.after_bytes.is_none() {
                return Err(Error::BadJournal {
                    op: step.dst.clone(),
                    message: "create step records no after_bytes to write".to_string(),
                });
            }
            if let Some(after) = step.after_bytes.as_deref() {
                step.after_hash = Fingerprint::of_bytes(after).hex();
            }
        }
        StepKind::Modify => {
            let dst = StepPath::parse(&step.dst)?;
            match writer.read_step(&dst) {
                Ok((bytes, _)) => {
                    step.before_hash = Some(Fingerprint::of_bytes(&bytes).hex());
                    step.before_bytes = Some(bytes);
                }
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    // The file is not at its path yet, but the operation
                    // itself is putting it there: a move with link repair
                    // (ADR-017 §8) renames the note and edits it at its new
                    // path in one journaled op, and the ADR-016 §8 undo rule
                    // requires the edit to be recorded at the path it ends up
                    // at. The caller describes the before state it read from
                    // the old path; nothing is guessed here. Applying the
                    // step re-reads the real file first, so the journal is
                    // corrected to fact before a single byte is written, and
                    // a step that carries no before state still refuses.
                    let Some(before) = step.before_bytes.as_ref() else {
                        return Err(Error::BadJournal {
                            op: step.dst.clone(),
                            message: "modify step names a file that is not there; only a step carrying its before bytes may describe one its own operation is moving in".to_string(),
                        });
                    };
                    step.before_hash = Some(Fingerprint::of_bytes(before).hex());
                }
                Err(e) => return Err(e),
            }
            if step.after_bytes.is_none() {
                return Err(Error::BadJournal {
                    op: step.dst.clone(),
                    message: "modify step records no after_bytes to write".to_string(),
                });
            }
            if let Some(after) = step.after_bytes.as_deref() {
                step.after_hash = Fingerprint::of_bytes(after).hex();
            }
        }
        StepKind::Move => {
            let src_text = step.src.as_deref().ok_or_else(|| Error::BadJournal {
                op: step.dst.clone(),
                message: "move step missing src".to_string(),
            })?;
            let src = StepPath::parse(src_text)?;
            // A rename does not change bytes, so the source's hash is both the
            // before state and the after state of the destination. The step
            // file keeps hashes only: a move is its own inverse and never
            // needs bytes.
            let (bytes, _) = writer.read_step(&src)?;
            let hex = Fingerprint::of_bytes(&bytes).hex();
            step.before_hash = Some(hex.clone());
            step.before_bytes = None;
            step.after_hash = hex;
        }
        StepKind::Delete => {
            let dst = StepPath::parse(&step.dst)?;
            let (bytes, _) = writer.read_step(&dst)?;
            step.before_hash = Some(Fingerprint::of_bytes(&bytes).hex());
            // A delete's after-state is absence, so the
            // after hash is the empty string. The bytes
            // stay as the caller recorded them: a small
            // trash record carries them so recovery and
            // undo can put it back, while a purged
            // image — which this crate must never copy
            // (ADR-016 §8) — records none and is
            // permanent by design.
            step.after_hash = String::new();
        }
    }
    Ok(())
}

/// Classify a single step against the current filesystem state.
///
/// `later_steps` are the steps of the same operation that run after
/// this one. A move with link repair (ADR-017 §8) renames a file
/// and then edits it at its new path, so the moved file's final
/// bytes carry the later edit's hash, not the move's; like undo
/// (op_log.rs), such a move still counts as executed when its
/// source is gone and a later step of the same operation leaves
/// the destination at the bytes on disk.
pub fn classify_step(
    writer: &Writer,
    step: &StepRecord,
    later_steps: &[StepRecord],
) -> Result<StepState> {
    let dst = StepPath::parse(&step.dst)?;
    match step.kind {
        StepKind::Create => match writer.read_step(&dst) {
            Ok((bytes, _)) => {
                let fp = Fingerprint::of_bytes(&bytes);
                if fp.hex() == step.after_hash {
                    Ok(StepState::Executed)
                } else {
                    Ok(StepState::ThirdState(format!(
                        "destination {} has unexpected hash {}",
                        step.dst,
                        fp.hex()
                    )))
                }
            }
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                Ok(StepState::NotExecuted)
            }
            Err(e) => Err(e),
        },
        StepKind::Modify => match writer.read_step(&dst) {
            Ok((bytes, _)) => {
                let fp = Fingerprint::of_bytes(&bytes);
                let hex = fp.hex();
                if hex == step.after_hash {
                    Ok(StepState::Executed)
                } else if step.before_hash.as_deref() == Some(&hex) {
                    Ok(StepState::NotExecuted)
                } else {
                    Ok(StepState::ThirdState(format!(
                        "destination {} has third-state hash {}",
                        step.dst, hex
                    )))
                }
            }
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => Ok(
                StepState::ThirdState(format!("destination {} missing on modify", step.dst)),
            ),
            Err(e) => Err(e),
        },
        StepKind::Move => {
            let src_text = step.src.as_deref().ok_or_else(|| Error::BadJournal {
                op: step.dst.clone(),
                message: "move step missing src".to_string(),
            })?;
            let src = StepPath::parse(src_text)?;

            let src_read = writer.read_step(&src);
            let dst_read = writer.read_step(&dst);

            let dst_hex = match &dst_read {
                Ok((bytes, _)) => Some(Fingerprint::of_bytes(bytes).hex()),
                _ => None,
            };
            let src_missing = matches!(
                &src_read,
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT)
            );

            // Move is applied iff dst has the after-hash — or the
            // hash a later step of this same operation left on
            // this path — and src is absent (ADR-016 §7). The
            // second form is the move-then-edit pattern (ADR-017
            // §8): the operation renames the file and then edits
            // it at its new path, so "executed" is judged per
            // file, like undo (op_log.rs), never per journal
            // line. Without this rule a crash after the edit but
            // before `committed` would misclassify the rename as
            // a third state, skip its rollback and tear the
            // group.
            let dst_has_after = dst_hex.as_deref() == Some(step.after_hash.as_str());
            let edited_later = src_missing
                && dst_hex.as_deref().is_some_and(|hex| {
                    later_steps
                        .iter()
                        .any(|later| later.dst == step.dst && later.after_hash == hex)
                });

            if src_missing && (dst_has_after || edited_later) {
                return Ok(StepState::Executed);
            }

            let src_has_before = match &src_read {
                Ok((bytes, _)) => {
                    step.before_hash.as_deref() == Some(&Fingerprint::of_bytes(bytes).hex())
                }
                _ => false,
            };
            let dst_missing = matches!(
                &dst_read,
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT)
            );

            if src_has_before && dst_missing {
                return Ok(StepState::NotExecuted);
            }

            Ok(StepState::ThirdState(format!(
                "move from {} to {} in ambiguous state (src exists: {}, dst exists: {})",
                src_text,
                step.dst,
                src_read.is_ok(),
                dst_read.is_ok()
            )))
        }
        StepKind::Delete => match writer.read_step(&dst) {
            // The after-state of a delete is absence.
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                Ok(StepState::Executed)
            }
            Ok((bytes, _)) => {
                let hex = Fingerprint::of_bytes(&bytes).hex();
                if step.before_hash.as_deref() == Some(&hex) {
                    Ok(StepState::NotExecuted)
                } else {
                    Ok(StepState::ThirdState(format!(
                        "delete target {} has third-state hash {}",
                        step.dst, hex
                    )))
                }
            }
            Err(e) => Err(e),
        },
    }
}

/// Run journal recovery across all ops under `.dimagine/journal/`.
///
/// Only runs when the write lock is held. Checks that the writer pid is dead
/// before touching an op. Unparsable journals are reported and never applied,
/// and one broken journal does not stop the others.
pub(crate) fn recover_journals(writer: &Writer, lock: &WriteLock) -> Result<Vec<RecoveryOutcome>> {
    let journal_dir = fs::resolve_dir(
        writer.root_file(),
        &[".dimagine".to_string(), "journal".to_string()],
        false,
    );
    let Ok(journal_dir) = journal_dir else {
        return Ok(Vec::new());
    };

    let op_ids = fs::list_names(&journal_dir)?;
    let mut outcomes = Vec::new();

    for op_id in op_ids {
        outcomes.push(match recover_op(writer, lock, &journal_dir, &op_id) {
            Ok(outcome) => outcome,
            Err(e) => RecoveryOutcome::ReportedCorrupt {
                op_id: op_id.clone(),
                error: e.to_string(),
            },
        });
    }

    Ok(outcomes)
}

/// Recover a single operation journal. `Err` means "this one is reported and
/// left alone", never "the file it describes is touched".
fn recover_op(
    writer: &Writer,
    lock: &WriteLock,
    journal_dir: &File,
    op_id: &str,
) -> Result<RecoveryOutcome> {
    let op_id = check_op_id(op_id)?;
    let op_dir = fs::resolve_dir(journal_dir, std::slice::from_ref(&op_id), false)?;

    let (meta_bytes, _) = fs::read_leaf(&op_dir, META_FILE).map_err(|e| Error::BadJournal {
        op: op_id.to_string(),
        message: format!("cannot read {META_FILE}: {e}"),
    })?;
    let meta: JournalMeta = serde_json::from_slice(&meta_bytes).map_err(|e| Error::BadJournal {
        op: op_id.to_string(),
        message: format!("unparsable {META_FILE}: {e}"),
    })?;

    if !is_process_dead(meta.pid, meta.boot_id.as_deref()) {
        return Ok(RecoveryOutcome::SkippedLiveWriter {
            op_id: op_id.to_string(),
            pid: meta.pid,
        });
    }

    // A partial batch already reached its terminal state: a
    // subset of steps landed and the caller was told which
    // ones. Recovery touches no user file — the landed files
    // count as success — and only removes the journal, whose
    // removal may have failed before (RW60b F2).
    if fs::leaf_exists(&op_dir, PARTIAL_MARKER) {
        cleanup_op_dir(writer.root_file(), &op_id)?;
        return Ok(RecoveryOutcome::PartialLanded {
            op_id: op_id.to_string(),
        });
    }

    let is_committed = fs::leaf_exists(&op_dir, COMMITTED_MARKER);

    let names = fs::list_names(&op_dir)?;
    let mut step_files: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with("step_") && n.ends_with(".json"))
        .collect();
    step_files.sort();

    let mut steps = if fs::leaf_exists(&op_dir, "steps.json") {
        let (bytes, _) = fs::read_leaf(&op_dir, "steps.json").map_err(|e| Error::BadJournal {
            op: op_id.to_string(),
            message: format!("cannot read steps.json: {e}"),
        })?;
        serde_json::from_slice::<Vec<StepRecord>>(&bytes).map_err(|e| Error::BadJournal {
            op: op_id.to_string(),
            message: format!("cannot parse steps.json: {e}"),
        })?
    } else {
        Vec::new()
    };
    for sfile in &step_files {
        let (bytes, _) = fs::read_leaf(&op_dir, sfile).map_err(|e| Error::BadJournal {
            op: op_id.to_string(),
            message: format!("cannot read {sfile}: {e}"),
        })?;
        let step = serde_json::from_slice::<StepRecord>(&bytes).map_err(|e| Error::BadJournal {
            op: op_id.to_string(),
            message: format!("cannot parse {sfile}: {e}"),
        })?;
        if let Some(old) = steps.iter_mut().find(|old| old.step_idx == step.step_idx) {
            *old = step;
        } else {
            steps.push(step);
        }
    }

    // Classify every step on its own. A third-state file never blocks the
    // known-state steps: they are still rolled forward (committed) or rolled
    // back (uncommitted), and only the third-state file is left alone and
    // reported (ADR-016 §7).
    let mut states = Vec::new();
    for (idx, step) in steps.iter().enumerate() {
        match classify_step(writer, step, &steps[idx + 1..]) {
            Ok(state) => states.push(state),
            Err(e) => states.push(StepState::ThirdState(format!("classification failed: {e}"))),
        }
    }

    // Fail closed before touching anything: a content step without its bytes
    // is a corrupt journal, never an empty write.
    for (step, state) in steps.iter().zip(states.iter()) {
        if is_committed
            && *state == StepState::NotExecuted
            && matches!(step.kind, StepKind::Create | StepKind::Modify)
            && step.after_bytes.is_none()
        {
            return Err(Error::BadJournal {
                op: op_id.to_string(),
                message: format!(
                    "step {} records no after_bytes to roll forward",
                    step.step_idx
                ),
            });
        }
        if !is_committed
            && *state == StepState::Executed
            && step.kind == StepKind::Modify
            && step.before_bytes.is_none()
        {
            return Err(Error::BadJournal {
                op: op_id.to_string(),
                message: format!(
                    "modify step {} recorded no before-bytes to restore",
                    step.step_idx
                ),
            });
        }
    }

    let third_state_reasons: Vec<String> = steps
        .iter()
        .zip(states.iter())
        .filter_map(|(step, state)| match state {
            StepState::ThirdState(reason) => {
                Some(format!("step {} ({}): {reason}", step.step_idx, step.dst))
            }
            _ => None,
        })
        .collect();

    let mut count = 0;
    if is_committed {
        // Roll forward the known unexecuted steps; third-state files are
        // skipped, never touched.
        for (step, state) in steps.iter().zip(states.iter()) {
            if *state != StepState::NotExecuted {
                continue;
            }
            match step.kind {
                StepKind::Create => {
                    let dst = StepPath::parse(&step.dst)?;
                    let bytes = step
                        .after_bytes
                        .as_deref()
                        .ok_or_else(|| Error::BadJournal {
                            op: op_id.to_string(),
                            message: format!(
                                "step {} records no after_bytes to roll forward",
                                step.step_idx
                            ),
                        })?;
                    writer.create_step_exclusive_locked(lock, &dst, bytes)?;
                }
                StepKind::Modify => {
                    let dst = StepPath::parse(&step.dst)?;
                    let (_, cur_fp) = writer.read_step(&dst)?;
                    let bytes = step
                        .after_bytes
                        .as_deref()
                        .ok_or_else(|| Error::BadJournal {
                            op: op_id.to_string(),
                            message: format!(
                                "step {} records no after_bytes to roll forward",
                                step.step_idx
                            ),
                        })?;
                    writer.update_existing_locked(lock, &dst, &cur_fp, bytes)?;
                }
                StepKind::Move => {
                    let src =
                        StepPath::parse(step.src.as_ref().ok_or_else(|| Error::BadJournal {
                            op: op_id.to_string(),
                            message: "move step missing src".to_string(),
                        })?)?;
                    let dst = StepPath::parse(&step.dst)?;
                    writer.move_step_noreplace_locked(lock, &src, &dst)?;
                }
                StepKind::Delete => {
                    let dst = StepPath::parse(&step.dst)?;
                    writer.unlink_step_locked(lock, &dst)?;
                }
            }
            count += 1;
        }
    } else {
        // Roll back the known executed steps, newest first; third-state files
        // are skipped, never touched.
        for (step, state) in steps.iter().rev().zip(states.iter().rev()) {
            if *state != StepState::Executed {
                continue;
            }
            match step.kind {
                StepKind::Create => {
                    let dst = StepPath::parse(&step.dst)?;
                    if dst.text() == crate::paths::TAG_ALIASES_PATH {
                        let (_, fp) = writer.read_step(&dst)?;
                        let trash_steps = crate::trash::alias_trash_steps(&op_id, &fp)?;
                        let record = StepPath::parse(&trash_steps[0].dst)?;
                        writer.create_step_exclusive_locked(
                            lock,
                            &record,
                            trash_steps[0].after_bytes.as_deref().unwrap(),
                        )?;
                        let trash = StepPath::parse(&trash_steps[1].dst)?;
                        writer.move_step_noreplace_locked(lock, &dst, &trash)?;
                    } else {
                        let dst_dir =
                            fs::resolve_dir(writer.root_file(), dst.parent_components(), false)?;
                        fs::unlink(&dst_dir, dst.name())?;
                        let _ = fs::fsync_dir(&dst_dir);
                    }
                }
                StepKind::Modify => {
                    let dst = StepPath::parse(&step.dst)?;
                    let before_bytes =
                        step.before_bytes
                            .as_ref()
                            .ok_or_else(|| Error::BadJournal {
                                op: op_id.to_string(),
                                message: format!(
                                    "modify step {} recorded no before-bytes to restore",
                                    step.step_idx
                                ),
                            })?;
                    let (_, cur_fp) = writer.read_step(&dst)?;
                    writer.update_existing_locked(lock, &dst, &cur_fp, before_bytes)?;
                }
                StepKind::Move => {
                    let src =
                        StepPath::parse(step.src.as_ref().ok_or_else(|| Error::BadJournal {
                            op: op_id.to_string(),
                            message: "move step missing src".to_string(),
                        })?)?;
                    let dst = StepPath::parse(&step.dst)?;
                    writer.move_step_noreplace_locked(lock, &dst, &src)?;
                }
                StepKind::Delete => {
                    // A permanent deletion is only rolled back when its
                    // bytes were recorded — the small trash record a
                    // restore consumes (ADR-017 §7). Without them there
                    // is nothing to restore, so the deletion stands and
                    // the step is left alone: purged images are
                    // unrecoverable by design, never copied into the
                    // journal (ADR-016 §8).
                    let dst = StepPath::parse(&step.dst)?;
                    let Some(bytes) = step.before_bytes.as_deref() else {
                        // Permanent: nothing was restored, nothing counted.
                        continue;
                    };
                    writer.create_step_exclusive_locked(lock, &dst, bytes)?;
                }
            }
            count += 1;
        }
    }

    if !third_state_reasons.is_empty() {
        // Known steps are resolved; the third-state files stay exactly as
        // they are and the journal stays as the incomplete marker, so the
        // next open re-reports them instead of silently dropping the report.
        return Ok(RecoveryOutcome::ThirdStateRefused {
            op_id: op_id.to_string(),
            reason: third_state_reasons.join("; "),
            applied_steps: count,
        });
    }

    if is_committed {
        cleanup_op_dir(writer.root_file(), &op_id)?;
        Ok(RecoveryOutcome::RolledForward {
            op_id: op_id.to_string(),
            steps: count,
        })
    } else {
        cleanup_op_dir(writer.root_file(), &op_id)?;
        Ok(RecoveryOutcome::RolledBack {
            op_id: op_id.to_string(),
            steps: count,
        })
    }
}
