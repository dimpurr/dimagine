//! End-to-end tests of `dimagine merge` (W77 方案 2, Scope B): a real process
//! over a library in a temporary folder. The pure planner's front-matter union
//! and link repoint are pinned in `merge_plan_test.rs`; here we check what the
//! command actually lands on disk — the drop's whole group under `.trash/`, the
//! kept note's bytes, the links in other notes, `--dry-run` leaving nothing
//! touched, `--auto` picking the keeper by image facts, and `undo` handing both
//! images back.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use image::{ImageBuffer, Rgb, RgbImage};

const BIN: &str = env!("CARGO_BIN_EXE_dimagine");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-merge-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create CLI merge test dir");
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

fn bytes_of(root: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel)).ok()
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
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

/// A real, decodable PNG of the given dimensions: a solid fill, so `scan`
/// reports these pixels and `--auto` can rank on them.
fn png(w: u32, h: u32) -> Vec<u8> {
    encode(
        RgbImage::from_pixel(w, h, Rgb([10, 120, 200])),
        image::ImageFormat::Png,
    )
}

/// A real, decodable lossy JPEG of the given dimensions.
fn jpeg(w: u32, h: u32) -> Vec<u8> {
    encode(
        RgbImage::from_pixel(w, h, Rgb([200, 120, 10])),
        image::ImageFormat::Jpeg,
    )
}

fn encode(img: ImageBuffer<Rgb<u8>, Vec<u8>>, format: image::ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), format).unwrap();
    out
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

// ---------------------------------------------------------------------------

#[test]
fn merge_folds_the_drop_into_the_keep_and_trashes_the_whole_drop_group() {
    let dir = tmp("fold");
    write(&dir.0, "big.jpg", jpeg(16, 16));
    write(
        &dir.0,
        "big.jpg.md",
        "---\ntags: [photo]\nsource: https://keep\nadded: 2024-05-01T00:00:00+00:00\n---\n\n![[big.jpg]]\n",
    );
    write(&dir.0, "refs/small.jpg", jpeg(4, 4));
    write(
        &dir.0,
        "refs/small.jpg.md",
        "---\ntags: [tg, photo]\nsource: https://drop\nadded: 2023-01-02T00:00:00+00:00\n---\n\n![[refs/small.jpg]]\n",
    );
    // A raw sidecar is part of the group and must travel with the image.
    write(&dir.0, "refs/small.jpg.side.json", "{\"item\":7}\n");

    let stdout = ok(&dir.0, &["merge", "big.jpg", "refs/small.jpg", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["schema"], "dimagine.merge/0.1");
    assert_eq!(doc["dry_run"], false);
    assert_eq!(doc["keep"], "big.jpg");
    assert_eq!(doc["drop"], "refs/small.jpg");
    assert_eq!(doc["keep_note"], "big.jpg.md");
    assert_eq!(doc["keep_note_created"], false);
    assert_eq!(doc["tags_added"], serde_json::json!(["tg"]));
    assert_eq!(doc["added_set"], "2023-01-02T00:00:00+00:00");
    assert_eq!(doc["sources_appended"], 1); // the merged entry (drop had no sources)
    assert!(doc["op"].is_string(), "an op id is returned for undo");

    // The drop's whole group left the library and landed under .trash/.
    for rel in [
        "refs/small.jpg",
        "refs/small.jpg.md",
        "refs/small.jpg.side.json",
    ] {
        assert!(!exists(&dir.0, rel), "{rel} left the library");
        assert!(
            exists(&dir.0, &format!(".trash/{rel}")),
            "{rel} is under .trash with its relative path"
        );
    }
    assert!(exists(&dir.0, ".trash/refs/small.jpg.trash.json"));
    // The kept image and its note stay put.
    assert!(exists(&dir.0, "big.jpg"));
    assert!(exists(&dir.0, "big.jpg.md"));

    // The kept note now carries the merged provenance, but never lost its own
    // `source`, and took the earlier `added`.
    let kept = read(&dir.0, "big.jpg.md");
    assert!(kept.contains("- type: merged"), "{kept}");
    assert!(kept.contains("path: refs/small.jpg"), "{kept}");
    assert!(kept.contains("source: https://keep"), "keep's own source");
    assert!(kept.contains("added: 2023-01-02"), "{kept}");
    assert!(kept.contains("tags: [photo, tg]"), "{kept}");

    // A library that was just merged still checks clean.
    let (code, _, stderr) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0, "merged library checks clean: {stderr}");
}

