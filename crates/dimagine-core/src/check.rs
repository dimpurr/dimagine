//! Findings behind `dimagine check` (HLD "check findings" + FORMAT).
//!
//! `check` NEVER changes files. Every problem found is a [`Finding`] with a
//! stable snake_case code, a severity, the file's library-relative path and a
//! human message; line/column detail rides along when known.
//!
//! Exit-code policy (HLD): a run that could not read the whole library exits
//! 3 and its missing/ambiguous results prove nothing; otherwise error or
//! warning findings make exit 1, while `info` findings (a missing self-embed
//! is not an error, FORMAT §3.2) stay non-failing.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::format;
use crate::library::{open_regular, FileClass, FileEntry, Library};
use crate::links::{self, key, Link, Outcome, Resolver, TargetClass};
use crate::note::{self, IdProperty};
use crate::sniff;

pub const SCHEMA: &str = "dimagine.check/0.1";
pub const DEFAULT_MAX_NOTE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// Stable snake_case finding codes.
pub mod codes {
    pub const UNREADABLE_IMAGE: &str = "unreadable_image";
    pub const UNREADABLE_FILE: &str = "unreadable_file";
    pub const FORMAT_MISMATCH: &str = "format_mismatch";
    pub const NOTE_WITHOUT_IMAGE: &str = "note_without_image";
    pub const INVALID_YAML: &str = "invalid_yaml";
    pub const INVALID_ID: &str = "invalid_id";
    pub const DUPLICATE_ID: &str = "duplicate_id";
    pub const MISSING_LINK: &str = "missing_link";
    pub const AMBIGUOUS_LINK: &str = "ambiguous_link";
    pub const INVALID_CANVAS: &str = "invalid_canvas";
    pub const BAD_FILENAME_CHAR: &str = "bad_filename_char";
    pub const MISSING_SELF_EMBED: &str = "missing_self_embed";
    pub const SPECIAL_FILE: &str = "special_file";
    pub const NOTE_TOO_LARGE: &str = "note_too_large";
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub path: String,
    pub path_non_utf8: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    /// The library root as it will be shown to the user.
    pub library: String,
    /// At least one displayed path contains lossy replacement characters.
    pub has_non_utf8_paths: bool,
    /// False when some directory could not be listed: missing and ambiguous
    /// link findings then prove nothing (FORMAT §1.5).
    pub read_complete: bool,
    pub read_warning: Option<&'static str>,
    pub unreadable_dirs: Vec<crate::library::UnreadableEntry>,
    pub findings: Vec<Finding>,
}

impl CheckReport {
    /// Problems that make `check` exit 1: anything above info level.
    pub fn has_problems(&self) -> bool {
        self.findings.iter().any(|f| f.severity != Severity::Info)
    }

    /// `[errors, warnings, infos]`.
    pub fn counts(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for finding in &self.findings {
            match finding.severity {
                Severity::Error => counts[0] += 1,
                Severity::Warning => counts[1] += 1,
                Severity::Info => counts[2] += 1,
            }
        }
        counts
    }

    /// The exit code for this report per HLD.
    pub fn exit_code(&self) -> i32 {
        if !self.read_complete {
            3
        } else if self.has_problems() {
            1
        } else {
            0
        }
    }
}

/// Run all 0.1 checks over a walked library. Read-only.
pub fn run(library: &Library) -> CheckReport {
    run_with_limit(library, DEFAULT_MAX_NOTE_BYTES)
}

