use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use dimagine_write::journal::{RecoveryOutcome, StepKind, StepRecord};
use dimagine_write::{
    Error, Fingerprint, PathReject, Precondition, RelPath, StepPath, UndoReason, UndoneItem, Writer,
};
use tempfile::tempdir;

/// Make the journaled writer look dead, so recovery is allowed to act.
fn kill_journal_writer(root: &std::path::Path, op_id: &str) {
    let meta_file = root.join(".dimagine/journal").join(op_id).join("meta.json");
    let mut meta: dimagine_write::journal::JournalMeta =
        serde_json::from_str(&fs::read_to_string(&meta_file).unwrap()).unwrap();
    meta.pid = 0;
    fs::write(&meta_file, serde_json::to_string(&meta).unwrap()).unwrap();
}

#[test]
fn test_move_noreplace_primitive() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let src = RelPath::parse("inbox/shot.jpg").unwrap();
    writer.create_exclusive(&src, b"jpeg bytes\n").unwrap();
    let note = RelPath::parse("inbox/shot.jpg.md").unwrap();
    writer
        .create_exclusive(&note, b"---\ntags: []\n---\n")
        .unwrap();

    let moved_image = RelPath::parse("archive/shot.jpg").unwrap();
    let moved_note = RelPath::parse("archive/shot.jpg.md").unwrap();
    writer.move_noreplace(&src, &moved_image).unwrap();
    writer.move_noreplace(&note, &moved_note).unwrap();

    assert!(writer.read(&src).is_err());
    assert_eq!(writer.read(&moved_image).unwrap().0, b"jpeg bytes\n");
    // A rename does not rewrite bytes, so an image move is not a write to it.
    assert_eq!(
        Fingerprint::of_bytes(b"jpeg bytes\n").hex(),
        writer.read(&moved_image).unwrap().1.hex()
    );

    // Refuses to clobber.
    let other = RelPath::parse("other.jpg").unwrap();
    writer.create_exclusive(&other, b"kept\n").unwrap();
    let before = writer.read(&other).unwrap().1;
    assert!(matches!(
        writer.move_noreplace(&moved_image, &other),
        Err(Error::Exists(_))
    ));
    assert_eq!(writer.read(&other).unwrap().1, before);
    assert!(writer.read(&moved_image).is_ok());

    // Moving through a symlinked component is refused like any other write.
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("archive"), dir.path().join("link_dir")).unwrap();
    let escape = RelPath::parse("link_dir/shot.jpg").unwrap();
    assert!(matches!(
        writer.move_noreplace(&moved_note, &escape),
        Err(Error::BadPath {
            reject: PathReject::Symlink,
            ..
        })
    ));
}

#[test]
fn test_journaled_move_rolls_back_and_undoes() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let src = RelPath::parse("shots/a.jpg").unwrap();
    let dst = RelPath::parse("archive/a.jpg").unwrap();
    writer.create_exclusive(&src, b"pixels\n").unwrap();

    let move_step = StepRecord {
        step_idx: 0,
        kind: StepKind::Move,
        src: Some("shots/a.jpg".to_string()),
        dst: "archive/a.jpg".to_string(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: None,
    };

    // Crash after the move, with no committed marker: recovery reverses it.
    let err = writer.execute_op_with_crash_injection(
        "op-move-crash",
        "test",
        None,
        serde_json::json!({"action": "mv"}),
        vec![move_step.clone()],
        Some(0),
    );
    assert!(err.is_err());
    assert!(writer.read(&dst).is_ok());

    kill_journal_writer(dir.path(), "op-move-crash");
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::RolledBack { steps: 1, .. }
        ),
        "{:?}",
        report.recovery
    );
    assert_eq!(writer.read(&src).unwrap().0, b"pixels\n");
    assert!(writer.read(&dst).is_err());

    // A completed move undoes as a reverse move.
    let undo_op = "op-move-undo";
    writer
        .execute_op(
            undo_op,
            "cli",
            None,
            serde_json::json!({"action": "mv"}),
            vec![StepRecord {
                step_idx: 0,
                kind: StepKind::Move,
                src: Some("shots/a.jpg".to_string()),
                dst: "archive/a.jpg".to_string(),
                ..move_step.clone()
            }],
        )
        .unwrap();
    assert!(writer.read(&src).is_err());

    let outcome = writer.undo(undo_op, None).unwrap();
    assert!(outcome.undone.contains(&UndoneItem::Moved {
        from: StepPath::Library(dst),
        to: StepPath::Library(src.clone())
    }));
    assert_eq!(writer.read(&src).unwrap().0, b"pixels\n");
    // The reverse move is journaled as its own operation
    // (RW60b F4): the audit log holds an undo-sourced line
    // pointing back at the undone op.
    let entries = writer.op_log_entries().unwrap();
    let undo_line = entries
        .iter()
        .find(|e| e.source == "undo" && e.intent == serde_json::json!({"undo_op": undo_op}))
        .expect("the reverse move is audited as an undo op");
    assert_eq!(undo_line.step_kind, StepKind::Move);
    assert_eq!(undo_line.path, "shots/a.jpg");
    assert_eq!(undo_line.src_path.as_deref(), Some("archive/a.jpg"));
}

#[test]
fn test_move_in_third_state_is_never_touched() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let src = RelPath::parse("shots/b.jpg").unwrap();
    let dst = RelPath::parse("archive/b.jpg").unwrap();
    writer.create_exclusive(&src, b"pixels\n").unwrap();

    let err = writer.execute_op_with_crash_injection(
        "op-move-third",
        "test",
        None,
        serde_json::json!({"action": "mv"}),
        vec![StepRecord {
            step_idx: 0,
            kind: StepKind::Move,
            src: Some("shots/b.jpg".to_string()),
            dst: "archive/b.jpg".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        }],
        Some(0),
    );
    assert!(err.is_err());

    // An external tool rewrites the destination: neither before nor after hash.
    fs::write(dir.path().join("archive/b.jpg"), b"somewhere else\n").unwrap();

    kill_journal_writer(dir.path(), "op-move-third");
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::ThirdStateRefused { .. }
        ),
        "{:?}",
        report.recovery
    );
    assert_eq!(
        writer.read(&dst).unwrap().0,
        b"somewhere else\n",
        "a third-state file must not be overwritten by recovery"
    );
    assert!(writer.read(&src).is_err());
}

