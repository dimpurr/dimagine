use dimagine_core::tag_batch::{edit_note_tags, TagOperation};

#[test]
fn legacy_merge_move_and_remove_preserve_unrelated_bytes() {
    let input = b"---\r\ntitle: Keep\r\ntags: [Old Name, target, tree/child, tree2, remove me]\r\nother: value\r\n---\r\nBody\r\n";
    let operations = [
        TagOperation::Merge {
            sources: vec!["Old Name".into()],
            target: "Target".into(),
        },
        TagOperation::Move {
            source: "tree".into(),
            target: "new".into(),
        },
        TagOperation::Remove {
            source: "remove me".into(),
        },
    ];
    let edit = edit_note_tags(input, &operations).unwrap();
    assert_eq!(edit.deduplicated, 1);
    assert_eq!(edit.bytes, b"---\r\ntitle: Keep\r\ntags: [target, tree2, new/child]\r\nother: value\r\n---\r\nBody\r\n");
}

#[test]
fn refuses_invalid_targets_duplicate_sources_and_uneditable_notes() {
    let rename = |source: &str, target: &str| TagOperation::Rename {
        source: source.into(),
        target: target.into(),
    };
    assert!(edit_note_tags(b"---\ntags: [a]\n---\n", &[rename("a", "bad//target")]).is_err());
    assert!(edit_note_tags(
        b"---\ntags: [a]\n---\n",
        &[rename("a", "b"), rename("A", "c")]
    )
    .is_err());
    assert!(edit_note_tags(b"---\ntags: [a, 'b']\n---\n", &[rename("a", "c")]).is_err());
    assert!(edit_note_tags(
        b"---\ntags: [a]\n---\n",
        &[TagOperation::Move {
            source: "a".into(),
            target: "a/b".into()
        }]
    )
    .is_err());
}

#[test]
fn library_plan_snapshots_and_detects_staleness_and_rename_collision() {
    use dimagine_core::tag_batch::plan_tag_batch;
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::write(
        dir.path().join("nested/a.jpg.md"),
        b"---\ntags: [old]\n---\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.md"), b"---\ntags: [Target]\n---\n").unwrap();
    fs::write(dir.path().join("a.jpg"), b"image bytes").unwrap();
    let ops = [TagOperation::Rename {
        source: "old".into(),
        target: "target".into(),
    }];
    let first = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert_eq!(first.notes.len(), 2);
    assert_eq!(first.conflicts.len(), 1);
    assert!(!first.ready());
    assert_eq!(first, plan_tag_batch(dir.path(), &ops, None).unwrap());
    fs::write(dir.path().join("new.md"), b"unchanged note").unwrap();
    assert_ne!(
        first.digest,
        plan_tag_batch(dir.path(), &ops, None).unwrap().digest
    );
    assert_eq!(fs::read(dir.path().join("a.jpg")).unwrap(), b"image bytes");
}

#[test]
fn alias_preparation_and_unreadable_notes_fail_closed() {
    use dimagine_core::tag_batch::plan_tag_batch;
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let ops = [TagOperation::Remove {
        source: "old name".into(),
    }];
    let proposed = br#"{"version":1,"aliases":{"old name":"new"}}"#;
    let plan = plan_tag_batch(dir.path(), &ops, Some(proposed)).unwrap();
    assert!(plan.ready());
    let aliases = plan.aliases.unwrap();
    assert_eq!(aliases.before, None);
    assert_eq!(aliases.after.as_deref(), Some(proposed.as_slice()));
    assert!(!dir.path().join("_dimagine").exists());
    assert!(plan_tag_batch(dir.path(), &ops, Some(b"bad")).is_err());
    fs::create_dir(dir.path().join("_dimagine")).unwrap();
    fs::write(dir.path().join("_dimagine/tag-aliases.json"), b"invalid").unwrap();
    fs::write(dir.path().join("broken.md"), b"---\ntags: [oops\n").unwrap();
    let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert!(!plan.ready());
    assert_eq!(plan.unreadable.len(), 2);
    assert_eq!(plan.notes[0].before, b"---\ntags: [oops\n");
    assert!(plan.aliases.is_none());
}

#[test]
fn alias_snapshot_includes_existing_bytes_and_detects_changes() {
    use dimagine_core::tag_batch::plan_tag_batch;
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("_dimagine")).unwrap();
    let path = dir.path().join("_dimagine/tag-aliases.json");
    let old = br#"{"version":1,"aliases":{}}"#;
    fs::write(&path, old).unwrap();
    let ops = [TagOperation::Remove { source: "x".into() }];
    let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert_eq!(
        plan.aliases.as_ref().unwrap().before.as_deref(),
        Some(old.as_slice())
    );
    fs::write(&path, br#"{"version":1,"aliases":{"x":"y"}}"#).unwrap();
    assert_ne!(
        plan.digest,
        plan_tag_batch(dir.path(), &ops, None).unwrap().digest
    );
}

