//! Merge two near-duplicate images into one (W77, design 方案 2).
//!
//! Everything here is pure: the planner is given the file list of one library
//! walk, its [`Resolver`], the current bytes of the two image notes and a
//! reader for the other notes and canvases, and returns a [`MergePlan`] of byte
//! edits — the kept note, and every note or canvas whose links pointed at the
//! dropped image. It opens nothing and changes nothing; the write core (a
//! separate crate) lands the plan as one journaled operation, moving the drop's
//! whole group to `.trash/` and undoing the lot together (ADR-016 §7–§8).
//!
//! The rules (W77 design 方案 2, FORMAT §7.3):
//!
//! - the kept note gains the drop's `tags` (union, the kept spelling wins on a
//!   case-insensitive match), the earlier of the two notes' `added` (else
//!   `imported`) as its own `added`, and one `type: merged` entry plus the
//!   drop's own `sources` entries appended to its `sources`;
//! - the kept note's `source` is never overwritten — the drop's `source` (when
//!   it had one) is recorded inside the `merged` entry instead, with the drop's
//!   library path and SHA-256;
//! - every note and canvas that linked the dropped image (or its note) is
//!   repointed at the kept image (or its note), reusing the exact span logic a
//!   move uses ([`crate::move_plan`], ADR-017 §8): syntax, alias, heading, block
//!   reference, alt and title all preserved, ambiguous links reported and left
//!   untouched, canvases rewritten by parsing their JSON;
//! - the dropped image's own note is never edited: its whole group, body
//!   included, moves to `.trash/` and is preserved there (判据 1, 来源不丢).

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::fm_edit::{
    add_tags, ensure_id, ensure_self_embed, front_matter_range, set_scalar, FmRange,
};
use crate::index::front_matter_json;
use crate::library::{FileClass, FileEntry};
use crate::links::{Outcome, Resolver};
use crate::move_plan::{
    ambiguous_link, body_spans, pointing_count, rewrite_canvas, rewrite_spans, scan_canvas,
    AmbiguousLink, CanvasScan, FileEdit, PlanWarning,
};

/// Why the merge is refused before anything is touched. Every variant means:
/// no file changes hands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeRefusal {
    /// The named image is not in the library walk.
    NotInLibrary { path: String },
    /// Keep and drop name the same image.
    SameImage { path: String },
    /// A note the plan must read or edit could not be read, or is not valid
    /// UTF-8 where UTF-8 is required.
    Unreadable { path: String, message: String },
    /// A note's front matter could not be edited, or is invalid.
    FrontMatter { note: String, message: String },
    /// A `sources` value is not a block list the planner can safely append to
    /// (a flow list, a scalar, or a nested block that is not a list of maps).
    SourcesNotMergeable { note: String },
    /// Re-parsing the built bytes did not confirm the merge landed as planned.
    VerificationFailed { note: String },
}

impl std::fmt::Display for MergeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInLibrary { path } => write!(f, "no image at library path {path}"),
            Self::SameImage { path } => write!(f, "keep and drop are the same image ({path})"),
            Self::Unreadable { path, message } => write!(f, "{path}: {message}"),
            Self::FrontMatter { note, message } => {
                write!(f, "{note}: the front matter cannot be edited: {message}")
            }
            Self::SourcesNotMergeable { note } => write!(
                f,
                "{note}: `sources` is not a block list, so it cannot be merged safely"
            ),
            Self::VerificationFailed { note } => {
                write!(
                    f,
                    "{note}: the merge did not verify against the written bytes"
                )
            }
        }
    }
}

impl std::error::Error for MergeRefusal {}

/// A link-repair refusal from the shared move logic becomes a merge refusal:
/// the merge is refused whole, with nothing changed (ADR-017 §4).
impl From<crate::move_plan::MoveRefusal> for MergeRefusal {
    fn from(value: crate::move_plan::MoveRefusal) -> Self {
        use crate::move_plan::MoveRefusal as M;
        match value {
            M::NotInLibrary { path } => MergeRefusal::NotInLibrary { path },
            M::Unreadable { path, message } => MergeRefusal::Unreadable { path, message },
            other => MergeRefusal::VerificationFailed {
                note: other.to_string(),
            },
        }
    }
}

