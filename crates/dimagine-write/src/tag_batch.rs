//! Locked, journaled application of a previously reviewed tag batch.

use dimagine_core::tag_batch::{plan_tag_batch, TagBatchPlan};

use crate::{Error, Precondition, Result, StepKind, StepPath, StepRecord, Writer};

impl Writer {
    /// Rebuild and compare the entire reviewed plan under the Writer lock, then
    /// apply its changes as one undoable operation. `None` denotes no changes.
    pub fn apply_tag_batch(&self, plan: &TagBatchPlan) -> Result<Option<String>> {
        self.apply_tag_batch_inner(plan, None, &mut |_, _| {})
    }

    /// Apply a batch and report completed journal steps while holding the write lock.
    pub fn apply_tag_batch_with_progress(
        &self,
        plan: &TagBatchPlan,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Option<String>> {
        self.apply_tag_batch_inner(plan, None, progress)
    }

    fn apply_tag_batch_inner(
        &self,
        plan: &TagBatchPlan,
        fail_after: Option<usize>,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Option<String>> {
        let lock = self.lock_library()?;
        // Both locations are reserved as import guards; metadata errors fail closed,
        // including dangling symlinks.
        for name in ["apply.pid", ".dimagine/apply.pid"] {
            let path = self.root_path().join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => return Err(refused(format!("{name} exists"))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(refused(format!("cannot inspect {name}: {e}"))),
            }
        }
        let alias_after = plan.aliases.as_ref().and_then(|a| a.after.as_deref());
        let current =
            plan_tag_batch(self.root_path(), &plan.operations, alias_after).map_err(refused)?;
        if !plan.ready() || !current.ready() {
            return Err(refused("plan has conflicts or unreadable files".into()));
        }
        if *plan != current {
            return Err(refused("plan is stale or has been modified".into()));
        }
        let mut steps = Vec::new();
        let mut expected = Vec::new();
        for note in &current.notes {
            if note.before == note.after {
                continue;
            }
            let path = StepPath::parse(&note.path)?;
            let (bytes, fp) = self.read_step(&path)?;
            if bytes != note.before {
                return Err(refused(format!("{} changed during preflight", note.path)));
            }
            expected.push(Precondition::from_read(path, fp));
            steps.push(step(steps.len(), StepKind::Modify, &note.path, &note.after));
        }
        let alias = current.aliases.as_ref().expect("ready plan has aliases");
        if let Some(after) = &alias.after {
            if alias.before.as_ref() != Some(after) {
                let path = StepPath::parse(dimagine_core::tag_aliases::ALIAS_PATH)?;
                let kind = if let Some(before) = &alias.before {
                    let (bytes, fp) = self.read_step(&path)?;
                    if &bytes != before {
                        return Err(refused("aliases changed during preflight".into()));
                    }
                    expected.push(Precondition::from_read(path, fp));
                    StepKind::Modify
                } else {
                    StepKind::Create
                };
                steps.push(step(
                    steps.len(),
                    kind,
                    dimagine_core::tag_aliases::ALIAS_PATH,
                    after,
                ));
            }
        }
        if steps.is_empty() {
            return Ok(None);
        }
        let op_id = ulid::Ulid::new().to_string();
        self.execute_op_locked_progress(
            &lock,
            &op_id,
            "tag_batch",
            None,
            serde_json::json!({"type": "tag_batch", "digest": plan.digest}),
            steps,
            fail_after,
            expected,
            progress,
        )?;
        Ok(Some(op_id))
    }
}

fn refused(message: String) -> Error {
    Error::IntentRefused {
        path: "tag batch".into(),
        message,
    }
}

fn step(step_idx: usize, kind: StepKind, dst: &str, bytes: &[u8]) -> StepRecord {
    StepRecord {
        step_idx,
        kind,
        src: None,
        dst: dst.into(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(bytes.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dimagine_core::tag_batch::TagOperation;
    use std::fs;

    const NOTE: &[u8] = b"---\ntags: [old]\n---\nBody\n";
    const ALIAS: &[u8] = br#"{"version":1,"aliases":{"old":"new"}}"#;
    fn setup() -> (tempfile::TempDir, Writer, TagBatchPlan) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), NOTE).unwrap();
        fs::write(dir.path().join("b.md"), NOTE).unwrap();
        fs::write(dir.path().join("image.jpg"), b"original image").unwrap();
        let writer = Writer::open_raw(dir.path()).unwrap();
        let plan = plan_tag_batch(
            dir.path(),
            &[TagOperation::Rename {
                source: "old".into(),
                target: "new".into(),
            }],
            Some(ALIAS),
        )
        .unwrap();
        (dir, writer, plan)
    }
    #[test]
    fn reads_continue_while_batch_holds_writer_lock() {
        let (dir, writer, plan) = setup();
        let root = dir.path().to_path_buf();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            writer
                .apply_tag_batch_with_progress(&plan, &mut |done, _| {
                    if done == 1 {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                })
                .unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let library = dimagine_core::library::Library::open(&root).unwrap();
        assert!(library.fully_read());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn apply_and_single_undo_restores_original_bytes_and_trashes_created_alias() {
        let (dir, writer, plan) = setup();
        let op = writer.apply_tag_batch(&plan).unwrap().unwrap();
        assert_eq!(
            fs::read(dir.path().join("_dimagine/tag-aliases.json")).unwrap(),
            ALIAS
        );
        writer.undo(&op, None).unwrap();
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), NOTE);
        assert_eq!(
            fs::read(dir.path().join("image.jpg")).unwrap(),
            b"original image"
        );
        assert!(!dir.path().join("_dimagine").exists());
        assert!(fs::read_dir(dir.path().join(".trash"))
            .unwrap()
            .next()
            .is_some());
    }
    #[test]
    fn alias_undo_and_recovery_keep_nonempty_directory() {
        for recover in [false, true] {
            let (dir, writer, plan) = setup();
            fs::create_dir(dir.path().join("_dimagine")).unwrap();
            fs::write(dir.path().join("_dimagine/keep"), b"keep").unwrap();
            if recover {
                assert!(writer
                    .apply_tag_batch_inner(&plan, Some(2), &mut |_, _| {})
                    .is_err());
                let opdir = fs::read_dir(dir.path().join(".dimagine/journal"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let meta = opdir.join("meta.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
                value["pid"] = 0.into();
                fs::write(meta, serde_json::to_vec(&value).unwrap()).unwrap();
                writer.startup_sweep().unwrap();
            } else {
                let op = writer.apply_tag_batch(&plan).unwrap().unwrap();
                writer.undo(&op, None).unwrap();
            }
            assert_eq!(
                fs::read(dir.path().join("_dimagine/keep")).unwrap(),
                b"keep"
            );
            assert!(!dir.path().join("_dimagine/tag-aliases.json").exists());
        }
    }
    #[test]
    fn redundant_parent_shares_batch_and_undo_with_other_operations() {
        let dir = tempfile::tempdir().unwrap();
        let a = b"---\ntags: [source, source/child, old]\n---\nA\n";
        let b = b"---\ntags: [source, sources/child]\n---\nB\n";
        fs::write(dir.path().join("a.md"), a).unwrap();
        fs::write(dir.path().join("b.md"), b).unwrap();
        let writer = Writer::open_raw(dir.path()).unwrap();
        let ops = [
            TagOperation::Rename {
                source: "old".into(),
                target: "new".into(),
            },
            TagOperation::RemoveRedundantParent,
        ];
        let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
        assert!(plan.ready());
        let op = writer.apply_tag_batch(&plan).unwrap().unwrap();
        let tags =
            dimagine_core::fm_edit::editable_tags(&fs::read(dir.path().join("a.md")).unwrap())
                .unwrap();
        assert_eq!(tags, ["source/child", "new"]);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), b);
        writer.undo(&op, None).unwrap();
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), a);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), b);
    }
    #[test]
    fn stale_notes_aliases_and_pid_are_refused() {
        let (dir, writer, plan) = setup();
        fs::write(dir.path().join("apply.pid"), b"0").unwrap();
        assert!(writer.apply_tag_batch(&plan).is_err());
        fs::remove_file(dir.path().join("apply.pid")).unwrap();
        fs::write(dir.path().join("a.md"), b"external").unwrap();
        assert!(writer.apply_tag_batch(&plan).is_err());
        fs::write(dir.path().join("a.md"), NOTE).unwrap();
        fs::create_dir_all(dir.path().join("_dimagine")).unwrap();
        fs::write(dir.path().join("_dimagine/tag-aliases.json"), ALIAS).unwrap();
        assert!(writer.apply_tag_batch(&plan).is_err());
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), NOTE);
    }
    #[test]
    fn post_preflight_edit_reports_partial_without_overwriting() {
        let (dir, writer, plan) = setup();
        let target = dir.path().join("b.md");
        crate::writer::STEP_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |i| {
                if i == 1 {
                    fs::write(&target, b"external edit").unwrap();
                }
            }))
        });
        let result = writer.apply_tag_batch(&plan);
        crate::writer::STEP_HOOK.with(|h| *h.borrow_mut() = None);
        assert!(
            matches!(result, Err(Error::BatchPartial { .. })),
            "{result:?}"
        );
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), b"external edit");
        assert_ne!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
        assert_eq!(
            fs::read(dir.path().join("image.jpg")).unwrap(),
            b"original image"
        );
    }
    #[test]
    fn interrupted_batch_is_recovered() {
        let (dir, writer, plan) = setup();
        assert!(writer
            .apply_tag_batch_inner(&plan, Some(0), &mut |_, _| {})
            .is_err());
        let journal = dir.path().join(".dimagine/journal");
        let opdir = fs::read_dir(journal)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let meta = opdir.join("meta.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
        value["pid"] = 0.into();
        fs::write(meta, serde_json::to_vec(&value).unwrap()).unwrap();
        let report = writer.startup_sweep().unwrap();
        assert!(!report.recovery.is_empty());
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), NOTE);
        assert_eq!(
            fs::read(dir.path().join("image.jpg")).unwrap(),
            b"original image"
        );
    }
    #[test]
    fn existing_alias_cas_and_undo_restore_bytes() {
        let (dir, writer, _) = setup();
        fs::create_dir_all(dir.path().join("_dimagine")).unwrap();
        let old = br#"{"version":1,"aliases":{}}"#;
        fs::write(dir.path().join("_dimagine/tag-aliases.json"), old).unwrap();
        let ops = [TagOperation::Rename {
            source: "old".into(),
            target: "new".into(),
        }];
        let plan = plan_tag_batch(dir.path(), &ops, Some(ALIAS)).unwrap();
        let target = dir.path().join("_dimagine/tag-aliases.json");
        let external = br#"{"version":1,"aliases":{"other":"new"}}"#;
        crate::writer::STEP_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |i| {
                if i == 2 {
                    fs::write(&target, external).unwrap();
                }
            }))
        });
        let result = writer.apply_tag_batch(&plan);
        crate::writer::STEP_HOOK.with(|h| *h.borrow_mut() = None);
        assert!(matches!(result, Err(Error::BatchPartial { .. })));
        assert_eq!(
            fs::read(dir.path().join("_dimagine/tag-aliases.json")).unwrap(),
            external
        );
        fs::write(dir.path().join("a.md"), NOTE).unwrap();
        fs::write(dir.path().join("b.md"), NOTE).unwrap();
        fs::write(dir.path().join("_dimagine/tag-aliases.json"), old).unwrap();
        let op = writer.apply_tag_batch(&plan).unwrap().unwrap();
        writer.undo(&op, None).unwrap();
        assert_eq!(
            fs::read(dir.path().join("_dimagine/tag-aliases.json")).unwrap(),
            old
        );
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), NOTE);
    }

    #[test]
    fn recovery_trashes_created_alias_after_interruption() {
        let (dir, writer, plan) = setup();
        assert!(writer
            .apply_tag_batch_inner(&plan, Some(2), &mut |_, _| {})
            .is_err());
        let journal = dir.path().join(".dimagine/journal");
        let opdir = fs::read_dir(journal)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let meta = opdir.join("meta.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
        value["pid"] = 0.into();
        fs::write(meta, serde_json::to_vec(&value).unwrap()).unwrap();
        let report = writer.startup_sweep().unwrap();
        assert!(!report.recovery.is_empty());
        assert!(!dir.path().join("_dimagine").exists());
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), NOTE);
        assert_eq!(
            fs::read(dir.path().join("image.jpg")).unwrap(),
            b"original image"
        );
        assert!(fs::read_dir(dir.path().join(".trash"))
            .unwrap()
            .next()
            .is_some());
    }
    #[test]
    fn refuses_unready_and_tampered_plans() {
        let (dir, writer, mut plan) = setup();
        plan.notes[0].after = b"tampered".to_vec();
        assert!(writer.apply_tag_batch(&plan).is_err());
        fs::write(dir.path().join("b.md"), b"---\ntags: [new]\n---\n").unwrap();
        let ops = [TagOperation::Rename {
            source: "old".into(),
            target: "new".into(),
        }];
        let collision = plan_tag_batch(dir.path(), &ops, None).unwrap();
        assert!(!collision.ready());
        assert!(writer.apply_tag_batch(&collision).is_err());
        fs::write(
            dir.path().join("b.md"),
            b"---\ntags: [\"unterminated]\n---\n",
        )
        .unwrap();
        let unreadable = plan_tag_batch(dir.path(), &ops, None).unwrap();
        assert!(!unreadable.ready());
        assert!(writer.apply_tag_batch(&unreadable).is_err());
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), NOTE);
    }
}