/// The move-then-edit-same-path pattern of ADR-017 §8: one
/// operation renames the group and then edits the moved note at
/// its new path. Crash after that edit lands but before
/// `committed`: recovery must classify the renames as executed
/// (undo's per-file rule — the destination carries a later
/// same-op step's hash) and roll the whole group back intact.
#[test]
fn test_crash_after_move_edit_rolls_back_the_group() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let image = RelPath::parse("a/x.jpg").unwrap();
    writer.create_exclusive(&image, b"jpeg\n").unwrap();
    let note = RelPath::parse("a/x.jpg.md").unwrap();
    let note_before = b"---\nid: note\n---\n\n![[a/x.jpg]]\n".to_vec();
    writer.create_exclusive(&note, &note_before).unwrap();
    let note_edited = b"---\nid: note\n---\n\n![[a/y.jpg]]\n".to_vec();

    let image_hash = Fingerprint::of_bytes(b"jpeg\n").hex();
    let before_hash = Fingerprint::of_bytes(&note_before).hex();
    let edited_hash = Fingerprint::of_bytes(&note_edited).hex();

    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Move,
            src: Some("a/x.jpg".to_string()),
            dst: "a/y.jpg".to_string(),
            before_hash: None,
            after_hash: image_hash.clone(),
            before_bytes: None,
            after_bytes: None,
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Move,
            src: Some("a/x.jpg.md".to_string()),
            dst: "a/y.jpg.md".to_string(),
            before_hash: None,
            after_hash: before_hash.clone(),
            before_bytes: None,
            after_bytes: None,
        },
        StepRecord {
            step_idx: 2,
            kind: StepKind::Modify,
            src: None,
            dst: "a/y.jpg.md".to_string(),
            before_hash: Some(before_hash.clone()),
            after_hash: edited_hash.clone(),
            before_bytes: Some(note_before.clone()),
            after_bytes: Some(note_edited.clone()),
        },
    ];

    // Crash after the moved note's edit landed, before `committed`.
    let err = writer.execute_op_with_crash_injection(
        "op-mv-edit-crash",
        "test",
        None,
        serde_json::json!({"action": "mv"}),
        steps,
        Some(2),
    );
    assert!(err.is_err());
    // The edit is on disk at the new path.
    assert_eq!(
        writer
            .read(&RelPath::parse("a/y.jpg.md").unwrap())
            .unwrap()
            .0,
        note_edited
    );

    kill_journal_writer(dir.path(), "op-mv-edit-crash");
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::RolledBack { steps: 3, .. }
        ),
        "{:?}",
        report.recovery
    );
    // The group is back at its original paths with its original bytes.
    assert_eq!(writer.read(&image).unwrap().0, b"jpeg\n");
    assert_eq!(writer.read(&note).unwrap().0, note_before);
    assert!(writer.read(&RelPath::parse("a/y.jpg").unwrap()).is_err());
    assert!(writer.read(&RelPath::parse("a/y.jpg.md").unwrap()).is_err());
    // No eternal journal: the operation reached its terminal state,
    // so the next open has nothing to report.
    assert!(!dir
        .path()
        .join(".dimagine/journal/op-mv-edit-crash")
        .exists());
    let report2 = writer.startup_sweep().unwrap();
    assert!(report2.recovery.is_empty(), "{:?}", report2.recovery);
}

/// The same move-then-edit pattern, crashing after `committed` but
/// before the journal directory is removed: recovery must not report
/// a spurious third state for the renamed-then-edited note and must
/// complete the cleanup, leaving the finished move in place.
#[test]
fn test_crash_after_committed_move_edit_cleans_up() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let image = RelPath::parse("a/x.jpg").unwrap();
    writer.create_exclusive(&image, b"jpeg\n").unwrap();
    let note = RelPath::parse("a/x.jpg.md").unwrap();
    let note_before = b"---\nid: note\n---\n\n![[a/x.jpg]]\n".to_vec();
    writer.create_exclusive(&note, &note_before).unwrap();
    let note_edited = b"---\nid: note\n---\n\n![[a/y.jpg]]\n".to_vec();

    let image_hash = Fingerprint::of_bytes(b"jpeg\n").hex();
    let before_hash = Fingerprint::of_bytes(&note_before).hex();
    let edited_hash = Fingerprint::of_bytes(&note_edited).hex();

    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Move,
            src: Some("a/x.jpg".to_string()),
            dst: "a/y.jpg".to_string(),
            before_hash: None,
            after_hash: image_hash,
            before_bytes: None,
            after_bytes: None,
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Move,
            src: Some("a/x.jpg.md".to_string()),
            dst: "a/y.jpg.md".to_string(),
            before_hash: None,
            after_hash: before_hash.clone(),
            before_bytes: None,
            after_bytes: None,
        },
        StepRecord {
            step_idx: 2,
            kind: StepKind::Modify,
            src: None,
            dst: "a/y.jpg.md".to_string(),
            before_hash: Some(before_hash),
            after_hash: edited_hash,
            before_bytes: Some(note_before),
            after_bytes: Some(note_edited.clone()),
        },
    ];

    // Crash after the `committed` marker, before the journal is removed.
    let err = writer.execute_op_with_crash_injection(
        "op-mv-edit-committed",
        "test",
        None,
        serde_json::json!({"action": "mv"}),
        steps,
        Some(3),
    );
    assert!(err.is_err());

    kill_journal_writer(dir.path(), "op-mv-edit-committed");
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::RolledForward { steps: 0, .. }
        ),
        "{:?}",
        report.recovery
    );
    // The finished move stands: the group is at its new paths and
    // the note carries its edit.
    assert_eq!(
        writer.read(&RelPath::parse("a/y.jpg").unwrap()).unwrap().0,
        b"jpeg\n"
    );
    assert_eq!(
        writer
            .read(&RelPath::parse("a/y.jpg.md").unwrap())
            .unwrap()
            .0,
        note_edited
    );
    assert!(writer.read(&image).is_err());
    assert!(writer.read(&note).is_err());
    assert!(!dir
        .path()
        .join(".dimagine/journal/op-mv-edit-committed")
        .exists());
    let report2 = writer.startup_sweep().unwrap();
    assert!(report2.recovery.is_empty(), "{:?}", report2.recovery);
}

