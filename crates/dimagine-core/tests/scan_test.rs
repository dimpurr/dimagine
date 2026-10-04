//! `dimagine scan` summaries.

mod support;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use dimagine_core::scan;
use support::*;

#[test]
fn untouched_folder_of_images_only() {
    let tmp = tmp("scan-plain");
    write(tmp.path(), "one.jpg", jpg());
    write(tmp.path(), "sub/two.png", png());
    let lib = open_library(tmp.path());
    let report = scan::run(&lib);
    assert_eq!(report.image_total, 2);
    assert_eq!(report.images["jpeg"], 1);
    assert_eq!(report.images["png"], 1);
    assert_eq!(report.image_notes.total, 0);
    assert_eq!(report.notes_total, 0);
    assert_eq!(report.collections, 0);
    assert_eq!(report.canvases, 0);
    assert_eq!(report.raw_files, 0);
    assert_eq!(report.other_files, 0);
    assert_eq!(report.unreadable_files.len(), 0);
    assert!(report.read_complete);
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn full_library_is_counted() {
    let tmp = tmp("scan-full");
    write(tmp.path(), "girl.jpg", jpg());
    write(
        tmp.path(),
        "girl.jpg.md",
        "---\ntitle: Girl\n---\n\n![[girl.jpg]]\n",
    );
    write(tmp.path(), "girl.jpg.eagle.json", "{}");
    write(tmp.path(), "lonely.png.md", "---\n---\nno image\n");
    write(
        tmp.path(),
        "col.md",
        "---\nkind: collection\n---\n![[girl.jpg]]\n",
    );
    write(tmp.path(), "plain.md", "text\n");
    write(tmp.path(), "board.canvas", "{}");
    write(tmp.path(), "manual.pdf", b"%PDF-1.4");
    write(tmp.path(), ".dimagine/cache", b"x");
    write(tmp.path(), "._sidecar", b"x");
    let lib = open_library(tmp.path());
    let report = scan::run(&lib);
    assert_eq!(report.image_total, 1);
    assert_eq!(report.images["jpeg"], 1);
    assert_eq!(report.image_notes.total, 2, "girl.jpg.md and lonely.png.md");
    assert_eq!(report.image_notes.paired, 1);
    assert_eq!(report.image_notes.unpaired, 1);
    assert_eq!(report.notes_total, 4);
    assert_eq!(report.collections, 1);
    assert_eq!(report.canvases, 1);
    assert_eq!(report.raw_files, 1);
    assert_eq!(report.other_files, 1);
    assert_eq!(report.ignored.total, 2);
    assert_eq!(report.ignored.hidden, 1);
    assert_eq!(report.ignored.resource_fork, 1);
    assert_eq!(report.exit_code(), 0);
}

#[test]
#[cfg(unix)]
fn unreadable_files_are_reported_and_exit_one() {
    let tmp = tmp("scan-unreadable");
    write(tmp.path(), "fine.png", png());
    write(tmp.path(), "broken.jpg", jpg());
    std::fs::set_permissions(
        tmp.path().join("broken.jpg"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let lib = open_library(tmp.path());
    let report = scan::run(&lib);
    assert_eq!(report.image_total, 2);
    assert_eq!(report.images["png"], 1);
    assert_eq!(
        report.images["unknown"], 1,
        "unreadable stays unknown, never empty"
    );
    assert_eq!(report.unreadable_files.len(), 1);
    assert_eq!(report.unreadable_files[0].path, "broken.jpg");
    assert!(!report.unreadable_files[0].reason.is_empty());
    assert_eq!(report.exit_code(), 1);
    std::fs::set_permissions(
        tmp.path().join("broken.jpg"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

#[test]
#[cfg(unix)]
fn incomplete_walk_exits_three() {
    let tmp = tmp("scan-exit3");
    write(tmp.path(), "a.png", png());
    write(tmp.path(), "locked/b.png", png());
    let locked = tmp.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let lib = open_library(tmp.path());
    let report = scan::run(&lib);
    assert!(!report.read_complete);
    assert_eq!(report.unreadable_dirs.len(), 1);
    assert_eq!(report.exit_code(), 3);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn mismatched_content_is_counted_by_real_format() {
    let tmp = tmp("scan-realtypes");
    write(tmp.path(), "fake.jpg", png()); // PNG content in .jpg file
    let lib = open_library(tmp.path());
    let report = scan::run(&lib);
    assert_eq!(report.images["png"], 1, "by real format, not extension");
    assert_eq!(report.images.get("jpeg").copied().unwrap_or(0), 0);
}
