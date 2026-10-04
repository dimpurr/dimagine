//! The summary behind `dimagine scan` (HLD: "walk the library, print a
//! summary"). Read-only: nothing is written anywhere.

use std::collections::BTreeMap;

use crate::library::{open_regular, FileClass, Library};
use crate::links::{extract_markdown_links, LinkSyntax, Outcome, Resolver};
use crate::note::parse_note;
use crate::sniff;

/// The `schema` field of the `scan` JSON output.
pub const SCHEMA: &str = "dimagine.scan/0.1";
pub const DEFAULT_MAX_NOTE_BYTES: u64 = 8 * 1024 * 1024;

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
    pub path_non_utf8: bool,
    pub reason: String,
}

/// Everything `scan` reports about one library.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ScanReport {
    /// The library root as it will be shown to the user.
    pub library: String,
    /// At least one displayed path contains lossy replacement characters.
    pub has_non_utf8_paths: bool,
    /// False when some directory could not be listed: counts may miss files
    /// and "found nothing" statements prove nothing (FORMAT §1.5).
    pub read_complete: bool,
    pub read_warning: Option<&'static str>,
    pub unreadable_dirs: Vec<crate::library::UnreadableEntry>,
    pub images: BTreeMap<String, usize>,
    pub image_total: usize,
    pub image_notes: ImageNoteCounts,
    /// All `*.md` notes, including image notes.
    pub notes_total: usize,
    /// Notes whose `kind` is `collection` (FORMAT §5).
    pub collections: usize,
    pub collections_explicit: usize,
    pub collections_embedded: usize,
    pub collections_unknown: usize,
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
    run_with_limit(library, DEFAULT_MAX_NOTE_BYTES)
}