#[test]
fn test_journal_records_actual_state_not_caller_guesses() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let note = RelPath::parse("guess.md").unwrap();
    writer.create_exclusive(&note, b"original\n").unwrap();

    // The caller supplies an intent plus a stale snapshot it read earlier: the
    // journal must record the bytes really on disk, because that is what a
    // rollback restores.
    let steps = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: "guess.md".to_string(),
        before_hash: Some("a guess written at the wrong time".to_string()),
        after_hash: String::new(),
        before_bytes: Some(b"stale bytes the caller once read".to_vec()),
        after_bytes: Some(b"rewritten\n".to_vec()),
    }];

    let err = writer.execute_op_with_crash_injection(
        "op-guess",
        "test",
        None,
        serde_json::json!({"action": "set"}),
        steps,
        Some(0),
    );
    assert!(err.is_err());
    assert_eq!(writer.read(&note).unwrap().0, b"rewritten\n");

    kill_journal_writer(dir.path(), "op-guess");
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::RolledBack { steps: 1, .. }
        ),
        "{:?}",
        report.recovery
    );
    assert_eq!(
        writer.read(&note).unwrap().0,
        b"original\n",
        "rollback must restore the bytes read at execution time, not the caller's stale copy"
    );

    // The audit line for that operation carries the real before-hash, so undo
    // can tell "already reverted" from "changed elsewhere".
    let entries = writer.op_log_entries().unwrap();
    let logged = entries
        .iter()
        .rev()
        .find(|e| e.op_id == "op-guess")
        .unwrap();
    assert_eq!(
        logged.before_hash.as_deref(),
        Some(Fingerprint::of_bytes(b"original\n").hex().as_str())
    );
    assert_eq!(logged.before_bytes.as_deref(), Some(&b"original\n"[..]));
}

/// Every file name anywhere under the library, for "nothing stray was left".
fn collect_names(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}

fn has_no_temp_files(root: &std::path::Path) {
    let leftovers = collect_names(root);
    assert!(
        !leftovers.iter().any(|n| n.starts_with(".dimagine.tmp.")),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn test_failed_create_leaves_no_temp_and_keeps_original() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let path = RelPath::parse("occupied.md").unwrap();
    let before = writer.create_exclusive(&path, b"first\n").unwrap();

    // The failing path for this primitive: the temp is written, the
    // no-clobber rename refuses, and the temp must not survive the error
    // (ADR-016 §4).
    for _ in 0..3 {
        assert!(matches!(
            writer.create_exclusive(&path, b"second\n"),
            Err(Error::Exists(_))
        ));
    }
    let sub = RelPath::parse("deeper/occupied.md").unwrap();
    writer.create_exclusive(&sub, b"mine\n").unwrap();
    assert!(matches!(
        writer.create_exclusive(&sub, b"yours\n"),
        Err(Error::Exists(_))
    ));

    let (bytes, fp) = writer.read(&path).unwrap();
    assert_eq!(bytes, b"first\n");
    assert_eq!(fp, before, "a failed write must not touch the original");
    has_no_temp_files(dir.path());
}

#[test]
fn test_partial_operation_failure_touches_nothing_unintended() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    writer
        .create_exclusive(&RelPath::parse("taken.md").unwrap(), b"theirs\n")
        .unwrap();

    // Step 0 lands, step 1 collides at execution time: what already reached
    // the disk stays there (ADR-016 §7 partial failure), and no temp is left.
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "created.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"ok\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "taken.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"clash\n".to_vec()),
        },
    ];
    assert!(writer
        .execute_op("op-partial", "test", None, serde_json::json!({}), steps)
        .is_err());
    assert_eq!(
        writer
            .read(&RelPath::parse("created.md").unwrap())
            .unwrap()
            .0,
        b"ok\n"
    );
    assert_eq!(
        writer.read(&RelPath::parse("taken.md").unwrap()).unwrap().0,
        b"theirs\n"
    );
    has_no_temp_files(dir.path());

    // An operation whose later step cannot even be described — a modify of a
    // note that does not exist — is refused up front, before any file moves.
    let doomed = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Modify,
            src: None,
            dst: "created.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"changed\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Modify,
            src: None,
            dst: "never-existed.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"changed\n".to_vec()),
        },
    ];
    assert!(writer
        .execute_op("op-doomed", "test", None, serde_json::json!({}), doomed)
        .is_err());
    assert_eq!(
        writer
            .read(&RelPath::parse("created.md").unwrap())
            .unwrap()
            .0,
        b"ok\n",
        "the whole operation must refuse rather than half-apply a known-bad plan"
    );
    assert!(!dir.path().join(".dimagine/journal/op-doomed").exists());
    has_no_temp_files(dir.path());
}

