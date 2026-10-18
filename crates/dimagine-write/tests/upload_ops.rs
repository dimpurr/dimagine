//! The upload operation end to end (ADR-017 §11): files land
//! as image groups with notes, one journaled operation per
//! request, undoable as a whole.

use std::cell::Cell;
use std::collections::HashMap;

use dimagine_core::body_edit::BodyCtx;
use dimagine_core::library::Library;
use dimagine_core::links::Resolver;
use dimagine_write::fingerprint::Fingerprint;
use dimagine_write::upload::{UploadItem, UploadRequest};
use dimagine_write::{RelPath, UndoneItem, Writer};
use tempfile::tempdir;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9C, 0x62, 0, 0x01, 0, 0, 0x05, 0, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];
const OTHER_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9C, 0x62, 0, 0x01, 0, 0, 0x05, 0, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0, 0, 0, 0, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x83,
];
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00,
];
const COLLECTION: &[u8] = b"---\nkind: collection\ntitle: Shots\n---\n\nFirst shot.\n";

/// Run `f` with a walk of the library as it stands.
fn with_ctx<T>(root: &std::path::Path, f: impl FnOnce(&BodyCtx<'_>) -> T) -> T {
    let library = Library::open(root).unwrap();
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    f(&ctx)
}

fn upload(
    op_id: &str,
    writer: &Writer,
    root: &std::path::Path,
    folder: &str,
    collection: Option<&str>,
    items: Vec<(&str, &[u8])>,
    dedupe: Option<&HashMap<String, String>>,
) -> Result<Vec<dimagine_write::UploadOutcome>, dimagine_write::Error> {
    with_ctx(root, |ctx| {
        writer.upload(
            op_id,
            "test",
            None,
            UploadRequest {
                folder: folder.to_string(),
                collection: collection.map(|rel| RelPath::parse(rel).unwrap()),
            },
            items
                .into_iter()
                .map(|(name, bytes)| UploadItem {
                    original_name: name.to_string(),
                    bytes: bytes.to_vec(),
                })
                .collect(),
            ctx,
            |sha| dedupe.and_then(|map| map.get(sha).cloned()),
            "dimagine test",
        )
    })
}

fn put(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// Nothing but the write core's own `.dimagine/` state
/// is in the library root.
fn nothing_landed(root: &std::path::Path) {
    let entries: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name != ".dimagine")
        .collect();
    assert!(entries.is_empty(), "{entries:?}");
}

/// A new file lands with its note, and the note carries the
/// provenance an upload records (ADR-017 §11.5).
#[test]
fn a_new_file_lands_with_a_note() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(
        &root.join(".obsidian/app.json"),
        b"{\"attachmentFolderPath\": \"inbox\"}",
    );
    let writer = Writer::open_raw(root).unwrap();

    let outcomes = upload(
        "OP-UPLOAD-00000000000001",
        &writer,
        root,
        "inbox",
        None,
        vec![("girl.png", PNG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].image, "inbox/girl.png");
    assert_eq!(outcomes[0].note, "inbox/girl.png.md");
    assert!(!outcomes[0].renamed);
    assert!(!outcomes[0].duplicate);
    assert_eq!(std::fs::read(root.join("inbox/girl.png")).unwrap(), PNG);

    let note = std::fs::read_to_string(root.join("inbox/girl.png.md")).unwrap();
    assert!(note.starts_with("---\nid: "), "{note}");
    assert!(note.contains("added: "), "{note}");
    assert!(note.contains("imported: "), "{note}");
    assert!(note.contains("  - type: upload\n"), "{note}");
    assert!(note.contains("    importer: \"dimagine test\"\n"), "{note}");
    assert!(note.contains("    original_name: \"girl.png\"\n"), "{note}");
    assert!(note.contains("\n\n![[inbox/girl.png]]\n"), "{note}");
    // A meaningful name is not a title.
    assert!(!note.contains("title:"), "{note}");
}

/// A meaningless camera name lands as `Upload <timestamp>`
/// with the original name as the note's `title`
/// (ADR-017 §11.2).
#[test]
fn a_meaningless_name_is_renamed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();

    let outcomes = upload(
        "OP-UPLOAD-00000000000002",
        &writer,
        root,
        "",
        None,
        vec![("IMG_1234.jpg", JPEG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].renamed);
    assert!(
        outcomes[0].image.starts_with("Upload 2") && outcomes[0].image.ends_with(".jpg"),
        "{}",
        outcomes[0].image
    );
    let note = std::fs::read_to_string(root.join(&outcomes[0].note)).unwrap();
    assert!(note.contains("title: \"IMG_1234.jpg\"\n"), "{note}");
    assert!(
        note.contains(&format!("![[{}]]\n", outcomes[0].image)),
        "{note}"
    );
}

/// Bytes the library already holds, by content hash, are
/// not written again: the outcome names the file that does
/// (ADR-017 §11.3).
#[test]
fn a_duplicate_by_content_is_not_written_again() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("refs/girl.png"), PNG);
    let writer = Writer::open_raw(root).unwrap();
    let sha = Fingerprint::of_bytes(PNG).hex();
    let mut known = HashMap::new();
    known.insert(sha, "refs/girl.png".to_string());

    let outcomes = upload(
        "OP-UPLOAD-00000000000003",
        &writer,
        root,
        "inbox",
        None,
        vec![("elsewhere.png", PNG)],
        Some(&known),
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].duplicate);
    assert_eq!(outcomes[0].image, "refs/girl.png");
    // Nothing new on disk, and the existing file is untouched.
    assert!(!root.join("inbox").exists());
    assert_eq!(std::fs::read(root.join("refs/girl.png")).unwrap(), PNG);
}

