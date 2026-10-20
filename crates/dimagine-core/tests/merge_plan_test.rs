//! Planner tests for a near-duplicate merge (W77 方案 2). The planner is pure,
//! so the front-matter union and the link repoint are pinned here on temp
//! libraries against the plan's own bytes; landing them and moving the drop's
//! group to `.trash/` is the write core's job, covered by the CLI tests.

use std::path::Path;

use dimagine_core::library::{FileEntry, Library};
use dimagine_core::links::Resolver;
use dimagine_core::merge_plan::{self, MergeFacts, MergePlan, MergeRefusal};

const SHA: &str = "aa11bb22cc33dd44aa11bb22cc33dd44aa11bb22cc33dd44aa11bb22cc33dd44";
const WHEN: &str = "2026-10-20T12:00:00+00:00";

fn write_file(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A fresh, well-formed ULID for the note id the caller mints — unique per
/// call, so a test can tell a real id from a constant.
fn note_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    const DIGITS: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut suffix = String::with_capacity(16);
    for shift in (0..16).rev() {
        suffix.push(DIGITS[((value >> (shift * 4)) & 15) as usize] as char);
    }
    format!("01JW77B5Q7{suffix}")
}

/// The id written in a note's front matter, `None` when it carries none.
fn id_of(note: &str) -> Option<String> {
    note.lines()
        .find(|line| line.starts_with("id: "))
        .map(|line| line["id: ".len()..].trim().to_string())
}

/// True for a canonical 26-character Crockford base32 ULID.
fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value
            .chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c))
}

/// Drive `plan_merge` over a temp library: the two named images' notes are read
/// from disk (empty when absent), every other note/canvas is read through the
/// same reader the CLI passes.
fn merge(dir: &Path, keep: &str, drop: &str) -> Result<MergePlan, MergeRefusal> {
    let library = Library::open(dir).unwrap();
    let resolver = Resolver::new(&library.files);
    let root = library.root.clone();
    let read_root = root.clone();
    let read = move |entry: &FileEntry| match std::fs::read(read_root.join(&entry.path)) {
        Ok(bytes) => Ok(bytes),
        Err(err) => Err(format!("cannot read: {err}")),
    };
    let note_bytes = |rel: &str| -> Vec<u8> { std::fs::read(root.join(rel)).unwrap_or_default() };
    let keep_note_rel = format!("{keep}.md");
    let drop_note_rel = format!("{drop}.md");
    let keep_note_bytes = note_bytes(&keep_note_rel);
    let keep_note_existed = std::fs::symlink_metadata(root.join(&keep_note_rel)).is_ok();
    let drop_note_bytes = note_bytes(&drop_note_rel);
    let new_note_id = note_id();
    let facts = MergeFacts {
        keep_image_rel: keep,
        drop_image_rel: drop,
        keep_note_rel: &keep_note_rel,
        drop_sha256: SHA,
        merged_at: WHEN,
        keep_embed_text: &format!("![[{keep}]]"),
        new_note_id: &new_note_id,
    };
    merge_plan::plan_merge(
        &library.files,
        &resolver,
        &facts,
        &keep_note_bytes,
        keep_note_existed,
        &drop_note_bytes,
        &read,
    )
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

/// Count the `sources:` entries in a built note by their `- type:` item lines.
fn sources_len(note_bytes: &[u8]) -> usize {
    utf8(note_bytes)
        .lines()
        .filter(|line| line.starts_with("  - type:"))
        .count()
}

#[test]
fn merge_unions_tags_keeps_earliest_added_and_appends_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "big.jpg", b"big");
    write_file(
        root,
        "big.jpg.md",
        b"---\ntags: [photo]\nsource: https://k\nadded: 2024-05-01T00:00:00+00:00\n---\n\n![[big.jpg]]\n",
    );
    write_file(root, "small.jpg", b"small");
    write_file(
        root,
        "small.jpg.md",
        b"---\ntags: [tg, photo]\nsource: https://d\nadded: 2023-01-02T00:00:00+00:00\nsources:\n  - type: eagle\n    item: L8X2\n---\n\n![[small.jpg]]\n",
    );

    let plan = merge(root, "big.jpg", "small.jpg").expect("merge plans");
    // keep tags unioned (photo kept, tg added), added = the earlier (2023), one
    // `merged` entry (recording the drop's path, sha256 and source) plus the
    // drop's own eagle entry appended; keep's own `source` is untouched.
    let expected = "---\ntags: [photo, tg]\nsource: https://k\nadded: 2023-01-02T00:00:00+00:00\nsources:\n  - type: merged\n    merged_at: 2026-10-20T12:00:00+00:00\n    path: small.jpg\n    sha256: aa11bb22cc33dd44aa11bb22cc33dd44aa11bb22cc33dd44aa11bb22cc33dd44\n    source: https://d\n  - type: eagle\n    item: L8X2\n---\n\n![[big.jpg]]\n";
    assert_eq!(utf8(&plan.keep_note_bytes), expected);
    assert_eq!(plan.tags_added, vec!["tg".to_string()]);
    assert_eq!(plan.added_set.as_deref(), Some("2023-01-02T00:00:00+00:00"));
    assert_eq!(plan.sources_appended, 2); // merged + drop's one entry
                                          // keep had no sources: 1 merged + 1 drop = 2 entries in the built bytes.
    assert_eq!(sources_len(&plan.keep_note_bytes), 2);
    assert_eq!(plan.links_repointed, 0);
}