#[test]
fn merge_repoints_other_notes_to_the_kept_image() {
    let dir = tmp("repoint");
    write(&dir.0, "keep.jpg", jpeg(16, 16));
    write(&dir.0, "keep.jpg.md", "---\n---\n\n![[keep.jpg]]\n");
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(&dir.0, "drop.jpg.md", "---\n---\n\n![[drop.jpg]]\n");
    write(
        &dir.0,
        "shelf.md",
        "---\nkind: collection\n---\n\n![[drop.jpg]]\nplain text\n",
    );

    let stdout = ok(&dir.0, &["merge", "keep.jpg", "drop.jpg", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["links_repointed"], 1);
    assert!(doc["repointed_notes"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("shelf.md")));

    let shelf = read(&dir.0, "shelf.md");
    assert!(shelf.contains("![[keep.jpg]]"), "{shelf}");
    assert!(
        !shelf.contains("drop.jpg"),
        "the shelf no longer names the drop: {shelf}"
    );

    let (code, _, stderr) = dimagine(&dir.0, &["check"]);
    assert_eq!(code, 0, "no link is left dangling: {stderr}");
}

#[test]
fn a_kept_image_without_a_note_gains_one() {
    let dir = tmp("create-note");
    write(&dir.0, "keep.jpg", jpeg(16, 16)); // no keep.jpg.md
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(
        &dir.0,
        "drop.jpg.md",
        "---\ntags: [tg]\n---\n\n![[drop.jpg]]\n",
    );

    let stdout = ok(&dir.0, &["merge", "keep.jpg", "drop.jpg", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["keep_note_created"], true);
    assert!(
        exists(&dir.0, "keep.jpg.md"),
        "a note is created for the keep"
    );
    let kept = read(&dir.0, "keep.jpg.md");
    assert!(kept.contains("- type: merged"), "{kept}");
    assert!(
        kept.contains("![[keep.jpg]]"),
        "it embeds its own image: {kept}"
    );
    // The id the note lands with is a real ULID minted for this merge, not the
    // planner's placeholder: `ensure_id` would refuse a non-ULID outright.
    let id = id_of(&kept).expect("the created note carries an id");
    assert!(is_ulid(&id), "{id} is not a canonical ULID: {kept}");
    assert_ne!(
        id, "01J00000000000000000000000",
        "a constant placeholder id"
    );
}

/// Two merges that each create a kept note must mint different ids (§3.3): a
/// shared constant would make every merge-created note the same image.
#[test]
fn two_created_kept_notes_get_distinct_ids() {
    let dir = tmp("create-ids");
    for name in ["first", "second", "third"] {
        write(&dir.0, &format!("{name}.jpg"), jpeg(16, 16));
        write(&dir.0, &format!("{name}-drop.jpg"), jpeg(8, 8));
        write(
            &dir.0,
            &format!("{name}-drop.jpg.md"),
            "---\ntags: [tg]\n---\n\n",
        );
        ok(
            &dir.0,
            &["merge", &format!("{name}.jpg"), &format!("{name}-drop.jpg")],
        );
    }
    let ids: Vec<String> = ["first", "second", "third"]
        .iter()
        .map(|name| id_of(&read(&dir.0, &format!("{name}.jpg.md"))).expect("a created id"))
        .collect();
    for id in &ids {
        assert!(is_ulid(id), "{id} is not a canonical ULID");
        assert_ne!(
            id, "01J00000000000000000000000",
            "a constant placeholder id"
        );
    }
    let unique: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(unique.len(), 3, "created notes share an id: {ids:?}");
}

/// A kept note that exists but holds nothing is still a note: the merge edits
/// it in place instead of trying to create a file that is already there, which
/// used to half-apply the operation and leave the drop in `.trash/`.
#[test]
fn a_kept_note_that_exists_but_is_empty_is_edited_in_place() {
    let dir = tmp("empty-note");
    write(&dir.0, "keep.jpg", jpeg(16, 16));
    write(&dir.0, "keep.jpg.md", ""); // present, zero bytes
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(
        &dir.0,
        "drop.jpg.md",
        "---\ntags: [tg]\n---\n\n![[drop.jpg]]\n",
    );

    let stdout = ok(&dir.0, &["merge", "keep.jpg", "drop.jpg", "--json"]);
    let doc = json_in(&stdout);
    assert_eq!(doc["keep_note_created"], false, "the note is edited");
    let kept = read(&dir.0, "keep.jpg.md");
    assert!(
        kept.contains("- type: merged"),
        "the kept note gained the provenance: {kept}"
    );
    assert!(kept.contains("tags: [tg]"), "the tags were unioned: {kept}");
    assert!(
        !exists(&dir.0, "drop.jpg") && exists(&dir.0, ".trash/drop.jpg"),
        "the drop group moved to the trash"
    );

    let op = doc["op"].as_str().unwrap().to_string();
    ok(&dir.0, &["undo", &op, "--json"]);
    assert!(exists(&dir.0, "drop.jpg"), "undo hands the drop back");
    assert_eq!(
        bytes_of(&dir.0, "keep.jpg.md"),
        Some(Vec::new()),
        "undo hands the note back with its empty bytes, because it existed"
    );
    assert!(
        !exists(&dir.0, ".trash/keep.jpg"),
        "nothing of the keep sits in the trash"
    );
}

/// An oversized kept note is an unknown, never an empty one: the merge refuses
/// before a single byte moves rather than planning over bytes it cannot read.
#[test]
fn a_kept_note_larger_than_the_limit_refuses_the_merge_before_any_write() {
    let dir = tmp("oversize-note");
    write(&dir.0, "keep.jpg", jpeg(16, 16));
    write(
        &dir.0,
        "keep.jpg.md",
        vec![b'a'; 4097usize].as_slice(), // one byte over the 4 KB limit below
    );
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(&dir.0, "drop.jpg.md", "---\ntags: [tg]\n---\n\n");
    let keep_bytes = bytes_of(&dir.0, "keep.jpg.md");

    let (code, stdout, stderr) = dimagine(
        &dir.0,
        &["merge", "keep.jpg", "drop.jpg", "--max-note-bytes", "4096"],
    );
    assert_eq!(code, 1, "the merge is refused: {stdout}\n{stderr}");
    assert!(
        stderr.contains("larger than the 4096-byte note limit"),
        "the refusal names the limit: {stderr}"
    );

    assert_eq!(
        bytes_of(&dir.0, "keep.jpg.md"),
        keep_bytes,
        "the oversized note is untouched"
    );
    assert!(
        exists(&dir.0, "drop.jpg") && exists(&dir.0, "drop.jpg.md"),
        "the drop group is still in place"
    );
    assert!(!exists(&dir.0, ".trash"), "nothing was trashed");
}

#[test]
fn dry_run_plans_without_touching_a_single_byte() {
    let dir = tmp("dry-run");
    write(&dir.0, "keep.jpg", jpeg(16, 16));
    write(
        &dir.0,
        "keep.jpg.md",
        "---\ntags: [a]\n---\n\n![[keep.jpg]]\n",
    );
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(
        &dir.0,
        "drop.jpg.md",
        "---\ntags: [b]\n---\n\n![[drop.jpg]]\n",
    );
    write(
        &dir.0,
        "shelf.md",
        "---\nkind: collection\n---\n\n![[drop.jpg]]\n",
    );

    let keep_before = bytes_of(&dir.0, "keep.jpg.md");
    let stdout = ok(
        &dir.0,
        &["merge", "keep.jpg", "drop.jpg", "--dry-run", "--json"],
    );
    let doc = json_in(&stdout);
    assert_eq!(doc["dry_run"], true);
    assert!(doc["op"].is_null(), "a dry run makes no operation");
    assert_eq!(doc["links_repointed"], 1);

    // Nothing moved: no trash, both images and notes still in place, notes intact.
    assert!(!exists(&dir.0, ".trash"), "no trash was created");
    assert!(exists(&dir.0, "drop.jpg") && exists(&dir.0, "drop.jpg.md"));
    assert_eq!(
        bytes_of(&dir.0, "keep.jpg.md"),
        keep_before,
        "keep note unchanged"
    );
    assert!(
        read(&dir.0, "shelf.md").contains("drop.jpg"),
        "links untouched"
    );
}

#[test]
fn undo_hands_both_images_back_and_restores_the_kept_note() {
    let dir = tmp("undo");
    write(&dir.0, "keep.jpg", jpeg(16, 16));
    let keep_note_before = "---\ntags: [a]\n---\n\n![[keep.jpg]]\n";
    write(&dir.0, "keep.jpg.md", keep_note_before);
    write(&dir.0, "drop.jpg", jpeg(8, 8));
    write(
        &dir.0,
        "drop.jpg.md",
        "---\ntags: [b]\n---\n\n![[drop.jpg]]\n",
    );

    let stdout = ok(&dir.0, &["merge", "keep.jpg", "drop.jpg", "--json"]);
    let op = json_in(&stdout)["op"].as_str().unwrap().to_string();
    assert!(
        !exists(&dir.0, "drop.jpg"),
        "the drop is in the trash first"
    );

    ok(&dir.0, &["undo", &op]);

    assert!(exists(&dir.0, "drop.jpg"), "the drop group is restored");
    assert!(exists(&dir.0, "drop.jpg.md"));
    assert_eq!(
        read(&dir.0, "keep.jpg.md"),
        keep_note_before,
        "the kept note is back to its bytes before the merge"
    );
    assert!(
        !exists(&dir.0, ".trash/keep.jpg"),
        "nothing of the keep is left in trash"
    );
}

#[test]
fn default_keeps_what_the_caller_named_in_any_order() {
    // Without --auto the first argument is the keeper, so arg order decides.
    let big_first = tmp("default-a");
    write(&big_first.0, "big.jpg", jpeg(16, 16));
    write(&big_first.0, "small.jpg", jpeg(4, 4));
    let doc = json_in(&ok(
        &big_first.0,
        &["merge", "big.jpg", "small.jpg", "--json"],
    ));
    assert_eq!(doc["keep"], "big.jpg");

    let small_first = tmp("default-b");
    write(&small_first.0, "big.jpg", jpeg(16, 16));
    write(&small_first.0, "small.jpg", jpeg(4, 4));
    let doc = json_in(&ok(
        &small_first.0,
        &["merge", "small.jpg", "big.jpg", "--json"],
    ));
    assert_eq!(doc["keep"], "small.jpg", "no auto ranking without --auto");
}

#[test]
fn auto_keeps_the_image_with_more_pixels_whatever_the_order() {
    // The 8×8 JPEG has more pixels than the 4×4 PNG, so it wins even when the
    // caller names the small one first.
    let dir = tmp("auto-pixels");
    write(&dir.0, "small.png", png(4, 4));
    write(&dir.0, "big.jpg", jpeg(8, 8));

    let doc = json_in(&ok(
        &dir.0,
        &["merge", "small.png", "big.jpg", "--auto", "--json"],
    ));
    assert_eq!(doc["keep"], "big.jpg", "more pixels wins");
    assert_eq!(doc["drop"], "small.png");
    assert!(exists(&dir.0, "big.jpg") && exists(&dir.0, ".trash/small.png"));
}

#[test]
fn auto_prefers_a_lossless_image_at_equal_pixels() {
    // Same pixel count (8×8); the lossless PNG beats the lossy JPEG.
    let dir = tmp("auto-lossless");
    write(&dir.0, "shot.jpg", jpeg(8, 8));
    write(&dir.0, "shot2.png", png(8, 8));

    let doc = json_in(&ok(
        &dir.0,
        &["merge", "shot.jpg", "shot2.png", "--auto", "--json"],
    ));
    assert_eq!(
        doc["keep"], "shot2.png",
        "lossless beats lossy at equal pixels"
    );
}

#[test]
fn merging_an_image_with_itself_is_refused() {
    let dir = tmp("same");
    write(&dir.0, "one.png", png(8, 8));
    let (code, _, stderr) = dimagine(&dir.0, &["merge", "one.png", "one.png"]);
    assert_eq!(code, 1, "keep and drop are one image");
    assert!(stderr.contains("same image"), "{stderr:?}");
    assert!(!exists(&dir.0, ".trash"), "nothing moved");
}

#[test]
fn merge_names_an_image_it_cannot_find() {
    let dir = tmp("missing");
    write(&dir.0, "one.png", png(8, 8));
    let (code, _, stderr) = dimagine(&dir.0, &["merge", "one.png", "nope.jpg"]);
    assert_eq!(code, 1);
    assert!(!stderr.is_empty(), "the reason is reported");
    assert!(exists(&dir.0, "one.png"), "the good image is left alone");
}
