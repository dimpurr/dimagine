//! Trash groups and their records (ADR-017 §7).
//!
//! A trashed group is the image, its note and its raw
//! metadata sidecars, moved by rename into `.trash/`
//! keeping the original relative path. Before the moves,
//! one record file — `<group>.trash.json` — is created
//! exclusively, holding each file's original path and
//! mtime. Restore only ever works from a record, and
//! purge only ever deletes a group that has one; files
//! Obsidian itself put in `.trash/` (flat, no record)
//! are left alone.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::result::Result as StdResult;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs;
use crate::paths::{RelPath, TrashPath, TRASH_DIR};
use crate::writer::Writer;

/// The suffix that marks a trash record file (ADR-017 §7).
pub const RECORD_SUFFIX: &str = ".trash.json";

/// One file of a trashed group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashedFile {
    /// The library-relative path the file had before it
    /// was moved into the trash.
    pub original: String,
    /// The path inside `.trash/` the file moved to.
    pub trash: String,
    /// The file's modification time in nanoseconds since
    /// the Unix epoch, so a restore can put it back
    /// (ADR-017 §7).
    pub mtime_ns: i64,
}

/// The record of one trashed group (ADR-017 §7), written
/// before the group moves and read by every restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashRecord {
    /// The operation that trashed the group.
    pub op_id: String,
    /// When the group was trashed, RFC 3339 UTC: the
    /// retention clock's start (ADR-017 §7).
    pub trashed_at: String,
    /// The group's files: the image, its note and its raw
    /// metadata sidecars, each with its original path and
    /// mtime.
    pub files: Vec<TrashedFile>,
}

/// A record file found under `.trash/`, with what reading
/// it yielded. The failure is the message to report: a
/// record that does not parse is flagged, never silently
/// dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedRecord {
    /// The record's path inside `.trash/`.
    pub path: TrashPath,
    /// The parsed record, or the reason it did not parse.
    pub record: StdResult<TrashRecord, String>,
}

impl TrashRecord {
    /// The record file's path for an image's original
    /// path: `<trash>/<original>.trash.json`. A group
    /// renamed on collision (` 2`, ` 3`, ADR-017 §7)
    /// moves the record with it, so restore finds
    /// records by scanning, never by name.
    pub fn path_for(image: &RelPath) -> TrashPath {
        TrashPath::parse(&format!("{TRASH_DIR}/{}.trash.json", image.text()))
            .expect("the trash door over a validated library path")
    }

    /// The record as JSON bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|e| Error::BadRecord {
            path: self.op_id.clone(),
            message: format!("cannot serialize: {e}"),
        })
    }

    /// Parse a record from its JSON bytes.
    pub fn parse(path_text: &str, bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| Error::BadRecord {
            path: path_text.to_string(),
            message: e.to_string(),
        })
    }
}

/// Every trash record in the library, in walk order.
/// Symlinked entries are not followed, and a record
/// that does not parse is reported, not dropped.
pub fn scan_records(writer: &Writer) -> Result<Vec<ScannedRecord>> {
    let root = writer.root_file();
    let trash_dir = match fs::resolve_dir(root, &[TRASH_DIR.to_string()], false) {
        Ok(dir) => dir,
        Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
            return Ok(Vec::new())
        }
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    walk_records(&trash_dir, TRASH_DIR, &mut out)?;
    Ok(out)
}

fn walk_records(dir: &File, prefix: &str, out: &mut Vec<ScannedRecord>) -> Result<()> {
    for name in fs::list_names(dir)? {
        let path_text = format!("{prefix}/{name}");
        if name.ends_with(RECORD_SUFFIX) {
            let path = TrashPath::parse(&path_text)?;
            let record = match fs::read_leaf(dir, &name) {
                Ok((bytes, _)) => TrashRecord::parse(&path_text, &bytes).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            out.push(ScannedRecord { path, record });
        }
        // Descend into subdirectories: a trashed group
        // keeps its relative path (ADR-017 §7).
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
            let child = unsafe { File::from_raw_fd(raw) };
            walk_records(&child, &path_text, out)?;
        }
    }
    Ok(())
}

/// The path a file keeps when its group is renamed on a
/// trash collision: ` 2`, ` 3`, ... inserted before the
/// image's extension (ADR-017 §7). `round` 1 is the
/// name itself.
pub fn bumped_name(name: &str, round: usize) -> String {
    if round <= 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() => format!("{stem} {round}.{ext}"),
        _ => format!("{name} {round}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_path_mirrors_the_original() {
        let image = RelPath::parse("refs/deep-sea/girl.jpg").unwrap();
        let record = TrashRecord::path_for(&image);
        assert_eq!(record.text(), ".trash/refs/deep-sea/girl.jpg.trash.json");
        // The record sits beside the group it describes:
        // its own folder is the image's folder.
        assert_eq!(
            record.components()[1..record.components().len() - 1],
            image.components()[..image.components().len() - 1]
        );
    }

    #[test]
    fn bumped_names_insert_before_the_extension() {
        assert_eq!(bumped_name("girl.jpg", 1), "girl.jpg");
        assert_eq!(bumped_name("girl.jpg", 2), "girl 2.jpg");
        assert_eq!(bumped_name("girl.photo.jpg", 3), "girl.photo 3.jpg");
        assert_eq!(bumped_name("noext", 2), "noext 2");
    }

    #[test]
    fn record_round_trips() {
        let record = TrashRecord {
            op_id: "01H".to_string(),
            trashed_at: "2026-10-13T00:00:00Z".to_string(),
            files: vec![TrashedFile {
                original: "refs/girl.jpg".to_string(),
                trash: ".trash/refs/girl.jpg".to_string(),
                mtime_ns: 1_700_000_000_000_000_000,
            }],
        };
        let bytes = record.to_bytes().unwrap();
        assert_eq!(
            TrashRecord::parse(".trash/refs/girl.jpg.trash.json", &bytes).unwrap(),
            record
        );
        assert!(TrashRecord::parse(".trash/x.trash.json", b"{oops").is_err());
    }
}
