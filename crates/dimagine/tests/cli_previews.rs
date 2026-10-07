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

/// Every rendition this library's cache holds.
fn cache_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.join(".dimagine/cache/previews")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.path().is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// The counts are about the cache, whose entries are named after the content
/// they came from — not about timestamps, which cannot tell a cached rendition
/// from one written in the same second the run started.
#[test]
fn previews_counts_ignore_the_timestamp_on_a_rendition() {
    let tmp = tmp("timestamp");
    write(&tmp.0, "one.jpg", JPG);
    let lib = tmp.0.to_str().unwrap();
    let (code, stdout, stderr) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["generated"], 2);

    let renditions = cache_files(&tmp.0);
    assert_eq!(renditions.len(), 2, "{renditions:?}");
    let bytes: Vec<Vec<u8>> = renditions
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();

    // Stamp them from the future: a file whose own clock is newer than the run
    // is still the rendition the cache already had.
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    for path in &renditions {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
    }

    let (code, stdout, stderr) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(
        report["cached"], 2,
        "a cached rendition stays cached whatever its timestamp says: {report}"
    );
    assert_eq!(report["generated"], 0, "{report}");
    let after: Vec<Vec<u8>> = renditions
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    assert_eq!(after, bytes, "nothing was rewritten to say so");
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
    // A folder this run cannot list, so the reading did not finish: with a
    // decode failure in the readable part too, exit code 3 wins because "not
    // found" here proves nothing, while a failure is something found.
    write(&tmp.0, "locked/hidden.gif", GIF);
    #[cfg(unix)]
    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();

    let (code, stdout, stderr) = dimagine(&["previews", "--library", lib]);
    assert_eq!(
        code, 3,
        "an incomplete reading outranks a finding: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("1 failed"), "{stdout}");
    assert!(stdout.contains("error tiny-broken.png"), "{stdout}");
    assert!(stdout.contains("could not read folder"), "{stdout}");
    assert!(stdout.contains("reading incomplete"), "{stdout}");

    let (code, stdout, stderr) = dimagine(&["--json", "previews", "--library", lib]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["read_complete"], false);
    assert_eq!(report["failed"], 1, "{report}");
    assert!(
        !report["unreadable_dirs"].as_array().unwrap().is_empty(),
        "{report}"
    );

    #[cfg(unix)]
    std::fs::set_permissions(tmp.0.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// An uncompressed 24-bit BMP, `side` by `side` (a multiple of four, so rows
/// need no padding), whose pixels move with `seed` so distinct seeds are
/// distinct files and share no cache key.
fn bmp(side: usize, seed: usize) -> Vec<u8> {
    assert_eq!(side % 4, 0, "BMP rows are padded to four bytes");
    let mut out = Vec::with_capacity(54 + side * side * 3);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + side * side * 3) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0]);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(side as u32).to_le_bytes());
    out.extend_from_slice(&(side as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in 0..side {
        for x in 0..side {
            out.push((x + y + seed * 29) as u8);
            out.push((x * 3 + y + seed) as u8);
            out.push((x + y * 2 + seed * 7) as u8);
        }
    }
    out
}

/// The `--jobs` bound has to show up in the run, or nothing distinguishes a
/// worker pool that obeys it from one that ignores it.
#[test]
fn previews_jobs_bound_shows_in_the_wall_clock() {
    let cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    if cpus < 2 {
        // One CPU has no parallelism to measure; the bound is honest but
        // invisible here, so there is nothing to assert.
        return;
    }
    let jobs = cpus.min(4);
    let tmp = tmp("bound");
    let lib = tmp.0.to_str().unwrap();
    // Six images large enough that decoding dominates: a run that skipped the
    // work would finish too fast to compare.
    for seed in 0..6 {
        write(&tmp.0, &format!("slow-{seed}.bmp"), bmp(640, seed));
    }

    let measure = |jobs: &str| {
        let started = std::time::Instant::now();
        let (code, stdout, stderr) = dimagine(&["previews", "--library", lib, "--jobs", jobs]);
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        assert!(
            stdout.contains("6 images (12 generated, 0 cached, 0 failed)"),
            "--jobs {jobs} must build, not skip: {stdout}"
        );
        started.elapsed()
    };

    let serial = measure("1");
    // The cache is derived state (FORMAT §8): deleting it is the supported way
    // to ask the same library for the same work again.
    std::fs::remove_dir_all(tmp.0.join(".dimagine")).unwrap();
    let parallel = measure(&jobs.to_string());

    // Half the workers a run asked for, and never less than one and a half:
    // that leaves room for process start-up, the other tests and a busy CI box,
    // while a pool that ran the images serially anyway lands at one.
    let speedup = serial.as_secs_f64() / parallel.as_secs_f64();
    let floor = (jobs as f64 / 2.0).max(1.5);
    assert!(
        speedup >= floor,
        "{jobs} workers took {parallel:?} against {serial:?} serially: {speedup:.1}x, \
         short of the {floor:.1}x the bound promises"
    );
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
