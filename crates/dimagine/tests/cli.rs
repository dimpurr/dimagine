//! End-to-end CLI tests: real process, exit codes, JSON documents,
//! read-only behaviour.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_dimagine");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        allow_everything(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn allow_everything(root: &Path) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o755));
            }
        }
    }
}

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");
const JPG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.jpg");

fn tmp(tag: &str) -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create CLI test dir");
    TempDir(dir)
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn dimagine(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(BIN).args(args).output().expect("run dimagine");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run dimagine");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn scan_json_on_untouched_folder() {
    let tmp = tmp("plain");
    write(&tmp.0, "one.jpg", JPG);
    write(&tmp.0, "sub/two.png", PNG);
    let args = vec!["scan", "--json", "--library", tmp.0.to_str().unwrap()];
    let (code, stdout, stderr) = dimagine(&args);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(json["schema"], "dimagine.scan/0.1");
    assert_eq!(json["image_total"], 2);
    assert_eq!(json["images"]["jpeg"], 1);
    assert_eq!(json["images"]["png"], 1);
    assert_eq!(json["read_complete"], true);
    assert_eq!(json["image_notes"]["total"], 0);
}

#[test]
fn scan_human_output_is_plain() {
    let tmp = tmp("human");
    write(&tmp.0, "one.jpg", JPG);
    let (code, stdout, _) = run_in(&tmp.0, &["scan"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("library: "));
    assert!(stdout.contains("images: 1 (jpeg 1)"));
    assert!(!stdout.contains('\u{1b}'), "no colour escapes");
}

#[test]
fn check_clean_library_exits_zero() {
    let tmp = tmp("clean");
    write(&tmp.0, "fine.jpg", JPG);
    let (code, stdout, _) = dimagine(&["check", "--library", tmp.0.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("no problems found"), "{stdout}");
}

#[test]
fn check_findings_exit_one_with_json_schema() {
    let tmp = tmp("bad");
    write(&tmp.0, "mismatch.jpg", PNG);
    let (code, stdout, stderr) =
        dimagine(&["check", "--json", "--library", tmp.0.to_str().unwrap()]);
    assert_eq!(code, 1);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(json["schema"], "dimagine.check/0.1");
    assert_eq!(json["findings"][0]["code"], "format_mismatch");
    assert_eq!(json["findings"][0]["severity"], "error");
    assert_eq!(json["findings"][0]["path"], "mismatch.jpg");
    let _ = stderr;
}

#[test]
fn check_info_only_still_exits_zero() {
    let tmp = tmp("info-only");
    write(&tmp.0, "girl.jpg", JPG);
    write(
        &tmp.0,
        "girl.jpg.md",
        "---\nkind: image\n---\nno self embed here\n",
    );
    let (code, stdout, _) = dimagine(&["check", "--library", tmp.0.to_str().unwrap()]);
    assert_eq!(code, 0, "info findings are not errors");
    assert!(stdout.contains("missing_self_embed"), "{stdout}");
}

#[test]
fn usage_errors_exit_two() {
    let (code, _, _) = dimagine(&["--definitely-not-a-flag"]);
    assert_eq!(code, 2);
    let (code, _, _) = dimagine(&[]);
    assert_eq!(code, 2, "missing subcommand is a usage error");
    let (code, _, _) = dimagine(&["frobnicate"]);
    assert_eq!(code, 2);
}

#[test]
fn library_flag_accepts_folders_otherwise_exit_one() {
    let tmp = tmp("libs");
    let (code, stdout, stderr) =
        dimagine(&["scan", "--library", tmp.0.join("missing").to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not a folder"), "{stderr}");
    let _ = stdout;

    let bad = tmp.0.join("file.txt");
    std::fs::write(&bad, b"x").unwrap();
    let (code, _, stderr) = dimagine(&["check", "--library", bad.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not a folder"));

    let (code, stdout, _) = dimagine(&[
        "scan",
        "--json",
        "--library",
        tmp.0.join("missing").to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("error JSON document");
    assert_eq!(json["schema"], "dimagine.error/0.1");
    assert!(json["error"].as_str().unwrap().contains("not a folder"));
}

#[test]
#[cfg(unix)]
fn unreadable_dir_exits_three_with_warning() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tmp("locked");
    write(&tmp.0, "a.jpg", JPG);
    write(&tmp.0, "locked/b.jpg", JPG);
    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let (code, stdout, _) = dimagine(&["scan", "--library", tmp.0.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(stdout.contains("reading incomplete"), "{stdout}");
    assert!(stdout.contains("proves nothing"), "{stdout}");
    let (code, stdout, _) = dimagine(&["check", "--json", "--library", tmp.0.to_str().unwrap()]);
    assert_eq!(code, 3);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON document");
    assert_eq!(json["read_complete"], false);
    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn flags_work_before_and_after_the_subcommand() {
    let tmp = tmp("flagorder");
    write(&tmp.0, "a.png", PNG);
    let (code, stdout, _) = dimagine(&["--library", tmp.0.to_str().unwrap(), "scan", "--json"]);
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(json["schema"], "dimagine.scan/0.1");
}

#[test]
fn runs_never_modify_the_library() {
    let tmp = tmp("readonly");
    write(&tmp.0, "refs/girl.jpg", JPG);
    write(
        &tmp.0,
        "refs/girl.jpg.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n![[ghost.jpg]]\n",
    );
    write(&tmp.0, "refs/shot[1].png", PNG);
    let before: Vec<(PathBuf, u64)> = collect(&tmp.0);
    let lib = tmp.0.to_str().unwrap();
    let _ = dimagine(&["scan", "--library", lib]);
    let (code, _, _) = dimagine(&["check", "--library", lib]);
    assert_eq!(code, 1, "the library has problems");
    let after: Vec<(PathBuf, u64)> = collect(&tmp.0);
    assert_eq!(before, after, "read-only means byte-identical");
}

fn collect(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let len = std::fs::metadata(&path).unwrap().len();
                files.push((path, len));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn version_and_help_are_available() {
    let (code, stdout, _) = dimagine(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("scan"));
    assert!(stdout.contains("check"));
    let (code, stdout, _) = dimagine(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("dimagine"));
}
