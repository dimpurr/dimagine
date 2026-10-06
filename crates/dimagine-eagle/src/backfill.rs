//! Backfill the `added` property into already-imported Eagle notes.
//!
//! `dimagine import eagle --backfill-added <library>` walks a dimagine library
//! and, for every image note that has a sibling `<image>.<ext>.eagle.json`
//! raw file (FORMAT §3.4) and no `added` property yet, inserts `added:`
//! rendered from that raw file's `btime` (epoch milliseconds) as ISO 8601
//! with the local UTC offset — the same value a fresh import would write.
//!
//! Every other byte of the note is preserved: unknown properties, their
//! order, the body, the self-embed and a leading UTF-8 BOM, because the
//! property is inserted as text right after the `imported:` line (or right
//! after the opening `---` when the note has no `imported:`), never by
//! re-serialising the front matter. The default is a dry run; `apply` writes
//! atomically (temp file + rename).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use dimagine_core::{FileClass, Library};

use crate::{btime_text, ImportError};

/// What a backfill run did (or, in a dry run, would do).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct BackfillReport {
    /// Image notes examined: they have a sibling `.eagle.json` raw file.
    pub scanned: usize,
    /// Notes that received (or, dry run, would receive) an `added:` property.
    pub updated: usize,
    /// Notes skipped because they already carry an `added:` property.
    pub already_had: usize,
    /// Notes skipped because their raw file carries no usable `btime`, either
    /// because the property is absent or because the file is not metadata at
    /// all.
    pub no_btime: usize,
    /// Notes skipped because they have no front matter to insert into, which is
    /// neither an existing `added:` nor a missing one.
    pub no_front_matter: usize,
    /// Notes skipped because the note or its raw file could not be read, which
    /// is a different state from "there is no btime in there".
    pub unreadable: usize,
}

/// Backfill `added` into the Eagle notes of `library`.
///
/// With `apply` false nothing is written and `updated` counts the notes that
/// would change; with `apply` true each changed note is rewritten atomically.
pub fn backfill_added(library: &Path, apply: bool) -> Result<BackfillReport, ImportError> {
    let library = Library::open(library)
        .map_err(|error| ImportError::InvalidSource(format!("not a dimagine library: {error}")))?;
    let raw_paths: HashSet<&str> = library
        .files
        .iter()
        .filter(|entry| entry.class == FileClass::Raw)
        .map(|entry| entry.rel.as_str())
        .collect();
    let mut report = BackfillReport::default();
    for entry in &library.files {
        if entry.class != FileClass::ImageNote {
            continue;
        }
        let Some(image_rel) = entry.paired_image_rel() else {
            continue;
        };
        let raw_rel = format!("{image_rel}.eagle.json");
        if !raw_paths.contains(raw_rel.as_str()) {
            continue;
        }
        report.scanned += 1;
        let note_path = library.root.join(&entry.rel);
        let text = match fs::read_to_string(&note_path) {
            Ok(text) => text,
            Err(_) => {
                report.unreadable += 1;
                continue;
            }
        };
        if note_has_property(&text, "added") {
            report.already_had += 1;
            continue;
        }
        let raw_path = library.root.join(&raw_rel);
        let raw = match fs::read(&raw_path) {
            Ok(raw) => raw,
            Err(_) => {
                report.unreadable += 1;
                continue;
            }
        };
        let metadata: serde_json::Value = match serde_json::from_slice(&raw) {
            Ok(metadata) => metadata,
            Err(_) => {
                report.no_btime += 1;
                continue;
            }
        };
        let Some(added) = btime_text(&metadata) else {
            report.no_btime += 1;
            continue;
        };
        let Some(updated) = insert_added(&text, &added) else {
            // No front matter is not an `added:` that is already there; saying
            // "already had added" would claim the library is up to date.
            report.no_front_matter += 1;
            continue;
        };
        if apply {
            crate::write_atomically(&note_path, updated.as_bytes())?;
        }
        report.updated += 1;
    }
    Ok(report)
}

/// Whether the note's front matter already defines a top-level `name`
/// property. Only column-0 `name:` lines count; nested keys are ignored.
fn note_has_property(text: &str, name: &str) -> bool {
    let Some(yaml) = front_matter(text) else {
        return false;
    };
    yaml.lines()
        .any(|line| line.starts_with(&format!("{name}:")))
}

/// The text between the `---` fences, or `None` when the note has no front
/// matter (or an unterminated block).
fn front_matter(text: &str) -> Option<&str> {
    let parts = split_front_matter(text)?;
    Some(parts.yaml)
}

struct FmParts<'a> {
    /// The leading UTF-8 BOM, empty when the note has none. Kept so a rewrite
    /// never silently drops it (FORMAT §3.1).
    bom: &'a str,
    /// Up to and including the opening `---` line.
    prefix: &'a str,
    /// Between the fences.
    yaml: &'a str,
    /// The closing `---` line and everything after it.
    rest: &'a str,
}

/// Split a note into the three byte ranges that [`insert_added`] reassembles,
/// so every byte outside the inserted property is preserved exactly.
fn split_front_matter(text: &str) -> Option<FmParts<'_>> {
    let (bom, s) = match text.strip_prefix('\u{feff}') {
        Some(without_bom) => ("\u{feff}", without_bom),
        None => ("", text),
    };
    let first_line_end = s.find('\n')?;
    if s[..first_line_end].trim_end_matches('\r') != "---" {
        return None;
    }
    let yaml_start = (first_line_end + 1).min(s.len());
    let mut cursor = yaml_start;
    loop {
        let line_end = match s[cursor..].find('\n') {
            Some(offset) => cursor + offset,
            None => s.len(),
        };
        if s[cursor..line_end].trim_end_matches('\r').trim_end() == "---" {
            return Some(FmParts {
                bom,
                prefix: &s[..yaml_start],
                yaml: &s[yaml_start..cursor],
                rest: &s[cursor..],
            });
        }
        if line_end == s.len() {
            return None;
        }
        cursor = line_end + 1;
    }
}

/// Insert `added: <value>` into the note's front matter, right after the
/// `imported:` line (or right after the opening `---` when there is none).
/// The inserted line matches the note's line ending. Returns `None` when the
/// note has no front matter to insert into.
fn insert_added(text: &str, added: &str) -> Option<String> {
    let parts = split_front_matter(text)?;
    let yaml = parts.yaml;
    let insert_at = after_line_starting_with(yaml, "imported:").unwrap_or(0);
    let newline = if yaml.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + added.len() + 16);
    out.push_str(parts.bom);
    out.push_str(parts.prefix);
    out.push_str(&yaml[..insert_at]);
    out.push_str(&format!("added: {added}{newline}"));
    out.push_str(&yaml[insert_at..]);
    out.push_str(parts.rest);
    Some(out)
}

/// Byte offset just past the first line that starts with `prefix`, including
/// its line terminator. `None` when no such line exists.
fn after_line_starting_with(yaml: &str, prefix: &str) -> Option<usize> {
    let mut offset = 0;
    for line in yaml.split_inclusive('\n') {
        if line.starts_with(prefix) {
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    None
}
