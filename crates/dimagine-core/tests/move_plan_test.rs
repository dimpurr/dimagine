//! Planner tests for move/rename + link repair (ADR-017 §8). The planner is
//! pure, so most of the semantics are pinned here on temp-dir libraries with
//! the plan's own data; landing those bytes on disk is the write core's job
//! and is covered by the CLI integration tests.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dimagine_core::library::{FileClass, FileEntry, Library};
use dimagine_core::links::Resolver;
use dimagine_core::move_plan::{self, MovePlan, MoveRefusal};

const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ULID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

fn write_file(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn plan(dir: &Path, src: &str, dst: &str) -> Result<MovePlan, MoveRefusal> {
    plan_with_ulid(dir, src, dst, ULID)
}

fn plan_with_ulid(dir: &Path, src: &str, dst: &str, ulid: &str) -> Result<MovePlan, MoveRefusal> {
    let library = Library::open(dir).unwrap();
    let resolver = Resolver::new(&library.files);
    let root = library.root.clone();
    let read_root = root.clone();
    let disk_root = root.clone();
    let read = move |entry: &FileEntry| match std::fs::read(read_root.join(&entry.path)) {
        Ok(bytes) => Ok(bytes),
        Err(err) => Err(format!("cannot read: {err}")),
    };
    // The same two probes the CLI and serve surfaces pass: `occupied` sees
    // anything on the path, `same_file` separates the file being moved from
    // a real occupant under a case-only spelling.
    let same_file = move_plan::same_file_probe(root);
    let occupied = move |rel: &str| disk_root.join(rel).symlink_metadata().is_ok();
    move_plan::plan_move(
        &library.files,
        &resolver,
        src,
        dst,
        ulid,
        &read,
        &occupied,
        &same_file,
    )
}

#[test]
fn a_move_rewrites_every_link_shape_byte_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(root, "sp ace/y.jpg", b"img");
    let body = "---\nkind: collection\n---\n\n\
![[a/x.jpg]]\n\
![[a/x.jpg|Alias]]\n\
![[a/x.jpg#head]]\n\
![[a/x.jpg#^blk]]\n\
![alt](a/x.jpg)\n\
![alt2](a/x.jpg \"title\")\n\
![alt3](<a/x.jpg>)\n\
![y](sp%20ace/y.jpg)\n\
```\n![[a/x.jpg]]\n```\n\
inline `![[a/x.jpg]]` stays\n";
    write_file(root, "shelf.md", body.as_bytes());

    let moved = plan(root, "a/x.jpg", "refs/x2.jpg").unwrap();
    assert_eq!(moved.renames.len(), 1);
    assert_eq!(moved.edits.len(), 1);
    let edit = &moved.edits[0];
    assert_eq!(edit.path, "shelf.md");
    let text = String::from_utf8(edit.bytes.clone()).unwrap();
    assert_eq!(
        text,
        "---\nkind: collection\n---\n\n\
![[refs/x2.jpg]]\n\
![[refs/x2.jpg|Alias]]\n\
![[refs/x2.jpg#head]]\n\
![[refs/x2.jpg#^blk]]\n\
![alt](refs/x2.jpg)\n\
![alt2](refs/x2.jpg \"title\")\n\
![alt3](refs/x2.jpg)\n\
![y](sp%20ace/y.jpg)\n\
```\n![[a/x.jpg]]\n```\n\
inline `![[a/x.jpg]]` stays\n"
    );
    assert_eq!(edit.replaced.len(), 7);
    assert!(edit.replaced.iter().all(|span| span.line >= 5));
    let first = &edit.replaced[0];
    assert_eq!(first.old_text, "![[a/x.jpg]]");
    assert_eq!(first.new_text, "![[refs/x2.jpg]]");
    assert_eq!(
        &edit.bytes[first.span.start..first.span.start + "![[refs/x2.jpg]]".len()],
        b"![[refs/x2.jpg]]"
    );
}

/// A Markdown destination that itself holds a `%` is
/// percent-encoded: the reader decodes percent sequences, so a
/// raw `%XX` in the written link would resolve to different
/// bytes (RW65b L3).
#[test]
fn a_markdown_destination_with_a_percent_is_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x%20b.jpg", b"img");
    // The link text encodes the percent sign, so that it
    // resolves to the file whose name really holds a '%'.
    let body = "---\n---\n\n![alt](a/x%2520b.jpg)\n";
    write_file(root, "shelf.md", body.as_bytes());
    let moved = plan(root, "a/x%20b.jpg", "refs/y%20z.jpg").unwrap();
    let edit = &moved.edits[0];
    assert_eq!(edit.replaced.len(), 1);
    assert_eq!(edit.replaced[0].new_text, "![alt](refs/y%2520z.jpg)");
    let text = String::from_utf8(edit.bytes.clone()).unwrap();
    assert!(text.contains("![alt](refs/y%2520z.jpg)"), "{text}");
}