#[test]
fn keep_own_source_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.png", b"k");
    write_file(
        root,
        "k.png.md",
        b"---\nsource: KEEP-URL\n---\n\n![[k.png]]\n",
    );
    write_file(root, "d.png", b"d");
    write_file(
        root,
        "d.png.md",
        b"---\nsource: DROP-URL\n---\n\n![[d.png]]\n",
    );
    let plan = merge(root, "k.png", "d.png").expect("merge");
    let text = utf8(&plan.keep_note_bytes);
    // Exactly one top-level `source:` line, and it is the keep's own.
    let top_sources: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("source:"))
        .collect();
    assert_eq!(top_sources, vec!["source: KEEP-URL"], "{text}");
    // The drop's source survives, recorded inside the merged entry (indented).
    assert!(text.contains("\n    source: DROP-URL\n"), "{text}");
}

#[test]
fn added_takes_the_earlier_and_survives_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(
        root,
        "k.jpg.md",
        b"---\nadded: 2020-01-01T00:00:00+00:00\n---\n",
    );
    write_file(root, "d.jpg", b"d");
    write_file(
        root,
        "d.jpg.md",
        b"---\nadded: 2019-06-06T00:00:00+00:00\n---\n",
    );
    let plan = merge(root, "k.jpg", "d.jpg").expect("earlier wins");
    assert_eq!(plan.added_set.as_deref(), Some("2019-06-06T00:00:00+00:00"));
    assert!(utf8(&plan.keep_note_bytes).contains("added: 2019-06-06"));

    // The keep's own `added` stays when it is already the earliest.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(
        root,
        "k.jpg.md",
        b"---\nadded: 2018-01-01T00:00:00+00:00\n---\n",
    );
    write_file(root, "d.jpg", b"d");
    write_file(
        root,
        "d.jpg.md",
        b"---\nadded: 2019-06-06T00:00:00+00:00\n---\n",
    );
    let plan = merge(root, "k.jpg", "d.jpg").expect("keep stays earliest");
    assert_eq!(plan.added_set, None);
    assert!(utf8(&plan.keep_note_bytes).contains("added: 2018-01-01"));

    // A keep with no `added` takes the drop's.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(root, "k.jpg.md", b"---\ntitle: K\n---\n");
    write_file(root, "d.jpg", b"d");
    write_file(
        root,
        "d.jpg.md",
        b"---\nadded: 2019-06-06T00:00:00+00:00\n---\n",
    );
    let plan = merge(root, "k.jpg", "d.jpg").expect("take drop's added");
    assert_eq!(plan.added_set.as_deref(), Some("2019-06-06T00:00:00+00:00"));
    assert!(utf8(&plan.keep_note_bytes).contains("added: 2019-06-06"));
}

#[test]
fn merge_repoints_members_embeds_and_canvases_to_keep() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "keep.jpg", b"k");
    write_file(root, "keep.jpg.md", b"---\n---\n");
    write_file(root, "drop.jpg", b"d");
    write_file(root, "drop.jpg.md", b"---\n---\n");
    // A collection with a member line, a note with a markdown image and a
    // wikilink, and a canvas file node — all pointing at the drop image.
    write_file(
        root,
        "shelf.md",
        b"---\nkind: collection\n---\n\n![[drop.jpg]]\nplain text\n",
    );
    write_file(
        root,
        "essay.md",
        b"---\n---\n\nsee ![alt](drop.jpg) and [[drop.jpg]]\n",
    );
    write_file(
        root,
        "board.canvas",
        b"{\"nodes\":[{\"type\":\"file\",\"file\":\"drop.jpg\"}]}",
    );

    let plan = merge(root, "keep.jpg", "drop.jpg").expect("merge");
    let edited: Vec<&str> = plan.edits.iter().map(|e| e.path.as_str()).collect();
    assert!(edited.contains(&"shelf.md"), "{edited:?}");
    assert!(edited.contains(&"essay.md"), "{edited:?}");
    assert!(edited.contains(&"board.canvas"), "{edited:?}");
    // The dropped image's own note is never rewritten (it moves to trash whole).
    assert!(!edited.contains(&"drop.jpg.md"), "{edited:?}");

    for edit in &plan.edits {
        let text = utf8(&edit.bytes);
        assert!(
            !text.contains("drop.jpg"),
            "{} still names drop: {text}",
            edit.path
        );
        assert!(text.contains("keep.jpg"), "{} lost keep: {text}", edit.path);
    }
    let shelf = plan.edits.iter().find(|e| e.path == "shelf.md").unwrap();
    assert!(utf8(&shelf.bytes).contains("![[keep.jpg]]"));
    assert_eq!(plan.links_repointed, 4); // shelf 1, essay 2, canvas 1
}