/// A stale dedupe row — the index still names a file the
/// filesystem no longer holds — is no duplicate: the upload
/// lands a new file.
#[test]
fn a_stale_dedupe_row_lands_a_new_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    let sha = Fingerprint::of_bytes(PNG).hex();
    let mut known = HashMap::new();
    known.insert(sha, "gone/girl.png".to_string());

    let outcomes = upload(
        "OP-UPLOAD-00000000000004",
        &writer,
        root,
        "",
        None,
        vec![("girl.png", PNG)],
        Some(&known),
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].duplicate);
    assert_eq!(outcomes[0].image, "girl.png");
    assert!(root.join("girl.png").exists());
}

/// The collection join lands in the same operation: one
/// embed line for the new image (ADR-017 §4), and `undo`
/// takes the whole group back — the collection edit is
/// restored and the created files are handed to the
/// trash (ADR-016 §8).
#[test]
fn a_collection_join_lands_and_undo_removes_the_group() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("shots.md"), COLLECTION);
    let writer = Writer::open_raw(root).unwrap();

    let outcomes = upload(
        "OP-UPLOAD-00000000000005",
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        vec![("girl.png", PNG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    let joined = std::fs::read_to_string(root.join("shots.md")).unwrap();
    assert!(
        joined.contains("First shot.\n![[inbox/girl.png]]\n"),
        "{joined}"
    );

    let outcome = writer.undo("OP-UPLOAD-00000000000005", None).unwrap();
    // The created image and note are handed back to the
    // trash...
    let mut created: Vec<String> = outcome
        .undone
        .iter()
        .filter_map(|item| match item {
            UndoneItem::TrashThis(path) => Some(path.text()),
            _ => None,
        })
        .collect();
    created.sort();
    assert_eq!(created, vec!["inbox/girl.png", "inbox/girl.png.md"]);
    // ...and the collection edit is already restored.
    assert_eq!(std::fs::read(root.join("shots.md")).unwrap(), COLLECTION);
    // The files are still on disk until the caller trashes
    // them (the CLI's undo does that as its own operation).
    assert!(root.join("inbox/girl.png").exists());
}

/// A duplicate that is already a collection member does not
/// add a second embed line: the join resolves membership
/// against the walk (ADR-017 §4).
#[test]
fn a_duplicate_member_is_not_joined_twice() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("refs/girl.png"), PNG);
    put(
        &root.join("refs/girl.png.md"),
        b"---\nid: 01A\n---\n\n![[refs/girl.png]]\n",
    );
    put(
        &root.join("shots.md"),
        b"---\nkind: collection\n---\n\n![[refs/girl.png]]\n",
    );
    let writer = Writer::open_raw(root).unwrap();
    let sha = Fingerprint::of_bytes(PNG).hex();
    let mut known = HashMap::new();
    known.insert(sha, "refs/girl.png".to_string());

    let outcomes = upload(
        "OP-UPLOAD-00000000000006",
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        vec![("again.png", PNG)],
        Some(&known),
    )
    .unwrap();
    assert!(outcomes[0].duplicate);
    // The collection still names the image exactly once.
    let joined = std::fs::read_to_string(root.join("shots.md")).unwrap();
    assert_eq!(joined.matches("girl.png").count(), 1, "{joined}");
}