#[test]
fn test_lock_library_gives_a_second_writer_exclusive_access() {
    let dir = tempdir().unwrap();
    let writer = Arc::new(Writer::open_raw(dir.path()).unwrap());
    let counter = dir.path().join("counter.md");
    fs::write(&counter, b"0\n").unwrap();

    // The importer's shape: hold the one lock and write with plain filesystem
    // calls. A read-modify-write under it must lose no update (ADR-016 §6).
    let (threads, iterations) = (4, 25);
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let w = Arc::clone(&writer);
            let path = counter.clone();
            thread::spawn(move || {
                for _ in 0..iterations {
                    let guard = w.lock_library().unwrap();
                    let text = fs::read_to_string(&path).unwrap();
                    thread::yield_now();
                    let value: u64 = text.trim().parse().unwrap();
                    fs::write(&path, format!("{}\n", value + 1).as_bytes()).unwrap();
                    drop(guard);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        fs::read_to_string(&counter).unwrap().trim(),
        format!("{}", threads * iterations),
        "writers that share the lock must not lose updates"
    );
}

#[test]
fn test_sweep_reports_temp_files_removed() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();
    fs::write(dir.path().join(".dimagine.tmp.swept"), b"x\n").unwrap();
    let report = writer.startup_sweep().unwrap();
    assert_eq!(report.temp_files_removed, 1);
    assert!(report.recovery.is_empty());
}

#[test]
fn test_cas_conflict_and_intent_retry() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let note_path = RelPath::parse("note.md").unwrap();
    let initial_fp = writer
        .create_exclusive(&note_path, b"hello world\n")
        .unwrap();

    // Verify initial read
    let (bytes, fp) = writer.read(&note_path).unwrap();
    assert_eq!(bytes, b"hello world\n");
    assert!(initial_fp.matches(&fp));

    // Writer B modifies the note out-of-band
    fs::write(dir.path().join("note.md"), b"hello out of band\n").unwrap();

    // Writer A attempts CAS update with initial_fp -> fails with Conflict
    let res = writer.update_existing(&note_path, &initial_fp, b"new content\n");
    match res {
        Err(Error::Conflict(conflict)) => {
            assert_eq!(conflict.path, note_path.to_path());
            assert_eq!(conflict.expected, initial_fp);
            assert_eq!(conflict.current_bytes, b"hello out of band\n");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Attempt apply_force with wrong hash -> Conflict
    let wrong_hash = [0u8; 32];
    assert!(writer
        .apply_force(&note_path, &wrong_hash, b"forced content\n")
        .is_err());

    // Attempt apply_force with the hash seen in 409 -> succeeds
    let (_cur_bytes, cur_fp) = writer.read(&note_path).unwrap();
    let forced_fp = writer
        .apply_force(&note_path, &cur_fp.sha256, b"forced content\n")
        .unwrap();
    let (after_force, _) = writer.read(&note_path).unwrap();
    assert_eq!(after_force, b"forced content\n");
    assert_ne!(forced_fp.sha256, cur_fp.sha256);

    // Test apply_intent with retry
    let (_read_bytes, _read_fp) = writer.read(&note_path).unwrap();
    let mut called = 0;
    let res = writer
        .apply_intent(&note_path, |b| {
            called += 1;
            if called == 1 {
                // Concurrent modification before first commit
                fs::write(dir.path().join("note.md"), b"modified while computing\n").unwrap();
            }
            let mut updated = b.to_vec();
            updated.extend_from_slice(b"+tag\n");
            Ok(Some(updated))
        })
        .unwrap();

    assert!(res.is_some());
    assert!(
        called >= 2,
        "intent should have been reapplied after CAS conflict"
    );
    let (final_bytes, _) = writer.read(&note_path).unwrap();
    assert_eq!(final_bytes, b"modified while computing\n+tag\n");
}

#[test]
fn test_exclusive_create_race() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let writer = Arc::new(Writer::open_raw(&root).unwrap());
    let path = RelPath::parse("race.md").unwrap();

    // Two threads race to create race.md
    let w1 = Arc::clone(&writer);
    let p1 = path.clone();
    let barrier = Arc::new(Barrier::new(2));

    let b1 = Arc::clone(&barrier);
    let t1 = thread::spawn(move || {
        b1.wait();
        w1.create_exclusive(&p1, b"thread 1\n")
    });

    let w2 = Arc::clone(&writer);
    let p2 = path.clone();
    let b2 = Arc::clone(&barrier);
    let t2 = thread::spawn(move || {
        b2.wait();
        w2.create_exclusive(&p2, b"thread 2\n")
    });

    let res1 = t1.join().unwrap();
    let res2 = t2.join().unwrap();

    let wins = (res1.is_ok() as u32) + (res2.is_ok() as u32);
    assert_eq!(
        wins, 1,
        "exactly one thread must succeed in exclusive create"
    );

    // Next free name should produce "race 2.md"
    let free_name = writer.next_free_name(None, "race", "md").unwrap();
    assert_eq!(free_name.text(), "race 2.md");

    // Concurrent create_with_next_free_name with 8 threads
    let threads = 8;
    let mut handles = Vec::new();
    let multi_barrier = Arc::new(Barrier::new(threads));

    for i in 0..threads {
        let w = Arc::clone(&writer);
        let b = Arc::clone(&multi_barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            w.create_with_next_free_name(
                None,
                "concurrent",
                "md",
                format!("thread {i}\n").as_bytes(),
            )
        }));
    }

    let mut created_names = Vec::new();
    for h in handles {
        let (path, _) = h.join().unwrap().unwrap();
        created_names.push(path.text());
    }
    assert_eq!(created_names.len(), threads);
    // All names must be unique
    let mut deduped = created_names.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(
        deduped.len(),
        threads,
        "each thread must get a distinct sibling name"
    );
}

#[test]
fn test_symlink_refused() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let target_file = dir.path().join("real.md");
    fs::write(&target_file, b"real file\n").unwrap();

    // Leaf symlink
    let leaf_link = dir.path().join("sym_leaf.md");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target_file, &leaf_link).unwrap();

    let leaf_rel = RelPath::parse("sym_leaf.md").unwrap();
    assert!(matches!(
        writer.read(&leaf_rel),
        Err(Error::BadPath {
            reject: PathReject::Symlink,
            ..
        })
    ));

    let dummy_fp = Fingerprint::of_bytes(b"dummy");
    assert!(matches!(
        writer.update_existing(&leaf_rel, &dummy_fp, b"new bytes"),
        Err(Error::BadPath {
            reject: PathReject::Symlink,
            ..
        })
    ));

    // Component symlink
    let real_dir = dir.path().join("real_dir");
    fs::create_dir(&real_dir).unwrap();
    fs::write(real_dir.join("inside.md"), b"inside\n").unwrap();

    let sym_dir = dir.path().join("sym_dir");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_dir, &sym_dir).unwrap();

    let comp_rel = RelPath::parse("sym_dir/inside.md").unwrap();
    assert!(matches!(
        writer.read(&comp_rel),
        Err(Error::BadPath {
            reject: PathReject::Symlink,
            ..
        })
    ));

    let create_rel = RelPath::parse("sym_dir/new.md").unwrap();
    assert!(matches!(
        writer.create_exclusive(&create_rel, b"new\n"),
        Err(Error::BadPath {
            reject: PathReject::Symlink,
            ..
        })
    ));
}

#[test]
fn test_path_refusals() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // Absolute
    assert!(matches!(
        RelPath::parse("/etc/hosts"),
        Err(Error::BadPath {
            reject: PathReject::Absolute,
            ..
        })
    ));

    // Traversal and NUL
    for p in ["foo/../bar.md", "a/./b.md", "a/\0b.md"] {
        assert!(
            matches!(
                RelPath::parse(p),
                Err(Error::BadPath {
                    reject: PathReject::Traversal,
                    ..
                })
            ),
            "expected Traversal for {p}"
        );
    }

    // Hidden components
    for p in [".git/config", ".dimagine/log", "folder/.secret.md"] {
        assert!(
            matches!(
                RelPath::parse(p),
                Err(Error::BadPath {
                    reject: PathReject::Hidden,
                    ..
                })
            ),
            "expected Hidden for {p}"
        );
    }

    // Non-note update is refused
    let img_path = RelPath::parse("photo.jpg").unwrap();
    let fp = Fingerprint::of_bytes(b"test");
    assert!(matches!(
        writer.update_existing(&img_path, &fp, b"update"),
        Err(Error::BadPath {
            reject: PathReject::NotANote,
            ..
        })
    ));

    // An operation id becomes a directory name, so it is refused like a path
    // component. Without this, `..` resolves the operation directory to
    // `.dimagine` itself and cleanup empties it.
    let note = RelPath::parse("a.md").unwrap();
    for bad_id in ["..", ".", "", "nested/op", ".hidden"] {
        let steps = vec![StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "a.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"x\n".to_vec()),
        }];
        assert!(
            matches!(
                writer.execute_op(bad_id, "test", None, serde_json::json!({}), steps),
                Err(Error::BadPath { .. })
            ),
            "expected {bad_id:?} refused as an operation id"
        );
    }
    assert!(
        dir.path().join(".dimagine/write.lock").exists(),
        ".dimagine must survive a refused operation id"
    );
    assert!(!dir.path().join(".dimagine/journal/.hidden").exists());
    writer
        .create_exclusive(&note, b"untouched\n")
        .unwrap_or_else(|e| panic!("writer still usable after refusals: {e}"));
}

