//! End-to-end tests of the write commands (ADR-016, ADR-017): a real process
//! over a library in a temporary folder, checking what lands on disk, the exit
//! code, the JSON document, and that an operation can be undone.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_dimagine");
const PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-write-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create CLI write test dir");
    TempDir(dir)
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_default()
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

/// Run one command over a library, with `--library` appended.
fn dimagine(library: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(BIN)
        .args(args)
        .arg("--library")
        .arg(library)
        .output()
        .expect("run dimagine");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Run one command and insist it succeeded, returning its stdout.
fn ok(library: &Path, args: &[&str]) -> String {
    let (code, stdout, stderr) = dimagine(library, args);
    assert_eq!(
        code,
        0,
        "`{}` failed: exit {code}\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" ")
    );
    stdout
}

fn json_in(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("one JSON document: {error}\n{stdout}"))
}

/// The codes of a `check --json` run, with the exit code.
fn check_codes(library: &Path) -> (i32, Vec<String>) {
    let (code, stdout, _) = dimagine(library, &["check", "--json"]);
    let doc = json_in(&stdout);
    let codes = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["code"].as_str().unwrap_or_default().to_string())
        .collect();
    (code, codes)
}

/// A library with one image and no note, as `scan` would find it.
fn one_image(tag: &str) -> TempDir {
    let dir = tmp(tag);
    write(&dir.0, "one.png", PNG);
    dir
}

/// The trash record files of a library, library-relative.
fn record_files(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(".trash")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".trash.json") {
            out.push(format!(".trash/{name}"));
        }
    }
    out
}

/// Rewrite one trash record's `trashed_at`, as a library left sitting for its
/// retention period would have it.
fn set_trashed_at(root: &Path, record: &str, stamp: &str) {
    const KEY: &str = "\"trashed_at\": \"";
    let path = root.join(record);
    let text = std::fs::read_to_string(&path).unwrap();
    let Some(at) = text.find(KEY) else { return };
    let value_start = at + KEY.len();
    let value_end = value_start + text[value_start..].find('"').unwrap();
    std::fs::write(
        &path,
        format!("{}{stamp}{}", &text[..value_start], &text[value_end..]),
    )
    .unwrap();
}

/// Move every trash record's clock back, as a library left sitting for its
/// retention period would have it.
fn age_records(dir: &Path) {
    for record in record_files(dir) {
        set_trashed_at(dir, &record, "2000-01-01T00:00:00Z");
    }
}

// ---------------------------------------------------------------------------
// tag / rate / set
// ---------------------------------------------------------------------------

#[test]
fn tag_creates_a_note_with_an_id_and_prints_the_front_matter() {
    let dir = one_image("tag-new");
    let stdout = ok(
        &dir.0,
        &["tag", "one.png", "--add", "landscape", "--add", "night"],
    );
    let note = read(&dir.0, "one.png.md");
    assert!(note.starts_with("---\n"), "front matter first: {note:?}");
    assert!(note.contains("tags: [landscape, night]"), "{note:?}");
    assert!(note.contains("id: "), "an id is assigned: {note:?}");
    assert!(
        note.contains("![[one.png]]"),
        "a note this command created carries the self-embed: {note:?}"
    );
    assert!(stdout.contains("tags: [landscape, night]"), "{stdout:?}");
    // The image bytes are never touched (invariant 3).
    assert_eq!(std::fs::read(dir.0.join("one.png")).unwrap(), PNG);
}