/// Two files with the same arrival name do not collide: the
/// second lands as ` 2` (ADR-017 §4), and the same bytes
/// twice in one batch land once.
#[test]
fn a_batch_collides_and_dedupes_with_itself() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();

    let outcomes = upload(
        "OP-UPLOAD-00000000000007",
        &writer,
        root,
        "",
        None,
        vec![("girl.png", PNG), ("girl.png", OTHER_PNG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].image, "girl.png");
    assert_eq!(outcomes[1].image, "girl 2.png");
    assert_eq!(std::fs::read(root.join("girl.png")).unwrap(), PNG);
    assert_eq!(std::fs::read(root.join("girl 2.png")).unwrap(), OTHER_PNG);

    // The same bytes twice: the second occurrence joins the
    // first's path and nothing more is written.
    let outcomes = upload(
        "OP-UPLOAD-00000000000008",
        &writer,
        root,
        "",
        None,
        vec![("one.png", PNG), ("two.png", PNG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].image, "one.png");
    assert!(outcomes[1].duplicate);
    assert_eq!(outcomes[1].image, "one.png");
    assert!(!root.join("two.png").exists());
}

/// The collision rule is on the bare name, library-wide
/// (ADR-017 §4): two same-named files landing in a folder
/// still take ` 2`, and a same-named file already in the
/// library is avoided wherever it sits.
#[test]
fn a_collision_in_a_folder_still_takes_the_suffix() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // An existing image elsewhere in the library with the
    // same bare name as the upload's.
    put(&root.join("refs/girl.png"), OTHER_PNG);
    let writer = Writer::open_raw(root).unwrap();

    let outcomes = upload(
        "OP-UPLOAD-00000000000016",
        &writer,
        root,
        "inbox",
        None,
        vec![("girl.png", PNG), ("girl.png", OTHER_PNG)],
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].image, "inbox/girl 2.png");
    assert_eq!(outcomes[1].image, "inbox/girl 3.png");
    assert_eq!(std::fs::read(root.join("inbox/girl 2.png")).unwrap(), PNG);
    assert_eq!(
        std::fs::read(root.join("inbox/girl 3.png")).unwrap(),
        OTHER_PNG
    );
}

