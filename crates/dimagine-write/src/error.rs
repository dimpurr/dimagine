//! Errors and the reasons a path or a commit attempt is refused.

use std::io;
use std::path::PathBuf;

use crate::fingerprint::Fingerprint;

/// Why a caller-supplied path was refused before touching the filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathReject {
    /// The path is absolute.
    Absolute,
    /// A component is `.` or `..`, or the path contains a NUL.
    Traversal,
    /// A component starts with `.`: hidden directories (`.git`, `.dimagine`,
    /// `.obsidian`, `.trash`) are unreachable through the ordinary API.
    Hidden,
    /// A component is not valid UTF-8.
    NonUtf8,
    /// The path is empty.
    Empty,
    /// A note target must be `.md` or `.canvas`; images and `*.json` sidecars
    /// are never opened for writing (ADR-016 invariant, FORMAT §2.1).
    NotANote,
    /// Resolution hit a symlink: neither a directory component nor the leaf is
    /// followed (ADR-018 §4).
    Symlink,
    /// The resolved location is not inside the library root, compared at a
    /// separator boundary so `library-other` does not look like `library/`.
    OutsideRoot,
    /// A permanent deletion may only target a file inside `.trash/`
    /// (ADR-017 §7): library content is never hard-deleted by this
    /// crate.
    NotInTrash,
}

impl PathReject {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Absolute => "path must be library-relative, not absolute",
            Self::Traversal => "path must not contain '.', '..' or a NUL byte",
            Self::Hidden => "path must not name a dot-prefixed directory or file",
            Self::NonUtf8 => "path components must be valid UTF-8",
            Self::Empty => "path must not be empty",
            Self::NotANote => "only note files (.md, .canvas) may be updated",
            Self::Symlink => "path resolution refuses symlinks",
            Self::OutsideRoot => "path resolves outside the library root",
            Self::NotInTrash => "a permanent deletion must be inside .trash/",
        }
    }
}

/// The outcome of `create_exclusive` when the name is taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{path:?} already exists")]
pub struct Exists {
    pub path: PathBuf,
}

/// A compare-and-swap failure: the file is not the version the caller read.
///
/// `current_bytes` lets the caller recompute its intent on the real bytes
/// (ADR-016 §3) or show them to the user (the 409 flow in ADR-016 §3).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("conflict on {path:?}: now {}, {} bytes; expected {}", current.hex(), current_bytes.len(), expected.hex())]
pub struct Conflict {
    pub path: PathBuf,
    pub expected: Fingerprint,
    pub current: Fingerprint,
    pub current_bytes: Vec<u8>,
}

/// One step of a batch that did not land (ADR-016 §7 partial failure).
///
/// The rest of the batch is unaffected: steps that reached the disk count as
/// success and are already in the audit log, so this only describes the steps
/// that did not. A step with `landed_unaudited` is the one exception: its
/// write reached the disk but its audit-log line did not, so the change is
/// real and must never be retried as a fresh write (RW60b F3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchStepError {
    pub step_idx: usize,
    pub path: PathBuf,
    pub message: String,
    /// The step's write landed on disk but its audit-log line did not.
    pub landed_unaudited: bool,
}

/// Everything that can go wrong in this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A path was refused by validation or by symlink-safe resolution.
    #[error("{path:?}: {}", reject.message())]
    BadPath { path: String, reject: PathReject },
    /// A compare-and-swap commit found the file changed underneath it.
    /// Boxed to keep `Error` small enough for `Result` returns.
    #[error(transparent)]
    Conflict(#[from] Box<Conflict>),
    /// The name already exists; the caller picks the next ` 2` suffix.
    #[error(transparent)]
    Exists(#[from] Exists),
    /// The operation targets a note whose front matter this crate must not
    /// rewrite blindly (the caller's intent builder refuses; kept as an error
    /// so intents can fail without panicking).
    #[error("intent refused for {path:?}: {message}")]
    IntentRefused { path: String, message: String },
    /// An operation could not be undone as a whole.
    #[error("undo of {op} refused: {mismatches:?}")]
    UndoRefused {
        op: String,
        mismatches: Vec<UndoMismatch>,
    },
    /// No operation with this id is in the log.
    #[error("no logged operation {0:?}")]
    UnknownOp(String),
    /// A batch partly landed: some steps are on disk and in the audit log,
    /// the listed steps are not. The operation reached a terminal state (the
    /// journal is gone, so recovery cannot roll the landed files back);
    /// retry or rename only the listed steps (ADR-016 §7).
    #[error("operation {op} partially applied: {errors:?}")]
    BatchPartial {
        op: String,
        errors: Vec<BatchStepError>,
    },
    /// A journal entry could not be read. It is reported, never applied
    /// (ADR-016 §7).
    #[error("unparsable journal {op:?}: {message}")]
    BadJournal { op: String, message: String },
    /// The step's write landed on disk, but its audit-log line did not
    /// (RW60b F3). The change is durable and real: a caller must not
    /// retry the step as a fresh write, and the file keeps no audit
    /// trail of the change.
    #[error("write to {path:?} landed but its audit line failed: {message}")]
    LandedButUnaudited { path: String, message: String },
    /// A trash record could not be parsed (ADR-017 §7). It is
    /// reported, never silently dropped: the group it describes is
    /// neither restored, counted nor purged.
    #[error("unparsable trash record {path:?}: {message}")]
    BadRecord { path: String, message: String },
    /// The library root is not usable.
    #[error("library root {0:?}")]
    BadRoot(String),
    /// Any filesystem failure. ENOSPC and friends land here: the temp file is
    /// removed, the original is untouched, and this error is returned
    /// (ADR-016 §4).
    #[error("{context}")]
    Io {
        context: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A participant that is not at its recorded after-hash, so `undo` refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoMismatch {
    pub path: PathBuf,
    /// Why this file blocks the undo.
    pub reason: UndoReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoReason {
    /// The file is at a hash that is neither the before nor the after value.
    Changed,
    /// The file is back at its before-hash already.
    Reverted,
    /// The file is gone.
    Missing,
    /// A move's source reappeared.
    SourceBack,
    /// A modify was logged without its before-bytes, so there is nothing to
    /// restore: undo must not pretend to have done something.
    NoRecord,
    /// The step permanently deleted its file (a purged trash
    /// group, ADR-017 §7): there is nothing to restore, so the
    /// operation is not undoable.
    Permanent,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(
    context: impl Into<String>,
    path: impl Into<PathBuf>,
    source: io::Error,
) -> Error {
    Error::Io {
        context: context.into(),
        path: path.into(),
        source,
    }
}

pub(crate) fn conflict_err(conflict: Conflict) -> Error {
    Error::Conflict(Box::new(conflict))
}