#[test]
fn a_link_already_ambiguous_before_the_merge_is_reported_not_touched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/keep.jpg", b"k");
    write_file(root, "a/keep.jpg.md", b"---\n---\n");
    write_file(root, "b/drop.jpg", b"d");
    write_file(root, "b/drop.jpg.md", b"---\n---\n");
    // `[[drop.jpg]]` names one file; this one is ambiguous between two notes.
    write_file(root, "x.md", b"---\n---\n\n[[note]]\n");
    write_file(root, "n1/note.md", b"---\n---\n");
    write_file(root, "n2/note.md", b"---\n---\n");
    let plan = merge(root, "a/keep.jpg", "b/drop.jpg").expect("merge");
    assert!(plan.edits.iter().all(|e| e.path != "x.md"));
}

#[test]
fn a_flow_sources_value_refuses_rather_than_guesses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(root, "k.jpg.md", b"---\nsources: []\n---\n");
    write_file(root, "d.jpg", b"d");
    write_file(root, "d.jpg.md", b"---\n---\n");
    let err = merge(root, "k.jpg", "d.jpg").unwrap_err();
    assert_eq!(
        err,
        MergeRefusal::SourcesNotMergeable {
            note: "k.jpg.md".to_string()
        }
    );
}

#[test]
fn a_kept_image_without_a_note_gains_one_with_the_merged_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k"); // no k.jpg.md
    write_file(root, "d.jpg", b"d");
    write_file(root, "d.jpg.md", b"---\ntags: [tg]\n---\n\n![[d.jpg]]\n");
    let plan = merge(root, "k.jpg", "d.jpg").expect("create kept note");
    assert!(!plan.keep_note_existed);
    let text = utf8(&plan.keep_note_bytes);
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("tags: [tg]"), "{text}");
    assert!(text.contains("- type: merged"), "{text}");
    assert!(text.contains("![[k.jpg]]"), "{text}");
    // The id is the caller's freshly minted ULID (§3.3), not a constant: two
    // merges of a noteless keep cannot hand out the same id.
    let second = {
        let dir2 = tempfile::tempdir().unwrap();
        write_file(dir2.path(), "k.jpg", b"k");
        write_file(dir2.path(), "d.jpg", b"d");
        write_file(
            dir2.path(),
            "d.jpg.md",
            b"---\ntags: [tg]\n---\n\n![[d.jpg]]\n",
        );
        let plan2 = merge(dir2.path(), "k.jpg", "d.jpg").expect("second create");
        id_of(&utf8(&plan2.keep_note_bytes)).expect("second note carries an id")
    };
    let id = id_of(&text).expect("the created note carries an id");
    assert!(is_ulid(&id), "{id} is not a canonical ULID: {text}");
    assert_ne!(id, "01J00000000000000000000000", "a placeholder id");
    assert_ne!(id, second, "two created notes share an id");
}

/// The kept note's existence — not its contents — decides modify versus create:
/// a note that is present but empty is still a note the merge edits in place.
#[test]
fn a_present_but_empty_kept_note_is_modified_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(root, "k.jpg.md", b""); // present, zero bytes
    write_file(root, "d.jpg", b"d");
    write_file(root, "d.jpg.md", b"---\ntags: [tg]\n---\n\n![[d.jpg]]\n");
    let plan = merge(root, "k.jpg", "d.jpg").expect("merge into the empty note");
    assert!(plan.keep_note_existed, "the note file is on disk");
    let text = utf8(&plan.keep_note_bytes);
    assert!(text.contains("tags: [tg]"), "{text}");
    assert!(text.contains("- type: merged"), "{text}");
    assert!(
        id_of(&text).is_none(),
        "an existing note gains no id: {text}"
    );
}

#[test]
fn merging_an_image_with_itself_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "k.jpg", b"k");
    write_file(root, "k.jpg.md", b"---\n---\n");
    let err = merge(root, "k.jpg", "k.jpg").unwrap_err();
    assert_eq!(
        err,
        MergeRefusal::SameImage {
            path: "k.jpg".to_string()
        }
    );
}