/// A refusal writes nothing: bytes that are not the format
/// the name claims, and SVG, HTML and XML, stop the whole
/// upload before the first file (ADR-018 §5).
#[test]
fn a_refusal_writes_nothing() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();

    // PNG bytes behind a JPEG name.
    let error = upload(
        "OP-UPLOAD-00000000000009",
        &writer,
        root,
        "",
        None,
        vec![("girl.jpg", PNG)],
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    assert!(error.to_string().contains("girl.jpg"), "{error}");
    // SVG behind a PNG name.
    let error = upload(
        "OP-UPLOAD-00000000000010",
        &writer,
        root,
        "",
        None,
        vec![("girl.png", b"<svg></svg>")],
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    // A .svg name is refused however its bytes begin.
    let error = upload(
        "OP-UPLOAD-00000000000011",
        &writer,
        root,
        "",
        None,
        vec![("girl.svg", PNG)],
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    // Nothing landed.
    nothing_landed(root);
}

/// A batch over the limit is refused before anything is
/// opened (ADR-018 §5).
#[test]
fn an_over_limit_batch_is_refused() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    let items: Vec<(&str, &[u8])> = (0..=dimagine_write::MAX_UPLOAD_BATCH)
        .map(|_| ("girl.png", PNG))
        .collect();
    let error = upload(
        "OP-UPLOAD-00000000000012",
        &writer,
        root,
        "",
        None,
        items,
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    nothing_landed(root);
    // Exactly at the limit is allowed.
    let items: Vec<(&str, &[u8])> = (0..dimagine_write::MAX_UPLOAD_BATCH)
        .map(|_| ("girl.png", PNG))
        .collect();
    let outcomes = upload(
        "OP-UPLOAD-00000000000013",
        &writer,
        root,
        "",
        None,
        items,
        None,
    )
    .unwrap();
    assert_eq!(outcomes.len(), dimagine_write::MAX_UPLOAD_BATCH);
}

/// A collection that does not exist refuses the upload
/// before anything is written.
#[test]
fn a_missing_collection_refuses_the_upload() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    let error = upload(
        "OP-UPLOAD-00000000000014",
        &writer,
        root,
        "",
        Some("nope.md"),
        vec![("girl.png", PNG)],
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    assert!(error.to_string().contains("nope.md"), "{error}");
    nothing_landed(root);
}

/// A dedupe hit joined into a collection gets the `id` ADR-017 §3 assigns on
/// the join: an id-less existing image gains a note with one and its
/// self-embed, while an image that already carries an `id` is left alone.
#[test]
fn a_dedupe_hit_joined_into_a_collection_gains_an_id() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // An id-less image with no note, and one that already has both.
    put(&root.join("refs/girl.png"), PNG);
    put(&root.join("refs/boy.png"), OTHER_PNG);
    put(
        &root.join("refs/boy.png.md"),
        b"---\nid: 01B\n---\n\n![[refs/boy.png]]\n",
    );
    put(&root.join("shots.md"), COLLECTION);
    let writer = Writer::open_raw(root).unwrap();
    let girl = Fingerprint::of_bytes(PNG).hex();
    let boy = Fingerprint::of_bytes(OTHER_PNG).hex();
    let mut known = HashMap::new();
    known.insert(girl, "refs/girl.png".to_string());
    known.insert(boy, "refs/boy.png".to_string());

    let outcomes = upload(
        "OP-UPLOAD-00000000000018",
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        vec![("a.png", PNG), ("b.png", OTHER_PNG)],
        Some(&known),
    )
    .unwrap();
    assert!(outcomes.iter().all(|outcome| outcome.duplicate));

    // The id-less image gained a note with an `id` and its self-embed.
    let girl_note = std::fs::read_to_string(root.join("refs/girl.png.md")).unwrap();
    assert!(girl_note.starts_with("---\nid: "), "{girl_note}");
    assert!(
        girl_note.contains("\n\n![[refs/girl.png]]\n"),
        "{girl_note}"
    );
    // The image whose note already held an `id` is byte-for-byte untouched.
    assert_eq!(
        std::fs::read(root.join("refs/boy.png.md")).unwrap(),
        b"---\nid: 01B\n---\n\n![[refs/boy.png]]\n"
    );
    // Both joined the collection, once each.
    let joined = std::fs::read_to_string(root.join("shots.md")).unwrap();
    assert_eq!(joined.matches("girl.png").count(), 1, "{joined}");
    assert_eq!(joined.matches("boy.png").count(), 1, "{joined}");
}

/// A collection whose body has an unclosed code block is
/// refused: the join would land inside code (ADR-017 §4).
#[test]
fn an_unclosed_fence_refuses_the_join() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(
        &root.join("shots.md"),
        b"---\nkind: collection\n---\n\n```\ncode\n",
    );
    let writer = Writer::open_raw(root).unwrap();
    let error = upload(
        "OP-UPLOAD-00000000000015",
        &writer,
        root,
        "",
        Some("shots.md"),
        vec![("girl.png", PNG)],
        None,
    )
    .unwrap_err();
    assert!(matches!(error, dimagine_write::Error::UploadRefused { .. }));
    assert!(error.to_string().contains("shots.md"), "{error}");
    // The image did not land either: the whole operation
    // refused.
    assert!(!root.join("girl.png").exists());
}