pub fn run_with_limit(library: &Library, max_note_bytes: u64) -> CheckReport {
    let mut report = CheckReport {
        library: library.root.display().to_string(),
        has_non_utf8_paths: library.root.to_str().is_none()
            || library.files.iter().any(|f| f.path.to_str().is_none()),
        read_complete: library.fully_read(),
        read_warning: (!library.fully_read())
            .then_some("Reading did not finish; missing and ambiguous results prove nothing."),
        unreadable_dirs: library.unreadable_dirs.clone(),
        findings: Vec::new(),
    };
    let resolver = Resolver::new(&library.files);
    // Image notes pair with same-folder images (FORMAT §3).
    let image_paths: HashSet<std::path::PathBuf> = library
        .files
        .iter()
        .filter(|f| f.class == FileClass::Image)
        .map(|f| f.path.clone())
        .collect();
    // id key -> (note path, id as written), NFC + case-insensitive.
    let mut ids: HashMap<String, Vec<(String, String, bool)>> = HashMap::new();

    for entry in &library.files {
        check_file_name(entry, &mut report.findings);
        match entry.class {
            FileClass::Image => {
                check_image(library, entry, &mut report.findings);
            }
            FileClass::ImageNote | FileClass::Note => {
                check_note(
                    library,
                    entry,
                    &image_paths,
                    &resolver,
                    &mut ids,
                    &mut report.findings,
                    max_note_bytes,
                );
            }
            FileClass::Canvas => {
                check_canvas(library, entry, &resolver, &mut report.findings);
            }
            FileClass::Raw | FileClass::Other => {}
            FileClass::Special => report.findings.push(Finding {
                severity: Severity::Error,
                code: codes::SPECIAL_FILE,
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                message: "special filesystem entry was skipped".to_string(),
                detail: None,
            }),
        }
    }

    for (_, mut notes) in ids {
        if notes.len() > 1 {
            notes.sort();
            let paths: Vec<&str> = notes.iter().map(|(path, _, _)| path.as_str()).collect();
            let path_lossy: Vec<bool> = notes.iter().map(|(_, _, lossy)| *lossy).collect();
            let id = &notes[0].1;
            report.findings.push(Finding {
                severity: Severity::Error,
                code: codes::DUPLICATE_ID,
                path: notes[0].0.clone(),
                path_non_utf8: notes[0].2,
                message: format!("duplicate id `{id}` is used by {} notes", paths.len()),
                detail: Some(
                    serde_json::json!({ "id": id, "paths": paths, "paths_non_utf8": path_lossy }),
                ),
            });
        }
    }

    report.read_complete &= !report.findings.iter().any(|finding| {
        matches!(
            finding.code,
            codes::UNREADABLE_IMAGE
                | codes::UNREADABLE_FILE
                | codes::NOTE_TOO_LARGE
                | codes::SPECIAL_FILE
        )
    });
    if !report.read_complete {
        report.read_warning =
            Some("Reading did not finish; missing and ambiguous results prove nothing.");
    }
    report
        .findings
        .sort_by(|a, b| (&a.path, a.code, &a.message).cmp(&(&b.path, b.code, &b.message)));
    report
}

fn check_file_name(entry: &FileEntry, findings: &mut Vec<Finding>) {
    let bad = format::banned_characters(&entry.name);
    if !bad.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            code: codes::BAD_FILENAME_CHAR,
            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
            message: format!(
                "file name contains {} (these characters break wikilinks)",
                bad.iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            detail: Some(serde_json::json!({ "characters": bad })),
        });
    }
}

/// Sniff every image (FORMAT §2.1): never decode, just read the header.
fn check_image(library: &Library, entry: &FileEntry, findings: &mut Vec<Finding>) {
    let Some(ext) = format::file_extension(&entry.name) else {
        return;
    };
    let Some(expected) = format::FormatFamily::from_extension(ext) else {
        return;
    };
    let read = match read_head(library, entry, sniff::SNIFF_LEN) {
        Ok(bytes) => bytes,
        Err(message) => {
            findings.push(Finding {
                severity: Severity::Error,
                code: codes::UNREADABLE_IMAGE,
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                message,
                detail: None,
            });
            return;
        }
    };
    let detected = sniff::detect(&read);
    if detected.family().is_none_or(|family| family != expected) {
        findings.push(Finding {
            severity: Severity::Error,
            code: codes::FORMAT_MISMATCH,
            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
            message: format!(
                "content does not match the extension: expected {} content, sniffed {}",
                expected.as_str(),
                detected.as_str()
            ),
            detail: Some(serde_json::json!({
                "expected": expected.as_str(),
                "sniffed": detected.as_str(),
            })),
        });
    }
}

