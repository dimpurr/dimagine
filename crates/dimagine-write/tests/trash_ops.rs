//! Trash-group, restore, purge and undo flows over the
//! journaled step machinery (ADR-016 §7, §8; ADR-017 §7),
//! plus the settings door (ADR-019).

use std::fs;

use dimagine_write::journal::{JournalMeta, RecoveryOutcome, StepKind, StepRecord};
use dimagine_write::paths::{RelPath, StepPath, TrashPath};
use dimagine_write::trash::{bumped_name, TrashRecord, TrashedFile};
use dimagine_write::{Error, UndoReason, UndoneItem, Writer};
use tempfile::tempdir;

/// Make the journaled writer look dead, so recovery is
/// allowed to act.
fn kill_journal_writer(root: &std::path::Path, op_id: &str) {
    let meta_file = root.join(".dimagine/journal").join(op_id).join("meta.json");
    let mut meta: JournalMeta =
        serde_json::from_str(&fs::read_to_string(&meta_file).unwrap()).unwrap();
    meta.pid = 0;
    fs::write(&meta_file, serde_json::to_string(&meta).unwrap()).unwrap();
}

/// A library with one image, its note and its raw
/// metadata sidecar, like an Eagle import left it.
fn populated(dir: &std::path::Path) -> Writer {
    let writer = Writer::open_raw(dir).unwrap();
    put(&dir.join("refs"), "girl.jpg", b"jpeg bytes\n");
    put(
        &dir.join("refs"),
        "girl.jpg.md",
        b"---\nid: null\ntags: []\n---\n",
    );
    put(&dir.join("refs"), "girl.jpg.eagle.json", b"{\"item\": 1}\n");
    writer
}

fn put(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), bytes).unwrap();
}

fn image() -> RelPath {
    RelPath::parse("refs/girl.jpg").unwrap()
}

fn note() -> RelPath {
    RelPath::parse("refs/girl.jpg.md").unwrap()
}

fn raw() -> RelPath {
    RelPath::parse("refs/girl.jpg.eagle.json").unwrap()
}

/// One group member's trash path at a collision round:
/// the image's name takes the ` 2` suffix and the note
/// and raw names follow it (ADR-017 §7).
fn member_trash(member: &RelPath, round: usize) -> TrashPath {
    let img = image();
    let image_name = img.name();
    let bumped = bumped_name(image_name, round);
    let suffix = member.name().strip_prefix(image_name).unwrap_or("");
    TrashPath::for_original(member)
        .sibling(&format!("{bumped}{suffix}"))
        .expect("a validated sibling name")
}

/// The record's path at a collision round.
fn record_trash(round: usize) -> TrashPath {
    let img = image();
    let bumped = bumped_name(img.name(), round);
    TrashPath::for_original(&img)
        .sibling(&format!("{bumped}.trash.json"))
        .expect("a validated sibling name")
}

/// The steps of a trash operation for one image group
/// (ADR-017 §7): the record is created before the group
/// moves, and every member moves by rename. A collided
/// group is renamed as a whole until every target is
/// free.
fn trash_steps(writer: &Writer, op_id: &str, trashed_at: &str) -> (Vec<u8>, Vec<StepRecord>) {
    let members = [image(), note(), raw()];
    let mut round = 1;
    loop {
        let targets: Vec<TrashPath> = members
            .iter()
            .map(|member| member_trash(member, round))
            .collect();
        let all_free = targets
            .iter()
            .all(|target| writer.read_step(&StepPath::Trash(target.clone())).is_err());
        if all_free {
            break;
        }
        round += 1;
    }

    let mut files = Vec::new();
    let mut steps = Vec::new();
    for member in members {
        // A group is the image plus whichever companions
        // exist beside it (ADR-017 §7).
        let (_, fp) = match writer.read(&member) {
            Ok(read) => read,
            Err(Error::Io { source, .. }) if source.raw_os_error() == Some(libc::ENOENT) => {
                continue;
            }
            Err(e) => panic!("unexpected read failure: {e}"),
        };
        let trash = member_trash(&member, round);
        files.push(TrashedFile {
            original: member.text(),
            trash: trash.text(),
            mtime_ns: fp.mtime_ns,
        });
        steps.push((member.text(), trash.text()));
    }
    let record = TrashRecord {
        op_id: op_id.to_string(),
        trashed_at: trashed_at.to_string(),
        files,
    };
    let record_bytes = record.to_bytes().unwrap();
    let mut all = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Create,
        src: None,
        dst: record_trash(round).text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(record_bytes.clone()),
    }];
    for (i, (src, dst)) in steps.into_iter().enumerate() {
        all.push(StepRecord {
            step_idx: i + 1,
            kind: StepKind::Move,
            src: Some(src),
            dst,
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        });
    }
    (record_bytes, all)
}