/// An external edit to the collection note between `prepare`'s read and the
/// commit is refused with a typed conflict, never silently reverted
/// (ADR-016 §3: never rename bytes computed from a pre-lock read).
#[test]
fn an_external_edit_to_the_collection_refuses_the_join() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("shots.md"), COLLECTION);
    let writer = Writer::open_raw(root).unwrap();
    let edited =
        b"---\nkind: collection\ntitle: Shots\n---\n\nFirst shot.\n\nEdited elsewhere.\n".to_vec();

    // The dedupe lookup runs in `plan`, after `prepare` read the collection
    // bytes and before the commit: writing the note from inside it stands in
    // for the external writer (Obsidian, an agent) editing the note in the
    // window the ADR's CAS closes.
    let injected = Cell::new(false);
    let result = with_ctx(root, |ctx| {
        writer.upload(
            "OP-UPLOAD-00000000000017",
            "test",
            None,
            UploadRequest {
                folder: "inbox".to_string(),
                collection: Some(RelPath::parse("shots.md").unwrap()),
            },
            vec![UploadItem {
                original_name: "girl.png".to_string(),
                bytes: PNG.to_vec(),
            }],
            ctx,
            |_sha| {
                if !injected.replace(true) {
                    std::fs::write(root.join("shots.md"), &edited).unwrap();
                }
                None
            },
            "dimagine test",
        )
    });
    let error = result.unwrap_err();
    assert!(
        matches!(error, dimagine_write::Error::Conflict(_)),
        "{error}"
    );
    // The external edit survives untouched, and nothing from the upload
    // landed: the whole operation refused.
    assert_eq!(std::fs::read(root.join("shots.md")).unwrap(), edited);
    assert!(!root.join("inbox").exists());
}

