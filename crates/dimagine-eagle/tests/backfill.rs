use dimagine_eagle::{backfill_added, BackfillReport};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const NOTE: &str = "---
id: 01JA8X3Q7K2M9V4T6R1B5N0C3Q
title: Girl sinking into deep water
tags: [underwater, sketch]
rating: 4
imported: 2026-10-04T14:30:12+01:00
custom: keep me
sources:
  - type: eagle
    library: References
    item: L8X2Q4M7Z1A9B
    imported: 2026-10-04T14:30:12+01:00
    importer: \"eagle-import prototype 0.2\"
    raw: girl-underwater.jpg.eagle.json
---

Free Markdown about the image. Links to other notes work as usual: [[sleep-pv]].

![[girl-underwater.jpg]]
";

const BTIME: i64 = 1_689_200_047_000;

fn expected_added() -> String {
    chrono::DateTime::from_timestamp_millis(BTIME)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        })
        .unwrap()
}

/// A library with one image, its Eagle note, and the sibling raw file.
fn library_with_note(temp: &TempDir, note: &str, raw: &str) -> std::path::PathBuf {
    let root = temp.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("girl-underwater.jpg"), b"jpeg-bytes").unwrap();
    fs::write(root.join("girl-underwater.jpg.md"), note.as_bytes()).unwrap();
    fs::write(root.join("girl-underwater.jpg.eagle.json"), raw.as_bytes()).unwrap();
    root
}

#[test]
fn backfill_inserts_added_after_imported_and_preserves_every_other_byte() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        NOTE,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 1,
            already_had: 0,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 0,
        }
    );
    let note_path = root.join("girl-underwater.jpg.md");
    let updated = fs::read_to_string(&note_path).unwrap();
    let added = format!("added: {}\n", expected_added());
    assert!(
        updated.contains(&added),
        "note should contain {added}:\n{updated}"
    );
    let imported = "imported: 2026-10-04T14:30:12+01:00\n";
    let imported_at = updated.find(imported).expect("imported line");
    let added_at = updated.find(&added).expect("added line");
    assert_eq!(added_at, imported_at + imported.len());
    let without_added = updated.replace(&added, "");
    assert_eq!(without_added, NOTE);
}

#[test]
fn backfill_is_idempotent() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        NOTE,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    backfill_added(&root, true).unwrap();
    let after_first = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 0,
            already_had: 1,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 0,
        }
    );
    let after_second = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    assert_eq!(after_first, after_second);
}

#[test]
fn backfill_skips_notes_that_already_have_added() {
    let temp = TempDir::new().unwrap();
    // Inside the front matter: a note that already carries the property. (The
    // previous version prepended the line before the opening fence, which is
    // not front matter at all; it only passed because "no front matter" was
    // counted as "already had added".)
    let with_added = NOTE.replace(
        "imported: 2026-10-04T14:30:12+01:00\n",
        &format!(
            "imported: 2026-10-04T14:30:12+01:00\nadded: {}\n",
            expected_added()
        ),
    );
    let root = library_with_note(
        &temp,
        &with_added,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 0,
            already_had: 1,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 0,
        }
    );
    let note = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    assert_eq!(note, with_added);
}

#[test]
fn backfill_dry_run_writes_nothing() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        NOTE,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, false).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 1,
            already_had: 0,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 0,
        }
    );
    let note = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    assert_eq!(note, NOTE);
}

#[test]
fn backfill_omits_added_when_btime_missing_or_invalid() {
    let temp = TempDir::new().unwrap();
    for raw in [
        r#"{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg"}"#,
        r#"{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":"soon"}"#,
        "not json",
    ] {
        let root = library_with_note(&temp, NOTE, raw);
        let report = backfill_added(&root, true).unwrap();
        assert_eq!(
            report,
            BackfillReport {
                scanned: 1,
                updated: 0,
                already_had: 0,
                no_btime: 1,
                no_front_matter: 0,
                unreadable: 0,
            },
            "raw: {raw}"
        );
        let note = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
        assert_eq!(note, NOTE);
    }
}