/// The facts about the two images the planner cannot read out of the notes
/// alone: the library paths, the drop's content hash, the merge time, and the
/// id for a note the merge creates.
pub struct MergeFacts<'a> {
    pub keep_image_rel: &'a str,
    pub drop_image_rel: &'a str,
    pub keep_note_rel: &'a str,
    /// The dropped image's SHA-256, lowercase hex, as the `merged` entry records
    /// what was folded in.
    pub drop_sha256: &'a str,
    /// RFC 3339 UTC time of the merge, written into the `merged` entry.
    pub merged_at: &'a str,
    /// The self-embed text a kept note that had none gets (ADR-017 §2), already
    /// resolved against the post-merge library. Ignored when the kept note
    /// already exists.
    pub keep_embed_text: &'a str,
    /// The id a newly created kept note carries (§3.3): a fresh ULID minted by
    /// the caller. Ignored when the kept note already exists.
    pub new_note_id: &'a str,
}

/// What the merge did, as pure data. The caller lands it as one operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergePlan {
    pub keep: String,
    pub drop: String,
    pub keep_note_rel: String,
    /// Whether the kept image already had a note (a `Modify` step), versus
    /// gaining one (a `Create` step, ADR-017 §2).
    pub keep_note_existed: bool,
    /// The kept note's bytes after the merge (front matter + any link repoint).
    #[serde(skip)]
    pub keep_note_bytes: Vec<u8>,
    /// Every other note or canvas whose bytes change (drop's own note excluded).
    #[serde(skip)]
    pub edits: Vec<FileEdit>,
    /// The tags the kept note gained (already a union; the drop tags that were
    /// already present, case-insensitively, are not listed).
    pub tags_added: Vec<String>,
    /// The `added` value written, when the merge set or changed it.
    pub added_set: Option<String>,
    /// How many entries were appended to `sources` (the merged entry plus the
    /// drop's own entries).
    pub sources_appended: usize,
    /// How many links were repointed from drop to keep.
    pub links_repointed: usize,
    pub ambiguous: Vec<AmbiguousLink>,
    pub warnings: Vec<PlanWarning>,
}