#[test]
fn test_crash_recovery_uncommitted_and_committed() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let writer = Writer::open_raw(&root).unwrap();

    // Pre-create file b.md
    let b_path = RelPath::parse("b.md").unwrap();
    let b_fp = writer.create_exclusive(&b_path, b"initial b\n").unwrap();

    let op_id = "test-uncommitted-crash";
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "a.md".to_string(),
            before_hash: None,
            after_hash: Fingerprint::of_bytes(b"initial a\n").hex(),
            before_bytes: None,
            after_bytes: Some(b"initial a\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Modify,
            src: None,
            dst: "b.md".to_string(),
            before_hash: Some(b_fp.hex()),
            after_hash: Fingerprint::of_bytes(b"updated b\n").hex(),
            before_bytes: Some(b"initial b\n".to_vec()),
            after_bytes: Some(b"updated b\n".to_vec()),
        },
        StepRecord {
            step_idx: 2,
            kind: StepKind::Create,
            src: None,
            dst: "c.md".to_string(),
            before_hash: None,
            after_hash: Fingerprint::of_bytes(b"initial c\n").hex(),
            before_bytes: None,
            after_bytes: Some(b"initial c\n".to_vec()),
        },
    ];

    // Crash after step 1: a.md is created, b.md is updated, but c.md is not created,
    // and committed marker is NOT written!
    let err = writer.execute_op_with_crash_injection(
        op_id,
        "test",
        None,
        serde_json::json!({"action": "batch"}),
        steps.clone(),
        Some(1),
    );
    assert!(err.is_err());

    // Both a.md and b.md were modified on disk
    let a_path = RelPath::parse("a.md").unwrap();
    assert!(writer.read(&a_path).is_ok());
    let (b_bytes, _) = writer.read(&b_path).unwrap();
    assert_eq!(b_bytes, b"updated b\n");

    // Overwrite meta.json in journal so recorded writer pid = 0 (definitely dead)
    kill_journal_writer(dir.path(), op_id);

    // Now run recovery via startup_sweep
    writer.startup_sweep().unwrap();

    // Verification: uncommitted rollback rolled back known steps!
    // a.md should be gone, b.md should be back to "initial b\n"
    assert!(
        writer.read(&a_path).is_err(),
        "a.md should have been rolled back / unlinked"
    );
    let (restored_b, _) = writer.read(&b_path).unwrap();
    assert_eq!(
        restored_b, b"initial b\n",
        "b.md should be restored to before-bytes"
    );

    // Case B: Crash AFTER committed marker (simulated crash after all steps committed)
    let op_id2 = "test-committed-crash";
    let steps2 = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Create,
        src: None,
        dst: "post_commit.md".to_string(),
        before_hash: None,
        after_hash: Fingerprint::of_bytes(b"post commit\n").hex(),
        before_bytes: None,
        after_bytes: Some(b"post commit\n".to_vec()),
    }];

    // Simulate crash after committed marker
    let err2 = writer.execute_op_with_crash_injection(
        op_id2,
        "test",
        None,
        serde_json::json!({"action": "post"}),
        steps2.clone(),
        Some(steps2.len()),
    );
    assert!(err2.is_err());

    kill_journal_writer(dir.path(), op_id2);

    // Startup sweep runs recovery: committed op is cleaned up and verified
    writer.startup_sweep().unwrap();

    let post_path = RelPath::parse("post_commit.md").unwrap();
    let (post_bytes, _) = writer.read(&post_path).unwrap();
    assert_eq!(post_bytes, b"post commit\n");
    assert!(!dir.path().join(".dimagine/journal").join(op_id2).exists());
}

#[test]
fn test_crash_recovery_third_state_untouched() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let mod_path = RelPath::parse("target.md").unwrap();
    let mod_fp = writer.create_exclusive(&mod_path, b"state 1\n").unwrap();

    let op_id = "test-third-state";
    let steps = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: "target.md".to_string(),
        before_hash: Some(mod_fp.hex()),
        after_hash: Fingerprint::of_bytes(b"state 2\n").hex(),
        before_bytes: Some(b"state 1\n".to_vec()),
        after_bytes: Some(b"state 2\n".to_vec()),
    }];

    // Execute with crash before step 0 finishes (or after step 0 executed)
    let _ = writer.execute_op_with_crash_injection(
        op_id,
        "test",
        None,
        serde_json::json!({"test": true}),
        steps,
        Some(0),
    );

    // Now an external tool rewrites target.md to a third state (neither state 1 nor state 2)
    fs::write(dir.path().join("target.md"), b"state 3 from agent\n").unwrap();

    kill_journal_writer(dir.path(), op_id);

    // Run recovery
    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(
            &report.recovery[0],
            RecoveryOutcome::ThirdStateRefused { .. }
        ),
        "{:?}",
        report.recovery
    );

    // target.md MUST NOT be touched!
    let (cur_bytes, _) = writer.read(&mod_path).unwrap();
    assert_eq!(cur_bytes, b"state 3 from agent\n");
}

