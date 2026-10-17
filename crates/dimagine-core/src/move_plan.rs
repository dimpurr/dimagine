//! Move/rename an image group and repair the links that pointed at it
//! (ADR-017 §8).
//!
//! Everything here is pure: the planner takes the files of one library walk,
//! the [`Resolver`] built from them and a reader for note and canvas bytes,
//! and returns a [`MovePlan`] of renames and byte edits, or a typed refusal.
//! It never opens a file, so the CLI and the HTTP API run the same planner
//! and the semantics are testable without a disk. The write core (a separate
//! crate) turns a plan into one journaled operation (ADR-016 §7): the
//! renames first, then the created note, then the byte edits, so recovery
//! and undo see the steps in that order.
//!
//! The rules this module implements (ADR-017 §8):
//!
//! - a move is planned for an image: its note and every `<name>.*` raw or
//!   other sidecar that hangs off the image name move with it, and the group
//!   refuses whole when any destination path is taken — never a partial
//!   move; an image without a note gains one (ADR-017 §2);
//! - the first write assigns the image an `id` (ADR-017 §3) through
//!   [`crate::fm_edit::ensure_id`];
//! - before anything moves, every note and canvas is scanned with the
//!   existing resolver and each link is recorded with its byte span, syntax,
//!   embed flag, alias, heading, block id and Markdown alt/title; links in
//!   fenced code blocks and inline code spans are skipped like everywhere
//!   else, and links are only looked for in the body after the front matter;
//! - only the recorded spans of links that resolve to a moved file are
//!   rewritten, in place, preserving alias, heading, block reference and
//!   alt/title; the moved note's own links are edited in its bytes, which
//!   travel with it, and verification resolves them at the new location;
//! - a link that was already ambiguous before the move is reported and left
//!   byte-identical (ADR-017 §8: do not guess);
//! - canvases are parsed as JSON and only `nodes[].file` values that resolve
//!   to a moved file are replaced — never a substring swap;
//! - written bytes are re-parsed and must resolve back to the moved file
//!   (ADR-017 §4), or the whole plan is refused before anything is written.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::{Component, PathBuf};

use serde::Serialize;

use crate::body_edit::{self, BodyCtx, EditError, Embed};
use crate::fm_edit::{ensure_id, ensure_self_embed, front_matter_range, FmRange};
use crate::format;
use crate::library::{FileClass, FileEntry};
use crate::links::{self, extract_link_spans, Outcome, Resolver, SpanLink};
use crate::note::is_valid_ulid;

/// Why the move is refused before anything is touched. Every variant means:
/// no file changes hands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveRefusal {
    /// The source is not an image of the library walk.
    NotInLibrary { path: String },
    /// A destination path is already taken, or a parent component of the
    /// destination names a file.
    TargetExists { path: String },
    /// The destination is not a usable library path inside the library
    /// (empty, absolute, or with a `.`/`..`/dot-prefixed component).
    BadDestination { path: String },
    /// Source and destination name the same path.
    SamePath { path: String },
    /// The destination does not end in an image extension, so it could not
    /// stay the image of the group.
    NoImageExtension { path: String },
    /// The destination name holds characters that break wikilinks
    /// (FORMAT §2.1), so no link could name the moved image.
    UnlinkableName { path: String, characters: Vec<char> },
    /// No unambiguous link text exists for the moved image at its new path.
    NotLinkable { path: String },
    /// The destination bare name would match more than one file after the
    /// move, leaving no unambiguous link text (ADR-017 §4).
    AmbiguousBareName { name: String, matches: Vec<String> },
    /// A path involved has a non-UTF-8 component: no link text and no
    /// journal record can name it, so its links could not be repaired or
    /// the move undone.
    NonUtf8Path { path: String },
    /// The given id is not a ULID (FORMAT §3.3).
    NotAUlid,
    /// The moved note's front matter is outside what the writer may touch,
    /// or invalid; a note the reader reports is never rewritten (ADR-016 §5).
    FrontMatter { note: String, message: String },
    /// A file the plan must read or edit could not be read, or is not valid
    /// UTF-8 where UTF-8 is required.
    Unreadable { path: String, message: String },
    /// The moved note has an unclosed fenced code block, so an appended
    /// self-embed would land inside code (ADR-017 §4).
    UnclosedFence { note: String },
    /// Re-parsing the written bytes did not confirm that every rewritten
    /// link points at the moved file (ADR-017 §4).
    VerificationFailed { note: String },
}

impl std::fmt::Display for MoveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInLibrary { path } => write!(f, "no image at library path {path}"),
            Self::TargetExists { path } => write!(f, "cannot move: {path} already exists"),
            Self::BadDestination { path } => {
                write!(f, "{path} is not a usable library-relative destination")
            }
            Self::SamePath { path } => write!(f, "{path} is already there"),
            Self::NoImageExtension { path } => {
                write!(f, "destination {path} does not end in an image extension")
            }
            Self::UnlinkableName { path, characters } => write!(
                f,
                "destination {path} contains {} that break links",
                characters
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Self::NotLinkable { path } => {
                write!(f, "no unambiguous link text exists for {path}")
            }
            Self::AmbiguousBareName { name, matches } => write!(
                f,
                "bare name {name} would match {} files after the move: pick a folder or another name",
                matches.len()
            ),
            Self::NonUtf8Path { path } => write!(
                f,
                "path {path} is not valid UTF-8: move it by hand so its links can be tracked"
            ),
            Self::NotAUlid => write!(f, "the id to assign is not a ULID"),
            Self::FrontMatter { note, message } => {
                write!(f, "{note}: the front matter cannot be edited: {message}")
            }
            Self::Unreadable { path, message } => write!(f, "{path}: {message}"),
            Self::UnclosedFence { note } => write!(
                f,
                "{note}: an unclosed code block would swallow an appended self-embed"
            ),
            Self::VerificationFailed { note } => write!(
                f,
                "{note}: the rewritten links did not resolve back to the moved image"
            ),
        }
    }
}

