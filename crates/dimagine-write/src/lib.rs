//! dimagine-write: the only code that writes inside a library (ADR-016).
//!
//! Invariant, in one sentence (ADR-016 §0): image bytes and raw metadata
//! sidecars are never opened for writing; every new file is created
//! exclusively; every commit compares hashes first; all dimagine writers share
//! one lock; and the only state outside notes, image groups and `.trash/` is
//! under `.dimagine/`.
//!
//! The crate is a library handle plus two durable primitives and the layers on
//! top of them:
//!
//! - [`Writer`] opens a library root once and turns caller paths into
//!   [`RelPath`]: validated (no NUL, no `..`, no absolute path, no
//!   dot-prefixed component) and resolved without following symlinks
//!   (ADR-018 §4).
//! - [`Writer::update_existing`] and [`Writer::create_exclusive`] are the only
//!   two ways bytes reach an existing or a new file; both are durable
//!   (temp + fsync + rename + directory fsync) and both compare the caller's
//!   [`Fingerprint`] before renaming (ADR-016 §3, §4).
//! - [`Writer::apply_intent`] replays a byte-transformer over a note until
//!   either the compare-and-swap commit lands or three attempts have
//!   conflicted.
//! - [`Writer::execute_op`] records an operation's steps in
//!   `.dimagine/journal/` before and while touching user files, so a crash
//!   leaves something [`Writer::startup_sweep`] can reason about (ADR-016 §7).
//! - The operation log (`.dimagine/log/ops.jsonl`) is the audit trail and the
//!   basis of [`Writer::undo`] (ADR-016 §8).
//!
//! Locking is deliberately coarse: one process-wide mutex plus an exclusive
//! `flock` on `.dimagine/write.lock`, held across "journal → steps → journal
//! delete". There are no per-path locks, because a multi-file operation taking
//! them would deadlock against itself (ADR-016 §6). The lock does not constrain
//! people, Obsidian or agents; protection against their edits is the
//! compare-and-swap.
//!
//! CLI and HTTP wiring is W63/W64, and so is the trash hand-back: this crate
//! normally reports a created file as "hand this to the trash"; the tag alias
//! file is moved to recorded trash within undo.

pub mod error;
pub mod fingerprint;
pub mod fs;
pub mod journal;
pub mod lock;
pub mod op_log;
pub mod paths;
pub mod tag_batch;
pub mod trash;
pub mod upload;
pub mod writer;

pub use error::{
    BatchStepError, Conflict, Error, Exists, PathReject, Result, UndoMismatch, UndoReason,
};
pub use fingerprint::Fingerprint;
pub use journal::{RecoveryOutcome, StepKind, StepRecord, StepState};
pub use lock::WriteLock;
pub use op_log::{OpEntry, UndoOutcome, UndoneItem};
pub use paths::{NoteKind, RelPath, StepPath, TrashPath};
pub use trash::{ScannedRecord, TrashRecord, TrashedFile, RECORD_SUFFIX};
pub use upload::{UploadItem, UploadOutcome, UploadRequest, MAX_UPLOAD_BATCH};
pub use writer::{Precondition, SweepReport, Writer, SETTINGS_DIR, SETTINGS_FILE};