#[test]
fn backfill_ignores_notes_without_a_sibling_raw_file() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("library");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("lonely.jpg"), b"jpeg").unwrap();
    fs::write(root.join("lonely.jpg.md"), NOTE.as_bytes()).unwrap();
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 0,
            updated: 0,
            already_had: 0,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 0,
        }
    );
    let note = fs::read_to_string(root.join("lonely.jpg.md")).unwrap();
    assert_eq!(note, NOTE);
}

#[test]
fn backfill_inserts_after_opening_fence_when_no_imported_line() {
    let temp = TempDir::new().unwrap();
    let note = "---
title: No imported line
---

Body.

![[girl-underwater.jpg]]
";
    let root = library_with_note(
        &temp,
        note,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(report.updated, 1);
    let updated = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    let added = format!("added: {}\n", expected_added());
    let expected =
        format!("---\n{added}title: No imported line\n---\n\nBody.\n\n![[girl-underwater.jpg]]\n");
    assert_eq!(updated, expected);
}

#[test]
fn backfill_rejects_a_missing_library() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join("absent");
    assert!(backfill_added(&missing, true).is_err());
}

#[test]
fn backfill_reports_counts_across_many_notes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("library");
    fs::create_dir_all(&root).unwrap();
    for index in 0..3 {
        let name = format!("img{index}.jpg");
        fs::write(root.join(&name), b"jpeg").unwrap();
        fs::write(root.join(format!("{name}.md")), NOTE.as_bytes()).unwrap();
        fs::write(
            root.join(format!("{name}.eagle.json")),
            format!(
                r#"{{"id":"item{index}","name":"img{index}","ext":"jpg","btime":{}}}"#,
                BTIME + index
            ),
        )
        .unwrap();
    }
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(report.scanned, 3);
    assert_eq!(report.updated, 3);
    for index in 0..3 {
        let note = fs::read_to_string(root.join(format!("img{index}.jpg.md"))).unwrap();
        assert!(note.contains("added: "));
    }
    let second = backfill_added(&root, true).unwrap();
    assert_eq!(second.updated, 0);
    assert_eq!(second.already_had, 3);
}

#[test]
fn backfill_preserves_crlf_and_bom() {
    let temp = TempDir::new().unwrap();
    let note = "\u{feff}---\r\ntitle: CRLF note\r\nimported: 2026-10-04T14:30:12+01:00\r\n---\r\n\r\nBody.\r\n";
    let root = library_with_note(
        &temp,
        note,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(report.updated, 1);
    let updated = fs::read(root.join("girl-underwater.jpg.md")).unwrap();
    let text = String::from_utf8(updated).unwrap();
    let added = format!("added: {}\r\n", expected_added());
    assert!(text.contains(&added), "note: {text:?}");
    assert!(text.contains("\r\nimported: 2026-10-04T14:30:12+01:00\r\n"));
    assert!(text.contains("\r\n---\r\n"));
    assert!(text.ends_with("Body.\r\n"));
}

/// RW26 M-2: `split_front_matter` consumed a leading BOM and nothing put it
/// back, so `--apply` deleted it from a library original while claiming to
/// preserve every byte.
#[test]
fn backfill_keeps_a_leading_bom_byte_for_byte() {
    let temp = TempDir::new().unwrap();
    let note = "\u{feff}---\ntitle: Pic one\nimported: 2026-10-04T14:30:12+01:00\n---\n\nBody.\n";
    let root = library_with_note(
        &temp,
        note,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(report.updated, 1);
    let updated = fs::read(root.join("girl-underwater.jpg.md")).unwrap();
    assert_eq!(
        &updated[..3],
        b"\xef\xbb\xbf",
        "the BOM is still there: {updated:?}"
    );
    let added = format!("added: {}\n", expected_added());
    let text = String::from_utf8(updated).unwrap();
    assert_eq!(
        text.replace(&added, ""),
        note,
        "every other byte is unchanged"
    );
    // The rewritten note still reads as having `added`, BOM and all.
    let again = backfill_added(&root, true).unwrap();
    assert_eq!(again.already_had, 1, "the BOM note is read back correctly");
    assert_eq!(
        fs::read(root.join("girl-underwater.jpg.md")).unwrap(),
        text.as_bytes(),
        "a second run changes nothing"
    );
}

#[test]
fn backfill_handles_nested_folders() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("library");
    let nested = root.join("deep/sea");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("fish.jpg"), b"jpeg").unwrap();
    fs::write(nested.join("fish.jpg.md"), NOTE.as_bytes()).unwrap();
    fs::write(
        nested.join("fish.jpg.eagle.json"),
        format!(r#"{{"id":"fish","name":"fish","ext":"jpg","btime":{BTIME}}}"#),
    )
    .unwrap();
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(report.updated, 1);
    let note = fs::read_to_string(nested.join("fish.jpg.md")).unwrap();
    assert!(note.contains("added: "));
}

#[test]
fn backfill_rejects_non_library_root() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("file.txt");
    fs::write(&file, "x").unwrap();
    assert!(backfill_added(&file, true).is_err());
}