/// Check one note: front matter, `id`, pairing, links and the self-embed.
fn check_note(
    library: &Library,
    entry: &FileEntry,
    image_paths: &HashSet<std::path::PathBuf>,
    resolver: &Resolver,
    ids: &mut HashMap<String, Vec<(String, String, bool)>>,
    findings: &mut Vec<Finding>,
    max_note_bytes: u64,
) {
    let file = match open_regular(&library.root, entry) {
        Ok(file) => file,
        Err(err) => {
            findings.push(unreadable(entry, format!("cannot read the note: {err}")));
            return;
        }
    };
    if file
        .metadata()
        .is_ok_and(|meta| meta.len() > max_note_bytes)
    {
        findings.push(Finding {
            severity: Severity::Error,
            code: codes::NOTE_TOO_LARGE,
            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
            message: format!("note exceeds the {max_note_bytes}-byte read limit"),
            detail: Some(serde_json::json!({"limit_bytes": max_note_bytes})),
        });
        return;
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    let read_result = file
        .take(max_note_bytes.saturating_add(1))
        .read_to_end(&mut bytes);
    let bytes = match read_result {
        Ok(_) => bytes,
        Err(err) => {
            findings.push(unreadable(entry, format!("cannot read the note: {err}")));
            return;
        }
    };
    if bytes.len() as u64 > max_note_bytes {
        findings.push(Finding {
            severity: Severity::Error,
            code: codes::NOTE_TOO_LARGE,
            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
            message: format!("note exceeds the {max_note_bytes}-byte read limit"),
            detail: Some(serde_json::json!({"limit_bytes": max_note_bytes})),
        });
        return;
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            findings.push(unreadable(entry, "note is not valid UTF-8".to_string()));
            return;
        }
    };
    let parsed = note::parse_note(&text);

    if let Some(error) = &parsed.error {
        let location = match (error.line, error.column) {
            (Some(line), Some(column)) => format!(" (at line {line}, column {column})"),
            _ => String::new(),
        };
        findings.push(Finding {
            severity: Severity::Error,
            code: codes::INVALID_YAML,
            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
            message: format!("invalid front matter: {}{}", error.message, location),
            detail: Some(serde_json::json!({
                "line": error.line,
                "column": error.column,
            })),
        });
    }

    match parsed.id {
        Some(IdProperty::Text(ref id)) if !note::is_valid_ulid(id) => {
            findings.push(Finding {
                severity: Severity::Error,
                code: codes::INVALID_ID,
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                message: format!("`id` is not a valid ULID: {id}"),
                detail: None,
            });
        }
        Some(IdProperty::NotAString) => {
            findings.push(Finding {
                severity: Severity::Error,
                code: codes::INVALID_ID,
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                message: "`id` is not a string".to_string(),
                detail: None,
            });
        }
        Some(IdProperty::Text(id)) => {
            // Duplicate detection compares NFC-normalised, case-insensitively:
            // a ULID spelled with different case is still the same id.
            ids.entry(key(&id)).or_default().push((
                entry.rel.clone(),
                id,
                entry.path.to_str().is_none(),
            ));
        }
        None => {}
    }

    let mut self_embed_seen = false;
    for link in links::extract_markdown_links(&parsed.body, parsed.body_line) {
        let outcome = resolver.resolve(&link.target, entry, link.syntax);
        if entry.class == FileClass::ImageNote {
            note_self_embed(
                library,
                entry,
                image_paths,
                &link,
                &outcome,
                &mut self_embed_seen,
            );
        }
        match outcome {
            Outcome::Resolved(_) => {}
            Outcome::NotImageTarget => {
                if let Some(note_outcome) = resolver.resolve_note_reference(&link.target, entry) {
                    match note_outcome {
                        Outcome::NotFound => findings.push(Finding {
                            severity: Severity::Warning,
                            code: codes::MISSING_LINK,
                            path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
                            message: format!("note link to `{}` on line {} does not match any file", link.raw, link.line),
                            detail: Some(serde_json::json!({"line": link.line, "target": link.raw, "kind": "note"})),
                        }),
                        Outcome::Ambiguous(candidates) => {
                            let mut names: Vec<String> = candidates.iter().map(|&i| library.files[i].rel.clone()).collect();
                            names.sort();
                            findings.push(Finding {
                                severity: Severity::Warning,
                                code: codes::AMBIGUOUS_LINK,
                                path: entry.rel.clone(),
            path_non_utf8: entry.path.to_str().is_none(),
                                message: format!("ambiguous note link to `{}` on line {}", link.raw, link.line),
                                detail: Some(serde_json::json!({"line": link.line, "target": link.raw, "matches": names, "kind": "note"})),
                            });
                        }
                        Outcome::Resolved(_) | Outcome::NotImageTarget => {}
                    }
                }
            }
            Outcome::NotFound => {
                // The resolver only yields NotFound for targets that claim to
                // be images: an image extension, or extensionless Markdown
                // image syntax.
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: codes::MISSING_LINK,
                    path: entry.rel.clone(),
                    path_non_utf8: entry.path.to_str().is_none(),
                    message: format!(
                        "link to `{}` on line {} does not match any file",
                        link.raw, link.line
                    ),
                    detail: Some(serde_json::json!({
                        "line": link.line,
                        "target": link.raw,
                    })),
                });
            }
            Outcome::Ambiguous(candidates) => {
                // Ambiguity is reported where a library user would have to
                // guess: image-ext targets and extensionless image referrals.
                // Ambiguous note links stay out of scope for 0.1.
                let class = links::target_class(&link.target);
                if class == TargetClass::Image
                    || class == TargetClass::None
                    || (class == TargetClass::Text
                        && link.target.to_ascii_lowercase().ends_with(".md"))
                {
                    let mut names: Vec<String> = candidates
                        .iter()
                        .map(|&i| library.files[i].rel.clone())
                        .collect();
                    names.sort();
                    findings.push(Finding {
                        severity: Severity::Warning,
                        code: codes::AMBIGUOUS_LINK,
                        path: entry.rel.clone(),
                        path_non_utf8: entry.path.to_str().is_none(),
                        message: format!(
                            "ambiguous link to `{}` on line {}: could be {}",
                            link.raw,
                            link.line,
                            names.join(" or ")
                        ),
                        detail: Some(serde_json::json!({
                            "line": link.line,
                            "target": link.raw,
                            "matches": names,
                        })),
                    });
                }
            }
        }
    }

    if entry.class == FileClass::ImageNote {
        if let Some(image_rel) = entry.paired_image_rel() {
            let paired_path = entry.paired_image_path().expect("image note path");
            if !image_paths.contains(&paired_path) {
                findings.push(Finding {
                    severity: Severity::Error,
                    code: codes::NOTE_WITHOUT_IMAGE,
                    path: entry.rel.clone(),
                    path_non_utf8: entry.path.to_str().is_none(),
                    message: format!("image note has no matching image `{image_rel}`"),
                    detail: None,
                });
            } else if !self_embed_seen {
                findings.push(Finding {
                    severity: Severity::Info,
                    code: codes::MISSING_SELF_EMBED,
                    path: entry.rel.clone(),
                    path_non_utf8: entry.path.to_str().is_none(),
                    message: format!("image note does not embed its own image `{image_rel}`"),
                    detail: None,
                });
            }
        }
    }
}