/// The steps of a restore operation: the group moves back
/// to its original paths, then the record is consumed by
/// a delete whose recorded bytes let undo put it back.
fn restore_steps(record: &TrashRecord) -> Vec<StepRecord> {
    let mut steps = Vec::new();
    for file in &record.files {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Move,
            src: Some(file.trash.clone()),
            dst: file.original.clone(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        });
    }
    let record_path = TrashRecord::path_for(&image());
    let record_bytes = record.to_bytes().unwrap();
    steps.push(StepRecord {
        step_idx: steps.len(),
        kind: StepKind::Delete,
        src: None,
        dst: record_path.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: Some(record_bytes),
        after_bytes: None,
    });
    steps
}

/// The steps of a purge operation: permanent deletes, no
/// bytes recorded — nothing to undo (ADR-017 §7).
fn purge_steps(record: &TrashRecord) -> Vec<StepRecord> {
    let mut steps = Vec::new();
    for file in &record.files {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Delete,
            src: None,
            dst: file.trash.clone(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        });
    }
    let record_path = TrashRecord::path_for(&image());
    steps.push(StepRecord {
        step_idx: steps.len(),
        kind: StepKind::Delete,
        src: None,
        dst: record_path.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: None,
    });
    steps
}

fn scanned_record(writer: &Writer) -> TrashRecord {
    let records = writer.trash_records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    records[0].record.clone().unwrap()
}

#[test]
fn trash_moves_the_group_and_writes_the_record_first() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (record_bytes, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op(
            "op-trash",
            "cli",
            None,
            serde_json::json!({"action": "trash"}),
            steps,
        )
        .unwrap();

    // The group moved by rename: same bytes, new location.
    assert!(writer.read(&image()).is_err());
    assert_eq!(
        writer
            .read_step(&StepPath::Trash(TrashPath::for_original(&image())))
            .unwrap()
            .0,
        b"jpeg bytes\n"
    );
    assert!(writer
        .read_step(&StepPath::Trash(TrashPath::for_original(&note())))
        .is_ok());
    assert!(writer
        .read_step(&StepPath::Trash(TrashPath::for_original(&raw())))
        .is_ok());

    // The record describes every original path and mtime.
    let record = scanned_record(&writer);
    assert_eq!(record.op_id, "op-trash");
    assert_eq!(record.trashed_at, "2026-10-13T00:00:00Z");
    assert_eq!(record.files.len(), 3);
    assert_eq!(record.files[0].original, "refs/girl.jpg");
    assert_eq!(record.files[0].trash, ".trash/refs/girl.jpg");
    assert!(record.files[0].mtime_ns > 0);
    // The record on disk is the record in hand.
    let on_disk = writer
        .read_step(&StepPath::Trash(TrashRecord::path_for(&image())))
        .unwrap()
        .0;
    assert_eq!(on_disk, record_bytes);
}

#[test]
fn restore_puts_the_group_back_and_consumes_the_record() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);

    writer
        .execute_op(
            "op-restore",
            "cli",
            None,
            serde_json::json!({"action": "restore"}),
            restore_steps(&record),
        )
        .unwrap();

    // Every member is back at its original path, bytes
    // unchanged, and the record is gone.
    assert_eq!(writer.read(&image()).unwrap().0, b"jpeg bytes\n");
    assert_eq!(
        writer.read(&note()).unwrap().0,
        b"---\nid: null\ntags: []\n---\n"
    );
    assert_eq!(writer.read(&raw()).unwrap().0, b"{\"item\": 1}\n");
    assert!(writer.trash_records().unwrap().is_empty());
    assert!(writer
        .read_step(&StepPath::Trash(TrashPath::for_original(&image())))
        .is_err());
}

#[test]
fn restore_restores_the_recorded_mtime() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);

    // The file's mtime drifts while it sits in the trash.
    let drifted = 1_000_000_000i64;
    writer
        .set_mtime(&StepPath::Trash(TrashPath::for_original(&image())), drifted)
        .unwrap();

    // A restore puts the recorded mtime back (ADR-017 §7):
    // the command applies the recorded times once the
    // moves have committed.
    writer
        .execute_op(
            "op-restore",
            "cli",
            None,
            serde_json::json!({}),
            restore_steps(&record),
        )
        .unwrap();
    for file in &record.files {
        let original = RelPath::parse(&file.original).unwrap();
        writer
            .set_mtime(&StepPath::Library(original), file.mtime_ns)
            .unwrap();
    }
    let (_, fp) = writer.read(&image()).unwrap();
    assert_eq!(fp.mtime_ns, record.files[0].mtime_ns);
}