#[test]
fn a_move_onto_a_taken_path_refuses_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    write_file(root, "a/x.jpg.eagle.json", b"{}");
    write_file(root, "refs/x2.jpg", b"other");
    let err = plan(root, "a/x.jpg", "refs/x2.jpg").unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::TargetExists { path } if path == "refs/x2.jpg"),
        "{err}"
    );
}

/// A destination occupied by a directory is invisible to the walk:
/// the planner must still refuse the whole move with nothing moved,
/// not plan a torn partial move (RW65b M1).
#[test]
fn a_destination_occupied_by_a_directory_refuses_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    write_file(
        root,
        "refs/y.jpg/keep.txt",
        b"a directory named like the destination",
    );
    let err = plan(root, "a/x.jpg", "refs/y.jpg").unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::TargetExists { path } if path == "refs/y.jpg"),
        "{err}"
    );
    // Nothing moved: the group is untouched.
    assert!(root.join("a/x.jpg").exists());
    assert!(root.join("a/x.jpg.md").exists());
}

/// A destination that differs from an existing file only by case or
/// Unicode form is taken under the library's fold-key identity: the
/// plan refuses with TargetExists instead of failing later with a
/// misleading verification error (RW65b M1).
#[test]
fn a_fold_equal_destination_name_refuses_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    write_file(root, "refs/x2.jpg", b"other");
    let err = plan(root, "a/x.jpg", "refs/X2.JPG").unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::TargetExists { path } if path == "refs/X2.JPG"),
        "{err}"
    );
    // Nothing moved: the group and the existing file are untouched.
    assert!(root.join("a/x.jpg").exists());
    assert!(root.join("a/x.jpg.md").exists());
    assert!(root.join("refs/x2.jpg").exists());
}

/// A rename step may displace only its own source: on a case-insensitive
/// volume the destination of a case-only self-rename (`a/x.jpg` ->
/// `a/X.JPG`, the routine way to fix display case) is occupied by the
/// file being moved, which counts as free, not as a taken destination
/// (RW65b D1).
#[test]
fn a_case_only_self_rename_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    let moved = plan(root, "a/x.jpg", "a/X.JPG").unwrap();
    let pairs: Vec<(&str, &str)> = moved
        .renames
        .iter()
        .map(|step| (step.src.as_str(), step.dst.as_str()))
        .collect();
    assert_eq!(
        pairs,
        vec![("a/x.jpg", "a/X.JPG"), ("a/x.jpg.md", "a/X.JPG.md")]
    );
}

/// The other half of the same rule, and the reason the planner needs both
/// facts: a fold-equal destination that is NOT the file being moved is still
/// taken. On a case-sensitive volume a directory can hold `a/X.JPG` while
/// `a/x.jpg` is a separate file — the walk never reports directories, so the
/// fold key alone waved it through and the group landed partially applied
/// (the note renamed, the image not; measured on a case-sensitive volume,
/// W65c D1). The two callers' answers are stated directly here because this
/// disk cannot hold both spellings.
#[test]
fn a_fold_equal_destination_held_by_something_else_refuses_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    let library = Library::open(root).unwrap();
    let resolver = Resolver::new(&library.files);
    let held = |rel: &str| rel == "a/X.JPG";
    let not_the_source = |_src: &str, _dst: &str| false;
    let read = |entry: &FileEntry| std::fs::read(root.join(&entry.path)).map_err(|e| e.to_string());
    let err = move_plan::plan_move(
        &library.files,
        &resolver,
        "a/x.jpg",
        "a/X.JPG",
        ULID,
        &read,
        &held,
        &not_the_source,
    )
    .unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::TargetExists { path } if path == "a/X.JPG"),
        "{err}"
    );
    // Nothing moved: the group is untouched.
    assert!(root.join("a/x.jpg").exists());
    assert!(root.join("a/x.jpg.md").exists());
}

