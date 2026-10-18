//! The upload operation (ADR-017 §11): land one or more
//! uploaded files as image groups — the image, its note, and
//! the collection join — inside a single journaled operation.
//!
//! The pure decisions are dimagine-core's
//! [`dimagine_core::upload`] (placement inputs, naming, the
//! format check, the note bytes, the join edit); this module
//! is the write half. It holds the one write lock across the
//! whole upload, plans every step from the facts the
//! filesystem and the caller's library walk give it, and
//! commits through the journal, so a crash recovers and
//! `dimagine undo` removes the whole group (ADR-016 §7, §8).

use std::collections::{HashMap, HashSet};

use chrono::{Local, Utc};
use dimagine_core::body_edit::{append_member, link_text_for, BodyCtx, Embed};
use dimagine_core::fm_edit::{ensure_id, ensure_self_embed};
use dimagine_core::library::FileClass;
use dimagine_core::sniff::SNIFF_LEN;
use dimagine_core::upload::{
    check_upload, join_collection_note, upload_embed_text, upload_file_name, upload_note_bytes,
    UploadNote,
};

use crate::error::{Error, Result};
use crate::fingerprint::Fingerprint;
use crate::journal::{StepKind, StepRecord};
use crate::paths::{self, RelPath, StepPath};
use crate::writer::{Precondition, Writer};

/// The most files one upload may carry (ADR-018 §5: a batch
/// over this is refused before anything is opened).
pub const MAX_UPLOAD_BATCH: usize = 1000;

/// One uploaded file: the name it arrived with and its bytes.
pub struct UploadItem {
    /// The file name the upload carried (a browser's spelling,
    /// not trusted: only its last path component is used).
    pub original_name: String,
    /// The file's bytes, already in memory.
    pub bytes: Vec<u8>,
}

/// Where an upload lands (ADR-017 §11.1).
pub struct UploadRequest {
    /// The folder the images land in, `""` for the library
    /// root: the explicit `--to` folder, or the library's
    /// default upload folder (`.obsidian/app.json`'s
    /// `attachmentFolderPath`, else `inbox/`), which the
    /// caller resolves from its own walk.
    pub folder: String,
    /// An existing collection note to join every landed image
    /// into (`--to-collection`).
    pub collection: Option<RelPath>,
}

/// What happened to one uploaded file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadOutcome {
    /// The image's library-relative path: where it landed, or,
    /// for a duplicate, where it already lived.
    pub image: String,
    /// The image note's path — a duplicate's own note, when it
    /// has one.
    pub note: String,
    /// The arrival name was meaningless, so the file landed as
    /// `Upload <timestamp>`; the original name is the note's
    /// `title`.
    pub renamed: bool,
    /// The bytes were already in the library, by content hash:
    /// nothing was written for this file.
    pub duplicate: bool,
}

/// What the pre-flight checks settled: the parsed
/// placement, with the collection's bytes read.
struct Prepared {
    folder: Option<RelPath>,
    collection: Option<RelPath>,
    collection_bytes: Option<Vec<u8>>,
    /// The collection note as [`Writer::prepare`] read it — the state the
    /// join's bytes are computed from and the precondition the join
    /// commits against (ADR-016 §3: never rename bytes computed from a
    /// pre-lock read).
    collection_fp: Option<Fingerprint>,
}

/// A planned upload: the steps that land it, the
/// preconditions those steps commit against, and the
/// per-file outcomes it reports.
struct Plan {
    steps: Vec<StepRecord>,
    preconditions: Vec<Precondition>,
    outcomes: Vec<UploadOutcome>,
}

