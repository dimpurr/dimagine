#![cfg(feature = "import-eagle")]
//! Compiled only when the matching built-in plugin feature is on; with
//! --no-default-features the plugin subcommands do not exist.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

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
        "dimagine-cli-import-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create import test dir");
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

fn item(root: &Path, id: &str, metadata: serde_json::Value, file: Option<(&str, &[u8])>) {
    let dir = root.join("images").join(format!("{id}.info"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("metadata.json"), metadata.to_string()).unwrap();
    if let Some((name, bytes)) = file {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// An Eagle library with two clean image items in one folder.
fn clean_eagle_library(root: &Path) {
    std::fs::create_dir_all(root.join("images")).unwrap();
    std::fs::write(
        root.join("metadata.json"),
        json!({"version": 4, "folders": [{"id": "f1", "name": "Photos"}]}).to_string(),
    )
    .unwrap();
    item(
        root,
        "a",
        json!({"id": "a", "name": "Alpha", "ext": "png", "folders": ["f1"]}),
        Some(("Alpha.png", b"png-a")),
    );
    item(
        root,
        "b",
        json!({"id": "b", "name": "Beta", "ext": "jpg"}),
        Some(("Beta.jpg", b"jpg-b")),
    );
}

/// An Eagle library with skips: trash, a video, a missing original and a
/// dangling folder reference.
fn messy_eagle_library(root: &Path) {
    clean_eagle_library(root);
    item(
        root,
        "trash",
        json!({"id": "trash", "name": "Trash", "ext": "jpg", "isDeleted": true}),
        Some(("Trash.jpg", b"trash")),
    );
    item(
        root,
        "movie",
        json!({"id": "movie", "name": "Clip", "ext": "mp4"}),
        Some(("Clip.mp4", b"video")),
    );
    item(
        root,
        "nofile",
        json!({"id": "nofile", "name": "Ghost", "ext": "png"}),
        None,
    );
    item(
        root,
        "dangling",
        json!({
            "id": "dangling",
            "name": "Dangling",
            "ext": "png",
            "folders": ["missing-folder"]
        }),
        Some(("Dangling.png", b"png-d")),
    );
}

fn canonical_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(path).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in &entries {
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), output);
            } else {
                output.push((
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .display()
                        .to_string(),
                    std::fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}

#[test]
fn import_eagle_clean_library_exits_zero() {
    let tmp = tmp("clean");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    clean_eagle_library(&source);
    let before = canonical_tree(&source);
    let (code, stdout, stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("imported 2"), "{stdout}");
    assert!(stdout.contains("renamed 0"), "{stdout}");
    assert_eq!(
        canonical_tree(&source),
        before,
        "import reads the source only"
    );
    assert!(
        std::fs::read(destination.join("Eagle/Fixture/Photos/Alpha.png")).is_ok(),
        "imported image at Eagle/Fixture/Photos/Alpha.png"
    );
}

#[test]
fn import_eagle_json_report_flattens_the_import_report() {
    let tmp = tmp("json");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    clean_eagle_library(&source);
    let (code, stdout, stderr) = dimagine(&[
        "--json",
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["schema"], "dimagine.import/0.1");
    assert_eq!(report["imported"].as_array().unwrap().len(), 2);
    assert_eq!(report["renamed"], 0);
    assert_eq!(report["skipped"], serde_json::json!([]));
    assert_eq!(report["folder_counts"]["Photos"], 1);
    let destination = report["library"].as_str().unwrap();
    assert!(Path::new(destination).ends_with("dest"), "{destination}");
}

#[test]
fn import_eagle_partial_import_exits_one() {
    let tmp = tmp("messy");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    messy_eagle_library(&source);
    let (code, stdout, _stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "skips and dangling refs are problems: {stdout}");
    assert!(stdout.contains("imported 3"), "{stdout}");
    assert!(stdout.contains("skipped 3"), "{stdout}");
    assert!(stdout.contains("trash: trash"), "{stdout}");
    assert!(stdout.contains("not_image: movie"), "{stdout}");
    assert!(stdout.contains("original_missing: nofile"), "{stdout}");
    assert!(stdout.contains("dangling folder refs: 1"), "{stdout}");
    assert!(!stdout.contains("dangling folder refs: 0"), "{stdout}");

    let (code, stdout, stderr) = dimagine(&[
        "--json",
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        tmp.0.join("dest-json").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["schema"], "dimagine.import/0.1");
    assert_eq!(report["imported"].as_array().unwrap().len(), 3);
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 3);
    assert!(skipped
        .iter()
        .any(|s| s["reason_code"] == "trash" && s["item"] == "trash"));
    assert_eq!(report["dangling_folder_refs"], 1);
}

#[test]
fn import_eagle_name_flag_sets_the_import_label() {
    let tmp = tmp("name");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    clean_eagle_library(&source);
    let (code, stdout, stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
        "--name",
        "Holidays 2024",
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        std::fs::read(destination.join("Eagle/Holidays 2024/Photos/Alpha.png")).is_ok(),
        "--name replaces the .library folder label"
    );
}

#[test]
fn import_eagle_usage_errors_exit_two() {
    let (code, _, _) = dimagine(&["import"]);
    assert_eq!(code, 2, "missing eagle subcommand is a usage error");
    let (code, _, _) = dimagine(&["import", "eagle"]);
    assert_eq!(code, 2, "missing source is a usage error");
    let (code, _, _) = dimagine(&["import", "picasa", "x"]);
    assert_eq!(code, 2, "unknown import source is a usage error");
}

#[test]
fn import_eagle_bad_source_exits_one() {
    let tmp = tmp("badsource");
    let destination = tmp.0.join("dest");
    let (code, _, stderr) = dimagine(&[
        "import",
        "eagle",
        tmp.0.join("nope.library").to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("No such file or directory"),
        "the importer reports the raw filesystem error: {stderr}"
    );

    // Not an Eagle library: metadata.json is missing.
    let plain = tmp.0.join("plain.library");
    std::fs::create_dir_all(&plain).unwrap();
    let (code, _, stderr) = dimagine(&[
        "import",
        "eagle",
        plain.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not an Eagle library"), "{stderr}");
}

#[test]
fn import_eagle_nonempty_destination_exits_one() {
    let tmp = tmp("nonempty");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    clean_eagle_library(&source);
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("blocker.txt"), b"not a settings folder").unwrap();
    let (code, _, stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("refuse"), "{stderr}");
}

#[test]
fn import_eagle_settings_folders_are_allowed_in_the_destination() {
    let tmp = tmp("settings");
    let source = tmp.0.join("Fixture.library");
    let destination = tmp.0.join("dest");
    clean_eagle_library(&source);
    std::fs::create_dir_all(destination.join(".dimagine")).unwrap();
    std::fs::create_dir_all(destination.join(".obsidian")).unwrap();
    let (code, stdout, stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(std::fs::read(destination.join("Eagle/Fixture/Photos/Alpha.png")).is_ok());
}

#[test]
fn import_eagle_overlapping_paths_are_refused() {
    let tmp = tmp("overlap");
    let source = tmp.0.join("Fixture.library");
    clean_eagle_library(&source);
    let (code, _, stderr) = dimagine(&[
        "import",
        "eagle",
        source.to_str().unwrap(),
        "--library",
        // The destination contains the source, so nothing can be copied
        // safely; the importer refuses before writing anything.
        tmp.0.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("refuse overlapping"), "{stderr}");
    assert!(
        !tmp.0.join("Eagle").exists(),
        "refused imports write nothing"
    );
}

#[test]
fn import_eagle_lists_in_help_and_subcommand_help() {
    let (code, stdout, _) = dimagine(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("import"), "{stdout}");
    let (code, stdout, _) = dimagine(&["import", "eagle", "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("NAME.LIBRARY"), "{stdout}");
    assert!(stdout.contains("--name"), "{stdout}");
}