#[test]
fn tag_json_reports_the_result_and_the_op_to_undo() {
    let dir = one_image("tag-json");
    let stdout = ok(&dir.0, &["tag", "one.png", "--add", "x", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["schema"], "dimagine.note/0.1");
    assert_eq!(doc["image"], "one.png");
    assert_eq!(doc["changed"], true);
    assert!(doc["front_matter"].as_str().unwrap().contains("tags: [x]"));
    let op = doc["op"].as_str().unwrap();
    assert!(!op.is_empty(), "the op id is printed: {doc}");
    ok(&dir.0, &["undo", op]);
    assert!(
        !exists(&dir.0, "one.png.md"),
        "the printed id undoes exactly this operation"
    );
}

#[test]
fn tag_that_changes_nothing_writes_nothing() {
    let dir = one_image("tag-same");
    ok(&dir.0, &["tag", "one.png", "--add", "x"]);
    let before = read(&dir.0, "one.png.md");
    let (code, stdout, _) = dimagine(&dir.0, &["tag", "one.png", "--add", "x"]);
    assert_eq!(code, 0, "nothing to change is not a failure");
    assert!(stdout.contains("nothing to change"), "{stdout:?}");
    assert_eq!(read(&dir.0, "one.png.md"), before);
    let (code, stdout, _) = dimagine(&dir.0, &["tag", "one.png", "--add", "x", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(json_in(&stdout)["changed"], false);
}

#[test]
fn tag_keeps_a_hand_written_note_and_its_body() {
    let dir = one_image("tag-hand");
    write(
        &dir.0,
        "one.png.md",
        "---\ntitle: Kept\ntags: [old]\n---\n\nThe body says something.\n",
    );
    ok(
        &dir.0,
        &["tag", "one.png", "--add", "new", "--remove", "old"],
    );
    let note = read(&dir.0, "one.png.md");
    assert!(note.contains("title: Kept"), "{note:?}");
    assert!(note.contains("new"), "{note:?}");
    assert!(!note.contains("old"), "{note:?}");
    assert!(note.contains("The body says something."), "{note:?}");
    assert!(
        !note.contains("![[one.png]]"),
        "an existing note is not rewritten with a self-embed: {note:?}"
    );
}

#[test]
fn rate_sets_a_number_and_clears_it() {
    let dir = one_image("rate");
    ok(&dir.0, &["rate", "one.png", "4"]);
    assert!(read(&dir.0, "one.png.md").contains("rating: 4"));
    ok(&dir.0, &["rate", "one.png", "0"]);
    assert!(read(&dir.0, "one.png.md").contains("rating: 0"));
    ok(&dir.0, &["rate", "one.png", "none"]);
    assert!(!read(&dir.0, "one.png.md").contains("rating:"));
    let (code, _, stderr) = dimagine(&dir.0, &["rate", "one.png", "6"]);
    assert_eq!(code, 1, "a rating outside 0-5 refuses");
    assert!(stderr.contains("outside 0-5"), "{stderr:?}");
    let (code, _, stderr) = dimagine(&dir.0, &["rate", "one.png", "lots"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("is not 0-5 or none"), "{stderr:?}");
}

#[test]
fn a_non_string_tag_element_refuses_the_edit() {
    let dir = one_image("tag-non-string");
    // A number inside `tags` is outside ADR-016's editable subset (the unit
    // suite pins the editor; this pins the CLI path and the exit code).
    let note = "---\ntags: [x, 3]\n---\n";
    write(&dir.0, "one.png.md", note);
    for edit in [
        ["tag", "one.png", "--add", "y"],
        ["tag", "one.png", "--remove", "x"],
    ] {
        let (code, _, stderr) = dimagine(&dir.0, &edit);
        assert_eq!(code, 1, "{edit:?} must refuse");
        assert!(
            stderr.contains("outside the editable subset"),
            "{edit:?}: {stderr:?}"
        );
        assert_eq!(read(&dir.0, "one.png.md"), note, "nothing is written");
    }
}

#[test]
fn set_writes_several_scalar_properties() {
    let dir = one_image("set");
    ok(
        &dir.0,
        &["set", "one.png", "title=First light", "author=Someone"],
    );
    let note = read(&dir.0, "one.png.md");
    assert!(note.contains("title: First light"), "{note:?}");
    assert!(note.contains("author: Someone"), "{note:?}");
    // A property given no value is cleared, which means the key is gone: an
    // empty property is not a thing a note can hold (RW61 L5).
    ok(&dir.0, &["set", "one.png", "author="]);
    let note = read(&dir.0, "one.png.md");
    assert!(!note.contains("author"), "deleted, not emptied: {note:?}");
    assert!(note.contains("title: First light"), "{note:?}");
    // Clearing a key that is not there changes nothing.
    let (code, stdout, _) = dimagine(&dir.0, &["set", "one.png", "author="]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing to change"), "{stdout:?}");
    // An `id` is not the caller's to remove (FORMAT §3.3).
    let (code, _, stderr) = dimagine(&dir.0, &["set", "one.png", "id="]);
    assert_eq!(code, 1);
    assert!(stderr.contains("id"), "{stderr:?}");
    assert!(read(&dir.0, "one.png.md").contains("id: "));
}

#[test]
fn an_unknown_or_ambiguous_image_refuses() {
    let dir = tmp("tag-missing");
    write(&dir.0, "a.png", PNG);
    write(&dir.0, "sub/a.png", PNG);
    let (code, _, stderr) = dimagine(&dir.0, &["tag", "nope.png", "--add", "x"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no image named"), "{stderr:?}");
    let (code, _, stderr) = dimagine(&dir.0, &["tag", "no/such/a.png", "--add", "x"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no image at library path"), "{stderr:?}");
    // `a.png` is the root file by exact path, but a new note for it needs a
    // self-embed that no bare link can carry unambiguously.
    let (code, _, stderr) = dimagine(&dir.0, &["tag", "a.png", "--add", "x"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("ambiguous"), "{stderr:?}");
    assert!(!exists(&dir.0, "a.png.md"), "nothing is written");
    ok(&dir.0, &["tag", "sub/a.png", "--add", "x"]);
    assert!(exists(&dir.0, "sub/a.png.md"));
}

// ---------------------------------------------------------------------------
// collect
// ---------------------------------------------------------------------------

#[test]
fn collect_new_writes_the_collection_note_and_the_image_notes() {
    let dir = tmp("collect-new");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    let stdout = ok(
        &dir.0,
        &[
            "collect", "--new", "Best of", "one.png", "two.png", "--json",
        ],
    );
    let doc = json_in(&stdout);
    assert_eq!(doc["schema"], "dimagine.collect/0.1");
    assert_eq!(doc["collection"], "Best of.md");
    assert_eq!(doc["created"], true);
    let note = read(&dir.0, "Best of.md");
    assert!(note.contains("kind: collection"), "{note:?}");
    assert!(note.contains("title: Best of"), "{note:?}");
    assert!(note.contains("![[one.png]]"), "{note:?}");
    assert!(note.contains("![[two.png]]"), "{note:?}");
    // Joining a collection gives each image an id (ADR-017 §3).
    for image in ["one.png.md", "two.png.md"] {
        let member = read(&dir.0, image);
        assert!(member.contains("id: "), "{image}: {member:?}");
    }
}

#[test]
fn collect_new_gives_a_taken_name_the_next_free_one() {
    let dir = tmp("collect-bump");
    write(&dir.0, "one.png", PNG);
    write(
        &dir.0,
        "Best of.md",
        "---\nkind: collection\ntitle: Best of\n---\n",
    );
    let before = read(&dir.0, "Best of.md");
    let stdout = ok(
        &dir.0,
        &["collect", "--new", "Best of", "one.png", "--json"],
    );
    assert_eq!(json_in(&stdout)["collection"], "Best of 2.md");
    assert_eq!(
        read(&dir.0, "Best of.md"),
        before,
        "the note taken is untouched"
    );
    assert!(exists(&dir.0, "Best of 2.md"));
}

#[test]
fn collect_add_and_remove_edit_one_note() {
    let dir = tmp("collect-edit");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    write(
        &dir.0,
        "Picks.md",
        "---\nkind: collection\ntitle: Picks\n---\n\n![[one.png]]\n",
    );
    ok(&dir.0, &["collect", "--add", "Picks.md", "two.png"]);
    let note = read(&dir.0, "Picks.md");
    assert!(note.contains("![[one.png]]"), "{note:?}");
    assert!(note.contains("![[two.png]]"), "{note:?}");
    // Byte-exact: the member line is appended and nothing else moves.
    assert_eq!(
        note,
        "---\nkind: collection\ntitle: Picks\n---\n\n![[one.png]]\n![[two.png]]\n"
    );
    // Adding what is already there is not a change.
    let (code, stdout, _) = dimagine(&dir.0, &["collect", "--add", "Picks.md", "two.png"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing to change"), "{stdout:?}");
    ok(&dir.0, &["collect", "--remove", "Picks.md", "one.png"]);
    let note = read(&dir.0, "Picks.md");
    assert!(!note.contains("![[one.png]]"), "{note:?}");
    assert!(note.contains("![[two.png]]"), "{note:?}");
    // Byte-exact: only the member line goes; the kept line and the front
    // matter are untouched (ADR-017 §5).
    assert_eq!(
        note,
        "---\nkind: collection\ntitle: Picks\n---\n\n![[two.png]]\n"
    );
}

#[test]
fn collect_names_what_it_cannot_find() {
    let dir = one_image("collect-missing");
    let (code, _, stderr) = dimagine(&dir.0, &["collect", "--add", "Nope.md", "one.png"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no note at"), "{stderr:?}");
    let (code, _, stderr) = dimagine(&dir.0, &["collect", "one.png"]);
    assert_eq!(code, 1, "one of --new/--add/--remove is required");
    assert!(stderr.contains("give one of"), "{stderr:?}");
}

// ---------------------------------------------------------------------------
// trash / restore
// ---------------------------------------------------------------------------

#[test]
fn trash_moves_the_whole_group_and_writes_a_record() {
    let dir = tmp("trash");
    write(&dir.0, "refs/girl.png", PNG);
    write(&dir.0, "refs/girl.png.md", "---\ntags: [x]\n---\n");
    write(&dir.0, "refs/girl.png.side.json", "{\"item\":1}\n");
    let stdout = ok(&dir.0, &["trash", "refs/girl.png", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["schema"], "dimagine.trash/0.1");
    assert_eq!(doc["trashed"][0], "refs/girl.png");
    for rel in [
        "refs/girl.png",
        "refs/girl.png.md",
        "refs/girl.png.side.json",
    ] {
        assert!(!exists(&dir.0, rel), "{rel} is out of the library");
        assert!(
            exists(&dir.0, &format!(".trash/{rel}")),
            "{rel} is under .trash keeping its relative path"
        );
    }
    assert!(exists(&dir.0, ".trash/refs/girl.png.trash.json"));
    // The record names each file and where it came from.
    let record = read(&dir.0, ".trash/refs/girl.png.trash.json");
    assert!(record.contains("refs/girl.png"), "{record:?}");
    assert!(record.contains("trashed_at"), "{record:?}");
    assert!(record.contains("mtime"), "{record:?}");
    // A library with a trashed group still checks clean.
    let (code, _, _) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0);
}

#[test]
fn restore_puts_the_group_back_and_removes_the_record() {
    let dir = tmp("restore");
    write(&dir.0, "girl.png", PNG);
    write(&dir.0, "girl.png.md", "---\ntags: [x]\n---\n");
    let image_bytes = std::fs::read(dir.0.join("girl.png")).unwrap();
    let mtime = std::fs::metadata(dir.0.join("girl.png"))
        .unwrap()
        .modified()
        .unwrap();
    ok(&dir.0, &["trash", "girl.png"]);
    // A bare name resolves against the records: the image is no longer in the walk.
    ok(&dir.0, &["restore", "girl.png"]);
    assert_eq!(std::fs::read(dir.0.join("girl.png")).unwrap(), image_bytes);
    assert_eq!(
        std::fs::metadata(dir.0.join("girl.png"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime,
        "the recorded time comes back"
    );
    assert_eq!(read(&dir.0, "girl.png.md"), "---\ntags: [x]\n---\n");
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "purge", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing to purge"), "{stdout:?}");
    let (code, _, stderr) = dimagine(&dir.0, &["restore", "girl.png"]);
    assert_eq!(code, 1, "a group already restored is not in the trash");
    assert!(stderr.contains("nothing trashed named"), "{stderr:?}");
}

#[test]
fn restore_refuses_when_the_original_path_is_taken() {
    let dir = tmp("restore-taken");
    write(&dir.0, "girl.png", PNG);
    ok(&dir.0, &["trash", "girl.png"]);
    write(&dir.0, "girl.png", b"someone else put this here");
    let (code, _, stderr) = dimagine(&dir.0, &["restore", "girl.png"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("already exists"), "{stderr:?}");
    assert_eq!(read(&dir.0, "girl.png"), "someone else put this here");
    assert!(
        exists(&dir.0, ".trash/girl.png"),
        "the group waits in the trash"
    );
}

#[test]
fn a_taken_trash_name_gets_the_next_bumped_one() {
    let dir = tmp("trash-bump");
    write(&dir.0, "one.png", PNG);
    // Something else already owns the trash slot (Obsidian trashes there too).
    write(&dir.0, ".trash/one.png", b"not mine");
    ok(&dir.0, &["trash", "one.png"]);
    assert!(
        exists(&dir.0, ".trash/one 2.png"),
        "the group got its own slot"
    );
    assert!(exists(&dir.0, ".trash/one 2.png.trash.json"));
    assert_eq!(
        read(&dir.0, ".trash/one.png"),
        "not mine",
        "the other file is untouched"
    );
    ok(&dir.0, &["restore", "one.png"]);
    assert!(
        exists(&dir.0, "one.png"),
        "and the record says where it came from"
    );
    assert_eq!(read(&dir.0, ".trash/one.png"), "not mine");
}

#[test]
fn a_stale_record_at_the_group_name_moves_the_group_to_the_next_round() {
    let dir = tmp("trash-record-bump");
    write(&dir.0, "x.png", PNG);
    // A leftover record already claims the round-1 record name, while the
    // group's own members are all free.
    let stale = "{\n  \"op_id\": \"stale\",\n  \"trashed_at\": \"2020-01-01T00:00:00Z\",\n  \
                 \"files\": [\n    {\n      \"original\": \"y.png\",\n      \"trash\": \
                 \".trash/y.png\",\n      \"mtime_ns\": 0\n    }\n  ]\n}";
    write(&dir.0, ".trash/x.png.trash.json", stale);
    ok(&dir.0, &["trash", "x.png"]);
    assert!(!exists(&dir.0, "x.png"));
    assert!(
        exists(&dir.0, ".trash/x 2.png"),
        "the group took the next free round instead of landing with no record"
    );
    assert!(exists(&dir.0, ".trash/x 2.png.trash.json"));
    assert_eq!(
        read(&dir.0, ".trash/x.png.trash.json"),
        stale,
        "the stale record is untouched"
    );
    // The group is restorable: it has a record of its own.
    ok(&dir.0, &["restore", "x.png"]);
    assert!(exists(&dir.0, "x.png"));
    assert_eq!(std::fs::read(dir.0.join("x.png")).unwrap(), PNG);
}

// ---------------------------------------------------------------------------
// undo
// ---------------------------------------------------------------------------

#[test]
fn undo_reverts_the_last_operation() {
    let dir = one_image("undo-tag");
    write(&dir.0, "one.png.md", "---\ntags: [before]\n---\n");
    ok(&dir.0, &["tag", "one.png", "--add", "after"]);
    assert!(read(&dir.0, "one.png.md").contains("after"));
    let stdout = ok(&dir.0, &["undo", "--json"]);
    assert_eq!(json_in(&stdout)["schema"], "dimagine.undo/0.1");
    let note = read(&dir.0, "one.png.md");
    assert!(note.contains("before"), "{note:?}");
    assert!(!note.contains("after"), "{note:?}");
}

#[test]
fn a_refused_undo_names_the_files_that_block_it() {
    let dir = one_image("undo-refused");
    write(&dir.0, "one.png.md", "---\ntags: [before]\n---\n");
    ok(&dir.0, &["tag", "one.png", "--add", "after"]);
    // Something else edits the note after the operation, so the undo refuses.
    write(&dir.0, "one.png.md", "---\ntags: [hand]\n---\n");
    let (code, _, stderr) = dimagine(&dir.0, &["undo"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("refused"), "{stderr:?}");
    assert!(
        stderr.contains("one.png.md"),
        "the blocking file is named: {stderr:?}"
    );
    assert!(
        !stderr.contains("UndoMismatch"),
        "a person reads text, not a Rust dump: {stderr:?}"
    );
    assert_eq!(read(&dir.0, "one.png.md"), "---\ntags: [hand]\n---\n");
}

#[test]
fn undo_names_an_operation_and_refuses_a_fresh_library() {
    let dir = tmp("undo-op");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    ok(&dir.0, &["tag", "one.png", "--add", "keep"]);
    let stdout = ok(&dir.0, &["trash", "two.png", "--json"]);
    let trash_op = json_in(&stdout)["op"].as_str().unwrap().to_string();
    // Something later, so "the last operation" is not the trash.
    ok(&dir.0, &["rate", "one.png", "3"]);
    assert!(!exists(&dir.0, "two.png"));
    ok(&dir.0, &["undo", &trash_op]);
    assert!(exists(&dir.0, "two.png"), "the named operation went back");
    assert!(
        read(&dir.0, "one.png.md").contains("rating: 3"),
        "the later one is untouched"
    );

    let fresh = one_image("undo-none");
    let (code, _, stderr) = dimagine(&fresh.0, &["undo"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no recorded operation"), "{stderr:?}");
}

#[test]
fn undo_of_a_created_note_moves_it_to_the_trash_not_a_delete() {
    let dir = one_image("undo-create");
    ok(&dir.0, &["tag", "one.png", "--add", "x"]);
    assert!(exists(&dir.0, "one.png.md"));
    ok(&dir.0, &["undo"]);
    assert!(
        !exists(&dir.0, "one.png.md"),
        "the note is out of the library"
    );
    assert!(exists(&dir.0, ".trash/one.png.md"), "and recoverable");
    assert!(exists(&dir.0, "one.png"), "the image was never touched");
}

#[test]
fn undo_of_a_collection_hands_every_created_file_back() {
    let dir = tmp("undo-collect");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    let stdout = ok(
        &dir.0,
        &["collect", "--new", "Picks", "one.png", "two.png", "--json"],
    );
    let op = json_in(&stdout)["op"].as_str().unwrap().to_string();
    for rel in ["Picks.md", "one.png.md", "two.png.md"] {
        assert!(exists(&dir.0, rel), "{rel} was created");
    }
    ok(&dir.0, &["undo", &op]);
    // Every created file is handed back, each as its own trash group.
    for rel in ["Picks.md", "one.png.md", "two.png.md"] {
        assert!(!exists(&dir.0, rel), "{rel} is out of the library");
        assert!(
            exists(&dir.0, &format!(".trash/{rel}")),
            "{rel} is in the trash"
        );
        assert!(
            exists(&dir.0, &format!(".trash/{rel}.trash.json")),
            "{rel} has a record of its own"
        );
    }
}

#[test]
fn undo_of_a_created_note_takes_a_free_trash_slot() {
    let dir = tmp("undo-create-bump");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["tag", "one.png", "--add", "x"]);
    assert!(exists(&dir.0, "one.png.md"));
    // Something else already owns the slot the hand-back would use.
    write(&dir.0, ".trash/one.png.md", b"not mine");
    ok(&dir.0, &["undo"]);
    assert!(
        !exists(&dir.0, "one.png.md"),
        "the note is out of the library"
    );
    assert_eq!(
        read(&dir.0, ".trash/one.png.md"),
        "not mine",
        "the foreign file is untouched"
    );
    assert!(
        exists(&dir.0, ".trash/one.png 2.md"),
        "the hand-back took the next free slot"
    );
    let record = read(&dir.0, ".trash/one.png 2.md.trash.json");
    assert!(
        record.contains("\"one.png.md\""),
        "the record names the file it trashed: {record:?}"
    );
    // Exactly one record names the group, so a restore can reach it.
    ok(&dir.0, &["restore", "one.png.md"]);
    assert!(exists(&dir.0, "one.png.md"));
}

#[test]
fn undo_of_a_trash_restores_the_group() {
    let dir = tmp("undo-trash");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    assert!(!exists(&dir.0, "one.png"));
    ok(&dir.0, &["undo"]);
    assert!(
        exists(&dir.0, "one.png"),
        "the inverse of a trash is a restore"
    );
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[test]
fn config_lists_gets_sets_and_resets() {
    let dir = one_image("config");
    let stdout = ok(&dir.0, &["config", "list"]);
    assert!(stdout.contains("trash.retentionDays = 180"), "{stdout:?}");
    assert!(
        !stdout.contains("changed"),
        "the default is not a change: {stdout:?}"
    );
    ok(&dir.0, &["config", "set", "trash.retentionDays", "30"]);
    assert!(exists(&dir.0, ".dimagine/settings.json"));
    let stdout = ok(&dir.0, &["config", "get", "trash.retentionDays", "--json"]);
    assert_eq!(json_in(&stdout)["value"], 30);
    let (code, _, stderr) = dimagine(&dir.0, &["config", "set", "trash.retentionDays", "9999"]);
    assert_eq!(code, 1, "a value outside the schema refuses");
    assert!(stderr.contains("not allowed"), "{stderr:?}");
    assert_eq!(
        json_in(&ok(
            &dir.0,
            &["config", "get", "trash.retentionDays", "--json"]
        ))["value"],
        30,
        "a refused set changes nothing"
    );
    let (code, _, stderr) = dimagine(&dir.0, &["config", "get", "no.such.key"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("unknown setting"), "{stderr:?}");
    ok(&dir.0, &["config", "reset", "trash.retentionDays"]);
    assert_eq!(
        json_in(&ok(
            &dir.0,
            &["config", "get", "trash.retentionDays", "--json"]
        ))["value"],
        180
    );
}

// ---------------------------------------------------------------------------
// trash purge
// ---------------------------------------------------------------------------

#[test]
fn purge_deletes_only_aged_groups_and_only_with_yes() {
    let dir = tmp("purge");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["config", "set", "trash.retentionDays", "1"]);
    ok(&dir.0, &["trash", "one.png"]);
    // Nothing is old yet.
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "purge", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing to purge"), "{stdout:?}");
    age_records(&dir.0);
    // A dry run lists and changes nothing.
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "purge", "--dry-run", "--json"]);
    assert_eq!(code, 0);
    let doc = json_in(&stdout);
    assert_eq!(doc["schema"], "dimagine.purge/0.1");
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["groups"].as_array().unwrap().len(), 1);
    assert!(exists(&dir.0, ".trash/one.png"));
    // Without --yes, nothing is deleted.
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "purge"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("re-run with --yes"), "{stdout:?}");
    assert!(exists(&dir.0, ".trash/one.png"));
    // With it, the group and its record are gone for good.
    let (code, stdout, stderr) = dimagine(&dir.0, &["trash", "purge", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("deleted"), "{stdout:?}");
    assert!(!exists(&dir.0, ".trash/one.png"));
    assert!(!exists(&dir.0, ".trash/one.png.trash.json"));
    // A permanent deletion has no inverse (ADR-017 §7).
    let (code, _, stderr) = dimagine(&dir.0, &["undo"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("refused"), "{stderr:?}");
}

#[test]
fn purge_stops_exactly_at_the_retention_boundary() {
    let dir = tmp("purge-boundary");
    write(&dir.0, "old.png", PNG);
    write(&dir.0, "young.png", PNG);
    ok(&dir.0, &["trash", "old.png"]);
    ok(&dir.0, &["trash", "young.png"]);
    let now = chrono::Utc::now();
    set_trashed_at(
        &dir.0,
        ".trash/old.png.trash.json",
        &(now - chrono::Duration::days(180)).to_rfc3339(),
    );
    set_trashed_at(
        &dir.0,
        ".trash/young.png.trash.json",
        &(now - chrono::Duration::days(179)).to_rfc3339(),
    );
    let (code, stdout, stderr) = dimagine(&dir.0, &["trash", "purge", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        !exists(&dir.0, ".trash/old.png"),
        "180 days old is past the default retention"
    );
    assert!(
        exists(&dir.0, ".trash/young.png"),
        "179 days old is inside the window"
    );
}

#[test]
fn retention_zero_never_purges() {
    let dir = tmp("purge-never");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["config", "set", "trash.retentionDays", "0"]);
    ok(&dir.0, &["trash", "one.png"]);
    age_records(&dir.0);
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "purge", "--yes"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing to purge"), "{stdout:?}");
    assert!(exists(&dir.0, ".trash/one.png"));
}

// ---------------------------------------------------------------------------
// check — write-state health
// ---------------------------------------------------------------------------

#[test]
fn check_reports_a_stray_temporary_file() {
    let dir = one_image("check-temp");
    write(&dir.0, ".dimagine.tmp.leftover", b"half a file");
    let (code, codes) = check_codes(&dir.0);
    assert_eq!(code, 1, "a warning is a problem: {codes:?}");
    assert!(
        codes.iter().any(|code| code == "write_orphan_temp"),
        "the stray temp file is named: {codes:?}"
    );
}

#[test]
fn check_reports_an_unrecovered_journal() {
    let dir = one_image("check-journal");
    std::fs::create_dir_all(dir.0.join(".dimagine/journal/op-stuck")).unwrap();
    let (code, stdout, _) = dimagine(&dir.0, &["check", "--json"]);
    assert_eq!(code, 1);
    let doc = json_in(&stdout);
    let found = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "write_journal_leftover");
    assert!(found.is_some(), "the stuck journal is named: {stdout}");
    assert_eq!(found.unwrap()["severity"], "error");
}

#[test]
fn check_reports_a_record_that_names_a_missing_file() {
    let dir = tmp("check-record");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    std::fs::remove_file(dir.0.join(".trash/one.png")).unwrap();
    let (code, codes) = check_codes(&dir.0);
    assert_eq!(code, 1);
    assert!(
        codes
            .iter()
            .any(|code| code == "write_trash_record_missing_file"),
        "{codes:?}"
    );
}

#[test]
fn check_warns_about_a_stranded_trash_group() {
    let dir = tmp("check-stranded");
    write(&dir.0, "one.png", PNG);
    // The state a trash that lost its record leaves behind: the group is in
    // .trash and nothing claims it, so restore and purge cannot reach it.
    write(&dir.0, ".trash/one.png", PNG);
    write(&dir.0, ".trash/one.png.md", "---\ntags: [x]\n---\n");
    let (code, stdout, _) = dimagine(&dir.0, &["check", "--json"]);
    assert_eq!(code, 1, "a stranded group is a problem: {stdout}");
    let doc = json_in(&stdout);
    let found = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| {
            finding["code"] == "write_trash_file_no_record" && finding["path"] == ".trash/one.png"
        })
        .unwrap_or_else(|| panic!("the stranded image is named: {stdout}"));
    assert_eq!(found["severity"], "warning");

    // A lone file another tool put in .trash stays info (ADR-017 §7 leaves it
    // alone), and does not make check fail.
    let other = tmp("check-foreign");
    write(&other.0, "one.png", PNG);
    write(&other.0, ".trash/obsidian-note.md", b"not ours");
    let (code, stdout, _) = dimagine(&other.0, &["check", "--json"]);
    assert_eq!(code, 0, "{stdout}");
    let doc = json_in(&stdout);
    let found = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "write_trash_file_no_record")
        .unwrap_or_else(|| panic!("the foreign file is named: {stdout}"));
    assert_eq!(found["severity"], "info");
}

#[test]
fn check_reports_two_records_claiming_one_trash_file() {
    let dir = tmp("check-double-claim");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    // A second record claims the file the first one owns (what a hand-back
    // into a taken slot used to leave behind).
    let stolen = read(&dir.0, ".trash/one.png.trash.json");
    let copy = stolen.replace("\"op_id\": \"", "\"op_id\": \"copy-");
    write(&dir.0, ".trash/two.png.trash.json", copy);
    let (code, codes) = check_codes(&dir.0);
    assert_eq!(code, 1, "a double claim is a problem: {codes:?}");
    assert!(
        codes.iter().any(|code| code == "write_trash_double_claim"),
        "the double claim is named: {codes:?}"
    );
}

#[test]
fn a_healthy_written_library_checks_clean() {
    let dir = one_image("check-clean");
    ok(&dir.0, &["tag", "one.png", "--add", "x"]);
    ok(&dir.0, &["rate", "one.png", "5"]);
    ok(&dir.0, &["collect", "--new", "Picks", "one.png"]);
    let (code, stdout, stderr) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("no problems found"), "{stdout:?}");
    assert!(!exists(&dir.0, ".trash"), "no trash was made");
    let leftovers = std::fs::read_dir(dir.0.join(".dimagine/journal"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "every operation closed its journal");
}

// ---------------------------------------------------------------------------
// trash empty — the immediate empty of ADR-017 §7
// ---------------------------------------------------------------------------

#[test]
fn trash_empty_deletes_every_recorded_group_only_with_yes() {
    let dir = tmp("empty");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    ok(&dir.0, &["trash", "two.png"]);
    // Nothing is aged: `empty` ignores retention entirely, which is the point.
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "empty"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("re-run with --yes"), "{stdout:?}");
    assert!(exists(&dir.0, ".trash/one.png"), "nothing deleted yet");
    assert!(exists(&dir.0, ".trash/two.png"));
    // With --yes, both groups and their records are gone.
    let (code, stdout, stderr) = dimagine(&dir.0, &["trash", "empty", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(!exists(&dir.0, ".trash/one.png"));
    assert!(!exists(&dir.0, ".trash/one.png.trash.json"));
    assert!(!exists(&dir.0, ".trash/two.png"));
    assert!(!exists(&dir.0, ".trash/two.png.trash.json"));
    // A permanent deletion has no inverse (ADR-018 §8).
    let (code, _, stderr) = dimagine(&dir.0, &["undo"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("refused"), "{stderr:?}");
    // And it is idempotent: an empty trash says so rather than failing.
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "empty"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("already empty"), "{stdout:?}");
}

#[test]
fn trash_empty_deletes_a_group_whose_file_is_gone() {
    let dir = tmp("empty-broken");
    write(&dir.0, "one.png", PNG);
    write(&dir.0, "two.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    ok(&dir.0, &["trash", "two.png"]);
    // One group lost a file: the state `check` reports as a missing file. The
    // delete used to refuse the whole batch, leaving both groups in place.
    std::fs::remove_file(dir.0.join(".trash/one.png")).unwrap();
    let (code, stdout, stderr) = dimagine(&dir.0, &["trash", "empty", "--yes"]);
    assert_eq!(
        code, 0,
        "one broken group must not fail the batch: {stderr}"
    );
    assert!(
        !exists(&dir.0, ".trash/one.png.trash.json"),
        "the broken group's record is gone: {stdout}"
    );
    assert!(
        !exists(&dir.0, ".trash/two.png"),
        "the healthy group went too"
    );
    assert!(!exists(&dir.0, ".trash/two.png.trash.json"));
    let (code, stdout, _) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0, "nothing is left to report: {stdout}");
}

#[test]
fn trash_empty_reports_under_its_own_schema() {
    let dir = tmp("empty-schema");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    let (code, stdout, _) = dimagine(&dir.0, &["trash", "empty", "--yes", "--json"]);
    assert_eq!(code, 0);
    // The result of `empty` is not a purge result: a caller must be able to
    // tell the two permanent-delete commands apart.
    assert_eq!(json_in(&stdout)["schema"], "dimagine.trash-empty/0.1");
}

#[test]
fn trash_empty_leaves_files_that_have_no_record() {
    let dir = tmp("empty-obsidian");
    write(&dir.0, "one.png", PNG);
    ok(&dir.0, &["trash", "one.png"]);
    // Obsidian trashes into the same folder under a flat name of its own.
    write(&dir.0, ".trash/obsidian-note.md", b"not ours");
    let (code, _, stderr) = dimagine(&dir.0, &["trash", "empty", "--yes"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!exists(&dir.0, ".trash/one.png"), "our group is gone");
    assert!(
        exists(&dir.0, ".trash/obsidian-note.md"),
        "a file dimagine never trashed is never deleted"
    );
}

// ---------------------------------------------------------------------------
// the old .dimagine/trash/ (ADR-017 §7 keeps it readable)
// ---------------------------------------------------------------------------

#[test]
fn check_reports_the_old_trash_location() {
    let dir = one_image("legacy-trash");
    write(&dir.0, ".dimagine/trash/girl.png", PNG);
    let (code, stdout, _) = dimagine(&dir.0, &["check", "--json"]);
    assert_eq!(code, 0, "info alone is not a problem");
    let doc = json_in(&stdout);
    let found = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "write_legacy_trash")
        .unwrap_or_else(|| panic!("the old location is named: {stdout}"));
    assert_eq!(found["severity"], "info");
    assert_eq!(found["path"], ".dimagine/trash/");
    let files = found["detail"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0], ".dimagine/trash/girl.png");
}

#[test]
fn an_empty_old_trash_directory_is_not_worth_reporting() {
    let dir = one_image("legacy-empty");
    std::fs::create_dir_all(dir.0.join(".dimagine/trash")).unwrap();
    let (code, stdout, _) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0, "stdout: {stdout}");
    assert!(stdout.contains("no problems found"), "{stdout:?}");
}

#[test]
fn restore_points_at_the_old_trash_location_instead_of_saying_nothing_is_there() {
    let dir = one_image("legacy-restore");
    write(&dir.0, ".dimagine/trash/girl.png", PNG);
    let (code, _, stderr) = dimagine(&dir.0, &["restore", "girl.png"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("old trash location"), "{stderr:?}");
    assert!(
        exists(&dir.0, ".dimagine/trash/girl.png"),
        "the file is reported, never moved"
    );
    // A name that is nowhere keeps the plain message.
    let (code, _, stderr) = dimagine(&dir.0, &["restore", "absent.png"]);
    assert_eq!(code, 1);
    assert!(!stderr.contains("old trash location"), "{stderr:?}");
}

#[test]
fn dimagine_writes_nothing_into_the_old_trash_location() {
    let dir = tmp("legacy-readonly");
    write(&dir.0, "girl.png", PNG);
    // The commands that most want a trash directory must not create the old one.
    ok(&dir.0, &["trash", "girl.png"]);
    assert!(exists(&dir.0, ".trash/girl.png.trash.json"));
    ok(&dir.0, &["trash", "empty", "--yes"]);
    assert!(!exists(&dir.0, ".trash/girl.png.trash.json"), "emptied");
    assert!(
        !exists(&dir.0, ".dimagine/trash"),
        "the old location is never written (ADR-017 §7)"
    );
}

#[test]
fn a_subcommand_of_trash_cannot_also_be_an_image_argument() {
    let dir = one_image("trash-conflict");
    // `purge` is a subcommand now, so it can no longer trash an image by that
    // bare name through this command; the image is still there afterwards.
    let (code, _, _) = dimagine(&dir.0, &["trash", "purge"]);
    assert_eq!(code, 0);
    assert!(exists(&dir.0, "one.png"), "the image is not trashed");
    // `trash` with nothing to do says so and fails, rather than doing nothing
    // quietly.
    let (code, _, stderr) = dimagine(&dir.0, &["trash"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("at least one image"), "{stderr:?}");
}

/// ADR-018 §7: the fourth deployment check names the files the service user
/// could not write. A healthy library run by its owner is writable all over,
/// so it stays clean; a note the service cannot write is an error.
#[test]
fn a_service_writable_library_is_clean_and_an_unwritable_note_is_reported() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = one_image("service-writable");
        // A real note lands first, so the derived folder and a note exist.
        ok(&dir.0, &["tag", "one.png", "--add", "cat"]);
        // Everything is the test user's: the check must stay clean.
        let (_, codes) = check_codes(&dir.0);
        assert!(
            !codes.iter().any(|c| c == "write_service_unwritable"),
            "a library the user owns is clean: {codes:?}"
        );
        // Now a note the owner cannot write (mode drops the write bit).
        std::fs::set_permissions(
            dir.0.join("one.png.md"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        let (code, codes) = check_codes(&dir.0);
        assert_eq!(code, 1, "an unwritable note is a problem: {codes:?}");
        assert!(
            codes.iter().any(|c| c == "write_service_unwritable"),
            "the unwritable note is named: {codes:?}"
        );
    }
}

/// ADR-018 §7: a `.trash` that is a symlink is reported — the write core
/// refuses to follow it, so a library whose trash was replaced by a link is a
/// problem, not a working trash.
#[test]
fn check_reports_a_trash_that_is_a_symlink() {
    #[cfg(unix)]
    {
        let dir = one_image("trash-symlink");
        std::os::unix::fs::symlink(std::env::temp_dir(), dir.0.join(".trash")).unwrap();
        let (code, codes) = check_codes(&dir.0);
        assert_eq!(code, 1, "a symlinked trash is a problem: {codes:?}");
        assert!(
            codes.iter().any(|c| c == "write_trash_symlink"),
            "the symlinked trash is named: {codes:?}"
        );
    }
}
