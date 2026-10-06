//! End-to-end: `dimagine scan` populates the index, and the view queries
//! answer against it (B3).

use std::path::{Path, PathBuf};
use std::process::Command;

use dimagine_index::{Index, SortKey, ViewQuery};

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
        "dimagine-cli-scan-index-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create scan-index test dir");
    TempDir(dir)
}

fn dimagine(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(BIN).args(args).output().expect("run dimagine");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn write_note(dir: &Path, rel: &str, front_matter: &str, embed: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("---\n{front_matter}---\n\n![[{embed}]]]\n")).unwrap();
}

fn ns(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap()
        .timestamp()
        * 1_000_000_000
}

fn build_library(root: &Path) {
    std::fs::create_dir_all(root.join("photos")).unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();
    std::fs::create_dir_all(root.join("inbox")).unwrap();
    std::fs::create_dir_all(root.join("collections")).unwrap();
    for name in [
        "photos/img1.jpg",
        "photos/img2.jpg",
        "other/img3.jpg",
        "inbox/img4.jpg",
    ] {
        std::fs::write(root.join(name), b"jpeg").unwrap();
    }
    write_note(
        root,
        "photos/img1.jpg.md",
        "title: Image One\nrating: 4\ntags: [nature, green]\nadded: 2023-07-12T20:54:07+01:00\nimported: 2026-10-04T14:30:12+01:00\n",
        "img1.jpg",
    );
    write_note(
        root,
        "photos/img2.jpg.md",
        "title: Image Two\nrating: 2\ntags: [nature]\nimported: 2026-10-04T14:30:12+01:00\n",
        "img2.jpg",
    );
    write_note(
        root,
        "other/img3.jpg.md",
        "title: Image Three\n",
        "img3.jpg",
    );
    // An orphan note: its image is gone, so it is in no list and must not be in
    // the sidebar counts either (RW26 L-6).
    std::fs::create_dir_all(root.join("refs2")).unwrap();
    std::fs::write(
        root.join("refs2/ghost.jpg.md"),
        "---\ntitle: Ghost\ntags: [nature]\n---\n\nGone.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("collections/album.md"),
        "---\nkind: collection\ntitle: Album\n---\n\n![[photos/img1.jpg]]\n\n![[photos/img2.jpg]]\n",
    )
    .unwrap();
}

fn open_index(root: &Path) -> Index {
    Index::open(root).expect("index opens")
}

