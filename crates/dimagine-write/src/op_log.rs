//! The operation audit log and undo capability (ADR-016 §8).
//!
//! `.dimagine/log/ops.jsonl` keeps an append-only audit trail of operations.
//! Exactly one line is appended and fsynced per touched file right after its
//! rename. The log retains the last 200 distinct operations, compacted on open.
//!
//! Undo verifies that all participants in an operation are at their recorded
//! after-hash before applying inverse steps. `--force` allows undoing only the
//! subset that still matches.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, UndoMismatch, UndoReason};
use crate::fingerprint::Fingerprint;
use crate::fs;
use crate::journal::StepKind;
use crate::lock::WriteLock;
use crate::paths::{RelPath, StepPath};
use crate::writer::{TempFileGuard, Writer};

pub const LOG_DIR: &str = ".dimagine/log";
pub const OPS_LOG_FILE: &str = "ops.jsonl";
pub const MAX_OPS: usize = 200;

/// A single entry in `.dimagine/log/ops.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpEntry {
    pub op_id: String,
    pub time: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_addr: Option<String>,
    pub intent: serde_json::Value,
    pub step_kind: StepKind,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_hash: Option<String>,
    pub after_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_bytes: Option<Vec<u8>>,
}

/// An action produced by undoing a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoneItem {
    /// A created library file to be handed to the trash
    /// by the caller (ADR-016 §8: create ⇒ delete via
    /// trash, never a hard delete).
    TrashThis(RelPath),
    /// A modified note whose before-bytes were restored
    /// with CAS.
    Modified(RelPath),
    /// A moved file whose move was reversed, from its
    /// after-path back to its before-path — either door.
    Moved { from: StepPath, to: StepPath },
    /// A trash-side file (a trash record) the undo
    /// removed permanently: it is trash-internal
    /// state, not library content, so its inverse is a
    /// deletion, not a hand-back to the trash.
    Removed(StepPath),
    /// A deleted trash record the undo recreated from
    /// its recorded bytes — the inverse of the delete
    /// a restore performs, so undoing a restore puts
    /// the group back into the trash whole.
    Recreated(StepPath),
}

/// The result of an undo operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoOutcome {
    pub op_id: String,
    pub undone: Vec<UndoneItem>,
    pub skipped: Vec<UndoMismatch>,
}

/// Append a single operation line and fsync it immediately.
pub fn append_op_entry(root: &File, entry: &OpEntry) -> Result<()> {
    let log_dir = fs::resolve_dir(root, &[".dimagine".to_string(), "log".to_string()], true)?;
    let mut file = fs::open_leaf(
        &log_dir,
        OPS_LOG_FILE,
        libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
        0o644,
    )?;
    let mut line = serde_json::to_vec(entry).map_err(|e| Error::Io {
        context: "serialize op log entry".to_string(),
        path: PathBuf::from(format!("{LOG_DIR}/{OPS_LOG_FILE}")),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })?;
    line.push(b'\n');
    file.write_all(&line)
        .map_err(|e| fs::io_failed("write ops.jsonl", OPS_LOG_FILE, e))?;
    fs::fsync_file(&file)?;
    fs::fsync_dir(&log_dir)?;
    Ok(())
}