pub fn run_with_limit(library: &Library, max_note_bytes: u64) -> ScanReport {
    let mut report = ScanReport {
        library: library.root.display().to_string(),
        has_non_utf8_paths: library.root.to_str().is_none()
            || library.files.iter().any(|f| f.path.to_str().is_none()),
        read_complete: library.fully_read(),
        read_warning: (!library.fully_read()).then_some(
            "Reading did not finish; counts may miss files and missing results prove nothing.",
        ),
        unreadable_dirs: library.unreadable_dirs.clone(),
        images: BTreeMap::new(),
        image_total: 0,
        image_notes: ImageNoteCounts::default(),
        notes_total: 0,
        collections: 0,
        collections_explicit: 0,
        collections_embedded: 0,
        collections_unknown: 0,
        canvases: 0,
        raw_files: 0,
        other_files: 0,
        ignored: library.ignored,
        unreadable_files: Vec::new(),
    };

    // Pair image notes with their images the same way check does: same
    // folder, same name, NFC-normalised, case-insensitively (FORMAT §3).
    let image_paths: std::collections::HashSet<std::path::PathBuf> = library
        .files
        .iter()
        .filter(|entry| entry.class == FileClass::Image)
        .map(|entry| entry.path.clone())
        .collect();
    let resolver = Resolver::new(&library.files);

    for entry in &library.files {
        match entry.class {
            FileClass::Image => {
                report.image_total += 1;
                let detected = match open_regular(&library.root, entry) {
                    Ok(mut file) => {
                        let mut head = [0u8; sniff::SNIFF_LEN];
                        match std::io::Read::read(&mut file, &mut head) {
                            Ok(n) => sniff::detect(&head[..n]).as_str().to_string(),
                            Err(err) => {
                                report.unreadable_files.push(UnreadableFile {
                                    path: entry.rel.clone(),
                                    path_non_utf8: entry.path.to_str().is_none(),
                                    reason: err.to_string(),
                                });
                                "unknown".to_string()
                            }
                        }
                    }
                    Err(err) => {
                        report.unreadable_files.push(UnreadableFile {
                            path: entry.rel.clone(),
                            path_non_utf8: entry.path.to_str().is_none(),
                            reason: err.to_string(),
                        });
                        "unknown".to_string()
                    }
                };
                *report.images.entry(detected).or_default() += 1;
            }
            FileClass::ImageNote | FileClass::Note => {
                report.notes_total += 1;
                if entry.paired_image_path().is_some() {
                    report.image_notes.total += 1;
                    if entry
                        .paired_image_path()
                        .is_some_and(|p| image_paths.contains(&p))
                    {
                        report.image_notes.paired += 1;
                    } else {
                        report.image_notes.unpaired += 1;
                    }
                }
                // Only front matter is needed to count collections; unlike
                // check, scan does not turn parse errors into findings, but
                // unreadable note files are still reported.
                let mut opened = open_regular(&library.root, entry);
                match opened.as_ref().map(|file| file.metadata()) {
                    Ok(Ok(meta)) if meta.len() > max_note_bytes => {
                        report.unreadable_files.push(UnreadableFile {
                            path: entry.rel.clone(),
                            path_non_utf8: entry.path.to_str().is_none(),
                            reason: format!("note exceeds the {max_note_bytes}-byte read limit"),
                        });
                        report.collections_unknown += 1;
                    }
                    _ => match opened
                        .as_mut()
                        .map_err(|err| std::io::Error::new(err.kind(), err.to_string()))
                        .and_then(|file| {
                            use std::io::Read;
                            let mut bytes = Vec::new();
                            file.take(max_note_bytes.saturating_add(1))
                                .read_to_end(&mut bytes)
                                .map(|_| bytes)
                        }) {
                        Ok(bytes) if bytes.len() as u64 > max_note_bytes => {
                            report.unreadable_files.push(UnreadableFile {
                                path: entry.rel.clone(),
                                path_non_utf8: entry.path.to_str().is_none(),
                                reason: format!(
                                    "note exceeds the {max_note_bytes}-byte read limit"
                                ),
                            });
                            report.collections_unknown += 1;
                        }
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(text) => {
                                let parsed = parse_note(&text);
                                match parsed.kind.as_deref() {
                                    Some("collection") => report.collections_explicit += 1,
                                    Some(_) | None if parsed.error.is_none() => {}
                                    _ => report.collections_unknown += 1,
                                }
                                let mut embedded = false;
                                for link in extract_markdown_links(&parsed.body, parsed.body_line) {
                                    if link.syntax != LinkSyntax::WikiEmbed
                                        && link.syntax != LinkSyntax::MarkdownImage
                                    {
                                        continue;
                                    }
                                    if let Outcome::Resolved(idx) =
                                        resolver.resolve(&link.target, entry.dir(), link.syntax)
                                    {
                                        if library.files[idx].class == FileClass::Image
                                            && !(entry.class == FileClass::ImageNote
                                                && entry.paired_image_path().as_ref()
                                                    == Some(&library.files[idx].path))
                                        {
                                            embedded = true;
                                        }
                                    }
                                }
                                if embedded {
                                    report.collections_embedded += 1;
                                }
                            }
                            Err(_) => {
                                report.unreadable_files.push(UnreadableFile {
                                    path: entry.rel.clone(),
                                    path_non_utf8: entry.path.to_str().is_none(),
                                    reason: "not valid UTF-8".to_string(),
                                });
                                report.collections_unknown += 1;
                            }
                        },
                        Err(err) => {
                            report.unreadable_files.push(UnreadableFile {
                                path: entry.rel.clone(),
                                path_non_utf8: entry.path.to_str().is_none(),
                                reason: err.to_string(),
                            });
                            report.collections_unknown += 1;
                        }
                    },
                }
            }
            FileClass::Canvas => report.canvases += 1,
            FileClass::Raw => report.raw_files += 1,
            FileClass::Other => report.other_files += 1,
            FileClass::Special => report.unreadable_files.push(UnreadableFile {
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                reason: "special filesystem entry skipped".to_string(),
            }),
        }
    }
    report.collections = report.collections_explicit;
    report.read_complete &= report.unreadable_files.is_empty();
    if !report.read_complete {
        report.read_warning = Some(
            "Reading did not finish; counts may miss files and missing results prove nothing.",
        );
    }
    report
}