#[test]
fn the_note_and_sidecars_rename_with_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    write_file(root, "a/x.jpg.eagle.json", b"{}");
    write_file(root, "a/x.jpg.xmp", b"<xmp/>");
    write_file(root, "a/other.jpg", b"img");
    write_file(root, "a/other.jpg.xmp", b"<not part of the group/>");
    let moved = plan(root, "a/x.jpg", "refs/deep/x2.jpg").unwrap();
    let pairs: Vec<(&str, &str)> = moved
        .renames
        .iter()
        .map(|step| (step.src.as_str(), step.dst.as_str()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("a/x.jpg", "refs/deep/x2.jpg"),
            ("a/x.jpg.md", "refs/deep/x2.jpg.md"),
            ("a/x.jpg.eagle.json", "refs/deep/x2.jpg.eagle.json"),
            ("a/x.jpg.xmp", "refs/deep/x2.jpg.xmp"),
        ]
    );
    // The note's self-embed resolves to the moved image, so its bytes are
    // edited at the new path; the id is already there and stays.
    let edit = moved.moved_note.as_ref().unwrap();
    assert_eq!(edit.path, "refs/deep/x2.jpg.md");
    assert_eq!(
        String::from_utf8(edit.bytes.clone()).unwrap(),
        "---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[refs/deep/x2.jpg]]\n"
    );
    assert!(!moved.id_assigned);
    assert!(moved.new_note.is_none());
}

#[test]
fn the_moved_note_gets_its_id_and_its_links_edited() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\ntitle: X\n---\n\n![[a/x.jpg]]\n\nand a link to ![[keep.jpg]]\n",
    );
    write_file(root, "keep.jpg", b"img");
    let moved = plan(root, "a/x.jpg", "b/x2.jpg").unwrap();
    let edit = moved.moved_note.as_ref().unwrap();
    assert_eq!(edit.path, "b/x2.jpg.md");
    assert!(moved.id_assigned);
    assert_eq!(
        String::from_utf8(edit.bytes.clone()).unwrap(),
        format!("---\ntitle: X\nid: {ULID}\n---\n\n![[b/x2.jpg]]\n\nand a link to ![[keep.jpg]]\n")
    );
    assert_eq!(edit.replaced.len(), 1);
    assert!(!edit.self_embed_appended);
}

#[test]
fn the_moved_note_keeps_its_id_and_gains_a_missing_self_embed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        format!("---\nid: {OTHER_ULID}\n---\n\nplain words, no embed\n").as_bytes(),
    );
    let moved = plan(root, "a/x.jpg", "b/x2.jpg").unwrap();
    let edit = moved.moved_note.as_ref().unwrap();
    assert!(!moved.id_assigned);
    assert!(edit.self_embed_appended);
    assert_eq!(
        String::from_utf8(edit.bytes.clone()).unwrap(),
        format!("---\nid: {OTHER_ULID}\n---\n\nplain words, no embed\n![[b/x2.jpg]]\n")
    );
}

#[test]
fn an_image_without_a_note_gains_one_at_the_new_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    let moved = plan(root, "a/x.jpg", "refs/x2.jpg").unwrap();
    assert!(moved.moved_note.is_none());
    assert!(moved.id_assigned);
    let note = moved.new_note.as_ref().unwrap();
    assert_eq!(note.path, "refs/x2.jpg.md");
    assert_eq!(
        String::from_utf8(note.bytes.clone()).unwrap(),
        format!("---\nid: {ULID}\n---\n\n![[refs/x2.jpg]]\n")
    );
    assert_eq!(moved.renames.len(), 1);
}

#[test]
fn a_root_note_embeds_its_bare_name_which_is_the_full_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "x.jpg", b"img");
    let moved = plan(root, "x.jpg", "y.jpg").unwrap();
    assert_eq!(
        String::from_utf8(moved.new_note.unwrap().bytes).unwrap(),
        format!("---\nid: {ULID}\n---\n\n![[y.jpg]]\n")
    );
}

#[test]
fn a_root_destination_with_a_shared_bare_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(root, "sub/x.jpg", b"img");
    let err = plan(root, "a/x.jpg", "x.jpg").unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::AmbiguousBareName { name, .. } if name == "x.jpg"),
        "{err}"
    );
}