#[test]
fn undo_of_trash_reverses_the_moves_and_removes_the_record() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op(
            "op-trash",
            "cli",
            None,
            serde_json::json!({"action": "trash"}),
            steps,
        )
        .unwrap();

    let outcome = writer.undo("op-trash", None).unwrap();

    // The moves reversed, the record removed: the group is
    // back in the library whole.
    assert_eq!(writer.read(&image()).unwrap().0, b"jpeg bytes\n");
    assert!(writer.read(&note()).is_ok());
    assert!(writer.read(&raw()).is_ok());
    assert!(writer.trash_records().unwrap().is_empty());
    let moved = outcome
        .undone
        .iter()
        .filter(|item| matches!(item, UndoneItem::Moved { .. }))
        .count();
    assert_eq!(moved, 3, "{:?}", outcome.undone);
    assert!(outcome
        .undone
        .iter()
        .any(|item| matches!(item, UndoneItem::Removed(_))));
    // The reverse moves are audited as their own operation
    // (RW60b F4).
    let entries = writer.op_log_entries().unwrap();
    assert!(entries
        .iter()
        .any(|e| e.source == "undo" && e.intent == serde_json::json!({"undo_op": "op-trash"})));
}

#[test]
fn undo_of_restore_retrashes_the_group_whole() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);
    writer
        .execute_op(
            "op-restore",
            "cli",
            None,
            serde_json::json!({"action": "restore"}),
            restore_steps(&record),
        )
        .unwrap();

    let outcome = writer.undo("op-restore", None).unwrap();

    // Undoing a restore puts the group back into the trash
    // whole: the moves reverse and the record is recreated
    // from its recorded bytes.
    assert!(writer.read(&image()).is_err());
    assert!(writer
        .read_step(&StepPath::Trash(TrashPath::for_original(&image())))
        .is_ok());
    let restored = scanned_record(&writer);
    assert_eq!(restored, record);
    assert!(outcome
        .undone
        .iter()
        .any(|item| matches!(item, UndoneItem::Recreated(_))));
}

#[test]
fn a_purged_group_is_permanent_and_not_undoable() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);

    writer
        .execute_op(
            "op-purge",
            "cli",
            None,
            serde_json::json!({"action": "purge"}),
            purge_steps(&record),
        )
        .unwrap();

    // The group and its record are gone for good.
    assert!(writer.trash_records().unwrap().is_empty());
    assert!(writer
        .read_step(&StepPath::Trash(TrashPath::for_original(&image())))
        .is_err());

    // A permanent deletion has no inverse: the undo refuses
    // naming the purged paths (ADR-016 §8).
    match writer.undo("op-purge", None) {
        Err(Error::UndoRefused { mismatches, .. }) => {
            assert!(mismatches.iter().all(|m| m.reason == UndoReason::Permanent));
            assert_eq!(mismatches.len(), 4);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_crash_mid_trash_rolls_back_to_the_library() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash-crash", "2026-10-13T00:00:00Z");

    // Crash after the record was created and the image
    // moved: the journal is uncommitted, so recovery
    // reverses both.
    let err = writer
        .execute_op_with_crash_injection(
            "op-trash-crash",
            "test",
            None,
            serde_json::json!({"action": "trash"}),
            steps,
            Some(1),
        )
        .unwrap_err();
    assert!(err.to_string().contains("simulated crash"));
    kill_journal_writer(dir.path(), "op-trash-crash");

    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(&report.recovery[0], RecoveryOutcome::RolledBack { .. }),
        "{:?}",
        report.recovery
    );

    // The group is back in the library, the record is
    // gone: the library is exactly as it was.
    assert_eq!(writer.read(&image()).unwrap().0, b"jpeg bytes\n");
    assert!(writer.read(&note()).is_ok());
    assert!(writer.read(&raw()).is_ok());
    assert!(writer.trash_records().unwrap().is_empty());
}

#[test]
fn a_crash_mid_restore_rolls_back_to_the_trash() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);

    // Crash after the image moved back but before the
    // record was consumed: recovery reverses the move.
    // The delete step never ran, so the record is intact.
    let steps = restore_steps(&record);
    let err = writer
        .execute_op_with_crash_injection(
            "op-restore-crash",
            "test",
            None,
            serde_json::json!({"action": "restore"}),
            steps,
            Some(0),
        )
        .unwrap_err();
    assert!(err.to_string().contains("simulated crash"));
    kill_journal_writer(dir.path(), "op-restore-crash");

    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(&report.recovery[0], RecoveryOutcome::RolledBack { .. }),
        "{:?}",
        report.recovery
    );

    // The group is back in the trash, the record intact.
    assert!(writer.read(&image()).is_err());
    let intact = scanned_record(&writer);
    assert_eq!(intact, record);
}