#[test]
fn test_undo_all_or_nothing_and_force() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let file_b = RelPath::parse("note_b.md").unwrap();
    let fp_b = writer.create_exclusive(&file_b, b"original b\n").unwrap();

    let op_id = "test-undo-op";
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "note_a.md".to_string(),
            before_hash: None,
            after_hash: Fingerprint::of_bytes(b"created a\n").hex(),
            before_bytes: None,
            after_bytes: Some(b"created a\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Modify,
            src: None,
            dst: "note_b.md".to_string(),
            before_hash: Some(fp_b.hex()),
            after_hash: Fingerprint::of_bytes(b"modified b\n").hex(),
            before_bytes: Some(b"original b\n".to_vec()),
            after_bytes: Some(b"modified b\n".to_vec()),
        },
    ];

    writer
        .execute_op(
            op_id,
            "cli",
            None,
            serde_json::json!({"action": "batch_create_mod"}),
            steps,
        )
        .unwrap();

    let file_a = RelPath::parse("note_a.md").unwrap();
    assert_eq!(writer.read(&file_a).unwrap().0, b"created a\n");
    assert_eq!(writer.read(&file_b).unwrap().0, b"modified b\n");

    // Externally tamper with note_a.md
    fs::write(dir.path().join("note_a.md"), b"tampered a\n").unwrap();

    // Ordinary undo refuses all-or-nothing
    let err = writer.undo(op_id, None);
    match err {
        Err(Error::UndoRefused { op, mismatches }) => {
            assert_eq!(op, op_id);
            assert_eq!(mismatches.len(), 1);
            assert_eq!(mismatches[0].path, file_a.to_path());
            assert_eq!(mismatches[0].reason, UndoReason::Changed);
        }
        other => panic!("expected UndoRefused, got {other:?}"),
    }

    // Verify note_b was NOT rolled back under all-or-nothing refusal
    assert_eq!(writer.read(&file_b).unwrap().0, b"modified b\n");

    // Force undo reverts the matching subset (note_b) and skips note_a
    let outcome = writer.undo_force(op_id, None).unwrap();
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].path, file_a.to_path());
    assert!(outcome
        .undone
        .contains(&UndoneItem::Modified(file_b.clone())));

    // note_b is now back to "original b\n"
    assert_eq!(writer.read(&file_b).unwrap().0, b"original b\n");
}

#[test]
fn test_op_log_compact_keeps_200() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // Create 250 ops
    for i in 0..250 {
        let op_id = format!("op-{:04}", i);
        let note_name = format!("log_note_{:04}.md", i);
        let steps = vec![StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: note_name.clone(),
            before_hash: None,
            after_hash: Fingerprint::of_bytes(format!("content {i}\n").as_bytes()).hex(),
            before_bytes: None,
            after_bytes: Some(format!("content {i}\n").into_bytes()),
        }];
        writer
            .execute_op(&op_id, "cli", None, serde_json::json!({"idx": i}), steps)
            .unwrap();
    }

    // Reopen writer to trigger compact on open
    let writer2 = Writer::open(dir.path()).unwrap();

    let entries = writer2.op_log_entries().unwrap();
    let mut distinct_ops = std::collections::HashSet::new();
    for entry in &entries {
        distinct_ops.insert(entry.op_id.clone());
    }

    assert_eq!(distinct_ops.len(), 200, "must keep exactly 200 ops");
    // Oldest ops (op-0000 .. op-0049) must be purged
    assert!(!distinct_ops.contains("op-0000"));
    assert!(!distinct_ops.contains("op-0049"));
    // Newest ops (op-0050 .. op-0249) must be kept
    assert!(distinct_ops.contains("op-0050"));
    assert!(distinct_ops.contains("op-0249"));
}

#[test]
fn test_temp_sweep() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // Place legitimate files
    let regular = dir.path().join("keep.md");
    fs::write(&regular, b"keep\n").unwrap();

    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let sub_regular = sub.join("keep2.md");
    fs::write(&sub_regular, b"keep2\n").unwrap();

    // Drop leftover dot-temp files
    let temp1 = dir.path().join(".dimagine.tmp.dead1");
    let temp2 = sub.join(".dimagine.tmp.dead2");
    let dimagine_dir = dir.path().join(".dimagine");
    fs::create_dir_all(&dimagine_dir).unwrap();
    let temp3 = dimagine_dir.join(".dimagine.tmp.dead3");

    fs::write(&temp1, b"temp1\n").unwrap();
    fs::write(&temp2, b"temp2\n").unwrap();
    fs::write(&temp3, b"temp3\n").unwrap();

    assert!(temp1.exists());
    assert!(temp2.exists());
    assert!(temp3.exists());

    // Run sweep
    writer.startup_sweep().unwrap();

    // Temps removed
    assert!(!temp1.exists());
    assert!(!temp2.exists());
    assert!(!temp3.exists());

    // Regular files untouched
    assert!(regular.exists());
    assert!(sub_regular.exists());
}

#[test]
fn test_partial_failure_reaches_terminal_state_and_survives_reopen() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    writer
        .create_exclusive(&RelPath::parse("taken.md").unwrap(), b"theirs\n")
        .unwrap();

    // Step 0 lands, step 1 collides at execution time. ADR-016 §7: the
    // landed file counts as success, the rest comes back as per-file
    // errors, and nothing is rolled back — neither now nor by a later
    // recovery that must not mistake this for a crash.
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "landed.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"ok\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "taken.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"clash\n".to_vec()),
        },
    ];
    match writer.execute_op(
        "op-partial-terminal",
        "test",
        None,
        serde_json::json!({}),
        steps,
    ) {
        Err(Error::BatchPartial { op, errors }) => {
            assert_eq!(op, "op-partial-terminal");
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].step_idx, 1);
        }
        other => panic!("expected BatchPartial, got {other:?}"),
    }
    assert_eq!(
        writer
            .read(&RelPath::parse("landed.md").unwrap())
            .unwrap()
            .0,
        b"ok\n"
    );
    // Terminal state: no journal left for recovery to misread as a crash.
    assert!(!dir
        .path()
        .join(".dimagine/journal/op-partial-terminal")
        .exists());
    // The landed file is in the audit log, so undo knows about it.
    assert!(writer
        .op_log_entries()
        .unwrap()
        .iter()
        .any(|e| e.op_id == "op-partial-terminal"));

    // Reopen (runs recovery): the landed file must survive it.
    drop(writer);
    let reopened = Writer::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .read(&RelPath::parse("landed.md").unwrap())
            .unwrap()
            .0,
        b"ok\n"
    );
    assert_eq!(
        reopened
            .read(&RelPath::parse("taken.md").unwrap())
            .unwrap()
            .0,
        b"theirs\n"
    );
}

