//! The walker: ignore rules, symlinks, classification, unreadable model
//! (FORMAT §2.2).

mod support;

#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use dimagine_core::library::{FileClass, FileClass as C, NotAFolder};
use support::*;

fn classes(lib: &dimagine_core::Library) -> Vec<(String, FileClass)> {
    lib.files.iter().map(|f| (f.rel.clone(), f.class)).collect()
}

#[test]
fn opens_an_untouched_folder_of_images() {
    let tmp = tmp("plain");
    write(tmp.path(), "a.jpg", jpg());
    write(tmp.path(), "b.png", png());
    let lib = open_library(tmp.path());
    assert_eq!(
        classes(&lib),
        vec![("a.jpg".into(), C::Image), ("b.png".into(), C::Image),]
    );
    assert!(lib.fully_read());
    assert_eq!(lib.ignored.total, 0);
}

#[test]
fn missing_or_file_root_is_not_a_library() {
    let tmp = tmp("noroot");
    write(tmp.path(), "file.txt", b"");
    assert!(matches!(
        dimagine_core::Library::open(&tmp.path().join("nope")),
        Err(NotAFolder(_))
    ));
    assert!(matches!(
        dimagine_core::Library::open(&tmp.path().join("file.txt")),
        Err(NotAFolder(_))
    ));
}

#[test]
fn ignores_every_format_2_2_entry() {
    let tmp = tmp("ignore");
    write(tmp.path(), "girl.jpg", jpg());
    write(tmp.path(), ".DS_Store", b"junk");
    write(tmp.path(), "._girl.jpg", b"junk");
    write(tmp.path(), "Thumbs.db", b"junk");
    write(tmp.path(), "desktop.ini", b"junk");
    write(tmp.path(), ".hidden/child.png", png());
    write(tmp.path(), ".dimagine/cache/index", b"junk");
    write(tmp.path(), "real/deep/girl.png", png());
    let lib = open_library(tmp.path());
    // Hidden folders are not walked into, so only real files are listed.
    assert_eq!(
        classes(&lib),
        vec![
            ("girl.jpg".into(), C::Image),
            ("real/deep/girl.png".into(), C::Image),
        ]
    );
    assert_eq!(lib.ignored.total, 6);
    // Hidden entries counted: .DS_Store, .hidden/ and .dimagine/ (the dirs
    // count once; their children are never seen at all).
    assert_eq!(lib.ignored.hidden, 3);
    assert_eq!(lib.ignored.resource_fork, 1);
    assert_eq!(lib.ignored.os_metadata, 2);
}

#[test]
#[cfg(unix)]
fn symlinks_are_never_followed() {
    let tmp = tmp("symlink");
    write(tmp.path(), "real.jpg", jpg());
    write(tmp.path(), "elsewhere/other.png", png());
    // A symlink to a file inside the library.
    symlink(tmp.path().join("real.jpg"), tmp.path().join("alias.jpg")).unwrap();
    // A symlink to a folder inside the library: not descended into.
    symlink(tmp.path().join("elsewhere"), tmp.path().join("linkdir")).unwrap();
    // A symlink that leaves the library: not followed either.
    symlink(std::env::temp_dir(), tmp.path().join("outside")).unwrap();
    let lib = open_library(tmp.path());
    assert_eq!(
        classes(&lib),
        vec![
            ("elsewhere/other.png".into(), C::Image),
            ("real.jpg".into(), C::Image),
        ]
    );
    assert_eq!(lib.ignored.symlink, 3);
    assert!(lib.ignored.total >= 3);
}

#[test]
#[cfg(unix)]
fn unreadable_directory_makes_the_walk_incomplete() {
    let tmp = tmp("unreadable-dir");
    write(tmp.path(), "a.png", png());
    write(tmp.path(), "locked/b.png", png());
    let locked = tmp.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let lib = open_library(tmp.path());
    assert!(!lib.fully_read());
    assert_eq!(lib.unreadable_dirs.len(), 1);
    assert_eq!(lib.unreadable_dirs[0].path, "locked");
    assert!(
        !lib.unreadable_dirs[0].reason.is_empty(),
        "the reason is real, never empty"
    );
    // Restore so TempDir's drop can clean up.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn walks_are_deterministic() {
    let tmp = tmp("order");
    for name in ["z.png", "a.jpg", "m/canvas.canvas", "m/a.md"] {
        write(
            tmp.path(),
            name,
            if name.ends_with(".jpg") { jpg() } else { png() },
        );
    }
    let first = open_library(tmp.path());
    let second = open_library(tmp.path());
    let rels_first: Vec<&str> = first.files.iter().map(|f| f.rel.as_str()).collect();
    let rels_second: Vec<&str> = second.files.iter().map(|f| f.rel.as_str()).collect();
    assert_eq!(rels_first, rels_second);
    assert_eq!(
        rels_first,
        vec!["a.jpg", "m/a.md", "m/canvas.canvas", "z.png"]
    );
}

#[test]
fn entry_dir_helpers() {
    let tmp = tmp("helpers");
    write(tmp.path(), "refs/a.jpg", jpg());
    write(tmp.path(), "refs/a.jpg.md", b"note");
    write(tmp.path(), "root.md", b"note");
    let lib = open_library(tmp.path());
    let note = lib.files.iter().find(|f| f.rel == "refs/a.jpg.md").unwrap();
    assert_eq!(note.dir(), "refs");
    assert_eq!(note.paired_image_name(), Some("a.jpg"));
    assert_eq!(note.paired_image_rel(), Some("refs/a.jpg"));
    let plain = lib.files.iter().find(|f| f.rel == "root.md").unwrap();
    assert_eq!(plain.dir(), "");
    assert_eq!(plain.paired_image_name(), None);

    let _ = Path::new("/dev/null");
}
