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
use crate::paths::RelPath;
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
    /// A created file to be handed to the trash by the caller.
    TrashThis(RelPath),
    /// A modified note whose before-bytes were restored with CAS.
    Modified(RelPath),
    /// A moved file whose move was reversed.
    Moved { from: RelPath, to: RelPath },
}

/// The result of an undo operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoOutcome {
    pub op_id: String,
    pub undone: Vec<UndoneItem>,
    pub skipped: Vec<UndoMismatch>,
    /// Inverse steps that landed but could not be written to the audit log.
    pub audit_errors: Vec<String>,
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
/// The apply phase below is deliberately *not* journaled (cf. ADR-016 §7 for
/// the forward path): verification is the all-or-nothing gate, and every
/// inverse that lands appends its own audit line (`record_undo_entry`), so a
/// crash mid-undo leaves the landed inverses in the audit log for the caller
/// to see via `audit_errors` and the follow-up entries. A dedicated undo
/// journal with crash replay is deferred to W63, when `undo` is wired to
/// users.
pub(crate) fn undo(
    writer: &Writer,
    lock: &WriteLock,
    op_id: &str,
    force: bool,
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

    for entry in &op_entries {
        let path_rel = RelPath::parse(&entry.path)?;
        match entry.step_kind {
            StepKind::Create => match writer.read(&path_rel) {
                Ok((bytes, _)) => {
                    let fp = Fingerprint::of_bytes(&bytes);
                    if fp.hex() == entry.after_hash {
                        valid_entries.push(entry.clone());
                    } else {
                        mismatches.push(UndoMismatch {
                            path: path_rel.to_path(),
                            reason: UndoReason::Changed,
                        });
                    }
                }
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    mismatches.push(UndoMismatch {
                        path: path_rel.to_path(),
                        reason: UndoReason::Missing,
                    });
                }
                Err(e) => return Err(e),
            },
            StepKind::Modify => match writer.read(&path_rel) {
                Ok((bytes, _)) => {
                    let fp = Fingerprint::of_bytes(&bytes);
                    let hex = fp.hex();
                    if hex == entry.after_hash {
                        if entry.before_bytes.is_none() {
                            mismatches.push(UndoMismatch {
                                path: path_rel.to_path(),
                                reason: UndoReason::NoRecord,
                            });
                        } else {
                            valid_entries.push(entry.clone());
                        }
                    } else if entry.before_hash.as_deref() == Some(&hex) {
                        mismatches.push(UndoMismatch {
                            path: path_rel.to_path(),
                            reason: UndoReason::Reverted,
                        });
                    } else {
                        mismatches.push(UndoMismatch {
                            path: path_rel.to_path(),
                            reason: UndoReason::Changed,
                        });
                    }
                }
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    mismatches.push(UndoMismatch {
                        path: path_rel.to_path(),
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
                let src_rel = RelPath::parse(src_text)?;

                let dst_read = writer.read(&path_rel);
                let src_read = writer.read(&src_rel);

                let src_exists = src_read.is_ok();
                if src_exists {
                    mismatches.push(UndoMismatch {
                        path: src_rel.to_path(),
                        reason: UndoReason::SourceBack,
                    });
                    continue;
                }

                match dst_read {
                    Ok((bytes, _)) => {
                        let fp = Fingerprint::of_bytes(&bytes);
                        if fp.hex() == entry.after_hash {
                            valid_entries.push(entry.clone());
                        } else {
                            mismatches.push(UndoMismatch {
                                path: path_rel.to_path(),
                                reason: UndoReason::Changed,
                            });
                        }
                    }
                    Err(Error::Io { source, .. })
                        if source.raw_os_error() == Some(libc::ENOENT) =>
                    {
                        mismatches.push(UndoMismatch {
                            path: path_rel.to_path(),
                            reason: UndoReason::Missing,
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

    // Apply inverse actions on valid entries in reverse order
    let mut undone = Vec::new();
    let mut audit_errors: Vec<String> = Vec::new();
    let undo_time = chrono::Utc::now().to_rfc3339();
    let undo_op_id = ulid::Ulid::new().to_string();

    for entry in valid_entries.iter().rev() {
        let path_rel = RelPath::parse(&entry.path)?;
        match entry.step_kind {
            StepKind::Create => {
                // Hand back to caller as "trash this" (ADR-016 §8, W63 wires trash)
                undone.push(UndoneItem::TrashThis(path_rel));
            }
            StepKind::Modify => {
                let (bytes, cur_fp) = writer.read(&path_rel)?;
                let Some(before_bytes) = entry.before_bytes.as_ref() else {
                    return Err(Error::BadJournal {
                        op: op_id.to_string(),
                        message: format!("no before-bytes recorded for {:?}", entry.path),
                    });
                };
                let new_fp =
                    writer.update_existing_locked(lock, &path_rel, &cur_fp, before_bytes)?;
                undone.push(UndoneItem::Modified(path_rel.clone()));

                // Record undo in op log
                let undo_entry = OpEntry {
                    op_id: undo_op_id.clone(),
                    time: undo_time.clone(),
                    source: "undo".to_string(),
                    client_addr: None,
                    intent: serde_json::json!({ "undo_op": op_id }),
                    step_kind: StepKind::Modify,
                    path: entry.path.clone(),
                    src_path: None,
                    before_hash: Some(cur_fp.hex()),
                    after_hash: new_fp.hex(),
                    before_bytes: Some(bytes),
                };
                record_undo_entry(writer, &undo_entry, &mut audit_errors);
            }
            StepKind::Move => {
                let src_text = entry.src_path.as_ref().unwrap();
                let src_rel = RelPath::parse(src_text)?;
                writer.move_noreplace_locked(lock, &path_rel, &src_rel)?;
                undone.push(UndoneItem::Moved {
                    from: path_rel.clone(),
                    to: src_rel.clone(),
                });

                // Record undo in op log
                let undo_entry = OpEntry {
                    op_id: undo_op_id.clone(),
                    time: undo_time.clone(),
                    source: "undo".to_string(),
                    client_addr: None,
                    intent: serde_json::json!({ "undo_op": op_id }),
                    step_kind: StepKind::Move,
                    path: src_text.clone(),
                    src_path: Some(entry.path.clone()),
                    before_hash: entry.before_hash.clone(),
                    after_hash: entry.after_hash.clone(),
                    before_bytes: None,
                };
                record_undo_entry(writer, &undo_entry, &mut audit_errors);
            }
        }
    }

    Ok(UndoOutcome {
        op_id: op_id.to_string(),
        undone,
        skipped: mismatches,
        audit_errors,
    })
}

/// The inverse step landed; a failed audit line is reported, not a failure of
/// the undo itself (the file change is already durable).
fn record_undo_entry(writer: &Writer, entry: &OpEntry, audit_errors: &mut Vec<String>) {
    if let Err(e) = append_op_entry(writer.root_file(), entry) {
        audit_errors.push(format!("{:?}: {e}", entry.path));
    }
}
