//! The summary behind `dimagine scan` (HLD: "walk the library, print a
//! summary"). Read-only: nothing is written anywhere.

use std::collections::BTreeMap;

use crate::library::{FileClass, Library};
use crate::links::key;
use crate::note::parse_note;
use crate::sniff;

/// The `schema` field of the `scan` JSON output.
pub const SCHEMA: &str = "dimagine.scan/0.1";

/// Counts of one kind of file.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ImageNoteCounts {
    pub total: usize,
    pub paired: usize,
    pub unpaired: usize,
}

/// A file whose content could not be read, and why.
#[derive(Clone, Debug, serde::Serialize)]
pub struct UnreadableFile {
    pub path: String,
    pub reason: String,
}

/// Everything `scan` reports about one library.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ScanReport {
    /// The library root as it will be shown to the user.
    pub library: String,
    /// False when some directory could not be listed: counts may miss files
    /// and "found nothing" statements prove nothing (FORMAT §1.5).
    pub read_complete: bool,
    pub unreadable_dirs: Vec<crate::library::UnreadableEntry>,
    pub images: BTreeMap<String, usize>,
    pub image_total: usize,
    pub image_notes: ImageNoteCounts,
    /// All `*.md` notes, including image notes.
    pub notes_total: usize,
    /// Notes whose `kind` is `collection` (FORMAT §5).
    pub collections: usize,
    pub canvases: usize,
    pub raw_files: usize,
    pub other_files: usize,
    pub ignored: crate::library::IgnoredCounts,
    pub unreadable_files: Vec<UnreadableFile>,
}

impl ScanReport {
    /// The exit code for this report per HLD: 0 nothing to report, 1 there
    /// are unreadable files, 3 the walk is incomplete.
    pub fn exit_code(&self) -> i32 {
        if !self.read_complete {
            3
        } else if !self.unreadable_files.is_empty() {
            1
        } else {
            0
        }
    }
}

/// Build the scan summary for a freshly walked library.
pub fn run(library: &Library) -> ScanReport {
    let mut report = ScanReport {
        library: library.root.display().to_string(),
        read_complete: library.fully_read(),
        unreadable_dirs: library.unreadable_dirs.clone(),
        images: BTreeMap::new(),
        image_total: 0,
        image_notes: ImageNoteCounts::default(),
        notes_total: 0,
        collections: 0,
        canvases: 0,
        raw_files: 0,
        other_files: 0,
        ignored: library.ignored,
        unreadable_files: Vec::new(),
    };

    // Pair image notes with their images the same way check does: same
    // folder, same name, NFC-normalised, case-insensitively (FORMAT §3).
    let image_paths: std::collections::HashSet<String> = library
        .files
        .iter()
        .filter(|entry| entry.class == FileClass::Image)
        .map(|entry| key(&entry.rel))
        .collect();

    for entry in &library.files {
        match entry.class {
            FileClass::Image => {
                report.image_total += 1;
                let detected = match std::fs::File::open(library.root.join(&entry.rel)) {
                    Ok(mut file) => {
                        let mut head = [0u8; sniff::SNIFF_LEN];
                        match std::io::Read::read(&mut file, &mut head) {
                            Ok(n) => sniff::detect(&head[..n]).as_str().to_string(),
                            Err(err) => {
                                report.unreadable_files.push(UnreadableFile {
                                    path: entry.rel.clone(),
                                    reason: err.to_string(),
                                });
                                "unknown".to_string()
                            }
                        }
                    }
                    Err(err) => {
                        report.unreadable_files.push(UnreadableFile {
                            path: entry.rel.clone(),
                            reason: err.to_string(),
                        });
                        "unknown".to_string()
                    }
                };
                *report.images.entry(detected).or_default() += 1;
            }
            FileClass::ImageNote | FileClass::Note => {
                report.notes_total += 1;
                if let Some(image_rel) = entry.paired_image_rel() {
                    report.image_notes.total += 1;
                    if image_paths.contains(&key(image_rel)) {
                        report.image_notes.paired += 1;
                    } else {
                        report.image_notes.unpaired += 1;
                    }
                }
                // Only front matter is needed to count collections; unlike
                // check, scan does not turn parse errors into findings, but
                // unreadable note files are still reported.
                match std::fs::read(library.root.join(&entry.rel)) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => {
                            if parse_note(&text).kind.as_deref() == Some("collection") {
                                report.collections += 1;
                            }
                        }
                        Err(_) => report.unreadable_files.push(UnreadableFile {
                            path: entry.rel.clone(),
                            reason: "not valid UTF-8".to_string(),
                        }),
                    },
                    Err(err) => report.unreadable_files.push(UnreadableFile {
                        path: entry.rel.clone(),
                        reason: err.to_string(),
                    }),
                }
            }
            FileClass::Canvas => report.canvases += 1,
            FileClass::Raw => report.raw_files += 1,
            FileClass::Other => report.other_files += 1,
        }
    }
    report
}