#[test]
fn scan_populates_the_index_and_view_queries_answer() {
    let tmp = tmp("scan-index");
    let root = tmp.0.join("library");
    build_library(&root);
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let index = open_index(&root);
    assert!(!index.rebuild_required());

    let all = index
        .view(&ViewQuery {
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(all.total, 4, "four images indexed");
    // Default sort is Added descending: the two first_seen images (now) are
    // newest, then img2 (imported 2026-10-04), then img1 (added 2023).
    let paths: Vec<&str> = all.items.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "other/img3.jpg",
            "inbox/img4.jpg",
            "photos/img2.jpg",
            "photos/img1.jpg"
        ]
    );

    let photos = index
        .view(&ViewQuery {
            folder: Some("photos".into()),
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(photos.total, 2);
    let photo_paths: Vec<&str> = photos.items.iter().map(|item| item.path.as_str()).collect();
    // Default sort is Added descending: img2's added falls back to its
    // imported (2026-10-04), newer than img1's explicit added
    // (2023-07-12).
    assert_eq!(photo_paths, ["photos/img2.jpg", "photos/img1.jpg"]);

    let non_recursive = index
        .view(&ViewQuery {
            folder: Some("photos".into()),
            recursive: false,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(non_recursive.total, 2);

    let root_only = index
        .view(&ViewQuery {
            folder: Some("".into()),
            recursive: false,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    // Every image lives in a subfolder, so nothing sits directly in
    // the library root.
    assert_eq!(root_only.total, 0);

    let by_added = index
        .view(&ViewQuery {
            folder: Some("photos".into()),
            sort: SortKey::Added,
            descending: true,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    let added_paths: Vec<&str> = by_added
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    // The fallback chain: img2 has no `added`, so its added_ns comes
    // from `imported` (2026-10-04); img1's explicit `added`
    // (2023-07-12) wins over its own newer `imported`, so it sorts
    // older. The no-note images in the global query above fall back
    // to first_seen_ns.
    assert_eq!(added_paths, ["photos/img2.jpg", "photos/img1.jpg"]);
    assert_eq!(
        by_added.items[0].added_ns,
        ns("2026-10-04T14:30:12+01:00"),
        "imported fallback"
    );
    assert_eq!(
        by_added.items[1].added_ns,
        ns("2023-07-12T20:54:07+01:00"),
        "added wins over imported"
    );
    assert_eq!(by_added.items[1].title.as_deref(), Some("Image One"));
    assert_eq!(by_added.items[1].rating, Some(4));
    assert_eq!(
        by_added.items[1].note_path.as_deref(),
        Some("photos/img1.jpg.md")
    );

    let by_name = index
        .view(&ViewQuery {
            sort: SortKey::Name,
            descending: false,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    let name_paths: Vec<&str> = by_name
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    // Name is the file name, case-insensitive: img1 < img2 < img3 < img4.
    assert_eq!(
        name_paths,
        [
            "photos/img1.jpg",
            "photos/img2.jpg",
            "other/img3.jpg",
            "inbox/img4.jpg"
        ]
    );

    let by_rating = index
        .view(&ViewQuery {
            sort: SortKey::Rating,
            descending: true,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    let rating_paths: Vec<&str> = by_rating
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(rating_paths[0], "photos/img1.jpg", "rating 4 first");
    assert_eq!(rating_paths[1], "photos/img2.jpg", "rating 2 second");
    // img3 and img4 have no rating: both sort last, tied, and the
    // tie is broken by path ascending (inbox before other).
    assert_eq!(rating_paths[2], "inbox/img4.jpg", "missing rating last");
    assert_eq!(rating_paths[3], "other/img3.jpg", "missing rating last");

    let nature = index
        .view(&ViewQuery {
            tags: vec!["nature".into()],
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(nature.total, 2, "both nature images");

    let green = index
        .view(&ViewQuery {
            tags: vec!["green".into()],
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(green.total, 1);
    assert_eq!(green.items[0].path, "photos/img1.jpg");

    let multi = index
        .view(&ViewQuery {
            tags: vec!["nature".into(), "green".into()],
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(multi.total, 1, "tags are ANDed");

    let folders = index.folder_counts().unwrap();
    let mut folder_map: std::collections::HashMap<&str, u64> =
        folders.iter().map(|(f, c)| (f.as_str(), *c)).collect();
    assert_eq!(folder_map.remove(""), Some(4));
    assert_eq!(folder_map.remove("photos"), Some(2));
    assert_eq!(folder_map.remove("other"), Some(1));
    assert_eq!(folder_map.remove("inbox"), Some(1));
    assert!(folder_map.is_empty(), "unexpected folders: {folder_map:?}");

    let tags = index.tag_counts().unwrap();
    let mut tag_map: std::collections::HashMap<&str, u64> =
        tags.iter().map(|(t, c)| (t.as_str(), *c)).collect();
    assert_eq!(tag_map.remove("nature"), Some(2));
    assert_eq!(tag_map.remove("green"), Some(1));
    assert!(
        tag_map.is_empty(),
        "an orphan note must not be counted: {tag_map:?}"
    );
    assert_eq!(nature.total, 2, "the sidebar count and the list agree");

    let collections = index.collections().unwrap();
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0].note_path, "collections/album.md");
    assert_eq!(collections[0].title, "Album");
    assert_eq!(collections[0].member_count, 2);

    let appears = index.appears_in("photos/img1.jpg").unwrap();
    assert_eq!(appears, ["collections/album.md"]);
    let appears2 = index.appears_in("inbox/img4.jpg").unwrap();
    assert!(appears2.is_empty());

    let collection_view = index
        .view(&ViewQuery {
            collection: Some("collections/album.md".into()),
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(collection_view.total, 2);
    let member_paths: Vec<&str> = collection_view
        .items
        .iter()
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(
        member_paths,
        ["photos/img1.jpg", "photos/img2.jpg"],
        "collection order"
    );

    let paged = index
        .view(&ViewQuery {
            sort: SortKey::Name,
            descending: false,
            offset: 1,
            limit: 2,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(paged.total, 4, "total counts before paging");
    let paged_paths: Vec<&str> = paged.items.iter().map(|item| item.path.as_str()).collect();
    // Name ascending: img1, img2, img3, img4; page 2 of 2.
    assert_eq!(paged_paths, ["photos/img2.jpg", "other/img3.jpg"]);
}

#[test]
fn scan_reports_a_corrupt_index_as_rebuild_required() {
    let tmp = tmp("scan-corrupt");
    let root = tmp.0.join("library");
    build_library(&root);
    let db = root.join(".dimagine/cache/index.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"not a sqlite database").unwrap();
    let (code, _stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "scan still succeeds: {stderr}");
    assert!(stderr.contains("index not refreshed"), "{stderr}");
    let index = open_index(&root);
    assert!(index.rebuild_required());
}

/// RW26 H-1: `first_seen_ns` must survive a move detected by a real
/// `dimagine scan`. The scan never hashes, so the pairing has to come from the
/// facts it already has.
#[test]
fn scan_keeps_the_added_position_across_a_move() {
    let tmp = tmp("scan-move");
    let root = tmp.0.join("library");
    let refs = root.join("refs");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("a.png"), b"png-bytes-for-the-move").unwrap();
    // No `added`, no `imported` and no `id`: the added position of this image
    // is its first_seen_ns, so a move that resets it is visible in `view`.
    std::fs::write(
        refs.join("a.png.md"),
        "---\ntitle: Movable\n---\n\nBody.\n\n![[a.png]]\n",
    )
    .unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let before = view_added(&root, "refs/a.png");

    // Long enough that a fresh first_seen_ns could never equal the old one.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::rename(refs.join("a.png"), refs.join("b.png")).unwrap();
    std::fs::rename(refs.join("a.png.md"), refs.join("b.png.md")).unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let after = view_added(&root, "refs/b.png");
    assert_eq!(after, before, "a move keeps the image's added position");
    assert!(
        view_added(&root, "refs/a.png").is_none(),
        "the old path is gone"
    );
}

/// Runs `cp` with the reviewer's own arguments, so the copy gets the mtime
/// `cp` gives it and not one this test invented: `std::fs::copy` preserves the
/// source mtime on macOS, which is `cp -p`, not `cp`.
fn cp(args: &[&std::ffi::OsStr]) {
    let status = Command::new("cp").args(args).status().expect("run cp");
    assert!(status.success(), "cp {args:?} failed");
}

/// RW26 H-1: reviewer workflow (a) — `cp` the image and its note, then rename
/// the original pair, all before one scan. The duplicate note carries the same
/// `id`, so the id pairs nothing; size and mtime still name the renamed original
/// alone, so the move is detected and the copy is dated as the new file it is.
#[test]
fn scan_copy_image_and_note_then_rename_original_does_not_transfer_to_copy() {
    let tmp = tmp("scan-copy-rename");
    let root = tmp.0.join("library");
    let refs = root.join("refs");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("a.png"), b"png-bytes-for-a").unwrap();
    std::fs::write(
        refs.join("a.png.md"),
        "---\nid: 01HZX000000000000000000001\ntitle: Original\n---\n\n![[a.png]]\n",
    )
    .unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let before_a = view_added(&root, "refs/a.png").expect("a.png is indexed");

    // Long enough that a fresh first_seen_ns could never equal the old one.
    std::thread::sleep(std::time::Duration::from_millis(20));
    cp(&[
        refs.join("a.png").as_os_str(),
        refs.join("c.png").as_os_str(),
    ]);
    cp(&[
        refs.join("a.png.md").as_os_str(),
        refs.join("c.png.md").as_os_str(),
    ]);
    std::fs::rename(refs.join("a.png"), refs.join("b.png")).unwrap();
    std::fs::rename(refs.join("a.png.md"), refs.join("b.png.md")).unwrap();

    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let after_b = view_added(&root, "refs/b.png").expect("b.png is indexed");
    let after_c = view_added(&root, "refs/c.png").expect("c.png is indexed");

    assert_eq!(
        after_b, before_a,
        "the renamed original keeps the added position of the file it replaces"
    );
    assert_ne!(
        after_c, before_a,
        "the copy is a new file and never inherits the original's added position"
    );
}

/// RW26 H-1: reviewer workflow (b) — `cp -p` beside a move, in one scan. The
/// copy and the moved file both carry the vanished file's size and mtime, so
/// neither of them is named alone and neither inherits.
#[test]
fn scan_preserving_copy_beside_a_move_transfers_to_neither() {
    let tmp = tmp("scan-cp-p-move");
    let root = tmp.0.join("library");
    let refs = root.join("refs");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("x.png"), b"png-bytes-for-x").unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let before_x = view_added(&root, "refs/x.png").expect("x.png is indexed");

    std::thread::sleep(std::time::Duration::from_millis(20));
    cp(&[
        std::ffi::OsStr::new("-p"),
        refs.join("x.png").as_os_str(),
        refs.join("xcopy.png").as_os_str(),
    ]);
    std::fs::rename(refs.join("x.png"), refs.join("y.png")).unwrap();

    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let after_xcopy = view_added(&root, "refs/xcopy.png").expect("xcopy.png is indexed");
    let after_y = view_added(&root, "refs/y.png").expect("y.png is indexed");

    assert_ne!(
        after_xcopy, before_x,
        "the preserving copy does not inherit an ambiguous first_seen_ns"
    );
    assert_ne!(
        after_y, before_x,
        "an ambiguous pairing transfers to neither side, the move included"
    );
}

fn view_added(root: &Path, path: &str) -> Option<i64> {
    open_index(root)
        .view(&ViewQuery {
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.path == path)
        .map(|item| item.added_ns)
}

/// RW26 M-4: a collection is a note that embeds images (FORMAT §5), or one
/// that says `kind: collection`; an image note's own self-embed is a preview,
/// not a membership (FORMAT §3.2). The view queries used to take `kind:
/// collection` as the only signal, so they disagreed with `dimagine serve`.
#[test]
fn collections_follow_format_section_5_not_just_the_kind_property() {
    let tmp = tmp("scan-collections");
    let root = tmp.0.join("library");
    for image in ["top.jpg", "refs/deep/d.jpg", "refs/deep/lonely.jpg"] {
        let path = root.join(image);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"jpeg").unwrap();
    }
    // A collection by its embeds, with no `kind` at all.
    std::fs::create_dir_all(root.join("collections")).unwrap();
    std::fs::write(
        root.join("collections/plain.md"),
        "---\ntitle: Plain\n---\n\n![[refs/deep/d.jpg]]\n\n![[top.jpg]]\n",
    )
    .unwrap();
    // A broken embed still means the note means to collect images.
    std::fs::write(
        root.join("collections/broken.md"),
        "---\ntitle: Broken\n---\n\n![[nowhere.jpg]]\n",
    )
    .unwrap();
    // An image note that calls itself a collection is not one: its only embed
    // is its own image, and that is a preview (FORMAT §3.2).
    write_note(
        &root,
        "refs/deep/d.jpg.md",
        "title: Deep\nkind: collection\n",
        "d.jpg",
    );
    // A plain wikilink to an image is not an embed, so this note is not a
    // collection either.
    std::fs::write(
        root.join("collections/linked.md"),
        "---\ntitle: Linked\n---\n\n[[top.jpg]]\n",
    )
    .unwrap();

    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let index = open_index(&root);

    let listed: Vec<(String, u64)> = index
        .collections()
        .unwrap()
        .into_iter()
        .map(|info| (info.note_path, info.member_count))
        .collect();
    assert_eq!(
        listed,
        [
            ("collections/broken.md".to_owned(), 0),
            ("collections/plain.md".to_owned(), 2),
            // `kind: collection` still lists an image note (FORMAT §5: the
            // property exists so tools list it), but its own image is not one
            // of its members, so it has none.
            ("refs/deep/d.jpg.md".to_owned(), 0),
        ],
        "embeds make a collection; a self-embed or a wikilink does not"
    );

    assert_eq!(
        index.appears_in("top.jpg").unwrap(),
        ["collections/plain.md"],
        "the viewer must agree that top.jpg is in a collection"
    );
    let appears_in_own_note = index.appears_in("refs/deep/d.jpg").unwrap();
    assert_eq!(
        appears_in_own_note,
        ["collections/plain.md"],
        "the collection that really embeds it"
    );
    assert!(
        !appears_in_own_note.contains(&"refs/deep/d.jpg.md".to_owned()),
        "an image is never a member of its own note"
    );

    let page = index
        .view(&ViewQuery {
            collection: Some("collections/plain.md".into()),
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(page.total, 2, "the collection's own embed order");

    let self_embed = index
        .view(&ViewQuery {
            collection: Some("refs/deep/d.jpg.md".into()),
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(
        self_embed.total, 0,
        "an image is not a member of its own note"
    );
}

/// W27f: the index API stays backward compatible — `collections()` still names
/// every collection, including an image note that embeds its siblings — but
/// each row now records whether the collection list carries it
/// (`dimagine_index::collection_is_listed`, FORMAT §3.2/§5), while `appears_in`
/// and the `collection` view keep answering for the unlisted ones.
#[test]
fn scan_keeps_image_note_collections_but_marks_them_off_the_list() {
    let tmp = tmp("scan-listed-collections");
    let root = tmp.0.join("library");
    for image in ["refs/page-01.png", "refs/page-02.png"] {
        let path = root.join(image);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"png").unwrap();
    }
    // The image note: a sibling embed and its own preview (FORMAT §3.2), which
    // makes it a collection the list leaves off.
    std::fs::write(
        root.join("refs/page-01.png.md"),
        "---\ntitle: Page one\n---\n\n![[page-02.png]]\n\n![[page-01.png]]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("guide.md"),
        "---\nkind: collection\ntitle: Guide\n---\nPlanned shots go here.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("roundup.md"),
        "---\ntitle: Roundup\n---\n\n![[refs/page-01.png]]\n",
    )
    .unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let index = open_index(&root);

    let seen: Vec<(String, bool)> = index
        .collections()
        .unwrap()
        .into_iter()
        .map(|info| (info.note_path, info.listed))
        .collect();
    assert_eq!(
        seen,
        [
            ("guide.md".to_owned(), true),
            ("refs/page-01.png.md".to_owned(), false),
            ("roundup.md".to_owned(), true),
        ],
        "still a collection, but the list leaves it off"
    );

    assert_eq!(
        index.appears_in("refs/page-02.png").unwrap(),
        ["refs/page-01.png.md"],
        "appears in still names the image note"
    );
    let members = index
        .view(&ViewQuery {
            collection: Some("refs/page-01.png.md".into()),
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    assert_eq!(members.total, 1, "the sibling, never its own image");
}

/// RW26 L-3: a derived `added_ns` could never be cleared, so removing the
/// `added:` line from a note left the old value in the index and only a
/// rebuild fixed it. The fallback chain of FORMAT §3.1 says it must return to
/// `first_seen`.
#[test]
fn removing_the_added_property_sends_the_image_back_to_first_seen() {
    let tmp = tmp("scan-clear-added");
    let root = tmp.0.join("library");
    std::fs::create_dir_all(root.join("refs")).unwrap();
    std::fs::write(root.join("refs/a.png"), b"png").unwrap();
    let with_added =
        "---\ntitle: A\nadded: 2020-01-01T00:00:00+00:00\n---\n\nBody.\n\n![[a.png]]\n";
    std::fs::write(root.join("refs/a.png.md"), with_added).unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        view_added(&root, "refs/a.png"),
        Some(ns("2020-01-01T00:00:00+00:00"))
    );

    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        root.join("refs/a.png.md"),
        "---\ntitle: A\n---\n\nBody.\n\n![[a.png]]\n",
    )
    .unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let cleared = view_added(&root, "refs/a.png").expect("the image is still indexed");
    assert!(
        cleared > ns("2020-01-01T00:00:00+00:00"),
        "the removed added must not survive the rescan: {cleared}"
    );
}

/// RW26 L-5: the index refresh failure was only reported when the library had
/// been read completely, so an incomplete read left the index silently stale
/// and the viewer served pre-truncation results with no hint anywhere.
#[test]
fn scan_reports_an_index_failure_even_when_the_read_was_incomplete() {
    let tmp = tmp("scan-index-failure");
    let root = tmp.0.join("library");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.png"), b"jpeg").unwrap();
    // Not valid UTF-8: scan reports the note as unreadable and exits 3, and
    // the index sync cannot read the note either.
    std::fs::write(root.join("a.png.md"), b"---\ntitle: \xff\xfe\n---\n").unwrap();
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("index not refreshed"), "{stderr}");
}

/// RW26 L-7: `rating: 7` (or 300) was read as a rating and sorted above 5,
/// while `rating: "4"` and `rating: 4.5` were silently read as no rating.
/// FORMAT §3.1 defines rating as an integer 0-5, so anything else is no rating.
#[test]
fn a_rating_outside_the_range_or_not_an_integer_is_no_rating() {
    let tmp = tmp("scan-rating");
    let root = tmp.0.join("library");
    std::fs::create_dir_all(&root).unwrap();
    for (name, rating) in [
        ("a-five.jpg", "rating: 5"),
        ("b-seven.jpg", "rating: 7"),
        ("c-three.jpg", "rating: 3"),
        ("d-zero.jpg", "rating: 0"),
        ("e-fraction.jpg", "rating: 4.5"),
        ("f-quoted.jpg", "rating: \"4\""),
        ("g-huge.jpg", "rating: 300"),
    ] {
        std::fs::write(root.join(name), b"jpeg").unwrap();
        write_note(
            &root,
            &format!("{name}.md"),
            &format!("title: {name}\n{rating}\n"),
            name,
        );
    }
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");

    let index = open_index(&root);
    let page = index
        .view(&ViewQuery {
            sort: SortKey::Rating,
            descending: true,
            limit: 100,
            ..ViewQuery::default()
        })
        .unwrap();
    let ratings: Vec<(String, Option<u8>)> = page
        .items
        .iter()
        .map(|item| (item.path.clone(), item.rating))
        .collect();
    assert_eq!(
        ratings,
        [
            ("a-five.jpg".to_owned(), Some(5)),
            ("c-three.jpg".to_owned(), Some(3)),
            ("d-zero.jpg".to_owned(), Some(0)),
            // Everything else reads as no rating, so it sorts last, tied, and
            // the tie breaks by path ascending.
            ("b-seven.jpg".to_owned(), None),
            ("e-fraction.jpg".to_owned(), None),
            ("f-quoted.jpg".to_owned(), None),
            ("g-huge.jpg".to_owned(), None),
        ]
    );
}

/// RW26 M-5: filtering, sorting and paging moved into SQL, so the folder
/// prefix trap (`refs` must not reach `refs2` or `REFSEA`), the recursive and
/// non-recursive split, the case-insensitive name sort and the path tie-break
/// are pinned here rather than in a unit test of removed helpers.
#[test]
fn folder_filters_name_sort_and_ties_survive_the_move_into_sql() {
    let tmp = tmp("scan-sql-filters");
    let root = tmp.0.join("library");
    for image in [
        "root.jpg",
        "refs/a.jpg",
        "refs/deep/sea/b.jpg",
        "refs2/c.jpg",
        "REFSEA/D.jpg",
    ] {
        let path = root.join(image);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Every image the same size, so every sort ties on size.
        std::fs::write(path, b"jpeg").unwrap();
    }
    let (code, stdout, stderr) = dimagine(&["scan", "--library", root.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let index = open_index(&root);
    let paths = |query: ViewQuery| -> Vec<String> {
        index
            .view(&ViewQuery {
                limit: 100,
                ..query
            })
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.path)
            .collect()
    };

    assert_eq!(
        paths(ViewQuery {
            folder: Some("refs".into()),
            // Sorted by name so the expectation does not depend on scan order.
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["refs/a.jpg", "refs/deep/sea/b.jpg"],
        "recursive, and no refs2, no REFSEA"
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some("refs".into()),
            recursive: false,
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["refs/a.jpg"]
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some("refs2".into()),
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["refs2/c.jpg"]
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some("refs/deep".into()),
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["refs/deep/sea/b.jpg"],
        "a folder with no image of its own still counts its subfolder"
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some("refs/deep/sea".into()),
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["refs/deep/sea/b.jpg"]
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some(String::new()),
            recursive: false,
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        ["root.jpg"],
        "the library root, non-recursive"
    );
    assert_eq!(
        paths(ViewQuery {
            folder: Some(String::new()),
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        })
        .len(),
        5,
        "the library root, recursive, is everything"
    );

    assert_eq!(
        paths(ViewQuery {
            sort: SortKey::Size,
            descending: true,
            ..ViewQuery::default()
        }),
        [
            "REFSEA/D.jpg",
            "refs/a.jpg",
            "refs/deep/sea/b.jpg",
            "refs2/c.jpg",
            "root.jpg",
        ],
        "equal sizes tie, and ties break by path ascending"
    );
    assert_eq!(
        paths(ViewQuery {
            sort: SortKey::Name,
            descending: false,
            ..ViewQuery::default()
        }),
        [
            "refs/a.jpg",
            "refs/deep/sea/b.jpg",
            "refs2/c.jpg",
            "REFSEA/D.jpg",
            "root.jpg",
        ],
        "the name sort is the file name, case-insensitively"
    );

    let folders: Vec<(String, u64)> = index.folder_counts().unwrap();
    assert_eq!(
        folders,
        [
            ("".to_owned(), 5),
            ("REFSEA".to_owned(), 1),
            ("refs".to_owned(), 2),
            ("refs/deep".to_owned(), 1),
            ("refs/deep/sea".to_owned(), 1),
            ("refs2".to_owned(), 1),
        ],
        "folder counts are grouped in SQL, subfolders included"
    );
}

/// Benchmark for the report: build a library with
/// `python3 scripts/dev/gen-library.py <dir> --images 20000 --seed 1401
/// --notes-ratio 0.2 --collections 10 --depth 4`, run `dimagine scan` on it,
/// then run this test with `DIMAGINE_BENCH_LIBRARY` pointing at it. It is
/// ignored so `cargo test` stays fast.
///
///     cargo test --release -p dimagine --test cli_scan_index -- \
///         --ignored --nocapture bench_view_queries
#[test]
#[ignore = "bench: needs a generated library and DIMAGINE_BENCH_LIBRARY"]
fn bench_view_queries() {
    let root = std::env::var("DIMAGINE_BENCH_LIBRARY")
        .expect("DIMAGINE_BENCH_LIBRARY points at a scanned library");
    let index = Index::open(&root).expect("index opens");
    assert!(!index.rebuild_required());
    let images: u64 = {
        let page = index
            .view(&ViewQuery {
                limit: 1,
                ..ViewQuery::default()
            })
            .unwrap();
        page.total
    };
    eprintln!("indexed images: {images}");
    fn timed(name: &str, mut query: impl FnMut() -> u64) {
        // Three runs, the fastest reported: the first one pays for the page
        // cache, which is not what this measures.
        let mut best = std::time::Duration::MAX;
        let mut rows = 0;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            rows = query();
            best = best.min(started.elapsed());
        }
        eprintln!("{name}: {best:?} ({rows} rows)");
    }
    let queries: [(&str, ViewQuery); 6] = [
        ("all, added desc", ViewQuery::default()),
        (
            "all, name asc, offset 500",
            ViewQuery {
                sort: SortKey::Name,
                descending: false,
                offset: 500,
                ..ViewQuery::default()
            },
        ),
        (
            "folder-01 recursive, rating asc",
            ViewQuery {
                folder: Some("folder-01".into()),
                sort: SortKey::Rating,
                descending: false,
                ..ViewQuery::default()
            },
        ),
        (
            "tags [synthetic, benchmark]",
            ViewQuery {
                tags: vec!["synthetic".into(), "benchmark".into()],
                ..ViewQuery::default()
            },
        ),
        (
            "text \"synthetic\" (FTS)",
            ViewQuery {
                text: Some("synthetic".into()),
                ..ViewQuery::default()
            },
        ),
        (
            "text \"wa\" (LIKE, under three characters)",
            ViewQuery {
                text: Some("wa".into()),
                ..ViewQuery::default()
            },
        ),
    ];
    for (name, query) in queries {
        let mut rows = 0u64;
        // Three runs, the fastest reported: the first one pays for the page
        // cache, which is not what this measures.
        let mut best = std::time::Duration::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            rows = index.view(&query).unwrap().items.len() as u64;
            best = best.min(started.elapsed());
        }
        eprintln!("view {name}: {best:?} ({rows} rows)");
        for line in index.view_query_plan(&query).unwrap() {
            eprintln!("plan {name}: {line}");
        }
    }
    timed("folder_counts", || {
        index.folder_counts().unwrap().len() as u64
    });
    timed("tag_counts", || index.tag_counts().unwrap().len() as u64);
    timed("collections", || index.collections().unwrap().len() as u64);
    timed("appears_in", || {
        index
            .appears_in("folder-01/image-000001.png")
            .unwrap()
            .len() as u64
    });
}