#[test]
fn test_partial_marker_recovery_touches_nothing() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    writer
        .create_exclusive(&RelPath::parse("taken.md").unwrap(), b"theirs\n")
        .unwrap();

    // Step 0 lands, step 1 collides, and the operation dies
    // between writing the terminal `partial` marker and
    // removing the journal (RW60b F2): the marker, not the
    // directory's existence, is what recovery must trust.
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "landed.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"ok\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "taken.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"clash\n".to_vec()),
        },
    ];
    assert!(matches!(
        writer.execute_op(
            "op-partial-marker",
            "test",
            None,
            serde_json::json!({}),
            steps,
        ),
        Err(Error::BatchPartial { .. })
    ));

    // Rebuild the crash-left state: an uncommitted journal
    // whose terminal partial marker survived the removal.
    let op_dir = dir.path().join(".dimagine/journal/op-partial-marker");
    fs::create_dir_all(&op_dir).unwrap();
    let meta = dimagine_write::journal::JournalMeta {
        op_id: "op-partial-marker".to_string(),
        pid: 0,
        boot_id: None,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
    };
    fs::write(op_dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    fs::write(op_dir.join("partial"), b"").unwrap();

    let report = writer.startup_sweep().unwrap();
    assert!(
        matches!(&report.recovery[0], RecoveryOutcome::PartialLanded { .. }),
        "{:?}",
        report.recovery
    );
    // A subset landed and the caller was told which: recovery
    // keeps the landed file, leaves the colliding target
    // alone, and removes the journal.
    assert_eq!(
        writer
            .read(&RelPath::parse("landed.md").unwrap())
            .unwrap()
            .0,
        b"ok\n"
    );
    assert_eq!(
        writer.read(&RelPath::parse("taken.md").unwrap()).unwrap().0,
        b"theirs\n"
    );
    assert!(!op_dir.exists());
    // The terminal state is stable: a later sweep finds nothing.
    let report = writer.startup_sweep().unwrap();
    assert!(report.recovery.is_empty());
}

#[test]
fn test_landed_but_unaudited_is_reported_as_landed() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // A directory where the audit log file should be makes
    // every op-log append fail right after a step's write
    // landed (RW60b F3).
    fs::create_dir_all(dir.path().join(".dimagine/log")).unwrap();
    fs::create_dir(dir.path().join(".dimagine/log/ops.jsonl")).unwrap();

    let steps = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Create,
        src: None,
        dst: "created.md".to_string(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(b"new\n".to_vec()),
    }];
    match writer.execute_op("op-unaudited", "test", None, serde_json::json!({}), steps) {
        Err(Error::BatchPartial { errors, .. }) => {
            assert_eq!(errors.len(), 1);
            assert!(
                errors[0].landed_unaudited,
                "the write landed; only its audit line failed"
            );
        }
        other => panic!("expected BatchPartial, got {other:?}"),
    }
    // The change is on disk: a caller must never retry it
    // as a fresh write.
    assert_eq!(
        writer
            .read(&RelPath::parse("created.md").unwrap())
            .unwrap()
            .0,
        b"new\n"
    );
    // And no audit line exists for it.
    assert!(writer.op_log_entries().is_err());
}

#[test]
fn test_recovery_applies_known_steps_beside_third_state() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let victim = RelPath::parse("victim.md").unwrap();
    writer.create_exclusive(&victim, b"before\n").unwrap();

    // Two-step uncommitted op, crashed after both landed. Step 0 will be in
    // a third state (rewritten externally); step 1 is a clean Executed.
    let op_id = "op-mixed-third";
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Modify,
            src: None,
            dst: "victim.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"after\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "sibling.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"sibling\n".to_vec()),
        },
    ];
    assert!(
        writer
            .execute_op_with_crash_injection(
                op_id,
                "test",
                None,
                serde_json::json!({}),
                steps,
                Some(1),
            )
            .is_err()
    );
    assert_eq!(writer.read(&victim).unwrap().0, b"after\n");

    // An external tool rewrites step 0's file into a third state.
    fs::write(dir.path().join("victim.md"), b"third state\n").unwrap();

    kill_journal_writer(dir.path(), op_id);
    let report = writer.startup_sweep().unwrap();
    match &report.recovery[0] {
        RecoveryOutcome::ThirdStateRefused {
            op_id: got,
            applied_steps,
            ..
        } => {
            assert_eq!(got, op_id);
            assert_eq!(*applied_steps, 1);
        }
        other => panic!("expected ThirdStateRefused with 1 applied step, got {other:?}"),
    }
    // The known Executed step was rolled back; the third-state file is
    // untouched.
    assert!(writer.read(&RelPath::parse("sibling.md").unwrap()).is_err());
    assert_eq!(writer.read(&victim).unwrap().0, b"third state\n");
    // The journal stays as the incomplete marker: the next open re-reports
    // the third-state file instead of silently dropping the report, and the
    // already-resolved step is not applied twice.
    assert!(dir.path().join(".dimagine/journal").join(op_id).exists());
    let report2 = writer.startup_sweep().unwrap();
    assert!(matches!(
        &report2.recovery[0],
        RecoveryOutcome::ThirdStateRefused { .. }
    ));
    assert_eq!(writer.read(&victim).unwrap().0, b"third state\n");

    // Committed direction: a NotExecuted step beside a third-state step is
    // still rolled forward while the third-state file is left alone.
    let op_id2 = "op-mixed-committed";
    let steps2 = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Modify,
            src: None,
            dst: "victim.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"after\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "other.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"other\n".to_vec()),
        },
    ];
    assert!(writer
        .execute_op_with_crash_injection(
            op_id2,
            "test",
            None,
            serde_json::json!({}),
            steps2,
            Some(2),
        )
        .is_err());
    // External interference after the committed crash: step 0 goes third
    // state, step 1 goes back to NotExecuted.
    fs::write(dir.path().join("victim.md"), b"third state again\n").unwrap();
    fs::remove_file(dir.path().join("other.md")).unwrap();

    kill_journal_writer(dir.path(), op_id2);
    let report3 = writer.startup_sweep().unwrap();
    let found = report3.recovery.iter().find(
        |o| matches!(o, RecoveryOutcome::ThirdStateRefused { op_id: got, .. } if got == op_id2),
    );
    match found {
        Some(RecoveryOutcome::ThirdStateRefused { applied_steps, .. }) => {
            assert_eq!(*applied_steps, 1);
        }
        other => panic!("expected ThirdStateRefused with 1 applied step, got {other:?}"),
    }
    assert_eq!(
        writer.read(&RelPath::parse("other.md").unwrap()).unwrap().0,
        b"other\n"
    );
    assert_eq!(writer.read(&victim).unwrap().0, b"third state again\n");
}