#[test]
fn backfill_report_is_serializable() {
    let report = BackfillReport {
        scanned: 2,
        updated: 1,
        already_had: 1,
        no_btime: 0,
        no_front_matter: 0,
        unreadable: 0,
    };
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["scanned"], 2);
    assert_eq!(json["updated"], 1);
    assert_eq!(json["already_had"], 1);
    assert_eq!(json["no_btime"], 0);
}

#[test]
fn backfill_added_value_matches_the_importer_format() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        NOTE,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    backfill_added(&root, true).unwrap();
    let note = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    let added = format!("added: {}", expected_added());
    assert!(note.contains(&added), "note: {note}");
}

/// RW26 L-1: a note with no usable front matter was counted as "already had
/// added", so the user was told the library was up to date when it was not.
#[test]
fn a_note_without_front_matter_is_its_own_outcome() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        "Just a body, no front matter.\n",
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let report = backfill_added(&root, true).unwrap();
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 0,
            already_had: 0,
            no_btime: 0,
            no_front_matter: 1,
            unreadable: 0,
        }
    );
    let note = fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap();
    assert_eq!(
        note, "Just a body, no front matter.\n",
        "nothing was written"
    );
}

/// RW26 L-2: an unreadable note or raw file was counted as "no btime", which
/// conflates "this file has no btime" with "this file could not be read".
/// `chmod 000` is the reviewer's repro; it is skipped where the test runs as
/// a user that can read anything anyway (root).
#[test]
#[cfg(unix)]
fn an_unreadable_raw_file_is_not_reported_as_a_missing_btime() {
    let temp = TempDir::new().unwrap();
    let root = library_with_note(
        &temp,
        NOTE,
        &format!(
            r#"{{"id":"L8X2Q4M7Z1A9B","name":"girl-underwater","ext":"jpg","btime":{BTIME}}}"#
        ),
    );
    let raw_path = root.join("girl-underwater.jpg.eagle.json");
    let permissions = std::fs::metadata(&raw_path).unwrap().permissions();
    let locked = lock_file(&raw_path, permissions.clone());
    let report = backfill_added(&root, true).unwrap();
    std::fs::set_permissions(&raw_path, permissions).unwrap();
    if !locked {
        // This process can read the file whatever its mode says, so there is
        // nothing to assert about the unreadable path.
        return;
    }
    assert_eq!(
        report,
        BackfillReport {
            scanned: 1,
            updated: 0,
            already_had: 0,
            no_btime: 0,
            no_front_matter: 0,
            unreadable: 1,
        }
    );
    assert_eq!(
        fs::read_to_string(root.join("girl-underwater.jpg.md")).unwrap(),
        NOTE,
        "nothing was written"
    );
}

/// Remove every read bit, and report whether that made the file unreadable
/// here (it never is for a privileged process such as root).
#[cfg(unix)]
fn lock_file(path: &Path, original: std::fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read(path).is_ok();
    if readable {
        std::fs::set_permissions(path, original).unwrap();
    }
    !readable
}