/// Read all entries currently in `ops.jsonl`.
pub fn read_all_entries(root: &File) -> Result<Vec<OpEntry>> {
    let log_dir = fs::resolve_dir(root, &[".dimagine".to_string(), "log".to_string()], false);
    let Ok(log_dir) = log_dir else {
        return Ok(Vec::new());
    };
    if !fs::leaf_exists(&log_dir, OPS_LOG_FILE) {
        return Ok(Vec::new());
    }
    let (bytes, _) = fs::read_leaf(&log_dir, OPS_LOG_FILE)?;
    let mut entries = Vec::new();
    let reader = BufReader::new(&bytes[..]);
    for line in reader.lines() {
        let line = line.map_err(|e| fs::io_failed("read ops.jsonl line", OPS_LOG_FILE, e))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<OpEntry>(&line) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Compact `ops.jsonl` to keep only the last `MAX_OPS` (200) distinct ops.
pub(crate) fn compact_op_log(root: &File, _lock: &WriteLock) -> Result<()> {
    let log_dir = fs::resolve_dir(root, &[".dimagine".to_string(), "log".to_string()], false);
    let Ok(log_dir) = log_dir else {
        return Ok(());
    };
    if !fs::leaf_exists(&log_dir, OPS_LOG_FILE) {
        return Ok(());
    }

    let entries = read_all_entries(root)?;
    if entries.is_empty() {
        return Ok(());
    }

    // Collect distinct op_ids in order
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for entry in &entries {
        if seen.insert(entry.op_id.clone()) {
            order.push(entry.op_id.clone());
        }
    }

    if order.len() <= MAX_OPS {
        return Ok(());
    }

    let keep_op_ids: HashSet<String> = order.into_iter().rev().take(MAX_OPS).collect();

    let kept_entries: Vec<&OpEntry> = entries
        .iter()
        .filter(|e| keep_op_ids.contains(&e.op_id))
        .collect();

    let nonce = ulid::Ulid::new().to_string();
    let temp_name = fs::temp_name(&nonce);
    let temp_file = fs::open_leaf(
        &log_dir,
        &temp_name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o644,
    )?;
    let mut temp_guard = TempFileGuard::new(&log_dir, temp_name.clone());

    let mut writer = temp_file;
    for entry in kept_entries {
        let mut line = serde_json::to_vec(entry).map_err(|e| Error::Io {
            context: "serialize during compaction".to_string(),
            path: PathBuf::from(OPS_LOG_FILE),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;
        line.push(b'\n');
        writer
            .write_all(&line)
            .map_err(|e| fs::io_failed("write compacted ops.jsonl", &temp_name, e))?;
    }
    fs::fsync_file(&writer)?;
    fs::rename_replace(&log_dir, &temp_name, OPS_LOG_FILE)?;
    temp_guard.defuse();
    fs::fsync_dir(&log_dir)?;

    Ok(())
}

/// Undo an operation by op_id.
///
/// Verifies every participant is at its after-hash. If any participant is not,
/// refuses listing the paths unless `force` is true.
///
/// The inverse steps are applied as one journaled operation of their
/// own — a new op id, source `undo`, intent `{"undo_op": <id>}` — so a
/// crash mid-undo is recovered exactly like any other operation (ADR-016
/// §7, §8; RW60b F4): the landed inverses roll back to the state after
/// the original operation. Created files are not written by undo; they
/// are handed back as [`UndoneItem::TrashThis`] for the caller to trash,
/// and the trash operation journals its own op.
pub(crate) fn undo(
    writer: &Writer,
    lock: &WriteLock,
    op_id: &str,
    force: bool,
    client_addr: Option<&str>,
) -> Result<UndoOutcome> {
    let all_entries = read_all_entries(writer.root_file())?;
    let op_entries: Vec<OpEntry> = all_entries
        .into_iter()
        .filter(|e| e.op_id == op_id)
        .collect();

    if op_entries.is_empty() {
        return Err(Error::UnknownOp(op_id.to_string()));
    }

    let mut mismatches = Vec::new();
    let mut valid_entries = Vec::new();

    for (entry_idx, entry) in op_entries.iter().enumerate() {
        let path = StepPath::parse(&entry.path)?;
        match entry.step_kind {
            StepKind::Create => match writer.read_step(&path) {
                Ok((bytes, _)) => {
                    let fp = Fingerprint::of_bytes(&bytes);
                    if fp.hex() == entry.after_hash {
                        valid_entries.push(entry.clone());
                    } else {
                        mismatches.push(UndoMismatch {
                            path: path.to_path(),
                            reason: UndoReason::Changed,
                        });
                    }
                }
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    mismatches.push(UndoMismatch {
                        path: path.to_path(),
                        reason: UndoReason::Missing,
                    });
                }
                Err(e) => return Err(e),
            },
            StepKind::Modify => match writer.read_step(&path) {
                Ok((bytes, _)) => {
                    let fp = Fingerprint::of_bytes(&bytes);
                    let hex = fp.hex();
                    if hex == entry.after_hash {
                        if entry.before_bytes.is_none() {
                            mismatches.push(UndoMismatch {
                                path: path.to_path(),
                                reason: UndoReason::NoRecord,
                            });
                        } else {
                            valid_entries.push(entry.clone());
                        }
                    } else if entry.before_hash.as_deref() == Some(&hex) {
                        mismatches.push(UndoMismatch {
                            path: path.to_path(),
                            reason: UndoReason::Reverted,
                        });
                    } else {
                        mismatches.push(UndoMismatch {
                            path: path.to_path(),
                            reason: UndoReason::Changed,
                        });
                    }
                }
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    mismatches.push(UndoMismatch {
                        path: path.to_path(),
                        reason: UndoReason::Missing,
                    });
                }
                Err(e) => return Err(e),
            },
            StepKind::Move => {
                let src_text = entry.src_path.as_deref().ok_or_else(|| Error::BadJournal {
                    op: op_id.to_string(),
                    message: "move log entry missing src_path".to_string(),
                })?;
                let src = StepPath::parse(src_text)?;

                let dst_read = writer.read_step(&path);
                let src_read = writer.read_step(&src);

                let src_exists = src_read.is_ok();
                if src_exists {
                    mismatches.push(UndoMismatch {
                        path: src.to_path(),
                        reason: UndoReason::SourceBack,
                    });
                    continue;
                }

                match dst_read {
                    Ok((bytes, _)) => {
                        let fp = Fingerprint::of_bytes(&bytes);
                        let hex = fp.hex();
                        // ADR-016 §8 asks every file to be in the state the
                        // operation left it in — per path, not per line. A
                        // move with link repair (ADR-017 §8) renames a note
                        // and then edits that same path within one operation,
                        // so the file's final bytes carry the later edit's
                        // hash, not the move's. A move still counts as
                        // executed when a later line of the same operation
                        // leaves this path at the bytes on disk.
                        let edited_later = op_entries[entry_idx + 1..]
                            .iter()
                            .any(|later| later.path == entry.path && later.after_hash == hex);
                        if hex == entry.after_hash || edited_later {
                            valid_entries.push(entry.clone());
                        } else {
                            mismatches.push(UndoMismatch {
                                path: path.to_path(),
                                reason: UndoReason::Changed,
                            });
                        }
                    }
                    Err(Error::Io { source, .. })
                        if source.raw_os_error() == Some(libc::ENOENT) =>
                    {
                        mismatches.push(UndoMismatch {
                            path: path.to_path(),
                            reason: UndoReason::Missing,
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
            StepKind::Delete => {
                // A delete's after-state is absence. With
                // recorded bytes — the small trash record a
                // restore consumes (ADR-017 §7) — the
                // inverse is a re-create. Without them the
                // deletion is permanent and the operation
                // is not undoable (ADR-016 §8).
                match writer.read_step(&path) {
                    Err(Error::Io { source, .. })
                        if source.raw_os_error() == Some(libc::ENOENT) =>
                    {
                        if entry.before_bytes.is_some() {
                            valid_entries.push(entry.clone());
                        } else {
                            mismatches.push(UndoMismatch {
                                path: path.to_path(),
                                reason: UndoReason::Permanent,
                            });
                        }
                    }
                    Ok((bytes, _)) => {
                        let hex = Fingerprint::of_bytes(&bytes).hex();
                        let reason = if entry.before_hash.as_deref() == Some(&hex) {
                            UndoReason::Reverted
                        } else {
                            UndoReason::Changed
                        };
                        mismatches.push(UndoMismatch {
                            path: path.to_path(),
                            reason,
                        });
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    if !mismatches.is_empty() && !force {
        return Err(Error::UndoRefused {
            op: op_id.to_string(),
            mismatches,
        });
    }

    // Apply the inverses of the valid entries, newest first.
    // Modify and move inverses become steps of one journaled
    // undo operation, so a crash mid-undo is recovered like
    // any other operation (ADR-016 §7, §8; RW60b F4).
    // Created library files are not written by undo: they are
    // handed back as [`UndoneItem::TrashThis`] for the caller
    // to trash, and the trash operation journals its own op.
    let mut undone = Vec::new();
    let mut steps = Vec::new();
    for entry in valid_entries.iter().rev() {
        let path = StepPath::parse(&entry.path)?;
        match entry.step_kind {
            StepKind::Create => match &path {
                // A created library file: the caller hands
                // it to the trash (ADR-016 §8).
                StepPath::Library(rel) => {
                    undone.push(UndoneItem::TrashThis(rel.clone()));
                }
                // A created trash-side file — a trash
                // record: trash-internal state whose
                // inverse is a permanent removal, not a
                // hand-back to the trash.
                StepPath::Trash(trash) => {
                    undone.push(UndoneItem::Removed(StepPath::Trash(trash.clone())));
                    steps.push(crate::journal::StepRecord {
                        step_idx: steps.len(),
                        kind: StepKind::Delete,
                        src: None,
                        dst: entry.path.clone(),
                        before_hash: None,
                        after_hash: String::new(),
                        before_bytes: None,
                        after_bytes: None,
                    });
                }
            },
            StepKind::Modify => {
                let Some(before_bytes) = entry.before_bytes.as_ref() else {
                    return Err(Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!("no before-bytes recorded for {:?}", entry.path),
                    });
                };
                let StepPath::Library(rel) = &path else {
                    return Err(Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!("modify step targets the trash: {:?}", entry.path),
                    });
                };
                undone.push(UndoneItem::Modified(rel.clone()));
                // The before-state is re-read from the file (which
                // sits at the recorded after-hash) and the after-state
                // is the recorded before-bytes, by `enrich_step` and
                // the journal write respectively.
                steps.push(crate::journal::StepRecord {
                    step_idx: steps.len(),
                    kind: StepKind::Modify,
                    src: None,
                    dst: entry.path.clone(),
                    before_hash: None,
                    after_hash: String::new(),
                    before_bytes: None,
                    after_bytes: Some(before_bytes.clone()),
                });
            }
            StepKind::Move => {
                let src_text = entry.src_path.as_ref().unwrap();
                let src = StepPath::parse(src_text)?;
                undone.push(UndoneItem::Moved {
                    from: path.clone(),
                    to: src.clone(),
                });
                // The move is reversed: from the recorded
                // destination back to the recorded source.
                steps.push(crate::journal::StepRecord {
                    step_idx: steps.len(),
                    kind: StepKind::Move,
                    src: Some(entry.path.clone()),
                    dst: src_text.clone(),
                    before_hash: None,
                    after_hash: String::new(),
                    before_bytes: None,
                    after_bytes: None,
                });
            }
            StepKind::Delete => {
                // The recorded bytes are the deleted
                // trash record: the inverse is a
                // re-create, so undoing a restore puts
                // the group back into the trash whole.
                let Some(before_bytes) = entry.before_bytes.as_ref() else {
                    return Err(Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!(
                            "delete entry recorded no before-bytes to restore: {:?}",
                            entry.path
                        ),
                    });
                };
                undone.push(UndoneItem::Recreated(path.clone()));
                steps.push(crate::journal::StepRecord {
                    step_idx: steps.len(),
                    kind: StepKind::Create,
                    src: None,
                    dst: entry.path.clone(),
                    before_hash: None,
                    after_hash: String::new(),
                    before_bytes: None,
                    after_bytes: Some(before_bytes.clone()),
                });
            }
        }
    }

    if !steps.is_empty() {
        let undo_op_id = ulid::Ulid::new().to_string();
        writer.execute_op_locked(
            lock,
            &undo_op_id,
            "undo",
            client_addr,
            serde_json::json!({ "undo_op": op_id }),
            steps,
            None,
        )?;
    }

    Ok(UndoOutcome {
        op_id: op_id.to_string(),
        undone,
        skipped: mismatches,
    })
}
