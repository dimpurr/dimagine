//! The step shape a move-with-link-repair needs (ADR-017 §8): a file is
//! renamed and then edited at its new path inside one operation. The journal
//! is written before any file moves, so such an edit rides its own before
//! bytes, and undo's whole-operation check is per path, not per line
//! (ADR-016 §7, §8).

use std::fs;

use dimagine_write::journal::{StepKind, StepRecord};
use dimagine_write::Writer;
use tempfile::tempdir;

const IMAGE: &[u8] = b"jpeg bytes\n";
const NOTE_BEFORE: &[u8] = b"---\nid: 01A\n---\nsee ![[refs/girl.jpg]]\n";
const NOTE_AFTER: &[u8] = b"---\nid: 01A\n---\nsee ![[new/girl.jpg]]\n";

fn put(path: std::path::PathBuf, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn move_step(idx: usize, src: &str, dst: &str) -> StepRecord {
    StepRecord {
        step_idx: idx,
        kind: StepKind::Move,
        src: Some(src.to_string()),
        dst: dst.to_string(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: None,
    }
}

/// The ADR-017 §8 order: every rename, then the moved note's edit at its new
/// path, carried with the bytes the caller read from the old path.
fn move_and_edit_steps() -> Vec<StepRecord> {
    vec![
        move_step(0, "refs/girl.jpg", "new/girl.jpg"),
        move_step(1, "refs/girl.jpg.md", "new/girl.jpg.md"),
        StepRecord {
            step_idx: 2,
            kind: StepKind::Modify,
            src: None,
            dst: "new/girl.jpg.md".to_string(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: Some(NOTE_BEFORE.to_vec()),
            after_bytes: Some(NOTE_AFTER.to_vec()),
        },
    ]
}

#[test]
fn a_move_can_edit_the_moved_note_at_its_new_path_and_undo_restores_it() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    put(root.join("refs/girl.jpg"), IMAGE);
    put(root.join("refs/girl.jpg.md"), NOTE_BEFORE);

    writer
        .execute_op(
            "OP-MOVE-EDIT-00000000000001",
            "test",
            None,
            serde_json::json!({"action": "move"}),
            move_and_edit_steps(),
        )
        .expect("the rename-then-edit operation lands");
    assert_eq!(fs::read(root.join("new/girl.jpg")).unwrap(), IMAGE);
    assert_eq!(fs::read(root.join("new/girl.jpg.md")).unwrap(), NOTE_AFTER);
    assert!(!root.join("refs/girl.jpg").exists());

    writer
        .undo("OP-MOVE-EDIT-00000000000001", None)
        .expect("the whole move undoes: the later edit makes the moved file's final state");
    assert_eq!(fs::read(root.join("refs/girl.jpg")).unwrap(), IMAGE);
    assert_eq!(
        fs::read(root.join("refs/girl.jpg.md")).unwrap(),
        NOTE_BEFORE
    );
    assert!(!root.join("new/girl.jpg").exists());
}

#[test]
fn an_undo_refuses_when_the_moved_note_was_edited_out_of_band() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    put(root.join("refs/girl.jpg"), IMAGE);
    put(root.join("refs/girl.jpg.md"), NOTE_BEFORE);

    writer
        .execute_op(
            "OP-MOVE-EDIT-00000000000002",
            "test",
            None,
            serde_json::json!({"action": "move"}),
            move_and_edit_steps(),
        )
        .unwrap();
    fs::write(root.join("new/girl.jpg.md"), b"someone edited this").unwrap();

    let error = writer
        .undo("OP-MOVE-EDIT-00000000000002", None)
        .expect_err("a third-state file blocks the undo");
    let text = error.to_string();
    assert!(text.contains("new/girl.jpg.md"), "{text}");
}

#[test]
fn a_modify_still_refuses_a_missing_file_it_cannot_describe() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let writer = Writer::open_raw(root).unwrap();
    put(root.join("shelf.md"), b"shelf\n");

    let error = writer
        .execute_op(
            "OP-MOVE-EDIT-00000000000003",
            "test",
            None,
            serde_json::json!({"action": "move"}),
            vec![StepRecord {
                step_idx: 0,
                kind: StepKind::Modify,
                src: None,
                dst: "absent/missing.md".to_string(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: Some(NOTE_AFTER.to_vec()),
            }],
        )
        .expect_err("a modify of a file nothing moves in is still refused");
    assert!(error.to_string().contains("before bytes"), "{error}");
    assert_eq!(fs::read(root.join("shelf.md")).unwrap(), b"shelf\n");
    assert!(!root.join("absent").exists());
}