impl Writer {
    /// Upload files as image groups (ADR-017 §11), one
    /// journaled operation for the whole request: every image
    /// and note this call creates, and the one collection
    /// edit, land or fail together, and `dimagine undo`
    /// removes the whole group.
    ///
    /// * `ctx` is the caller's walk of the library as it was
    ///   before the upload — the collection join for a
    ///   duplicate resolves membership through it.
    /// * `dedupe` answers "does the library already hold
    ///   these bytes" by content hash (the index's
    ///   `files.sha256`), returning the existing image's
    ///   library-relative path. A hit is verified against the
    ///   filesystem, so a stale index row never reports a
    ///   file that is not there.
    /// * `importer` is the `dimagine <version>` spelling the
    ///   note's `sources` entry records.
    ///
    /// Any refusal — a format check, an unclosed collection
    /// fence, a batch over [`MAX_UPLOAD_BATCH`] — fails the
    /// whole upload before the first byte is written.
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &self,
        op_id: &str,
        source: &str,
        client_addr: Option<&str>,
        request: UploadRequest,
        items: Vec<UploadItem>,
        ctx: &BodyCtx<'_>,
        dedupe: impl Fn(&str) -> Option<String>,
        importer: &str,
    ) -> Result<Vec<UploadOutcome>> {
        let prepared = self.prepare(&request, items.len())?;
        let _lock = self.lock_library()?;
        let plan = self.plan(&prepared, items, ctx, dedupe, importer)?;
        let intent = serde_json::json!({
            "action": "upload",
            "files": plan.outcomes.len(),
        });
        self.execute_op_locked(
            &_lock,
            op_id,
            source,
            client_addr,
            intent,
            plan.steps,
            None,
            plan.preconditions,
        )?;
        Ok(plan.outcomes)
    }

    /// Plan an upload without writing anything: the same
    /// placement, naming, dedupe and collision decisions
    /// [`Writer::upload`] would make, reported per file (the
    /// `--dry-run` half of the upload command). Advisory
    /// only — the plan takes no lock, so another writer may
    /// change the library between the plan and a real
    /// upload.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_upload(
        &self,
        request: UploadRequest,
        items: Vec<UploadItem>,
        ctx: &BodyCtx<'_>,
        dedupe: impl Fn(&str) -> Option<String>,
        importer: &str,
    ) -> Result<Vec<UploadOutcome>> {
        let prepared = self.prepare(&request, items.len())?;
        let plan = self.plan(&prepared, items, ctx, dedupe, importer)?;
        Ok(plan.outcomes)
    }

    /// The pre-flight every upload passes through: the batch
    /// limit, the placement's folder, and the collection's
    /// bytes, read before any file is planned so a missing or
    /// unreadable target refuses the whole upload first.
    fn prepare(&self, request: &UploadRequest, count: usize) -> Result<Prepared> {
        if count > MAX_UPLOAD_BATCH {
            return Err(Error::UploadRefused {
                message: format!(
                    "an upload may carry at most {MAX_UPLOAD_BATCH} files; this one carries {count}"
                ),
            });
        }
        let folder = if request.folder.is_empty() {
            None
        } else {
            Some(RelPath::parse(&request.folder)?)
        };
        // The collection's bytes and its fingerprint as one read: the join
        // computes from the bytes and commits against the fingerprint
        // (ADR-016 §3).
        let collection_read = match &request.collection {
            Some(collection) => match self.read(collection) {
                Ok(read) => Some((collection.clone(), read.0, read.1)),
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                    return Err(Error::UploadRefused {
                        message: format!("collection {} does not exist", collection.text()),
                    });
                }
                Err(e) => return Err(e),
            },
            None => None,
        };
        let collection_bytes = collection_read.as_ref().map(|(_, bytes, _)| bytes.clone());
        let collection_fp = collection_read.map(|(_, _, fp)| fp);
        Ok(Prepared {
            folder,
            collection: request.collection.clone(),
            collection_bytes,
            collection_fp,
        })
    }

    /// The whole plan, built from the facts the filesystem
    /// and the caller's walk give it. [`Writer::upload`]
    /// calls this under the write lock; the dry-run plan
    /// takes no lock and is advisory.
    fn plan(
        &self,
        prepared: &Prepared,
        items: Vec<UploadItem>,
        ctx: &BodyCtx<'_>,
        dedupe: impl Fn(&str) -> Option<String>,
        importer: &str,
    ) -> Result<Plan> {
        let folder = prepared.folder.as_ref();
        // One walk for the whole batch: the collision rule (a
        // name colliding anywhere in the library, NFC +
        // case-insensitive, ADR-017 §4) is the same for every
        // file, and the names this batch takes are remembered
        // as it goes, so a second file with the same arrival
        // name lands as ` 2` without another walk.
        let folded_lib = self.library_fold_set()?;
        let mut taken: HashSet<String> = HashSet::new();
        let mut by_sha: HashMap<String, String> = HashMap::new();
        let mut steps: Vec<StepRecord> = Vec::new();
        let mut preconditions: Vec<Precondition> = Vec::new();
        let mut outcomes: Vec<UploadOutcome> = Vec::new();
        let mut landed: Vec<String> = Vec::new();
        let note_time = Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let name_time = Local::now().format("%Y%m%d%H%M%S").to_string();

        for item in items {
            // The format check comes first: a file whose bytes
            // are not the format its name claims, or that is
            // SVG, HTML or XML, refuses the whole upload before
            // anything is written (ADR-018 §5).
            let head = &item.bytes[..item.bytes.len().min(SNIFF_LEN)];
            if let Err(refusal) = check_upload(head, &item.original_name) {
                return Err(Error::UploadRefused {
                    message: format!("{}: {}", item.original_name, refusal.message()),
                });
            }
            let sha = Fingerprint::of_bytes(&item.bytes).hex();

            // A duplicate by content: the library (or this
            // very batch) already holds these bytes, so
            // nothing is written and the outcome names the
            // file that does.
            let existing = if let Some(path) = by_sha.get(&sha) {
                // This batch's own create: planned, not on
                // disk yet, so there is nothing to verify.
                Some(path.clone())
            } else {
                dedupe(&sha).and_then(|path| {
                    // The index may be a step behind the
                    // filesystem: a row whose file is gone
                    // is no duplicate.
                    RelPath::parse(&path)
                        .ok()
                        .filter(|rel| self.read(rel).is_ok())
                        .map(|_| path)
                })
            };
            if let Some(image) = existing {
                landed.push(image.clone());
                outcomes.push(UploadOutcome {
                    note: format!("{image}.md"),
                    image,
                    renamed: false,
                    duplicate: true,
                });
                continue;
            }

            let (stem, ext, renamed) = upload_file_name(&item.original_name, &name_time);
            let image_rel = next_free_name(&folded_lib, &taken, folder, &stem, &ext)?;
            let image_folder = image_rel
                .parent_components()
                .iter()
                .map(|component| component.as_str())
                .collect::<Vec<_>>()
                .join("/");
            let embed = upload_embed_text(&image_folder, image_rel.name());
            let note = upload_note_bytes(
                &UploadNote {
                    id: &ulid::Ulid::new().to_string(),
                    time: &note_time,
                    importer,
                    original_name: &item.original_name,
                    title: renamed.then_some(item.original_name.as_str()),
                },
                &embed,
            );

            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Create,
                src: None,
                dst: image_rel.text(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: Some(item.bytes),
            });
            let note_rel = RelPath::parse(&format!("{}.md", image_rel.text()))?;
            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Create,
                src: None,
                dst: note_rel.text(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: Some(note),
            });
            // The collision rule is on the bare name,
            // library-wide (ADR-017 §4): the name this
            // batch took is remembered by its fold key,
            // the same key `next_free_name` checks.
            taken.insert(paths::fold_key(&paths::to_nfc(image_rel.name())));
            by_sha.insert(sha, image_rel.text());
            landed.push(image_rel.text());
            outcomes.push(UploadOutcome {
                note: note_rel.text(),
                image: image_rel.text(),
                renamed,
                duplicate: false,
            });
        }

        // The collection join: one edit for the whole request.
        // A file this operation creates is a member the moment
        // its embed lands, so the join judges membership by the
        // embed text; a duplicate already exists, so its join
        // resolves membership against the walk instead
        // (ADR-017 §4).
        if let (Some(collection), Some(bytes)) = (&prepared.collection, &prepared.collection_bytes)
        {
            let note_idx =
                ctx.index_of(&collection.text())
                    .ok_or_else(|| Error::UploadRefused {
                        message: format!(
                            "collection {} is not in the library walk",
                            collection.text()
                        ),
                    })?;
            let mut joined = bytes.clone();
            let mut id_done: HashSet<String> = HashSet::new();
            for image in &landed {
                let rel = RelPath::parse(image)?;
                let embed = upload_embed_text(
                    &rel.parent_components()
                        .iter()
                        .map(|component| component.as_str())
                        .collect::<Vec<_>>()
                        .join("/"),
                    rel.name(),
                );
                match ctx.index_of(image) {
                    // A duplicate: resolve membership through
                    // the walk, the same edit `collect --add`
                    // makes. Joining assigns the image an `id`
                    // (ADR-017 §3), so the duplicate's own note
                    // is created or updated in this operation too —
                    // the step `collect --add`'s id assignment makes.
                    Some(image_idx) => {
                        joined = append_member(&joined, note_idx, image_idx, ctx)
                            .map_err(|error| Error::UploadRefused {
                                message: format!("collection {}: {error}", collection.text()),
                            })?
                            .bytes;
                        if id_done.insert(image.clone()) {
                            self.push_duplicate_id_step(
                                &mut steps,
                                &mut preconditions,
                                ctx,
                                image,
                            )?;
                        }
                    }
                    // Created by this operation: the embed text
                    // is exactly the link that will name it.
                    None => {
                        joined = join_collection_note(&joined, &embed).map_err(|error| {
                            Error::UploadRefused {
                                message: format!("collection {}: {error}", collection.text()),
                            }
                        })?
                    }
                }
            }
            if joined != *bytes {
                steps.push(StepRecord {
                    step_idx: steps.len(),
                    kind: StepKind::Modify,
                    src: None,
                    dst: collection.text(),
                    before_hash: None,
                    after_hash: String::new(),
                    before_bytes: None,
                    after_bytes: Some(joined),
                });
                // Only a join that will write carries the precondition: an
                // upload whose join turned out to be a no-op never intends to
                // touch the collection, so a concurrent edit to it must not
                // refuse this one. The fingerprint is `prepare`'s read, before
                // the write lock — the join is the last step of up to 2×1000
                // creates, and this is what stops it renaming bytes computed
                // from a read that has since gone stale (ADR-016 §3).
                if let Some(fp) = &prepared.collection_fp {
                    preconditions.push(Precondition::from_read(
                        StepPath::Library(collection.clone()),
                        fp.clone(),
                    ));
                }
            }
        }

        Ok(Plan {
            steps,
            preconditions,
            outcomes,
        })
    }

    /// Give a duplicate target's own note an `id` (ADR-017 §3: a
    /// join assigns one) and, for a note this creates, its
    /// self-embed (ADR-017 §2), appending the create or modify step
    /// the join operation carries. A note that already holds an `id`
    /// adds no step, so the join is a no-op for it. This mirrors the
    /// `collect --add` id assignment for an image the upload did not
    /// land itself.
    ///
    /// A modify step carries the precondition its own read earned it
    /// (ADR-016 §3): the note is read while the operation is planned
    /// and rewritten at the very end, so an edit made in between must
    /// be a typed conflict rather than a silent revert.
    fn push_duplicate_id_step(
        &self,
        steps: &mut Vec<StepRecord>,
        preconditions: &mut Vec<Precondition>,
        ctx: &BodyCtx<'_>,
        image_rel: &str,
    ) -> Result<()> {
        let note_text = format!("{image_rel}.md");
        let note_rel = RelPath::parse(&note_text)?;
        let exists = ctx
            .files
            .iter()
            .any(|entry| entry.class == FileClass::ImageNote && entry.rel == note_text);
        // A read that fails is a refusal, never an empty base: planning
        // `ensure_id` over empty bytes would replace the note with a
        // document built from nothing.
        let read = if exists {
            Some(self.read(&note_rel).map_err(|error| Error::UploadRefused {
                message: format!("{note_text}: {error}"),
            })?)
        } else {
            None
        };
        let base = read
            .as_ref()
            .map(|(bytes, _)| bytes.clone())
            .unwrap_or_default();
        let edited = ensure_id(&base, &ulid::Ulid::new().to_string()).map_err(|error| {
            Error::UploadRefused {
                message: format!("{note_text}: {error}"),
            }
        })?;
        if exists && edited == base {
            return Ok(());
        }
        let final_bytes = if exists {
            edited
        } else {
            let image_idx = ctx
                .index_of(image_rel)
                .ok_or_else(|| Error::UploadRefused {
                    message: format!("{image_rel} is not in the library walk"),
                })?;
            let embed = match link_text_for(image_idx, ctx) {
                Ok(Embed::Text(text)) => text,
                Ok(Embed::RenameOrMove { name, .. }) => {
                    return Err(Error::UploadRefused {
                        message: format!(
                        "bare name {name} is ambiguous; move it into a folder or rename it first"
                    ),
                    })
                }
                Err(error) => {
                    return Err(Error::UploadRefused {
                        message: error.to_string(),
                    })
                }
            };
            let mut seeded = edited;
            seeded.push(b'\n');
            ensure_self_embed(&seeded, &embed).map_err(|error| Error::UploadRefused {
                message: format!("{note_text}: {error}"),
            })?
        };
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: if exists {
                StepKind::Modify
            } else {
                StepKind::Create
            },
            src: None,
            dst: note_rel.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(final_bytes),
        });
        if let Some((_, fp)) = read {
            preconditions.push(Precondition::from_read(StepPath::Library(note_rel), fp));
        }
        Ok(())
    }
}