/// An external writer standing in for Obsidian: it waits for the operation's
/// journal directory — created after the precondition check and before the
/// first apply — then edits the note once. An edit that lands there is inside
/// the window the check cannot see, so only the guarded step's own
/// compare-and-swap can refuse it (ADR-016 §3).
fn external_editor(
    root: &std::path::Path,
    op_id: &str,
    note: &str,
    bytes: &[u8],
) -> std::thread::JoinHandle<()> {
    let journal = root.join(".dimagine/journal").join(op_id);
    let note = root.join(note);
    let bytes = bytes.to_vec();
    std::thread::spawn(move || {
        while !journal.exists() {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        std::fs::write(&note, &bytes).unwrap();
    })
}

/// The operation reported a typed conflict on `path` — refused whole when the
/// edit was there before the journal, or, when the edit landed after the check
/// and other steps had already reached the disk, as the one step that did not
/// (ADR-016 §7).
fn assert_conflict(error: &dimagine_write::Error, path: &str) {
    match error {
        dimagine_write::Error::Conflict(conflict) => {
            assert!(conflict.path.ends_with(path), "{conflict:?}");
        }
        dimagine_write::Error::BatchPartial { errors, .. } => {
            let named = errors
                .iter()
                .any(|error| error.path.ends_with(path) && error.message.contains("conflict on"));
            assert!(named, "{errors:?}");
        }
        other => panic!("expected a conflict on {path}, got {other:?}"),
    }
}

/// The bytes of one uploaded file: the generated PNG plus a tail byte, so every
/// file of a batch is distinct and each lands instead of deduping.
fn png_variant(tag: u8) -> Vec<u8> {
    let mut bytes = PNG.to_vec();
    bytes.push(tag);
    bytes
}

/// An external edit landing after the precondition check must not be reverted by
/// the collection join's rename, which is the last step of the operation
/// (ADR-016 §3: never rename bytes computed from an old read).
#[test]
fn an_edit_after_the_check_refuses_the_collection_join() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("shots.md"), COLLECTION);
    put(&root.join("refs/girl.png"), PNG);
    put(
        &root.join("refs/girl.png.md"),
        b"---\nadded: 2026-01-02\n---\n\n![[refs/girl.png]]\n",
    );
    let writer = Writer::open_raw(root).unwrap();
    let mut known = HashMap::new();
    known.insert(
        Fingerprint::of_bytes(PNG).hex(),
        "refs/girl.png".to_string(),
    );
    let edited =
        b"---\nkind: collection\ntitle: Shots\n---\n\nFirst shot.\n\nEdited elsewhere.\n".to_vec();
    // A batch long enough that the join's rename is seconds of creates away
    // from the check the harness signals.
    // The duplicate is in the batch too: it is the one file whose own note
    // this operation rewrites, and the creates around it open the window.
    let batch: Vec<(String, Vec<u8>)> = std::iter::once(("girl.png".to_string(), PNG.to_vec()))
        .chain((0..24).map(|tag| (format!("f{tag:02}.png"), png_variant(tag))))
        .collect();

    let op_id = "OP-UPLOAD-0000000000001A";
    let editor = external_editor(root, op_id, "shots.md", &edited);
    let error = upload(
        op_id,
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        batch
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect(),
        Some(&known),
    )
    .unwrap_err();
    editor.join().unwrap();

    assert_conflict(&error, "shots.md");
    // The external edit survives byte for byte: no silent revert of another
    // writer's note by a rename of bytes computed from `prepare`'s read.
    assert_eq!(std::fs::read(root.join("shots.md")).unwrap(), edited);
}

/// The same window on the duplicate's own note: the `id` ADR-017 §3 assigns is
/// written into a note read while the operation was planned, so an edit after
/// the check is a conflict on that note too (ADR-016 §3).
#[test]
fn an_edit_after_the_check_refuses_the_duplicate_id_note() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("shots.md"), COLLECTION);
    put(&root.join("refs/girl.png"), PNG);
    put(
        &root.join("refs/girl.png.md"),
        b"---\nadded: 2026-01-02\n---\n\n![[refs/girl.png]]\n",
    );
    let writer = Writer::open_raw(root).unwrap();
    let mut known = HashMap::new();
    known.insert(
        Fingerprint::of_bytes(PNG).hex(),
        "refs/girl.png".to_string(),
    );
    let edited =
        b"---\nadded: 2026-01-02\n---\n\n![[refs/girl.png]]\n\nCaption typed elsewhere.\n".to_vec();
    // The duplicate is in the batch too: it is the one file whose own note
    // this operation rewrites, and the creates around it open the window.
    let batch: Vec<(String, Vec<u8>)> = std::iter::once(("girl.png".to_string(), PNG.to_vec()))
        .chain((0..24).map(|tag| (format!("f{tag:02}.png"), png_variant(tag))))
        .collect();

    let op_id = "OP-UPLOAD-0000000000001B";
    let editor = external_editor(root, op_id, "refs/girl.png.md", &edited);
    let error = upload(
        op_id,
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        batch
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect(),
        Some(&known),
    )
    .unwrap_err();
    editor.join().unwrap();

    assert_conflict(&error, "refs/girl.png.md");
    // The note keeps the external bytes — no `id` line renamed over them.
    assert_eq!(
        std::fs::read(root.join("refs/girl.png.md")).unwrap(),
        edited
    );
    // The join is its own file's intent (ADR-016 §7): it still lands.
    let joined = std::fs::read_to_string(root.join("shots.md")).unwrap();
    assert_eq!(joined.matches("girl.png").count(), 1, "{joined}");
}