/// Plan the merge of `drop` into `keep`.
///
/// `keep_note_bytes` and `drop_note_bytes` are the current bytes of the two
/// image notes (empty when a note does not exist); `keep_note_existed` says
/// whether the kept note file is there, which is what decides edit versus
/// create — empty bytes of a note that exists are still a note. `read` returns
/// the current bytes of any other note or canvas. See the module header for the
/// rules.
pub fn plan_merge(
    files: &[FileEntry],
    resolver: &Resolver,
    facts: &MergeFacts<'_>,
    keep_note_bytes: &[u8],
    keep_note_existed: bool,
    drop_note_bytes: &[u8],
    read: &dyn Fn(&FileEntry) -> Result<Vec<u8>, String>,
) -> Result<MergePlan, MergeRefusal> {
    if facts.keep_image_rel == facts.drop_image_rel {
        return Err(MergeRefusal::SameImage {
            path: facts.drop_image_rel.to_string(),
        });
    }
    image_index(files, facts.keep_image_rel)?;
    let drop_idx = image_index(files, facts.drop_image_rel)?;
    let drop_note_idx = note_index(files, &format!("{}.md", facts.drop_image_rel));
    let keep_note_idx = note_index(files, facts.keep_note_rel);

    // --- 1. the kept note's front matter: tags, added, sources. ---
    let keep_props = props_of(keep_note_bytes);
    let drop_props = props_of(drop_note_bytes);

    let drop_tags = string_list(&drop_props, "tags");
    let mut bytes = keep_note_bytes.to_vec();
    let mut tags_added: Vec<String> = Vec::new();
    if !drop_tags.is_empty() {
        let before: Vec<String> = string_list(&keep_props, "tags");
        let before_ci: HashSet<String> = before.iter().map(|t| t.to_lowercase()).collect();
        tags_added = drop_tags
            .iter()
            .filter(|t| !before_ci.contains(&t.to_lowercase()))
            .cloned()
            .collect();
        if !tags_added.is_empty() {
            let refs: Vec<&str> = tags_added.iter().map(String::as_str).collect();
            bytes = add_tags(&bytes, &refs).map_err(|e| MergeRefusal::FrontMatter {
                note: facts.keep_note_rel.to_string(),
                message: e.to_string(),
            })?;
        }
    }

    let sources_appended =
        append_merged_sources(&mut bytes, facts, drop_note_bytes, &keep_props, &drop_props)?;

    let added_set = match earliest_added(&keep_props, &drop_props) {
        Some(token) => {
            let current = scalar(&keep_props, "added");
            if current.as_deref() != Some(token.as_str()) {
                bytes =
                    set_scalar(&bytes, "added", &token).map_err(|e| MergeRefusal::FrontMatter {
                        note: facts.keep_note_rel.to_string(),
                        message: e.to_string(),
                    })?;
                Some(token)
            } else {
                None
            }
        }
        None => None,
    };

    // A kept image with no note gains one: the id, a blank line, the self-embed
    // (ADR-017 §2) — the same shape a move gives a bare image.
    if !keep_note_existed {
        bytes = ensure_id(&bytes, facts.new_note_id).map_err(|e| MergeRefusal::FrontMatter {
            note: facts.keep_note_rel.to_string(),
            message: e.to_string(),
        })?;
        bytes.push(b'\n');
        bytes = ensure_self_embed(&bytes, facts.keep_embed_text).map_err(|e| {
            MergeRefusal::FrontMatter {
                note: facts.keep_note_rel.to_string(),
                message: e.to_string(),
            }
        })?;
    }

    // --- 2. repoint every link that resolved to the dropped image (or its
    // note) at the kept image (or its note), drop's own note excluded. ---
    let mut removed: HashSet<usize> = HashSet::new();
    let mut new_rel_of: HashMap<usize, String> = HashMap::new();
    removed.insert(drop_idx);
    new_rel_of.insert(drop_idx, facts.keep_image_rel.to_string());
    if let Some(idx) = drop_note_idx {
        removed.insert(idx);
        new_rel_of.insert(idx, facts.keep_note_rel.to_string());
    }

    // The library after the merge, for verification: the drop's image and note
    // are gone; a kept note that is newly created now exists.
    let post_files = post_files_after_merge(
        files,
        &removed,
        keep_note_existed,
        keep_note_idx,
        facts.keep_note_rel,
    );
    let post_resolver = Resolver::new(&post_files);
    let keep_targets = keep_targets(&post_files, facts);

    let mut repointed = 0usize;
    let mut ambiguous: Vec<AmbiguousLink> = Vec::new();
    let mut warnings: Vec<PlanWarning> = Vec::new();

    // The kept note's own body may also link the dropped image. It is repointed
    // into the same bytes, so it is not a separate edit.
    {
        let text = std::str::from_utf8(&bytes).map_err(|_| MergeRefusal::Unreadable {
            path: facts.keep_note_rel.to_string(),
            message: "not valid UTF-8".to_string(),
        })?;
        let entry = kept_note_entry(files, keep_note_idx, facts.keep_note_rel, keep_note_existed);
        let pointing: Vec<(crate::links::SpanLink, usize)> = body_spans(text, &entry)
            .into_iter()
            .filter_map(
                |span| match resolver.resolve(&span.target, &entry, span.syntax) {
                    Outcome::Resolved(target) if removed.contains(&target) => Some((span, target)),
                    Outcome::Ambiguous(candidates)
                        if candidates.iter().any(|c| removed.contains(c)) =>
                    {
                        ambiguous.push(ambiguous_link(&entry, &span, &candidates, files));
                        None
                    }
                    _ => None,
                },
            )
            .collect();
        if !pointing.is_empty() {
            let (edited, replaced) = rewrite_spans(text, &pointing, &new_rel_of)?;
            repointed += replaced.len();
            bytes = edited;
            let post_text = std::str::from_utf8(&bytes).map_err(|_| MergeRefusal::Unreadable {
                path: facts.keep_note_rel.to_string(),
                message: "not valid UTF-8".to_string(),
            })?;
            if pointing_count(post_text, &entry, &post_resolver, &keep_targets) != pointing.len() {
                return Err(MergeRefusal::VerificationFailed {
                    note: facts.keep_note_rel.to_string(),
                });
            }
        }
    }

    // The other notes and canvases.
    let mut edits: Vec<FileEdit> = Vec::new();
    for (idx, entry) in files.iter().enumerate() {
        if !matches!(
            entry.class,
            FileClass::Note | FileClass::ImageNote | FileClass::Canvas
        ) {
            continue;
        }
        if idx == drop_idx || Some(idx) == drop_note_idx || Some(idx) == keep_note_idx {
            continue;
        }
        let raw = match read(entry) {
            Ok(raw) => raw,
            Err(message) => {
                warnings.push(PlanWarning {
                    path: entry.rel.clone(),
                    message: format!("not scanned: {message}"),
                });
                continue;
            }
        };
        if entry.class == FileClass::Canvas {
            match scan_canvas(&raw, entry, resolver, &removed, files) {
                CanvasScan::Hits {
                    replacements,
                    ambiguous: hits,
                } => {
                    ambiguous.extend(hits);
                    if !replacements.is_empty() {
                        let bytes = rewrite_canvas(&raw, &replacements, &new_rel_of, entry)?;
                        if bytes != raw {
                            repointed += replacements.len();
                            edits.push(FileEdit {
                                path: entry.rel.clone(),
                                before: raw,
                                bytes,
                                replaced: Vec::new(),
                                self_embed_appended: false,
                            });
                        }
                    }
                }
                CanvasScan::Nothing => {}
                CanvasScan::Unreadable(message) => warnings.push(PlanWarning {
                    path: entry.rel.clone(),
                    message,
                }),
            }
            continue;
        }
        let text = match std::str::from_utf8(&raw) {
            Ok(text) => text,
            Err(_) => {
                warnings.push(PlanWarning {
                    path: entry.rel.clone(),
                    message: "not scanned: not valid UTF-8".to_string(),
                });
                continue;
            }
        };
        let mut pointing: Vec<(crate::links::SpanLink, usize)> = Vec::new();
        let mut pointing_count_before = 0usize;
        for span in body_spans(text, entry) {
            match resolver.resolve(&span.target, entry, span.syntax) {
                Outcome::Resolved(target) if removed.contains(&target) => {
                    pointing_count_before += 1;
                    pointing.push((span, target));
                }
                Outcome::Ambiguous(candidates)
                    if candidates.iter().any(|c| removed.contains(c)) =>
                {
                    ambiguous.push(ambiguous_link(entry, &span, &candidates, files));
                }
                _ => {}
            }
        }
        if pointing.is_empty() {
            continue;
        }
        let (edited, replaced) = rewrite_spans(text, &pointing, &new_rel_of)?;
        let post_text = std::str::from_utf8(&edited).map_err(|_| MergeRefusal::Unreadable {
            path: entry.rel.clone(),
            message: "not valid UTF-8".to_string(),
        })?;
        // Every link that pointed at the group must now resolve to the kept
        // image (or note); none may be lost or double-counted (ADR-017 §4).
        if pointing_count(post_text, entry, &post_resolver, &keep_targets) != pointing_count_before
        {
            return Err(MergeRefusal::VerificationFailed {
                note: entry.rel.clone(),
            });
        }
        repointed += replaced.len();
        if edited != raw {
            edits.push(FileEdit {
                path: entry.rel.clone(),
                before: raw,
                bytes: edited,
                replaced,
                self_embed_appended: false,
            });
        }
    }

    ambiguous.sort_by(|a, b| (&a.note, a.span.start).cmp(&(&b.note, b.span.start)));
    ambiguous.dedup();

    Ok(MergePlan {
        keep: facts.keep_image_rel.to_string(),
        drop: facts.drop_image_rel.to_string(),
        keep_note_rel: facts.keep_note_rel.to_string(),
        keep_note_existed,
        keep_note_bytes: bytes,
        edits,
        tags_added,
        added_set,
        sources_appended,
        links_repointed: repointed,
        ambiguous,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Reading the notes.
// ---------------------------------------------------------------------------

/// The front matter of a note (its bytes, empty for a note that does not exist)
/// as a JSON object, read the way the index reads it.
fn props_of(note_bytes: &[u8]) -> serde_json::Value {
    if note_bytes.is_empty() {
        return serde_json::Value::Null;
    }
    let text = String::from_utf8_lossy(note_bytes);
    let json = front_matter_json(&text);
    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null)
}

fn scalar(props: &serde_json::Value, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn string_list(props: &serde_json::Value, key: &str) -> Vec<String> {
    props
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn sources_count(props: &serde_json::Value) -> usize {
    props
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

/// The earlier of the two notes' `added` (else `imported`, FORMAT §3.1), as the
/// datetime string it was written as. `None` when neither note carries a
/// readable one. A note whose own value cannot be parsed is skipped rather than
/// treated as the earliest.
fn earliest_added(keep: &serde_json::Value, drop: &serde_json::Value) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    for props in [keep, drop] {
        let value = ["added", "imported"]
            .iter()
            .filter_map(|key| scalar(props, key))
            .find_map(|text| parse_ns(&text).map(|ns| (ns, text)));
        if let Some(candidate) = value {
            best = Some(match best {
                Some(current) if current.0 <= candidate.0 => current,
                _ => candidate,
            });
        }
    }
    best.map(|(_, text)| text)
}

fn parse_ns(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_nanos_opt().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Appending the merged `sources` entries.
// ---------------------------------------------------------------------------

/// Append one `type: merged` entry, then the drop's own `sources` entries
/// verbatim, to the kept note's `sources`. Returns how many entries were
/// appended. A `sources` value that is not a block list refuses (the planner
/// never guesses at YAML it does not understand).
fn append_merged_sources(
    bytes: &mut Vec<u8>,
    facts: &MergeFacts<'_>,
    drop_note_bytes: &[u8],
    keep_props: &serde_json::Value,
    drop_props: &serde_json::Value,
) -> Result<usize, MergeRefusal> {
    // The drop's own entries, verbatim, come from its raw `sources:` block.
    let drop_block = if drop_props.get("sources").is_some() {
        match sources_block_of(drop_note_bytes, "sources") {
            Some(block) if block.form == BlockForm::List && block.item_indent.is_some() => {
                Some(block)
            }
            Some(_) => {
                return Err(MergeRefusal::SourcesNotMergeable {
                    note: format!("{}.md", facts.drop_image_rel),
                })
            }
            None => {
                return Err(MergeRefusal::SourcesNotMergeable {
                    note: format!("{}.md", facts.drop_image_rel),
                })
            }
        }
    } else {
        None
    };

    let keep_block = sources_block_of(bytes, "sources");
    if let Some(block) = &keep_block {
        if block.form == BlockForm::Other {
            return Err(MergeRefusal::SourcesNotMergeable {
                note: facts.keep_note_rel.to_string(),
            });
        }
    }

    let target_indent = keep_block.as_ref().and_then(|b| b.item_indent).unwrap_or(2);

    let keep_count = sources_count(keep_props);
    let drop_count = sources_count(drop_props);

    let newline = newline_style(bytes);
    let drop_source = scalar(drop_props, "source");
    let mut block = render_merged_entry(facts, drop_source.as_deref(), target_indent, newline);
    if let Some(block_bytes) = &drop_block {
        let drop_indent = block_bytes.item_indent.unwrap_or(2);
        block.extend_from_slice(&reindent(
            &block_bytes.body,
            drop_indent,
            target_indent,
            newline,
        ));
    }

    *bytes = splice_sources(bytes, keep_block.as_ref(), &block, newline)?;

    // Verify against the bytes that were actually built.
    let after = props_of(bytes);
    let after_count = sources_count(&after);
    if after
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .is_none()
    {
        return Err(MergeRefusal::VerificationFailed {
            note: facts.keep_note_rel.to_string(),
        });
    }
    if after_count != keep_count + 1 + drop_count {
        return Err(MergeRefusal::VerificationFailed {
            note: facts.keep_note_rel.to_string(),
        });
    }
    Ok(1 + drop_count)
}

/// The byte region of a top-level `key:` value in a note's front matter.
struct SourcesBlock {
    /// Byte offset of the key line's start within the whole note.
    key_start: usize,
    /// The block body (the lines after the key line), verbatim.
    body: Vec<u8>,
    /// Byte offset just past the last body line, within the whole note.
    body_end: usize,
    /// Indentation of the first `-` item, when there is one.
    item_indent: Option<usize>,
    form: BlockForm,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockForm {
    /// A block list of maps (the `sources` shape, or an empty `sources:` key).
    List,
    /// A flow list, scalar, or something the planner will not touch.
    Other,
}

/// Locate a top-level `sources:` entry in `raw`, reading the front-matter
/// region line by line. `None` when there is no such key.
fn sources_block_of(raw: &[u8], key: &str) -> Option<SourcesBlock> {
    let (yaml_start, close_start) = match front_matter_range(raw) {
        FmRange::Closed {
            yaml_start,
            close_start,
            ..
        } => (yaml_start, close_start),
        _ => return None,
    };
    let region = &raw[yaml_start..close_start];
    let key_with_colon = format!("{key}:");
    let mut cursor = 0usize;
    let mut key_line: Option<(usize, usize)> = None;
    while cursor < region.len() {
        let (line, break_end) = line_at(region, cursor);
        let trimmed_start = region[line.clone()]
            .iter()
            .take_while(|b| **b == b' ')
            .count();
        if trimmed_start == 0 && region[line.start..].starts_with(key_with_colon.as_bytes()) {
            let colon = line.start + key_with_colon.len();
            key_line = Some((line.start, colon));
            break;
        }
        cursor = break_end.unwrap_or(region.len());
        if break_end.is_none() {
            break;
        }
    }
    let (key_start_local, after_colon_local) = key_line?;
    let rest = trim_ws(&region[after_colon_local..line_content_end(region, key_start_local)]);
    let key_start = yaml_start + key_start_local;

    // Body = every following line that is blank or indented, until the next
    // top-level line.
    let key_line_end = {
        let (line, break_end) = line_at(region, key_start_local);
        break_end.unwrap_or(line.end)
    };
    let mut body_end_local = key_line_end;
    let mut cursor = key_line_end;
    let mut item_indent: Option<usize> = None;
    let mut saw_content = false;
    while cursor < region.len() {
        let (line, break_end) = line_at(region, cursor);
        let text = region[line.clone()].to_vec();
        let blank = text.iter().all(|b| b.is_ascii_whitespace());
        let indent = text.iter().take_while(|b| **b == b' ').count();
        if !blank && indent == 0 {
            break;
        }
        if item_indent.is_none() && !blank && region[line.start + indent..].starts_with(b"-") {
            item_indent = Some(indent);
        }
        if !blank {
            saw_content = true;
        }
        body_end_local = break_end.unwrap_or(line.end);
        cursor = break_end.unwrap_or(region.len());
        if break_end.is_none() {
            break;
        }
    }

    let form = if !saw_content && (rest.is_empty() || rest.starts_with(b"#")) {
        // An empty `sources:` key: writable as a fresh block list.
        BlockForm::List
    } else if saw_content && item_indent.is_some() {
        BlockForm::List
    } else {
        BlockForm::Other
    };

    let body = region[key_line_end..body_end_local].to_vec();
    Some(SourcesBlock {
        key_start,
        body,
        body_end: yaml_start + body_end_local,
        item_indent,
        form,
    })
}

/// Insert the rendered block into `raw`, returning the new bytes.
fn splice_sources(
    raw: &[u8],
    keep_block: Option<&SourcesBlock>,
    block: &[u8],
    newline: &[u8],
) -> Result<Vec<u8>, MergeRefusal> {
    match front_matter_range(raw) {
        FmRange::Unterminated { .. } => Err(MergeRefusal::FrontMatter {
            note: String::new(),
            message: "front matter block is never closed".to_string(),
        }),
        FmRange::NoFrontMatter { bom_len } => {
            let mut out = Vec::with_capacity(raw.len() + block.len() + 32);
            out.extend_from_slice(&raw[..bom_len]);
            out.extend_from_slice(b"---");
            out.extend_from_slice(newline);
            out.extend_from_slice(b"sources:");
            out.extend_from_slice(newline);
            out.extend_from_slice(block);
            out.extend_from_slice(b"---");
            out.extend_from_slice(newline);
            out.extend_from_slice(&raw[bom_len..]);
            Ok(out)
        }
        FmRange::Closed { close_start, .. } => {
            let mut out = Vec::with_capacity(raw.len() + block.len() + 16);
            match keep_block {
                Some(block0) if block0.form == BlockForm::List && block0.item_indent.is_some() => {
                    // Append after the last item line.
                    out.extend_from_slice(&raw[..block0.body_end]);
                    out.extend_from_slice(block);
                    out.extend_from_slice(&raw[block0.body_end..]);
                }
                Some(block0) => {
                    // `sources:` present but empty: give it the block under the
                    // key line.
                    let (line, break_end) = line_at(raw, block0.key_start);
                    let insert_at = break_end.unwrap_or(line.end);
                    out.extend_from_slice(&raw[..insert_at]);
                    out.extend_from_slice(block);
                    out.extend_from_slice(&raw[insert_at..]);
                }
                None => {
                    // No `sources:` key: append one at the end of the front
                    // matter, before the closing fence.
                    out.extend_from_slice(&raw[..close_start]);
                    out.extend_from_slice(b"sources:");
                    out.extend_from_slice(newline);
                    out.extend_from_slice(block);
                    out.extend_from_slice(&raw[close_start..]);
                }
            }
            Ok(out)
        }
    }
}

/// The rendered `type: merged` entry, at `indent` spaces, each line ending in
/// `newline`. The drop's own `source` URL (when it had one) rides along, so the
/// provenance survives without touching the kept note's `source`.
fn render_merged_entry(
    facts: &MergeFacts<'_>,
    drop_source: Option<&str>,
    indent: usize,
    newline: &[u8],
) -> Vec<u8> {
    let inner = " ".repeat(indent + 2);
    let mut out = Vec::new();
    push_line(
        &mut out,
        &format!("{}- type: merged", " ".repeat(indent)),
        newline,
    );
    push_line(
        &mut out,
        &format!("{inner}merged_at: {}", facts.merged_at),
        newline,
    );
    push_line(
        &mut out,
        &format!("{inner}path: {}", yaml_token(facts.drop_image_rel)),
        newline,
    );
    push_line(
        &mut out,
        &format!("{inner}sha256: {}", yaml_token(facts.drop_sha256)),
        newline,
    );
    if let Some(source) = drop_source {
        push_line(
            &mut out,
            &format!("{inner}source: {}", yaml_token(source)),
            newline,
        );
    }
    out
}

/// Copy the drop's `sources` block body verbatim, shifting only the left margin
/// so its items sit at `target_indent`.
fn reindent(body: &[u8], base: usize, target_indent: usize, newline: &[u8]) -> Vec<u8> {
    let delta = target_indent as isize - base as isize;
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    for line in text.split_inclusive(['\n']) {
        let content = line.trim_end_matches(['\n', '\r']);
        if content.trim().is_empty() {
            out.extend_from_slice(newline);
            continue;
        }
        let lead = content.bytes().take_while(|b| *b == b' ').count();
        let new_lead = (lead as isize + delta).max(0) as usize;
        out.extend_from_slice(vec![b' '; new_lead].iter().as_slice());
        out.extend_from_slice(&content.as_bytes()[lead..]);
        out.extend_from_slice(newline);
    }
    out
}

// ---------------------------------------------------------------------------
// Library views for link verification.
// ---------------------------------------------------------------------------

fn image_index(files: &[FileEntry], rel: &str) -> Result<usize, MergeRefusal> {
    files
        .iter()
        .position(|e| e.class == FileClass::Image && e.rel == rel)
        .ok_or_else(|| MergeRefusal::NotInLibrary {
            path: rel.to_string(),
        })
}

fn note_index(files: &[FileEntry], rel: &str) -> Option<usize> {
    files
        .iter()
        .position(|e| e.class == FileClass::ImageNote && e.rel == rel)
}

/// The library after the merge, at the same indices where a file stays: the
/// drop's image and note are replaced by inert `Other` entries (so no bare name
/// still resolves to them), and a newly created kept note is appended.
fn post_files_after_merge(
    files: &[FileEntry],
    removed: &HashSet<usize>,
    keep_note_existed: bool,
    keep_note_idx: Option<usize>,
    keep_note_rel: &str,
) -> Vec<FileEntry> {
    let mut post = files.to_vec();
    for &idx in removed {
        if let Some(entry) = post.get_mut(idx) {
            entry.class = FileClass::Other;
        }
    }
    if !keep_note_existed && keep_note_idx.is_none() {
        post.push(new_note_entry(keep_note_rel));
    }
    post
}

/// The kept image (and, when it exists, its note) as post-library indices, for
/// counting the repointed links that must now resolve to them.
fn keep_targets(post_files: &[FileEntry], facts: &MergeFacts<'_>) -> HashSet<usize> {
    let mut set = HashSet::new();
    for (idx, entry) in post_files.iter().enumerate() {
        if entry.class == FileClass::Image && entry.rel == facts.keep_image_rel {
            set.insert(idx);
        }
        if entry.class == FileClass::ImageNote && entry.rel == facts.keep_note_rel {
            set.insert(idx);
        }
    }
    set
}

/// A note entry as the walk would show it (for the resolver calls that name the
/// kept note).
fn kept_note_entry(
    files: &[FileEntry],
    keep_note_idx: Option<usize>,
    keep_note_rel: &str,
    keep_note_existed: bool,
) -> FileEntry {
    if let Some(idx) = keep_note_idx.filter(|_| keep_note_existed) {
        return files[idx].clone();
    }
    new_note_entry(keep_note_rel)
}

fn new_note_entry(rel: &str) -> FileEntry {
    let path = std::path::PathBuf::from(rel);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(rel)
        .to_string();
    FileEntry {
        rel: rel.to_string(),
        name,
        path,
        native_name: std::ffi::OsString::from(
            std::path::Path::new(rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(rel),
        ),
        class: FileClass::ImageNote,
    }
}

// ---------------------------------------------------------------------------
// Byte helpers.
// ---------------------------------------------------------------------------

fn line_at(s: &[u8], at: usize) -> (std::ops::Range<usize>, Option<usize>) {
    let end = match s[at..].iter().position(|&b| b == b'\n') {
        Some(offset) => at + offset,
        None => s.len(),
    };
    let break_end = (end < s.len()).then_some(end + 1);
    (at..end, break_end)
}

fn line_content_end(s: &[u8], at: usize) -> usize {
    line_at(s, at).0.end
}

fn trim_ws(mut slice: &[u8]) -> &[u8] {
    while matches!(slice.first(), Some(b' ') | Some(b'\t')) {
        slice = &slice[1..];
    }
    while matches!(slice.last(), Some(b' ') | Some(b'\t') | Some(b'\r')) {
        slice = &slice[..slice.len().saturating_sub(1)];
    }
    slice
}

fn newline_style(raw: &[u8]) -> &'static [u8] {
    if raw.windows(2).any(|w| w == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    }
}

fn push_line(out: &mut Vec<u8>, line: &str, newline: &[u8]) {
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(newline);
}

/// Quote a scalar the way FORMAT §3.1 asks: only when YAML could misread it.
fn yaml_token(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.contains(": ")
        || value.ends_with(':')
        || value.starts_with([
            '&', '*', '!', '%', '@', '`', '#', '[', ']', '{', '}', '|', '>', '"', '\'',
        ])
        || value.starts_with("- ")
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "yes" | "no" | "on" | "off" | "true" | "false" | "null" | "~"
        );
    if needs_quotes {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}