impl std::error::Error for MoveRefusal {}

/// One file of the group that changes its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameStep {
    pub src: String,
    pub dst: String,
}

/// A note created at its path (the moved image had none, ADR-017 §2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewNote {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// One link text replaced in place, with the line it sat on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpanEdit {
    /// 1-based line of the link in the note before the move.
    pub line: usize,
    /// Byte range of the old link text in the note's bytes.
    pub span: Range<usize>,
    pub old_text: String,
    pub new_text: String,
}

/// One existing file whose bytes change, addressed at the path it has after
/// the move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    pub bytes: Vec<u8>,
    /// The file's bytes before the move. The write core reads a Modify step's
    /// before-state from disk when the file is there; for the moved note it is
    /// not yet at its new path when the journal is written, and the caller
    /// hands these bytes to the step instead (ADR-016 §7, ADR-017 §8).
    pub before: Vec<u8>,
    pub replaced: Vec<SpanEdit>,
    /// True when the moved note gained a self-embed it did not have
    /// (ADR-017 §2).
    pub self_embed_appended: bool,
}

/// A link that was already ambiguous before the move and names one of the
/// moved files among its candidates: reported and left byte-identical
/// (ADR-017 §8). Canvases have no byte spans, so their span is empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AmbiguousLink {
    pub note: String,
    pub line: usize,
    pub span: Range<usize>,
    pub target: String,
    pub matches: Vec<String>,
}

/// A file the scan could not inspect. Its links, if any pointed at the
/// group, stay dangling; the plan says so instead of claiming otherwise
/// (unknown is never empty).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanWarning {
    pub path: String,
    pub message: String,
}

/// Everything one move does, as pure data. The caller lands it as journaled
/// steps in the order ADR-017 §8 gives: the renames, then the created note,
/// then the byte edits — the moved note's edit last, at its new path, with
/// its `before` bytes handed to the journal because the file only reaches
/// that path through the operation's own rename. Undo reverses exactly this
/// (ADR-016 §7–§8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePlan {
    pub src: String,
    pub dst: String,
    pub renames: Vec<RenameStep>,
    pub new_note: Option<NewNote>,
    /// The moved image's own note, edited at its new path; `None` when its
    /// bytes do not change, or it did not exist before the move.
    pub moved_note: Option<FileEdit>,
    /// Every other note or canvas whose bytes change.
    pub edits: Vec<FileEdit>,
    /// True when this move assigns the image its first `id` (ADR-017 §3).
    pub id_assigned: bool,
    pub ambiguous: Vec<AmbiguousLink>,
    pub warnings: Vec<PlanWarning>,
}