/// Track whether an image embed in this note points at the note's own image
/// (FORMAT §3.2). Accepts the §5.1-resolved path or a literal bare-name path
/// spelling, mirroring the prototype add-self-embeds.py rules.
fn note_self_embed(
    library: &Library,
    entry: &FileEntry,
    image_paths: &HashSet<std::path::PathBuf>,
    link: &Link,
    outcome: &Outcome,
    self_embed_seen: &mut bool,
) {
    if !link.syntax.is_strong_image() {
        return;
    }
    let Some(_) = entry.paired_image_path() else {
        return;
    };
    let paired_path = entry.paired_image_path().expect("image note path");
    if !image_paths.contains(&paired_path) {
        return; // unpaired; note_without_image covers it
    }
    if let Outcome::Resolved(idx) = outcome {
        if library.files[*idx].path == paired_path {
            *self_embed_seen = true;
        }
    }
}

/// Read a canvas, validate its JSON and resolve its file nodes.
fn check_canvas(
    library: &Library,
    entry: &FileEntry,
    resolver: &Resolver,
    findings: &mut Vec<Finding>,
) {
    let mut file = match open_regular(&library.root, entry) {
        Ok(file) => file,
        Err(err) => {
            findings.push(unreadable(entry, format!("cannot read the canvas: {err}")));
            return;
        }
    };
    let mut bytes = Vec::new();
    let bytes = match std::io::Read::read_to_end(&mut file, &mut bytes) {
        Ok(_) => bytes,
        Err(err) => {
            findings.push(unreadable(entry, format!("cannot read the canvas: {err}")));
            return;
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            findings.push(unreadable(entry, "canvas is not valid UTF-8".to_string()));
            return;
        }
    };
    // A canvas we cannot parse would otherwise look like it has no links at
    // all; unknown is never empty ( FORMAT §1.5).
    let canvas: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            findings.push(Finding {
                severity: Severity::Error,
                code: codes::INVALID_CANVAS,
                path: entry.rel.clone(),
                path_non_utf8: entry.path.to_str().is_none(),
                message: format!("canvas is not valid JSON: {err}"),
                detail: None,
            });
            return;
        }
    };
    for reference in links::extract_canvas_refs(&canvas) {
        let outcome = resolver.resolve_canvas(&reference.file);
        match outcome {
            Outcome::Resolved(_) | Outcome::NotImageTarget => {}
            Outcome::NotFound => {
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: codes::MISSING_LINK,
                    path: entry.rel.clone(),
                    path_non_utf8: entry.path.to_str().is_none(),
                    message: format!(
                        "canvas file node `{}` does not match any file",
                        reference.file
                    ),
                    detail: Some(serde_json::json!({
                        "node": reference.node_id,
                        "target": reference.file,
                    })),
                });
            }
            Outcome::Ambiguous(candidates) => {
                let mut names: Vec<String> = candidates
                    .iter()
                    .map(|&i| library.files[i].rel.clone())
                    .collect();
                names.sort();
                findings.push(Finding {
                    severity: Severity::Warning,
                    code: codes::AMBIGUOUS_LINK,
                    path: entry.rel.clone(),
                    path_non_utf8: entry.path.to_str().is_none(),
                    message: format!(
                        "ambiguous canvas file node `{}`: could be {}",
                        reference.file,
                        names
                            .iter()
                            .map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join(" or ")
                    ),
                    detail: Some(serde_json::json!({
                        "node": reference.node_id,
                        "target": reference.file,
                        "matches": names,
                    })),
                });
            }
        }
    }
}

fn unreadable(entry: &FileEntry, message: String) -> Finding {
    Finding {
        severity: Severity::Error,
        code: codes::UNREADABLE_FILE,
        path: entry.rel.clone(),
        path_non_utf8: entry.path.to_str().is_none(),
        message,
        detail: None,
    }
}

/// Read up to `limit` leading bytes of a file; `Err` carries the message for
/// an unreadable-image finding.
fn read_head(library: &Library, entry: &FileEntry, limit: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file = open_regular(&library.root, entry)
        .map_err(|err| format!("cannot read the image: {err}"))?;
    let mut head = vec![0u8; limit];
    let read = file
        .read(&mut head)
        .map_err(|err| format!("cannot read the image: {err}"))?;
    head.truncate(read);
    Ok(head)
}