#[test]
fn a_link_ambiguous_before_the_move_is_reported_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "x.jpg", b"img");
    write_file(root, "a/x.jpg", b"img");
    let body = "---\n---\n\n![[x.jpg]]\n\n![[x.jpg]] and ![[a/x.jpg]] on one line\n";
    write_file(root, "n.md", body.as_bytes());
    let moved = plan(root, "x.jpg", "b/y.jpg").unwrap();
    assert!(
        moved.edits.is_empty(),
        "ambiguous links are never rewritten"
    );
    assert_eq!(moved.ambiguous.len(), 2);
    assert_eq!(moved.ambiguous[0].target, "x.jpg");
    assert_eq!(
        moved.ambiguous[0].matches,
        vec!["a/x.jpg".to_string(), "x.jpg".to_string()]
    );
    assert_eq!(moved.ambiguous[0].line, 4);
    assert_eq!(moved.ambiguous[1].line, 6);
    // The one line also holds an unambiguous link to a/x.jpg, which stays:
    // that file is not moving.
    let bytes = std::fs::read(root.join("n.md")).unwrap();
    assert_eq!(bytes, body.as_bytes());
}

#[test]
fn a_non_ambiguous_link_to_the_moved_image_gets_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "x.jpg", b"img");
    write_file(root, "a/x.jpg", b"img");
    let body = "![[x.jpg]] then ![[a/x.jpg]]\n";
    write_file(root, "n.md", body.as_bytes());
    let moved = plan(root, "a/x.jpg", "b/y.jpg").unwrap();
    // The path link resolves uniquely to the moving file and is rewritten;
    // the bare name is ambiguous (two files are named x.jpg) and is not.
    let edit = moved.edits.iter().find(|e| e.path == "n.md").unwrap();
    assert_eq!(edit.replaced.len(), 1);
    assert_eq!(edit.replaced[0].old_text, "![[a/x.jpg]]");
    assert_eq!(moved.ambiguous.len(), 1);
}

#[test]
fn canvas_file_nodes_are_replaced_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    let canvas = "{\"nodes\":[{\"id\":\"1\",\"type\":\"file\",\"file\":\"a/x.jpg\",\"name\":\"a/x.jpg is my title\"},{\"id\":\"2\",\"type\":\"text\",\"text\":\"mentions a/x.jpg\"}],\"edges\":[]}";
    write_file(root, "board.canvas", canvas.as_bytes());
    let moved = plan(root, "a/x.jpg", "refs/x2.jpg").unwrap();
    let edit = moved
        .edits
        .iter()
        .find(|e| e.path == "board.canvas")
        .unwrap();
    // The rewrite is a JSON round-trip: only the file node's value changed.
    let after: serde_json::Value = serde_json::from_slice(&edit.bytes).unwrap();
    assert_eq!(after["nodes"][0]["file"], "refs/x2.jpg");
    assert_eq!(after["nodes"][0]["name"], "a/x.jpg is my title");
    assert_eq!(after["nodes"][1]["text"], "mentions a/x.jpg");
    let text = String::from_utf8(edit.bytes.clone()).unwrap();
    assert!(
        !text.contains('\n'),
        "a compact canvas stays compact: {text}"
    );
    assert!(edit.replaced.is_empty());
}

#[test]
fn a_pretty_canvas_keeps_a_pretty_shape_and_its_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "board.canvas",
        b"{\n  \"nodes\": [\n    {\"id\":\"1\",\"type\":\"file\",\"file\":\"a/x.jpg\"}\n  ]\n}\n",
    );
    let moved = plan(root, "a/x.jpg", "refs/x2.jpg").unwrap();
    let edit = moved
        .edits
        .iter()
        .find(|e| e.path == "board.canvas")
        .unwrap();
    let text = String::from_utf8(edit.bytes.clone()).unwrap();
    assert!(text.contains("\"file\": \"refs/x2.jpg\""), "{text}");
    assert!(text.ends_with("}\n"));
}