/// Plan the move of the image at library-relative `src` to library-relative
/// `dst`, assigning `ulid` as the `id` when the note lacks one.
///
/// `read` returns the current bytes of a library file, or a message for why
/// it could not be read. The planner calls it for every note and canvas; a
/// caller with a size limit must return an error for files over it, and the
/// plan then reports such files as warnings, never as files without links.
///
/// `occupied` answers whether anything — a file, a directory, anything the
/// walk never reports — holds the given library-relative path on the
/// filesystem. The planner never opens a file, so destination paths are
/// checked against it before the plan is accepted: a move whose destination
/// is taken refuses whole, with nothing moved (ADR-017 §8).
///
/// `same_file` answers whether two library-relative paths name the same
/// directory entry (device and inode, no final symlink followed), and
/// separates the one occupant a rename step may displace from a real one: a
/// case-only self-rename (`a/x.jpg` -> `a/X.JPG`, the routine way to fix
/// display case) resolves to the entry being moved on a case-insensitive
/// volume, while on a case-sensitive volume the same spelling can be a
/// different file or a directory — invisible to the walk, so only the caller
/// can tell the two apart. A symlink at the destination is always its own
/// entry, never the file it points at. [`same_file_probe`] builds the probe
/// both surfaces pass.
#[allow(clippy::too_many_arguments)]
pub fn plan_move(
    files: &[FileEntry],
    resolver: &Resolver,
    src: &str,
    dst: &str,
    ulid: &str,
    read: &dyn Fn(&FileEntry) -> Result<Vec<u8>, String>,
    occupied: &dyn Fn(&str) -> bool,
    same_file: &dyn Fn(&str, &str) -> bool,
) -> Result<MovePlan, MoveRefusal> {
    if !is_valid_ulid(ulid) {
        return Err(MoveRefusal::NotAUlid);
    }
    let src_rel = src.trim_start_matches("./");
    let dst_rel = dst.trim_start_matches("./");
    check_destination_shape(dst_rel)?;
    let image_idx = files
        .iter()
        .position(|entry| entry.class == FileClass::Image && entry.rel == src_rel)
        .ok_or_else(|| MoveRefusal::NotInLibrary {
            path: src_rel.to_string(),
        })?;
    let image = &files[image_idx];
    exact_rel(image).ok_or_else(|| MoveRefusal::NonUtf8Path {
        path: image.rel.clone(),
    })?;
    if src_rel == dst_rel {
        return Err(MoveRefusal::SamePath {
            path: src_rel.to_string(),
        });
    }
    if !format::file_extension(dst_rel).is_some_and(format::is_image_extension) {
        return Err(MoveRefusal::NoImageExtension {
            path: dst_rel.to_string(),
        });
    }
    let dst_name = dst_rel.rsplit('/').next().unwrap_or(dst_rel);
    let banned = format::banned_characters(dst_name);
    if !banned.is_empty() {
        return Err(MoveRefusal::UnlinkableName {
            path: dst_rel.to_string(),
            characters: banned,
        });
    }

    // --- the group: image, its note, and `<name>.*` raw/other sidecars. ---
    let mut member_indices = vec![image_idx];
    let existing_note_idx = files.iter().position(|entry| {
        entry.class == FileClass::ImageNote
            && entry.paired_image_path().as_deref() == Some(image.path.as_path())
    });
    if let Some(note_idx) = existing_note_idx {
        member_indices.push(note_idx);
    }
    let sidecar_prefix = format!("{}.", image.name);
    for (idx, entry) in files.iter().enumerate() {
        if idx != image_idx
            && existing_note_idx != Some(idx)
            && matches!(entry.class, FileClass::Raw | FileClass::Other)
            && entry.dir_path() == image.dir_path()
            && entry.name.starts_with(&sidecar_prefix)
        {
            member_indices.push(idx);
        }
    }
    for &idx in &member_indices {
        exact_rel(&files[idx]).ok_or_else(|| MoveRefusal::NonUtf8Path {
            path: files[idx].rel.clone(),
        })?;
    }

    let mut renames: Vec<RenameStep> = Vec::new();
    let mut new_rel_of: HashMap<usize, String> = HashMap::new();
    for &idx in &member_indices {
        let entry = &files[idx];
        let suffix = entry
            .name
            .strip_prefix(&image.name)
            .unwrap_or_default()
            .to_string();
        let new_rel = join_parent(dst_rel, &format!("{dst_name}{suffix}"));
        new_rel_of.insert(idx, new_rel.clone());
        renames.push(RenameStep {
            src: entry.rel.clone(),
            dst: new_rel,
        });
    }
    let new_note_rel = format!("{dst_rel}.md");

    // --- destinations: free, and every parent folder of the destination
    // must not be an existing file. ---
    let rels: HashSet<&str> = files.iter().map(|entry| entry.rel.as_str()).collect();
    check_parents_free(dst_rel, &rels)?;
    for step in &renames {
        if destination_taken(
            step.dst.as_str(),
            step.src.as_str(),
            files,
            occupied,
            same_file,
        ) {
            return Err(MoveRefusal::TargetExists {
                path: step.dst.clone(),
            });
        }
    }
    if existing_note_idx.is_none()
        && destination_taken(new_note_rel.as_str(), "", files, occupied, same_file)
    {
        return Err(MoveRefusal::TargetExists {
            path: new_note_rel.clone(),
        });
    }

    // --- the library as it will look after the move, for the link text the
    // new note and the self-embed get, and for verification. ---
    let mut post_files = files.to_vec();
    let mut moved_post: HashSet<usize> = HashSet::new();
    for &idx in &member_indices {
        post_files[idx] = repositioned(&post_files[idx], &new_rel_of[&idx]);
        moved_post.insert(idx);
    }
    let note_post_idx = match existing_note_idx {
        Some(note_idx) => note_idx,
        None => {
            post_files.push(new_note_entry(&new_note_rel));
            let idx = post_files.len() - 1;
            moved_post.insert(idx);
            idx
        }
    };
    let post_resolver = Resolver::new(&post_files);
    let post_ctx = BodyCtx::new(&post_files, &post_resolver);

    // A root destination must stay nameable by its own full path — its bare
    // name — which needs that name to match exactly one file after the move.
    if !dst_rel.contains('/') {
        let hits: Vec<usize> = post_resolver
            .bare_name_hits(dst_name)
            .iter()
            .copied()
            .filter(|&idx| post_files[idx].class == FileClass::Image)
            .collect();
        if hits.len() > 1 {
            return Err(MoveRefusal::AmbiguousBareName {
                name: dst_name.to_string(),
                matches: hits
                    .iter()
                    .filter_map(|&idx| post_files.get(idx))
                    .map(|entry| entry.rel.clone())
                    .collect(),
            });
        }
    }
    let embed_text = match body_edit::link_text_for(image_idx, &post_ctx) {
        Ok(Embed::Text(text)) => text,
        Ok(Embed::RenameOrMove { name, matches }) => {
            return Err(MoveRefusal::AmbiguousBareName { name, matches })
        }
        Err(EditError::UnlinkableName { characters, .. }) => {
            return Err(MoveRefusal::UnlinkableName {
                path: dst_rel.to_string(),
                characters,
            })
        }
        Err(_) => {
            return Err(MoveRefusal::NotLinkable {
                path: dst_rel.to_string(),
            })
        }
    };

    // --- scan every note and canvas, recording spans before anything moves. ---
    let mut warnings: Vec<PlanWarning> = Vec::new();
    let mut ambiguous: Vec<AmbiguousLink> = Vec::new();
    let mut scans: Vec<NoteScan> = Vec::new();
    let mut canvas_hits: Vec<CanvasHits> = Vec::new();
    let mut contents: HashMap<usize, Vec<u8>> = HashMap::new();
    for (idx, entry) in files.iter().enumerate() {
        if !matches!(
            entry.class,
            FileClass::Note | FileClass::ImageNote | FileClass::Canvas
        ) {
            continue;
        }
        let is_moved_note = existing_note_idx == Some(idx);
        let bytes = match read(entry) {
            Ok(bytes) => bytes,
            Err(message) if is_moved_note => {
                return Err(MoveRefusal::Unreadable {
                    path: entry.rel.clone(),
                    message,
                })
            }
            Err(message) => {
                warnings.push(PlanWarning {
                    path: entry.rel.clone(),
                    message: format!("not scanned: {message}"),
                });
                continue;
            }
        };
        if entry.class == FileClass::Canvas {
            match scan_canvas(&bytes, entry, resolver, &moved_post, files) {
                CanvasScan::Hits {
                    replacements,
                    ambiguous: hits,
                } => {
                    ambiguous.extend(hits);
                    // An ambiguous-only canvas (no reference that
                    // resolves into the group) is reported through
                    // the ambiguous list and stays untouched: there
                    // is nothing to rewrite in it.
                    if !replacements.is_empty() {
                        canvas_hits.push(CanvasHits { idx, replacements });
                        contents.insert(idx, bytes);
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
        let Ok(text) = std::str::from_utf8(&bytes) else {
            if is_moved_note {
                return Err(MoveRefusal::Unreadable {
                    path: entry.rel.clone(),
                    message: "not valid UTF-8".to_string(),
                });
            }
            warnings.push(PlanWarning {
                path: entry.rel.clone(),
                message: "not scanned: not valid UTF-8".to_string(),
            });
            continue;
        };
        let mut pointing: Vec<(SpanLink, usize)> = Vec::new();
        let mut pointing_count = 0usize;
        for span in body_spans(text, entry) {
            match resolver.resolve(&span.target, entry, span.syntax) {
                Outcome::Resolved(target) if moved_post.contains(&target) => {
                    pointing_count += 1;
                    pointing.push((span, target));
                }
                Outcome::Ambiguous(candidates)
                    if candidates.iter().any(|c| moved_post.contains(c)) =>
                {
                    ambiguous.push(ambiguous_link(entry, &span, &candidates, files));
                }
                _ => {}
            }
        }
        let touched = !pointing.is_empty();
        if touched {
            scans.push(NoteScan {
                idx,
                pointing_count,
                pointing,
            });
        }
        if touched || is_moved_note {
            contents.insert(idx, bytes);
        }
    }

    // --- the moved image's own note: span rewrites, then the id, then the
    // self-embed, all in its bytes, which move with it (ADR-017 §8: the
    // moved note's links are edited against its new path). ---
    let mut moved_note: Option<FileEdit> = None;
    let mut id_assigned = false;
    if let Some(note_idx) = existing_note_idx {
        let entry = &files[note_idx];
        let raw = contents
            .remove(&note_idx)
            .ok_or_else(|| MoveRefusal::Unreadable {
                path: entry.rel.clone(),
                message: "the scanned bytes were not kept".to_string(),
            })?;
        let text = std::str::from_utf8(&raw).map_err(|_| MoveRefusal::Unreadable {
            path: entry.rel.clone(),
            message: "not valid UTF-8".to_string(),
        })?;
        let spans = scans
            .iter()
            .find(|scan| scan.idx == note_idx)
            .map(|scan| scan.pointing.clone())
            .unwrap_or_default();
        let pre_pointing = scans
            .iter()
            .find(|scan| scan.idx == note_idx)
            .map(|scan| scan.pointing_count)
            .unwrap_or(0);
        let (bytes, replaced) = rewrite_spans(text, &spans, &new_rel_of)?;
        let with_id = ensure_id(&bytes, ulid).map_err(|error| MoveRefusal::FrontMatter {
            note: entry.rel.clone(),
            message: error.to_string(),
        })?;
        id_assigned = with_id != bytes;
        let mut self_embed_appended = false;
        let final_bytes = if embeds(&with_id, note_idx, image_idx, &post_ctx) {
            with_id
        } else {
            if links::unclosed_fence(&body_text(&with_id)).is_some() {
                return Err(MoveRefusal::UnclosedFence {
                    note: entry.rel.clone(),
                });
            }
            let appended = append_embed(&with_id, &embed_text);
            self_embed_appended = appended != with_id;
            appended
        };
        let post_text = std::str::from_utf8(&final_bytes).map_err(|_| MoveRefusal::Unreadable {
            path: entry.rel.clone(),
            message: "not valid UTF-8".to_string(),
        })?;
        let post_pointing = pointing_count(
            post_text,
            &post_files[note_idx],
            &post_resolver,
            &moved_post,
        );
        let expected = pre_pointing + usize::from(self_embed_appended);
        if post_pointing != expected || !embeds(&final_bytes, note_idx, image_idx, &post_ctx) {
            return Err(MoveRefusal::VerificationFailed {
                note: entry.rel.clone(),
            });
        }
        if final_bytes != raw {
            moved_note = Some(FileEdit {
                path: new_rel_of[&note_idx].clone(),
                before: raw,
                bytes: final_bytes,
                replaced,
                self_embed_appended,
            });
        }
    }

    // --- every other edited file, at the path it keeps. ---
    let mut edits: Vec<FileEdit> = Vec::new();
    for scan in scans {
        if member_indices.contains(&scan.idx) {
            continue;
        }
        let entry = &files[scan.idx];
        let raw = contents
            .remove(&scan.idx)
            .ok_or_else(|| MoveRefusal::Unreadable {
                path: entry.rel.clone(),
                message: "the scanned bytes were not kept".to_string(),
            })?;
        let text = std::str::from_utf8(&raw).map_err(|_| MoveRefusal::Unreadable {
            path: entry.rel.clone(),
            message: "not valid UTF-8".to_string(),
        })?;
        let (bytes, replaced) = rewrite_spans(text, &scan.pointing, &new_rel_of)?;
        let post_text = std::str::from_utf8(&bytes).map_err(|_| MoveRefusal::Unreadable {
            path: entry.rel.clone(),
            message: "not valid UTF-8".to_string(),
        })?;
        if pointing_count(post_text, entry, &post_resolver, &moved_post) != scan.pointing_count {
            return Err(MoveRefusal::VerificationFailed {
                note: entry.rel.clone(),
            });
        }
        if bytes != raw {
            edits.push(FileEdit {
                path: entry.rel.clone(),
                before: raw,
                bytes,
                replaced,
                self_embed_appended: false,
            });
        }
    }
    for hit in canvas_hits {
        if member_indices.contains(&hit.idx) {
            continue;
        }
        let entry = &files[hit.idx];
        let raw = contents
            .remove(&hit.idx)
            .ok_or_else(|| MoveRefusal::Unreadable {
                path: entry.rel.clone(),
                message: "the scanned bytes were not kept".to_string(),
            })?;
        let bytes = rewrite_canvas(&raw, &hit.replacements, &new_rel_of, entry)?;
        if bytes != raw {
            edits.push(FileEdit {
                path: entry.rel.clone(),
                before: raw,
                bytes,
                replaced: Vec::new(),
                self_embed_appended: false,
            });
        }
    }

    // --- the note created for an image that had none (ADR-017 §2): the id,
    // a blank line, the self-embed. ---
    let new_note = if existing_note_idx.is_some() {
        None
    } else {
        let with_id = ensure_id(b"", ulid).map_err(|error| MoveRefusal::FrontMatter {
            note: new_note_rel.clone(),
            message: error.to_string(),
        })?;
        id_assigned = true;
        let mut seeded = with_id;
        seeded.push(b'\n');
        let bytes =
            ensure_self_embed(&seeded, &embed_text).map_err(|error| MoveRefusal::FrontMatter {
                note: new_note_rel.clone(),
                message: error.to_string(),
            })?;
        if !embeds(&bytes, note_post_idx, image_idx, &post_ctx) {
            return Err(MoveRefusal::VerificationFailed {
                note: new_note_rel.clone(),
            });
        }
        Some(NewNote {
            path: new_note_rel.clone(),
            bytes,
        })
    };

    ambiguous.sort_by(|a, b| (&a.note, a.span.start).cmp(&(&b.note, b.span.start)));
    ambiguous.dedup();
    Ok(MovePlan {
        src: src_rel.to_string(),
        dst: dst_rel.to_string(),
        renames,
        new_note,
        moved_note,
        edits,
        id_assigned,
        ambiguous,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Scan helpers.
// ---------------------------------------------------------------------------

/// One note with links pointing at the group: the spans to rewrite and how
/// many links pointed at the group before the move (the verification count).
struct NoteScan {
    idx: usize,
    pointing_count: usize,
    pointing: Vec<(SpanLink, usize)>,
}

struct CanvasHits {
    idx: usize,
    replacements: Vec<(String, usize)>,
}

/// Links recorded for one note's body: byte ranges made absolute against
/// the whole file, with the front matter excluded from the search (links are
/// body facts, YAML is not spliced).
fn body_spans(text: &str, entry: &FileEntry) -> Vec<SpanLink> {
    let (body_start, first_line) = body_origin(text.as_bytes());
    if body_start >= text.len() {
        return Vec::new();
    }
    extract_link_spans(&text[body_start..], &entry.rel, first_line)
        .into_iter()
        .map(|mut span| {
            span.range.start += body_start;
            span.range.end += body_start;
            span
        })
        .collect()
}

/// Where a note's body starts and with which line number: right after the
/// front matter when there is one.
fn body_origin(raw: &[u8]) -> (usize, usize) {
    match front_matter_range(raw) {
        FmRange::NoFrontMatter { bom_len } => (bom_len, 1),
        FmRange::Unterminated { body_start } => (body_start, 2),
        FmRange::Closed {
            body_start,
            close_line_no,
            ..
        } => (body_start, close_line_no + 1),
    }
}

fn body_text(raw: &[u8]) -> String {
    let (body_start, _) = body_origin(raw);
    String::from_utf8_lossy(&raw[body_start.min(raw.len())..]).into_owned()
}

/// How many of a note's body links resolve to one of `moved`.
fn pointing_count(
    text: &str,
    entry: &FileEntry,
    resolver: &Resolver,
    moved: &HashSet<usize>,
) -> usize {
    body_spans(text, entry)
        .iter()
        .filter(|span| {
            matches!(
                resolver.resolve(&span.target, entry, span.syntax),
                Outcome::Resolved(target) if moved.contains(&target)
            )
        })
        .count()
}

/// Whether a note's bytes embed image `image` as a member line would
/// (resolver-based, never a string compare; ADR-017 §4).
fn embeds(bytes: &[u8], note_idx: usize, image_idx: usize, ctx: &BodyCtx) -> bool {
    body_edit::member_spans(bytes, note_idx, image_idx, ctx).is_ok_and(|hits| !hits.is_empty())
}

fn ambiguous_link(
    entry: &FileEntry,
    span: &SpanLink,
    candidates: &[usize],
    files: &[FileEntry],
) -> AmbiguousLink {
    let mut matches: Vec<String> = candidates
        .iter()
        .filter_map(|&i| files.get(i))
        .map(|entry| entry.rel.clone())
        .collect();
    matches.sort();
    AmbiguousLink {
        note: entry.rel.clone(),
        line: span.line,
        span: span.range.clone(),
        target: span.target.clone(),
        matches,
    }
}

enum CanvasScan {
    Hits {
        replacements: Vec<(String, usize)>,
        ambiguous: Vec<AmbiguousLink>,
    },
    Nothing,
    Unreadable(String),
}

/// One canvas: every `file` node value resolving into the group, and every
/// ambiguous value that names the group among its candidates.
fn scan_canvas(
    bytes: &[u8],
    entry: &FileEntry,
    resolver: &Resolver,
    moved: &HashSet<usize>,
    files: &[FileEntry],
) -> CanvasScan {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return CanvasScan::Unreadable("not scanned: not valid UTF-8".to_string());
    };
    let Ok(canvas) = serde_json::from_str::<serde_json::Value>(text) else {
        return CanvasScan::Unreadable(
            "not scanned: canvas is not valid JSON (check reports it)".to_string(),
        );
    };
    let mut replacements = Vec::new();
    let mut ambiguous = Vec::new();
    for reference in links::extract_canvas_refs(&canvas) {
        match resolver.resolve_canvas(&reference.file) {
            Outcome::Resolved(target) if moved.contains(&target) => {
                replacements.push((reference.file.clone(), target));
            }
            Outcome::Ambiguous(candidates) if candidates.iter().any(|c| moved.contains(c)) => {
                let mut matches: Vec<String> = candidates
                    .iter()
                    .filter_map(|&i| files.get(i))
                    .map(|file| file.rel.clone())
                    .collect();
                matches.sort();
                ambiguous.push(AmbiguousLink {
                    note: entry.rel.clone(),
                    line: 0,
                    span: 0..0,
                    target: reference.file.clone(),
                    matches,
                });
            }
            _ => {}
        }
    }
    if replacements.is_empty() && ambiguous.is_empty() {
        CanvasScan::Nothing
    } else {
        CanvasScan::Hits {
            replacements,
            ambiguous,
        }
    }
}

/// Replace the `file` values of a parsed canvas. Only `nodes[]` entries of
/// type `file` whose value equals a recorded old value are touched; the JSON
/// is re-serialized, compact when it was compact and pretty when it was
/// pretty, keeping a trailing newline (ADR-017 §8: no substring replacement).
/// A re-serialized canvas may reorder object keys — JSON object members are
/// unordered and a canvas is rewritten only when it really references the
/// moved file.
fn rewrite_canvas(
    raw: &[u8],
    replacements: &[(String, usize)],
    new_rel_of: &HashMap<usize, String>,
    entry: &FileEntry,
) -> Result<Vec<u8>, MoveRefusal> {
    let text = std::str::from_utf8(raw).map_err(|_| MoveRefusal::Unreadable {
        path: entry.rel.clone(),
        message: "not valid UTF-8".to_string(),
    })?;
    let mut canvas: serde_json::Value =
        serde_json::from_str(text).map_err(|_| MoveRefusal::VerificationFailed {
            note: entry.rel.clone(),
        })?;
    if let Some(nodes) = canvas
        .get_mut("nodes")
        .and_then(serde_json::Value::as_array_mut)
    {
        for node in nodes {
            if node.get("type").and_then(serde_json::Value::as_str) != Some("file") {
                continue;
            }
            let Some(current) = node.get("file").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Some((_, target)) = replacements.iter().find(|(old, _)| old == current) {
                if let Some(new_rel) = new_rel_of.get(target) {
                    node["file"] = serde_json::Value::String(new_rel.clone());
                }
            }
        }
    }
    let mut out = if text.contains('\n') {
        serde_json::to_string_pretty(&canvas).map_err(|_| MoveRefusal::VerificationFailed {
            note: entry.rel.clone(),
        })?
    } else {
        serde_json::to_string(&canvas).map_err(|_| MoveRefusal::VerificationFailed {
            note: entry.rel.clone(),
        })?
    };
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// Splice the recorded spans with their new spellings, in byte order.
fn rewrite_spans(
    text: &str,
    pointing: &[(SpanLink, usize)],
    new_rel_of: &HashMap<usize, String>,
) -> Result<(Vec<u8>, Vec<SpanEdit>), MoveRefusal> {
    let mut ordered: Vec<&(SpanLink, usize)> = pointing.iter().collect();
    ordered.sort_by_key(|(span, _)| span.range.start);
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut replaced: Vec<SpanEdit> = Vec::with_capacity(ordered.len());
    let mut cursor = 0usize;
    for (span, target) in ordered {
        if span.range.start < cursor || span.range.end > text.len() {
            // Overlapping or out-of-range recorded spans cannot come from one
            // extraction pass; refuse rather than splice garbage.
            return Err(MoveRefusal::VerificationFailed {
                note: span.note.clone(),
            });
        }
        let new_rel = new_rel_of
            .get(target)
            .ok_or(MoveRefusal::VerificationFailed {
                note: span.note.clone(),
            })?;
        let new_text = new_link_text(span, new_rel);
        out.extend_from_slice(&text.as_bytes()[cursor..span.range.start]);
        out.extend_from_slice(new_text.as_bytes());
        replaced.push(SpanEdit {
            line: span.line,
            span: span.range.clone(),
            old_text: text[span.range.clone()].to_string(),
            new_text,
        });
        cursor = span.range.end;
    }
    out.extend_from_slice(&text.as_bytes()[cursor..]);
    Ok((out, replaced))
}

/// The new link text for one recorded span: the moved file's full library
/// path, in the same syntax, with alias, heading, block reference, alt and
/// title preserved (ADR-017 §8).
fn new_link_text(span: &SpanLink, new_rel: &str) -> String {
    match span.syntax {
        links::LinkSyntax::MarkdownImage => {
            let alt = span.alias.clone().unwrap_or_default();
            let dest = markdown_dest(new_rel);
            match &span.markdown_title {
                Some(title) => format!("![{alt}]({dest} {})", quote_title(title)),
                None => format!("![{alt}]({dest})"),
            }
        }
        _ => {
            let mut out = String::with_capacity(new_rel.len() + 8);
            if span.embed {
                out.push('!');
            }
            out.push_str("[[");
            out.push_str(new_rel);
            if let Some(block) = &span.block {
                out.push_str(&format!("#^{block}"));
            } else if let Some(heading) = &span.heading {
                out.push_str(&format!("#{heading}"));
            }
            if let Some(alias) = &span.alias {
                out.push('|');
                out.push_str(alias);
            }
            out.push_str("]]");
            out
        }
    }
}

/// A Markdown destination: the path as written, percent-encoded where a bare
/// spelling would break or silently change the link (spaces, parentheses,
/// angle brackets, quotes — and `%` itself, because the reader decodes
/// percent sequences, so a raw `%XX` would resolve to different bytes).
fn markdown_dest(path: &str) -> String {
    let needs = path
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '"' | '\'' | '%'));
    if !needs {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '"' => out.push_str("%22"),
            '\'' => out.push_str("%27"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    out
}

/// A Markdown image title in the shape the reader round-trips: double quotes
/// when the text has none, single quotes otherwise.
fn quote_title(title: &str) -> String {
    if title.contains('"') && !title.contains('\'') {
        format!("'{title}'")
    } else {
        format!("\"{}\"", title.replace('\"', "\\\""))
    }
}

// ---------------------------------------------------------------------------
// Byte and path helpers.
// ---------------------------------------------------------------------------

/// Refuse destinations that could not name a library file: empty, absolute,
/// or with `.`/`..`/dot-prefixed components (dot paths belong to other
/// tools, ADR-018 §4).
fn check_destination_shape(dst: &str) -> Result<(), MoveRefusal> {
    let bad = dst.is_empty()
        || dst.starts_with('/')
        || dst
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.starts_with('.'));
    if bad {
        return Err(MoveRefusal::BadDestination {
            path: dst.to_string(),
        });
    }
    Ok(())
}

fn check_parents_free(dst: &str, rels: &HashSet<&str>) -> Result<(), MoveRefusal> {
    let mut prefix = String::new();
    let parts: Vec<&str> = dst.split('/').collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if rels.contains(prefix.as_str()) {
            return Err(MoveRefusal::TargetExists { path: prefix });
        }
    }
    Ok(())
}

/// A destination path is taken when anything occupies it on the
/// filesystem — a directory included, which the walk never reports —
/// or when a walked file holds it under the library's NFC +
/// casefold identity ([`links::key`]): exact-string matching misses
/// fold-equal spellings (`refs/X2.JPG` over an existing
/// `refs/x2.jpg`), which would otherwise surface after the renames
/// as a misleading verification failure instead of a refusal. The
/// one file a rename step may displace is the file it moves: a move
/// may differ from its source only by case or Unicode form, never
/// land on a different file.
///
/// Both facts are needed to allow that one file. The fold key alone cannot
/// see what holds the destination: on a case-insensitive volume a case-only
/// spelling IS the source, but on a case-sensitive volume the same spelling
/// can be a directory (the walk never reports directories) or an ignored
/// file, and moving onto it would land the group partially applied. So the
/// walk's fold-equal branch, the caller's `occupied` and the caller's
/// `same_file` are all required: free is "nothing there, or the very file
/// this step moves". `same_file` is entry identity ([`same_file_probe`]),
/// so a symlink at the destination that points at the source is a different
/// entry, not the file this step moves, and refuses whole (RW65c R1).
fn destination_taken(
    dst: &str,
    src: &str,
    files: &[FileEntry],
    occupied: &dyn Fn(&str) -> bool,
    same_file: &dyn Fn(&str, &str) -> bool,
) -> bool {
    let dst_key = links::key(dst);
    // The destination counts as free when its occupant is the file the step
    // moves: `occupied` alone would describe the source itself on a
    // case-insensitive volume, and the fold key alone would wave through a
    // directory that holds the case-shifted spelling on a case-sensitive
    // one. A different file is caught here or by the walk below.
    if occupied(dst) && !(links::key(src) == dst_key && same_file(src, dst)) {
        return true;
    }
    files
        .iter()
        .any(|entry| entry.rel != src && links::key(&entry.rel) == dst_key)
}

/// The `same_file` probe for [`plan_move`], over a library rooted at `root`:
/// two library-relative paths name the same file when they name the same
/// directory entry — device and inode on Unix, read without following a
/// final symlink (RW65c R1). Following links (`canonicalize`, `same_file`)
/// reads a symlink at the destination that points at the source as the
/// source itself, so the planner accepts a case-only self-rename it cannot
/// land: the writer's no-replace rename refuses the existing entry and the
/// group tears at apply time. Entry identity keeps a symlink — or any second
/// entry — distinct from the file it names, so the whole group refuses with
/// `TargetExists` instead. A case-only spelling of the source resolves to
/// the source's own entry, so the routine display-case fix still answers
/// true. Either path missing (or unreadable) is not the same file.
pub fn same_file_probe(root: PathBuf) -> impl Fn(&str, &str) -> bool {
    move |a: &str, b: &str| same_entry(&root.join(a), &root.join(b))
}

/// Whether two paths name the same directory entry, without following a
/// final symlink.
#[cfg(unix)]
fn same_entry(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Off Unix there is no portable device-and-inode: fall back to comparing
/// resolved paths, which answers the case-only spelling correctly wherever
/// the filesystem cannot put a fold-equal alias beside its target.
#[cfg(not(unix))]
fn same_entry(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn join_parent(parent_rel: &str, name: &str) -> String {
    match parent_rel.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

/// The library-relative path spelled exactly as on disk, `None` when a
/// component is not valid UTF-8 (then no link text or journal record can
/// name the file).
fn exact_rel(entry: &FileEntry) -> Option<String> {
    let mut out = String::new();
    for component in entry.path.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let text = part.to_str()?;
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(text);
    }
    (!out.is_empty()).then_some(out)
}

/// The same entry at a new library-relative path (valid UTF-8 by plan).
fn repositioned(entry: &FileEntry, new_rel: &str) -> FileEntry {
    let path = PathBuf::from(new_rel);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(new_rel)
        .to_string();
    FileEntry {
        rel: new_rel.to_string(),
        name: name.clone(),
        path,
        native_name: OsString::from(&name),
        class: entry.class,
    }
}

/// A freshly created image note as a walk entry would show it.
fn new_note_entry(new_rel: &str) -> FileEntry {
    let mut entry = repositioned(
        &FileEntry {
            rel: String::new(),
            name: String::new(),
            path: PathBuf::new(),
            native_name: OsString::new(),
            class: FileClass::ImageNote,
        },
        new_rel,
    );
    entry.class = FileClass::ImageNote;
    entry
}

/// Append one embed line in the file's own newline style (the same shape
/// [`crate::fm_edit::ensure_self_embed`] writes, without its byte-presence
/// check: the caller has already decided the embed is missing by resolving).
fn append_embed(raw: &[u8], embed: &str) -> Vec<u8> {
    let newline: &[u8] = if raw.contains(&b'\n') {
        match raw.iter().position(|&b| b == b'\n') {
            Some(pos) if pos > 0 && raw[pos - 1] == b'\r' => b"\r\n",
            _ => b"\n",
        }
    } else if raw.contains(&b'\r') {
        b"\r"
    } else {
        b"\n"
    };
    let after_bom = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + embed.len() + 2);
    if after_bom.is_empty() {
        out.extend_from_slice(&raw[..raw.len() - after_bom.len()]);
        out.extend_from_slice(embed.as_bytes());
        out.extend_from_slice(newline);
        return out;
    }
    out.extend_from_slice(raw);
    match out.last() {
        Some(b'\n') => {}
        Some(b'\r') if newline != b"\r" => out.push(b'\n'),
        Some(b'\r') => {}
        _ => out.extend_from_slice(newline),
    }
    out.extend_from_slice(embed.as_bytes());
    out.extend_from_slice(newline);
    out
}