#[test]
fn test_next_free_name_is_library_wide_and_fold_aware() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // A fold-colliding sibling in another folder counts (ADR-017 §4
    // library-wide rule): folder-scoped checking would hand out "girl.jpg".
    writer
        .create_exclusive(&RelPath::parse("refs/girl.jpg").unwrap(), b"img\n")
        .unwrap();

    let free_ = writer.next_free_name(None, "girl", "jpg").unwrap();
    assert_eq!(free_.text(), "girl 2.jpg");

    // The create path applies the same fold rule instead of relying on the
    // kernel's exact-name EEXIST (which never fires for a case-only
    // collision on a case-sensitive filesystem).
    let (path, _) = writer
        .create_with_next_free_name(None, "Girl", "jpg", b"img2\n")
        .unwrap();
    assert_eq!(path.text(), "Girl 2.jpg");
    assert_eq!(writer.read(&path).unwrap().0, b"img2\n");

    // NFC variants fold together: precomposed "é" and "e" + combining acute
    // are the same name.
    writer
        .create_exclusive(&RelPath::parse("caf\u{e9}.md").unwrap(), b"x\n")
        .unwrap();
    let free_nfc = writer.next_free_name(None, "cafe\u{301}", "md").unwrap();
    assert_eq!(free_nfc.text(), "caf\u{e9} 2.md");
}

#[test]
fn test_create_modify_without_after_bytes_refused_up_front() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    // A Create step with after_bytes: None must not silently write empty
    // bytes: the whole operation is refused before any file moves.
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: "fine.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(b"fine\n".to_vec()),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Create,
            src: None,
            dst: "empty.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        },
    ];
    assert!(matches!(
        writer.execute_op("op-no-bytes", "test", None, serde_json::json!({}), steps),
        Err(Error::BadJournal { .. })
    ));
    assert!(writer.read(&RelPath::parse("fine.md").unwrap()).is_err());
    assert!(writer.read(&RelPath::parse("empty.md").unwrap()).is_err());
    assert!(!dir.path().join(".dimagine/journal/op-no-bytes").exists());
}

#[test]
fn test_apply_intent_retry_exhaustion_returns_conflict() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let note_path = RelPath::parse("note.md").unwrap();
    writer.create_exclusive(&note_path, b"start\n").unwrap();

    // Every attempt races: the intent rewrites the file out-of-band before
    // computing, so no CAS commit ever matches and all 4 attempts conflict.
    let mut called = 0;
    let res = writer.apply_intent(&note_path, |b| {
        called += 1;
        fs::write(
            dir.path().join("note.md"),
            format!("external {called}\n").as_bytes(),
        )
        .unwrap();
        let mut updated = b.to_vec();
        updated.extend_from_slice(b"+tag\n");
        Ok(Some(updated))
    });
    assert_eq!(called, 4, "intent must be tried 4 times before giving up");
    match res {
        Err(Error::Conflict(conflict)) => {
            assert_eq!(
                conflict.current_bytes,
                format!("external {called}\n").into_bytes()
            );
        }
        other => panic!("expected Conflict after exhausted retries, got {other:?}"),
    }
}

/// A whole-replace operation carries the hash of the note as the caller read
/// it (ADR-016 §3). A mismatch must refuse the whole operation with a typed
/// `Conflict` before any file moves, and a match must land the edit.
#[test]
fn execute_op_guarded_refuses_a_stale_precondition_and_writes_nothing() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let note = RelPath::parse("note.md").unwrap();
    writer.create_exclusive(&note, b"one\n").unwrap();
    let (_, stale) = writer.read(&note).unwrap();

    // The note changes out from under the caller before the guarded op runs.
    fs::write(dir.path().join("note.md"), b"two\n").unwrap();

    let modify = |after: &[u8]| StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: "note.md".to_string(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(after.to_vec()),
    };

    match writer.execute_op_guarded(
        "op-stale",
        "test",
        None,
        serde_json::json!({}),
        vec![modify(b"three\n")],
        &[Precondition::from_read(
            StepPath::Library(note.clone()),
            stale.clone(),
        )],
    ) {
        Err(Error::Conflict(conflict)) => {
            assert_eq!(conflict.current_bytes, b"two\n");
            assert_eq!(conflict.expected.hex(), stale.hex());
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Nothing was written and no journal was left behind.
    assert_eq!(fs::read(dir.path().join("note.md")).unwrap(), b"two\n");
    assert!(!dir.path().join(".dimagine/journal/op-stale").exists());

    // With the fingerprint the caller just read, the same edit lands.
    let (_, fresh) = writer.read(&note).unwrap();
    writer
        .execute_op_guarded(
            "op-fresh",
            "test",
            None,
            serde_json::json!({}),
            vec![modify(b"three\n")],
            &[Precondition::from_read(
                StepPath::Library(note.clone()),
                fresh.clone(),
            )],
        )
        .unwrap();
    assert_eq!(fs::read(dir.path().join("note.md")).unwrap(), b"three\n");
}

/// A web client holds only the note's sha256, not its identity fields. The
/// precondition is on the digest (ADR-016 §3), so a `Fingerprint::from_hex`
/// expectation must accept the matching bytes and refuse any other.
#[test]
fn execute_op_guarded_accepts_a_digest_only_precondition() {
    let dir = tempdir().unwrap();
    let writer = Writer::open_raw(dir.path()).unwrap();

    let note = RelPath::parse("note.md").unwrap();
    writer.create_exclusive(&note, b"alpha\n").unwrap();
    let digest = Fingerprint::of_bytes(b"alpha\n").hex();
    let expect = Fingerprint::from_hex(&digest).unwrap();
    assert_eq!(expect.hex(), digest);
    assert!(Fingerprint::from_hex("not-hex").is_none());
    assert!(Fingerprint::from_hex(&"0".repeat(63)).is_none());

    let modify = |after: &[u8]| StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: "note.md".to_string(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(after.to_vec()),
    };

    writer
        .execute_op_guarded(
            "op-digest-ok",
            "test",
            None,
            serde_json::json!({}),
            vec![modify(b"beta\n")],
            &[Precondition::from_digest(
                StepPath::Library(note.clone()),
                expect.clone(),
            )],
        )
        .unwrap();
    assert_eq!(fs::read(dir.path().join("note.md")).unwrap(), b"beta\n");

    // The old digest no longer describes the file: a 409, nothing written.
    match writer.execute_op_guarded(
        "op-digest-stale",
        "test",
        None,
        serde_json::json!({}),
        vec![modify(b"gamma\n")],
        &[Precondition::from_digest(
            StepPath::Library(note.clone()),
            expect.clone(),
        )],
    ) {
        Err(Error::Conflict(conflict)) => assert_eq!(conflict.current_bytes, b"beta\n"),
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(fs::read(dir.path().join("note.md")).unwrap(), b"beta\n");
}
