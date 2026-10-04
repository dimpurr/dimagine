//! End-to-end CLI tests for the core-plugins switch
//! (`<library>/.dimagine/core-plugins.json`, ADR-013): a disabled built-in
//! plugin disappears from help and usage.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        "dimagine-cli-plugins-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create plugin test dir");
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

fn set_switch(library: &Path, body: &str) {
    let path = library.join(".dimagine/core-plugins.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// The library used for import (fixture-free): an empty folder with two
/// images, so scan works and import could run if it were enabled.
fn library(tag: &str) -> TempDir {
    let dir = tmp(tag);
    std::fs::write(dir.0.join("one.jpg"), b"not really a jpeg").unwrap();
    dir
}

#[test]
fn switch_off_hides_import_eagle() {
    let lib = library("off");
    set_switch(&lib.0, r#"{"import-eagle": false}"#);
    let lib_arg = lib.0.to_str().unwrap();

    let (code, stdout, _) = dimagine(&["--library", lib_arg, "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("scan"), "{stdout}");
    assert!(stdout.contains("check"), "{stdout}");
    assert!(
        !stdout.contains("import"),
        "a disabled plugin is not offered: {stdout}"
    );

    let (code, _, stderr) = dimagine(&["--library", lib_arg, "import", "eagle", "x.library"]);
    assert_eq!(code, 2, "disabled subcommands are usage errors: {stderr}");
    assert!(stderr.contains("unrecognized subcommand"), "{stderr}");
}

#[test]
fn switch_missing_file_offers_everything() {
    let lib = tmp("missing");
    let lib_arg = lib.0.to_str().unwrap();
    let (code, stdout, _) = dimagine(&["--library", lib_arg, "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("scan"), "{stdout}");
    assert!(stdout.contains("check"), "{stdout}");
    assert!(stdout.contains("import"), "{stdout}");
}

#[test]
fn switch_malformed_file_warns_and_offers_everything() {
    let lib = tmp("malformed");
    set_switch(&lib.0, "{definitely not json");
    let lib_arg = lib.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["--library", lib_arg, "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("import"), "{stdout}");
    assert!(stderr.contains("core-plugins.json"), "{stderr}");
    assert!(
        stderr.contains("dimagine:"),
        "a proper warning prefix: {stderr}"
    );
}

#[test]
fn switch_unknown_and_wrong_typed_keys_warn_but_keep_defaults() {
    let lib = tmp("unknown");
    set_switch(&lib.0, r#"{"vector-search": true, "import-eagle": "off"}"#);
    let lib_arg = lib.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["--library", lib_arg, "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("import"), "{stdout}");
    assert!(stderr.contains("unknown plugin key"), "{stderr}");
    assert!(stderr.contains("expected a boolean"), "{stderr}");
}

#[test]
fn switch_false_for_all_plugins_leaves_scan_and_check() {
    let lib = tmp("alloff");
    set_switch(
        &lib.0,
        r#"{"import-eagle": false, "previews": false, "serve": false}"#,
    );
    let lib_arg = lib.0.to_str().unwrap();
    let (code, stdout, _) = dimagine(&["--library", lib_arg, "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("scan"), "{stdout}");
    assert!(stdout.contains("check"), "{stdout}");
    assert!(!stdout.contains("previews"), "{stdout}");
    assert!(!stdout.contains("serve"), "{stdout}");
    assert!(!stdout.contains("import"), "{stdout}");

    // The always-on commands still run on that library.
    std::fs::write(lib.0.join("a.jpg"), b"x").unwrap();
    let (code, stdout, _) = dimagine(&["--library", lib_arg, "scan", "--json"]);
    assert_eq!(code, 0, "{stdout}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("scan JSON");
    assert_eq!(json["image_total"], 1);
}

#[test]
fn switch_is_honoured_when_the_flag_comes_after_the_subcommand() {
    let lib = library("after");
    set_switch(&lib.0, r#"{"import-eagle": false}"#);
    let lib_arg = lib.0.to_str().unwrap();
    let (code, _, _) = dimagine(&["import", "eagle", "x.library", "--library", lib_arg]);
    assert_eq!(code, 2, "the switch is read from the right library");
}