/// A canvas whose only reference to the group is ambiguous is
/// reported like a note's ambiguous link, and stays untouched:
/// nothing in it resolves into the group, so there is nothing
/// to rewrite (RW65b M2).
///
/// The two fold-equal paths cannot both exist on a
/// case-insensitive disk, so the walk is built by hand: the
/// planner is pure and reads only note and canvas bytes.
#[test]
fn an_ambiguous_only_canvas_is_reported_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let canvas = "{\"nodes\":[{\"id\":\"1\",\"type\":\"file\",\"file\":\"a/x.jpg\"}],\"edges\":[]}";
    write_file(root, "board.canvas", canvas.as_bytes());

    let entry = |rel: &str, class: FileClass| FileEntry {
        rel: rel.to_string(),
        name: rel.rsplit('/').next().unwrap().to_string(),
        path: PathBuf::from(rel),
        native_name: OsString::from(rel.rsplit('/').next().unwrap()),
        class,
    };
    let files = vec![
        entry("a/x.jpg", FileClass::Image),
        entry("A/x.jpg", FileClass::Image),
        entry("board.canvas", FileClass::Canvas),
    ];
    let resolver = Resolver::new(&files);
    let disk_root = root.to_path_buf();
    let read = move |e: &FileEntry| match std::fs::read(disk_root.join(&e.path)) {
        Ok(bytes) => Ok(bytes),
        Err(err) => Err(format!("cannot read: {err}")),
    };
    let moved = move_plan::plan_move(
        &files,
        &resolver,
        "a/x.jpg",
        "b/y.jpg",
        ULID,
        &read,
        &|_| false,
        &|_, _| false,
    )
    .unwrap();
    assert!(
        moved.edits.is_empty(),
        "an ambiguous-only canvas is never rewritten"
    );
    assert_eq!(moved.ambiguous.len(), 1);
    assert_eq!(moved.ambiguous[0].note, "board.canvas");
    assert_eq!(moved.ambiguous[0].target, "a/x.jpg");
    assert_eq!(
        moved.ambiguous[0].matches,
        vec!["A/x.jpg".to_string(), "a/x.jpg".to_string()]
    );
    // Untouched: the canvas bytes are exactly as they were.
    let bytes = std::fs::read(root.join("board.canvas")).unwrap();
    assert_eq!(bytes, canvas.as_bytes());
}

#[test]
fn a_moved_note_refuses_when_its_only_self_embed_sits_in_code() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n```\n![[a/x.jpg]]\n",
    );
    let err = plan(root, "a/x.jpg", "b/x2.jpg").unwrap_err();
    assert!(
        matches!(&err, MoveRefusal::UnclosedFence { note } if note == "a/x.jpg.md"),
        "{err}"
    );
}

#[test]
fn unreadable_notes_are_reported_never_claimed_clean() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "a/x.jpg", b"img");
    write_file(
        root,
        "a/x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\nok\n",
    );
    write_file(
        root,
        "broken.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[a/x.jpg]]\n",
    );
    let library = Library::open(root).unwrap();
    let resolver = Resolver::new(&library.files);
    let disk_root = root.to_path_buf();
    let broken = PathBuf::from("broken.md");
    let occupied = move |rel: &str| disk_root.join(rel).symlink_metadata().is_ok();
    let same_file = move_plan::same_file_probe(root.to_path_buf());
    let moved = move_plan::plan_move(
        &library.files,
        &resolver,
        "a/x.jpg",
        "b/x2.jpg",
        ULID,
        &|entry: &FileEntry| {
            if entry.path == broken {
                return Err("too large for the read limit".to_string());
            }
            std::fs::read(root.join(&entry.path)).map_err(|e| e.to_string())
        },
        &occupied,
        &same_file,
    )
    .unwrap();
    assert_eq!(moved.warnings.len(), 1);
    assert_eq!(moved.warnings[0].path, "broken.md");
    assert!(moved.warnings[0].message.contains("too large"));
    // Nothing else changed: the plan still renames the image.
    assert_eq!(moved.renames.len(), 2);
}

#[test]
fn destinations_outside_the_library_shape_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "x.jpg", b"img");
    write_file(root, "y.txt", b"not an image");
    for (dst, want) in [
        (".trash/x.jpg", false),
        ("/abs/x.jpg", false),
        ("../out.jpg", false),
        ("y.txt", true),
    ] {
        let err = plan(root, "x.jpg", dst).unwrap_err();
        let typed = match &err {
            MoveRefusal::BadDestination { .. } | MoveRefusal::TargetExists { .. } => !want,
            MoveRefusal::NoImageExtension { .. } => want,
            other => panic!("unexpected refusal for {dst}: {other}"),
        };
        assert!(typed, "wrong refusal for {dst}: {err}");
    }
}

#[test]
fn a_self_reference_written_bare_rewrites_and_stays_resolvable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "x.jpg", b"img");
    write_file(
        root,
        "x.jpg.md",
        b"---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[x.jpg|my pic]]\n",
    );
    let moved = plan(root, "x.jpg", "shelf/photo.jpg").unwrap();
    let edit = moved.moved_note.as_ref().unwrap();
    assert_eq!(
        String::from_utf8(edit.bytes.clone()).unwrap(),
        "---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n---\n\n![[shelf/photo.jpg|my pic]]\n"
    );
    // Verification would have refused had the rewritten text not resolved:
    assert!(!edit.self_embed_appended);
}