/// The next free name for one uploaded file: the first
/// candidate that collides with neither the library (one
/// fold-set walk, taken by the caller for the whole batch)
/// nor the names this batch has already taken (ADR-017 §4:
/// NFC + case-insensitive, library-wide, with the ` 2`, ` 3`
/// suffixes).
fn next_free_name(
    folded_lib: &HashSet<String>,
    taken: &HashSet<String>,
    parent: Option<&RelPath>,
    stem: &str,
    ext: &str,
) -> Result<RelPath> {
    let dotted = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            format!("{stem}{dotted}")
        } else {
            format!("{stem} {n}{dotted}")
        };
        let nfc = paths::to_nfc(&candidate);
        let key = paths::fold_key(&nfc);
        if !folded_lib.contains(&key) && !taken.contains(&key) {
            return match parent {
                Some(folder) => folder.join(&nfc),
                None => RelPath::parse(&nfc),
            };
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_free_name_avoids_the_library_and_the_batch() {
        let mut lib = HashSet::new();
        lib.insert(paths::fold_key("girl.png"));
        lib.insert(paths::fold_key("GIRL.PNG"));
        let mut taken = HashSet::new();
        taken.insert(paths::fold_key("boy.png"));
        let root = None;
        // A library collision and a case-only collision both
        // move to ` 2`.
        assert_eq!(
            next_free_name(&lib, &taken, root, "girl", "png")
                .unwrap()
                .text(),
            "girl 2.png"
        );
        // A name this batch took moves on too.
        assert_eq!(
            next_free_name(&lib, &taken, root, "boy", "png")
                .unwrap()
                .text(),
            "boy 2.png"
        );
        // An untouched name is used as-is.
        assert_eq!(
            next_free_name(&lib, &taken, root, "cat", "png")
                .unwrap()
                .text(),
            "cat.png"
        );
        // NFC folding: a composed and a decomposed name collide.
        let mut lib = HashSet::new();
        lib.insert(paths::fold_key("café.png"));
        assert_eq!(
            next_free_name(&lib, &HashSet::new(), root, "café", "png")
                .unwrap()
                .text(),
            "café 2.png"
        );
    }
}