/// A dedupe hit whose note exists without an `id` is updated in place, not
/// replaced wholesale: the note keeps its other keys and its body.
#[test]
fn a_dedupe_hit_with_an_id_less_note_gains_one() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("refs/girl.png"), PNG);
    put(
        &root.join("refs/girl.png.md"),
        b"---\nadded: 2026-01-02\n---\n\n![[refs/girl.png]]\n\nA caption.\n",
    );
    put(&root.join("shots.md"), COLLECTION);
    let writer = Writer::open_raw(root).unwrap();
    let mut known = HashMap::new();
    known.insert(
        Fingerprint::of_bytes(PNG).hex(),
        "refs/girl.png".to_string(),
    );

    upload(
        "OP-UPLOAD-0000000000001C",
        &writer,
        root,
        "inbox",
        Some("shots.md"),
        vec![("girl.png", PNG)],
        Some(&known),
    )
    .unwrap();

    let note = std::fs::read_to_string(root.join("refs/girl.png.md")).unwrap();
    assert!(note.starts_with("---\nadded: 2026-01-02\n"), "{note}");
    assert!(note.contains("\nid: "), "{note}");
    assert!(note.contains("A caption."), "{note}");
}

/// An upload whose join turns out to change nothing carries no precondition for
/// the collection: a concurrent edit to a note this upload never intends to
/// write must not refuse it.
#[test]
fn a_no_op_join_does_not_conflict_on_the_collection() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    put(&root.join("refs/girl.png"), PNG);
    put(
        &root.join("refs/girl.png.md"),
        b"---\nid: 01B\n---\n\n![[refs/girl.png]]\n",
    );
    // The duplicate is already a member, so the join changes nothing.
    put(
        &root.join("shots.md"),
        b"---\nkind: collection\ntitle: Shots\n---\n\n![[refs/girl.png]]\n",
    );
    let writer = Writer::open_raw(root).unwrap();
    let mut known = HashMap::new();
    known.insert(
        Fingerprint::of_bytes(PNG).hex(),
        "refs/girl.png".to_string(),
    );
    let edited =
        b"---\nkind: collection\ntitle: Shots\n---\n\n![[refs/girl.png]]\n\nEdited elsewhere.\n"
            .to_vec();

    // The dedupe lookup runs in `plan`, before the precondition check: an
    // edit there is outside every window this operation writes in.
    let injected = Cell::new(false);
    let outcomes = with_ctx(root, |ctx| {
        writer.upload(
            "OP-UPLOAD-0000000000001D",
            "test",
            None,
            UploadRequest {
                folder: "inbox".to_string(),
                collection: Some(RelPath::parse("shots.md").unwrap()),
            },
            vec![UploadItem {
                original_name: "girl.png".to_string(),
                bytes: PNG.to_vec(),
            }],
            ctx,
            |sha| {
                if !injected.replace(true) {
                    std::fs::write(root.join("shots.md"), &edited).unwrap();
                }
                known.get(sha).cloned()
            },
            "dimagine test",
        )
    })
    .unwrap();

    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].duplicate);
    assert_eq!(
        std::fs::read(root.join("shots.md")).unwrap(),
        edited,
        "the upload never wrote the collection, so the edit must stand"
    );
}
