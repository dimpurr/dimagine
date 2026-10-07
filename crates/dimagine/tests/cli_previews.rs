#![cfg(feature = "previews")]
//! Compiled only when the matching built-in plugin feature is on; with
//! --no-default-features the plugin subcommands do not exist.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use dimagine_preview::RENDITION_GENERATION;

const BIN: &str = env!("CARGO_BIN_EXE_dimagine");

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");
const JPG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.jpg");
const GIF: &[u8] = include_bytes!("../../../tests/fixtures/pixel.gif");

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

fn tmp(tag: &str) -> TempDir {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-previews-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create previews test dir");
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

fn originals(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(path: &Path, output: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(path).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in &entries {
            if entry.file_type().unwrap().is_dir() {
                visit(&entry.path(), output);
            } else {
                output.push((
                    entry.path().display().to_string(),
                    std::fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(root, &mut output);
    output
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn previews_generates_then_caches_without_touching_originals() {
    let tmp = tmp("gen");
    write(&tmp.0, "one.jpg", JPG);
    write(&tmp.0, "sub/two.png", PNG);
    let before = originals(&tmp.0);
    let lib = tmp.0.to_str().unwrap();

    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("previews: 2 images (4 generated, 0 cached, 0 failed)"),
        "{stdout}"
    );

    let cache_root = tmp.0.join(".dimagine/cache/previews");
    assert!(
        cache_root.is_dir(),
        "renditions land in .dimagine/cache/previews"
    );
    let files: Vec<(PathBuf, u64)> = std::fs::read_dir(&cache_root)
        .unwrap()
        .flatten()
        .flat_map(|shard| {
            std::fs::read_dir(shard.path())
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .map(|path| {
            let len = std::fs::metadata(&path).unwrap().len();
            (path, len)
        })
        .collect();
    assert_eq!(files.len(), 4, "thumb+view for each of the two images");
    for path in files.iter().map(|(p, _)| p) {
        let name = path.file_name().unwrap().to_str().unwrap();
        // Name shape: <hash>-<kind>-<generation>.<ext>, the generation
        // tag dimagine-preview keys renditions by (FORMAT §8.2).
        assert!(
            name.ends_with(&format!("-thumb-{RENDITION_GENERATION}.jpg"))
                || name.ends_with(&format!("-view-{RENDITION_GENERATION}.jpg"))
                || name.ends_with(&format!("-thumb-{RENDITION_GENERATION}.png"))
                || name.ends_with(&format!("-view-{RENDITION_GENERATION}.png")),
            "rendition names are hash-kind-generation.ext: {name}"
        );
    }

    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("previews: 2 images (0 generated, 4 cached, 0 failed)"),
        "a second run finds the cache: {stdout}"
    );

    // Originals never change; the only additions are inside .dimagine/.
    let after = originals(&tmp.0);
    let added: Vec<&(String, Vec<u8>)> = after
        .iter()
        .filter(|(p, _)| !p.contains(".dimagine"))
        .collect();
    assert_eq!(
        added.len(),
        before.len(),
        "no new entries outside .dimagine/"
    );
    for (path, bytes) in added {
        let original = before
            .iter()
            .find(|(p, _)| p == path)
            .expect("original still present byte-identical");
        assert_eq!(&original.1, bytes, "{path} was modified");
    }
}

#[test]
fn previews_json_document_shape() {
    let tmp = tmp("json");
    write(&tmp.0, "one.jpg", JPG);
    let lib = tmp.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["schema"], "dimagine.previews/0.1");
    assert_eq!(report["images"], 1);
    assert_eq!(report["generated"], 2);
    assert_eq!(report["cached"], 0);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["findings"], serde_json::json!([]));
    assert_eq!(report["read_complete"], true);

    let (code, stdout, _) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 0);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["generated"], 0);
    assert_eq!(report["cached"], 2, "second run is a cache hit");
}

#[test]
fn previews_truncated_image_is_a_finding_exit_one() {
    let tmp = tmp("truncated");
    let mut truncated = JPG.to_vec();
    truncated.truncate(truncated.len() / 2);
    write(&tmp.0, "broken.jpg", truncated);
    write(&tmp.0, "ok.gif", GIF);
    let lib = tmp.0.to_str().unwrap();

    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(
        code, 1,
        "one image failing to decode is a problem: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("error broken.jpg: decode_failed"),
        "{stdout}"
    );
    assert!(stdout.contains("corrupt or undecodable image"), "{stdout}");
    assert!(
        stdout.contains("previews: 2 images (2 generated, 0 cached, 1 failed)"),
        "{stdout}"
    );

    let (code, stdout, _) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 1);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["schema"], "dimagine.previews/0.1");
    let findings = report["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["path"], "broken.jpg");
    assert_eq!(findings[0]["code"], "decode_failed");
    assert!(
        findings[0]["reason"]
            .as_str()
            .unwrap()
            .contains("Not enough bytes"),
        "the raw decoder complaint is the reason: {findings:?}"
    );
    assert_eq!(
        report["generated"], 0,
        "this is the second run for the library"
    );
    assert_eq!(
        report["cached"], 2,
        "the healthy image renders and its renditions persist"
    );
    assert_eq!(report["failed"], 1);
}

#[test]
#[cfg(unix)]
fn previews_unreadable_dir_exits_three() {
    let tmp = tmp("locked");
    let lib = tmp.0.to_str().unwrap();
    std::fs::create_dir_all(tmp.0.join("locked")).unwrap();
    write(&tmp.0, "locked/hidden.gif", GIF);
    write(&tmp.0, "open.jpg", JPG);
    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(
        code, 3,
        "an unreadable folder means the walk did not finish: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("reading incomplete"), "{stdout}");
    assert!(stdout.contains("could not read folder"), "{stdout}");
    assert!(stdout.contains("proves nothing"), "{stdout}");

    let (code, stdout, _) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 3);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["read_complete"], false);
    assert!(!report["unreadable_dirs"].as_array().unwrap().is_empty());
    assert_eq!(report["images"], 1, "only the readable image was attempted");

    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn previews_failed_and_unreadable_reports_both_exits_three() {
    let tmp = tmp("both");
    let lib = tmp.0.to_str().unwrap();
    let mut truncated = PNG.to_vec();
    truncated.truncate(20);
    write(&tmp.0, "tiny-broken.png", truncated);
    let (code, stdout, _) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("1 failed"), "{stdout}");
}

#[test]
fn previews_empty_library_and_missing_library() {
    let tmp = tmp("empty");
    let lib = tmp.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("previews: 0 images"), "{stdout}");

    let (code, _, stderr) = dimagine(&[
        "previews",
        "--library",
        tmp.0.join("missing").to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not a folder"), "{stderr}");
}

#[test]
fn previews_jobs_flag_bounds_concurrency() {
    let tmp = tmp("jobs");
    write(&tmp.0, "one.jpg", JPG);
    write(&tmp.0, "two.gif", GIF);
    let (code, stdout, stderr) = dimagine(&[
        "previews",
        "--library",
        tmp.0.to_str().unwrap(),
        "--jobs",
        "1",
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("4 generated"), "{stdout}");

    write(&tmp.0, "three.png", PNG);
    let (code, stdout, stderr) = dimagine(&[
        "previews",
        "--library",
        tmp.0.to_str().unwrap(),
        "--jobs",
        "0",
    ]);
    assert_eq!(
        code, 0,
        "zero jobs floor to one: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("2 generated"), "{stdout}");
    assert!(stdout.contains("4 cached"), "{stdout}");
}

#[test]
fn previews_identical_images_share_one_cache_entry() {
    let tmp = tmp("dedupe");
    write(&tmp.0, "a.jpg", JPG);
    std::fs::create_dir_all(tmp.0.join("copy")).unwrap();
    write(&tmp.0, "copy/b.jpg", JPG);
    let lib = tmp.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("previews: 2 images (4 generated, 0 cached, 0 failed)"),
        "both copies were processed in this run: {stdout}"
    );
    let cache_root = tmp.0.join(".dimagine/cache/previews");
    let count = std::fs::read_dir(&cache_root)
        .unwrap()
        .flatten()
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap())
        .count();
    assert_eq!(
        count, 2,
        "identical bytes share SHA-keyed renditions; two files cover both copies"
    );

    let (code, stdout, _) = dimagine(&["previews", "--library", lib]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("previews: 2 images (0 generated, 4 cached, 0 failed)"),
        "the shared cache serves both copies on re-runs: {stdout}"
    );
}

#[test]
fn previews_lists_in_help() {
    let (code, stdout, _) = dimagine(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("previews"), "{stdout}");
}