#[test]
fn rename_collision_accounts_for_earlier_operations() {
    use dimagine_core::tag_batch::plan_tag_batch;
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), b"---\ntags: [a]\n---\n").unwrap();
    fs::write(dir.path().join("b.md"), b"---\ntags: [b]\n---\n").unwrap();
    let ops = [
        TagOperation::Rename {
            source: "a".into(),
            target: "c".into(),
        },
        TagOperation::Rename {
            source: "b".into(),
            target: "c".into(),
        },
    ];
    let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    assert!(!plan.ready());
}

#[test]
fn redundant_parent_requires_a_same_note_slash_descendant_and_runs_last() {
    let op = TagOperation::RemoveRedundantParent;
    let raw = b"---\ntags: [source, source/telegram, source/telegram/dimcamera, sources, other/child]\n---\nBody\n";
    let edit = edit_note_tags(raw, std::slice::from_ref(&op)).unwrap();
    let tags = dimagine_core::fm_edit::editable_tags(&edit.bytes).unwrap();
    assert_eq!(
        tags,
        ["source/telegram/dimcamera", "sources", "other/child"]
    );
    let untouched = b"---\ntags: [source, sources/child]\n---\n";
    assert_eq!(
        edit_note_tags(untouched, std::slice::from_ref(&op))
            .unwrap()
            .bytes,
        untouched
    );
    assert!(edit_note_tags(raw, &[op.clone(), op]).is_err());
    let transformed = edit_note_tags(
        b"---\ntags: [old, source]\n---\n",
        &[
            TagOperation::Rename {
                source: "old".into(),
                target: "source/child".into(),
            },
            TagOperation::RemoveRedundantParent,
        ],
    )
    .unwrap();
    assert_eq!(
        dimagine_core::fm_edit::editable_tags(&transformed.bytes).unwrap(),
        ["source/child"]
    );
}

#[test]
fn composed_operations_and_alias_snapshot_match_a_fresh_plan() {
    use dimagine_core::tag_batch::plan_tag_batch;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.md"),
        b"---\ntags: [a, b, tree/leaf]\ntitle: untouched\n---\nBody\n",
    )
    .unwrap();
    let ops = [
        TagOperation::Rename {
            source: "a".into(),
            target: "new".into(),
        },
        TagOperation::Remove { source: "b".into() },
        TagOperation::Move {
            source: "tree".into(),
            target: "branch".into(),
        },
    ];
    let mut plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert_eq!(plan.notes.len(), 1);
    assert_eq!(
        plan.notes[0].after,
        b"---\ntags: [new, branch/leaf]\ntitle: untouched\n---\nBody\n"
    );
    let aliases = br#"{"version":1,"aliases":{"a":"new"}}"#;
    plan.set_alias_after(aliases).unwrap();
    assert_eq!(
        plan,
        plan_tag_batch(dir.path(), &ops, Some(aliases)).unwrap()
    );
}

#[test]
fn remembered_aliases_follow_operations_in_order() {
    use dimagine_core::tag_batch::{plan_tag_batch, remembered_aliases};
    let dir = tempfile::tempdir().unwrap();
    for (name, tag) in [("1", "a"), ("2", "b/x"), ("3", "b")] {
        std::fs::write(
            dir.path().join(format!("{name}.md")),
            format!("---\ntags: [{tag}]\n---\n"),
        )
        .unwrap();
    }
    let rename = TagOperation::Rename {
        source: "a".into(),
        target: "b/c".into(),
    };
    let move_op = TagOperation::Move {
        source: "b".into(),
        target: "deep".into(),
    };
    for (ops, expected) in [
        (vec![rename.clone(), move_op.clone()], "deep/c"),
        (vec![move_op, rename], "b/c"),
    ] {
        let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
        let aliases: serde_json::Value =
            serde_json::from_slice(&remembered_aliases(&plan).unwrap().unwrap()).unwrap();
        assert_eq!(aliases["aliases"]["a"], expected);
    }
}

#[test]
fn remembered_alias_omits_a_name_removed_as_redundant_parent() {
    use dimagine_core::tag_batch::{plan_tag_batch, remembered_aliases};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("one.md"),
        "---\ntags: [a, b/c/child]\n---\n",
    )
    .unwrap();
    let ops = vec![
        TagOperation::Rename {
            source: "a".into(),
            target: "b/c".into(),
        },
        TagOperation::RemoveRedundantParent,
    ];
    let plan = plan_tag_batch(dir.path(), &ops, None).unwrap();
    assert!(remembered_aliases(&plan).unwrap().is_none());
}