#[test]
fn a_crash_after_the_delete_re_creates_the_record() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();
    let record = scanned_record(&writer);

    // Crash after every step of the restore ran — the
    // moves back and the record's delete — but before
    // the commit marker: recovery reverses the moves and
    // re-creates the record from its recorded bytes, so
    // the trash is exactly as it was.
    let steps = restore_steps(&record);
    let err = writer
        .execute_op_with_crash_injection(
            "op-restore-crash-2",
            "test",
            None,
            serde_json::json!({"action": "restore"}),
            steps,
            Some(3),
        )
        .unwrap_err();
    assert!(err.to_string().contains("simulated crash"));
    kill_journal_writer(dir.path(), "op-restore-crash-2");

    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(&report.recovery[0], RecoveryOutcome::RolledBack { .. }),
        "{:?}",
        report.recovery
    );

    assert!(writer.read(&image()).is_err());
    let intact = scanned_record(&writer);
    assert_eq!(intact, record);
}

#[test]
fn a_collided_group_is_renamed_as_a_whole() {
    let dir = tempdir().unwrap();
    let writer = populated(dir.path());
    let (_, steps) = trash_steps(&writer, "op-trash", "2026-10-13T00:00:00Z");
    writer
        .execute_op("op-trash", "cli", None, serde_json::json!({}), steps)
        .unwrap();

    // A second group with the same name: every target of
    // the first group exists, so the whole group moves in
    // under the ` 2` name (ADR-017 §7).
    put(&dir.path().join("refs"), "girl.jpg", b"second copy\n");
    put(
        &dir.path().join("refs"),
        "girl.jpg.md",
        b"---\ntags: []\n---\n",
    );
    put(
        &dir.path().join("refs"),
        "girl.jpg.eagle.json",
        b"{\"item\": 2}\n",
    );
    let writer2 = Writer::open_raw(dir.path()).unwrap();
    let (_, steps) = trash_steps(&writer2, "op-trash-2", "2026-10-13T01:00:00Z");
    let record_step = &steps[0];
    assert_eq!(record_step.dst, ".trash/refs/girl 2.jpg.trash.json");
    let move_dsts: Vec<&str> = steps.iter().skip(1).map(|step| step.dst.as_str()).collect();
    assert!(move_dsts.contains(&".trash/refs/girl 2.jpg"));
    assert!(move_dsts.contains(&".trash/refs/girl 2.jpg.md"));
    assert!(move_dsts.contains(&".trash/refs/girl 2.jpg.eagle.json"));

    writer2
        .execute_op("op-trash-2", "cli", None, serde_json::json!({}), steps)
        .unwrap();

    // Both groups coexist: the first under its own name,
    // the second under the bumped one, each with a record.
    let records = writer2.trash_records().unwrap();
    assert_eq!(records.len(), 2);
    let texts: Vec<String> = records.iter().map(|r| r.path.text()).collect();
    assert!(texts.contains(&".trash/refs/girl.jpg.trash.json".to_string()));
    assert!(texts.contains(&".trash/refs/girl 2.jpg.trash.json".to_string()));
}

#[test]
fn settings_doors_read_and_write_the_file() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // No file yet: every setting defaults.
    assert!(writer.read_settings().unwrap().is_none());

    writer
        .write_settings(b"{\n  \"trash.retentionDays\": 30\n}")
        .unwrap();
    assert_eq!(
        writer.read_settings().unwrap(),
        Some(b"{\n  \"trash.retentionDays\": 30\n}".to_vec())
    );

    // A rewrite replaces the whole file atomically.
    writer.write_settings(b"{}").unwrap();
    assert_eq!(writer.read_settings().unwrap(), Some(b"{}".to_vec()));
}

#[test]
fn set_mtime_puts_a_recorded_time_back() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();
    let image = image();
    writer.create_exclusive(&image, b"jpeg bytes\n").unwrap();

    let recorded = 1_700_000_000_000_000_000i64;
    writer
        .set_mtime(&StepPath::Library(image.clone()), recorded)
        .unwrap();

    let (_, fp) = writer.read(&image).unwrap();
    assert_eq!(fp.mtime_ns, recorded);
}

#[test]
fn the_trash_door_is_the_only_hidden_door_a_step_may_name() {
    // A delete of library content is refused before
    // anything is unlinked (ADR-016 §0).
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();
    let image = image();
    writer.create_exclusive(&image, b"jpeg bytes\n").unwrap();

    let err = writer
        .execute_op(
            "op-bad-delete",
            "test",
            None,
            serde_json::json!({"action": "purge"}),
            vec![StepRecord {
                step_idx: 0,
                kind: StepKind::Delete,
                src: None,
                dst: image.text(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: None,
            }],
        )
        .unwrap_err();
    match err {
        Error::BatchPartial { errors, .. } => {
            assert_eq!(errors.len(), 1);
            assert!(
                errors[0]
                    .message
                    .contains("permanent deletion must be inside .trash/"),
                "{errors:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    // The library file is untouched.
    assert!(writer.read(&image).is_ok());
}
