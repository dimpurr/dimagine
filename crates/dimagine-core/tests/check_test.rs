//! `dimagine check` findings: one test per finding code, plus the exit-code
//! policy of the report (HLD).

mod support;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use dimagine_core::check::{self, run, Severity};
use support::*;

fn codes_of(lib: &dimagine_core::Library) -> Vec<(String, &'static str, Severity)> {
    run(lib)
        .findings
        .into_iter()
        .map(|f| (f.path, f.code, f.severity))
        .collect()
}

#[test]
fn clean_library_has_no_findings_and_exits_zero() {
    let tmp = tmp("clean");
    write(tmp.path(), "a.jpg", jpg());
    write(tmp.path(), "b.png", png());
    let lib = open_library(tmp.path());
    let report = run(&lib);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(report.read_complete);
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn check_never_writes_anything() {
    let tmp = tmp("readonly");
    write(tmp.path(), "girl.jpg", jpg());
    write(
        tmp.path(),
        "girl.jpg.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n![[girl.jpg]]\n",
    );
    write(
        tmp.path(),
        "col.md",
        "---\nkind: collection\n---\n![[ghost.jpg]]\n",
    );
    let before: Vec<_> = walk_meta(tmp.path());
    let lib = open_library(tmp.path());
    let _ = run(&lib);
    let _ = dimagine_core::scan::run(&lib);
    let after: Vec<_> = walk_meta(tmp.path());
    assert_eq!(before, after, "no file may change from a read-only run");
}

fn walk_meta(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    fn walk(dir: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>, prefix: &str) {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if path.is_dir() {
                walk(&path, out, &rel);
            } else {
                let meta = std::fs::metadata(&path).unwrap();
                out.push((rel, meta.len(), meta.modified().unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out, "");
    out
}

#[test]
fn finds_format_mismatch() {
    let tmp = tmp("mismatch");
    write(tmp.path(), "actually_png.jpg", png()); // PNG bytes with .jpg extension
    write(tmp.path(), "good.jpg", jpg());
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings
            .iter()
            .any(|(path, code, severity)| path == "actually_png.jpg"
                && *code == check::codes::FORMAT_MISMATCH
                && *severity == Severity::Error),
        "{findings:?}"
    );
}

#[test]
fn finds_note_without_image() {
    let tmp = tmp("orphan");
    write(
        tmp.path(),
        "gone.jpg.md",
        "---\ntitle: Gone\n---\n![[gone.jpg]]\n",
    );
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings
            .iter()
            .any(|(path, code, sev)| path == "gone.jpg.md"
                && *code == check::codes::NOTE_WITHOUT_IMAGE
                && *sev == Severity::Error),
        "{findings:?}"
    );
    // The note's link to the missing image is missing too.
    assert!(
        findings
            .iter()
            .any(|(_, code, _)| *code == check::codes::MISSING_LINK),
        "{findings:?}"
    );
}

#[test]
fn finds_invalid_yaml_with_location() {
    let tmp = tmp("yaml");
    write(
        tmp.path(),
        "broken.md",
        "---\ntitle: Fine\na: b: c\n---\ntext\n",
    );
    let lib = open_library(tmp.path());
    let report = run(&lib);
    let finding = report
        .findings
        .iter()
        .find(|f| f.code == check::codes::INVALID_YAML)
        .expect("invalid_yaml finding");
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.path, "broken.md");
    let detail = finding.detail.as_ref().unwrap();
    assert_eq!(detail["line"], 3, "line points into the whole file");
    assert!(detail["column"].is_u64());
}

#[test]
fn finds_invalid_id() {
    let tmp = tmp("id");
    write(tmp.path(), "one.md", "---\nid: not-a-ulid\n---\ntext\n");
    write(tmp.path(), "two.md", "---\nid: 46\n---\ntext\n");
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings.iter().any(|(path, code, sev)| path == "one.md"
            && *code == check::codes::INVALID_ID
            && *sev == Severity::Error),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|(path, code, _)| path == "two.md" && *code == check::codes::INVALID_ID),
        "non-string id, {findings:?}"
    );
}

#[test]
fn finds_duplicate_ids_nfc_and_case_insensitive() {
    let tmp = tmp("dup");
    write(
        tmp.path(),
        "a/x.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n\n",
    );
    write(
        tmp.path(),
        "b/y.md",
        format!("---\nid: {}\n---\n\n", "01ja8x3q7k2m9v4t6r1b5n0c3q"),
    );
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    let duplicates: Vec<_> = findings
        .iter()
        .filter(|(_, code, _)| *code == check::codes::DUPLICATE_ID)
        .collect();
    assert_eq!(
        duplicates.len(),
        1,
        "one finding per duplicate id group: {findings:?}"
    );
}

#[test]
fn duplicate_ids_do_not_fire_for_distinct_notes() {
    let tmp = tmp("unique-ids");
    write(
        tmp.path(),
        "a.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n\n",
    );
    write(
        tmp.path(),
        "b.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3R\n---\n\n",
    );
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(!findings
        .iter()
        .any(|(_, code, _)| *code == check::codes::DUPLICATE_ID));
}

#[test]
fn finds_missing_and_ambiguous_links_separately() {
    let tmp = tmp("links");
    write(tmp.path(), "a/girl.jpg", jpg());
    write(tmp.path(), "b/girl.jpg", jpg());
    write(
        tmp.path(),
        "col.md",
        "---\nkind: collection\n---\n![[ghost.jpg]]\nsee also [[a/girl.jpg]]... and ![[girl.jpg]]\n",
    );
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings.iter().any(|(path, code, sev)| path == "col.md"
            && *code == check::codes::MISSING_LINK
            && *sev == Severity::Warning),
        "{findings:?}"
    );
    assert!(
        findings.iter().any(|(path, code, sev)| path == "col.md"
            && *code == check::codes::AMBIGUOUS_LINK
            && *sev == Severity::Warning),
        "{findings:?}"
    );
    // The path link resolves unambiguously.
    let resolver_hits = findings
        .iter()
        .filter(|(_, code, _)| *code == check::codes::AMBIGUOUS_LINK)
        .count();
    assert_eq!(resolver_hits, 1, "{findings:?}");
}

#[test]
fn standard_markdown_and_canvas_links_are_checked_too() {
    let tmp = tmp("md-links");
    write(tmp.path(), "real.png", png());
    write(
        tmp.path(),
        "c.md",
        "missing ![ghost](ghost.png) and ok ![real](real%20copy.png)\n",
    );
    write(
        tmp.path(),
        "board.canvas",
        r#"{"nodes": [{"id": "1", "type": "file", "file": "ghost.jpg"}]}"#,
    );
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings
            .iter()
            .any(|(path, code, _)| path == "c.md" && *code == check::codes::MISSING_LINK),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|(path, code, _)| path == "board.canvas" && *code == check::codes::MISSING_LINK),
        "{findings:?}"
    );
}

#[test]
fn finds_invalid_canvas_json() {
    let tmp = tmp("canvas");
    write(tmp.path(), "board.canvas", "{not json");
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings
            .iter()
            .any(|(path, code, sev)| path == "board.canvas"
                && *code == check::codes::INVALID_CANVAS
                && *sev == Severity::Error),
        "{findings:?}"
    );
}

#[test]
fn finds_bad_file_name_characters() {
    let tmp = tmp("names");
    write(tmp.path(), "shot[1].jpg", jpg());
    write(tmp.path(), "tag^2.png", png());
    write(tmp.path(), "fine.jpg", jpg());
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    let bad: Vec<_> = findings
        .iter()
        .filter(|(_, code, _)| *code == check::codes::BAD_FILENAME_CHAR)
        .collect();
    assert_eq!(bad.len(), 2, "{findings:?}");
    assert!(findings
        .iter()
        .filter(|(_, code, _)| *code == check::codes::BAD_FILENAME_CHAR)
        .all(|(_, _, sev)| *sev == Severity::Warning));
}

#[test]
fn missing_self_embed_is_info_not_an_error() {
    let tmp = tmp("selfembed");
    write(tmp.path(), "girl.jpg", jpg());
    write(
        tmp.path(),
        "girl.jpg.md",
        "---\ntitle: Girl\n---\nSome words.\n",
    );
    let lib = open_library(tmp.path());
    let report = run(&lib);
    let finding = report
        .findings
        .iter()
        .find(|f| f.code == check::codes::MISSING_SELF_EMBED)
        .expect("self-embed finding");
    assert_eq!(finding.severity, Severity::Info);
    // Info findings alone mean "nothing to report" (exit 0).
    assert!(!report.has_problems());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn self_embed_accepted_in_bare_path_and_extensionless_spellings() {
    let tmp = tmp("selfforms");
    write(tmp.path(), "refs/girl.jpg", jpg());
    write(tmp.path(), "refs/girl.jpg.md", "words\n\n![[girl.jpg]]\n");
    write(tmp.path(), "refs/dir/girl2.jpg", jpg());
    write(
        tmp.path(),
        "refs/dir/girl2.jpg.md",
        "words\n\n![[refs/dir/girl2.jpg]]\n",
    );
    write(tmp.path(), "refs/dir/girl3.jpg", jpg());
    write(tmp.path(), "refs/dir/girl3.jpg.md", "words\n\n![[girl3]]\n");
    write(tmp.path(), "refs/dir/girl4.jpg", jpg());
    write(
        tmp.path(),
        "refs/dir/girl4.jpg.md",
        "words\n\n![](refs/dir/girl4.jpg)\n",
    );
    let lib = open_library(tmp.path());
    let report = run(&lib);
    let missing: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.code == check::codes::MISSING_SELF_EMBED)
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
#[cfg(unix)]
fn finds_unreadable_image_and_file() {
    let tmp = tmp("unreadable");
    write(tmp.path(), "dark.jpg", jpg());
    write(tmp.path(), "dark.md", "text\n");
    std::fs::set_permissions(
        tmp.path().join("dark.jpg"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    std::fs::set_permissions(
        tmp.path().join("dark.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let lib = open_library(tmp.path());
    let findings = codes_of(&lib);
    assert!(
        findings.iter().any(|(path, code, sev)| path == "dark.jpg"
            && *code == check::codes::UNREADABLE_IMAGE
            && *sev == Severity::Error),
        "{findings:?}"
    );
    assert!(
        findings.iter().any(|(path, code, sev)| path == "dark.md"
            && *code == check::codes::UNREADABLE_FILE
            && *sev == Severity::Error),
        "{findings:?}"
    );
    // Any failed read is incomplete, so negative results prove nothing.
    let report = run(&lib);
    assert!(!report.read_complete);
    assert_eq!(report.exit_code(), 3);
    // Restore for TempDir cleanup.
    std::fs::set_permissions(
        tmp.path().join("dark.jpg"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::set_permissions(
        tmp.path().join("dark.md"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

#[test]
fn note_limit_reports_incomplete_read() {
    let tmp = tmp("note-limit");
    write(tmp.path(), "large.md", "123456789");
    let lib = open_library(tmp.path());
    let report = check::run_with_limit(&lib, 4);
    assert!(!report.read_complete);
    assert_eq!(report.exit_code(), 3);
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::NOTE_TOO_LARGE));
}

#[test]
fn file_removed_after_walk_is_an_incomplete_read() {
    let tmp = tmp("removed-after-walk");
    write(tmp.path(), "vanished.jpg", jpg());
    let lib = open_library(tmp.path());
    std::fs::remove_file(tmp.path().join("vanished.jpg")).unwrap();
    let report = run(&lib);
    assert!(!report.read_complete);
    assert_eq!(report.exit_code(), 3);
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::UNREADABLE_IMAGE));
}

#[test]
fn eof_delimiter_yields_invalid_yaml_finding_without_panicking() {
    let tmp = tmp("eof-front-matter");
    write(tmp.path(), "a.md", "---");
    let report = run(&open_library(tmp.path()));
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::INVALID_YAML));
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn grouped_duplicate_contains_each_path_once() {
    let tmp = tmp("dup-group");
    for name in ["a.md", "b.md", "c.md"] {
        write(
            tmp.path(),
            name,
            "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n",
        );
    }
    let report = run(&open_library(tmp.path()));
    let finding = report
        .findings
        .iter()
        .find(|f| f.code == check::codes::DUPLICATE_ID)
        .unwrap();
    assert_eq!(
        finding.detail.as_ref().unwrap()["paths"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn missing_and_ambiguous_markdown_note_links_are_reported() {
    let tmp = tmp("note-links");
    write(tmp.path(), "a/ideas.md", "note\n");
    write(tmp.path(), "b/ideas.md", "note\n");
    write(tmp.path(), "collection.md", "[[missing.md]] [[ideas.md]]\n");
    let report = run(&open_library(tmp.path()));
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::MISSING_LINK && f.message.contains("missing.md")));
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::AMBIGUOUS_LINK && f.message.contains("ideas.md")));
}

#[test]
fn canvas_paths_are_library_root_relative() {
    let tmp = tmp("canvas-root");
    write(tmp.path(), "refs/x.jpg", jpg());
    write(
        tmp.path(),
        "boards/a.canvas",
        r#"{"nodes":[{"id":"1","type":"file","file":"x.jpg"}]}"#,
    );
    let report = run(&open_library(tmp.path()));
    assert!(report
        .findings
        .iter()
        .any(|f| f.path == "boards/a.canvas" && f.code == check::codes::MISSING_LINK));
}

#[test]
fn ambiguous_image_embed_does_not_satisfy_self_preview() {
    let tmp = tmp("ambiguous-self");
    write(tmp.path(), "a/x.jpg", jpg());
    write(tmp.path(), "b/x.jpg", jpg());
    write(tmp.path(), "a/x.jpg.md", "![[x.jpg]]\n");
    let report = run(&open_library(tmp.path()));
    assert!(report
        .findings
        .iter()
        .any(|f| f.path == "a/x.jpg.md" && f.code == check::codes::MISSING_SELF_EMBED));
}

#[test]
#[cfg(unix)]
fn non_utf8_paths_keep_distinct_native_identity() {
    use std::os::unix::ffi::OsStringExt;
    let tmp = tmp("non-utf8");
    let raw_name = std::ffi::OsString::from_vec(b"x\xff.jpg".to_vec());
    let raw_path = std::path::PathBuf::from(raw_name.clone());
    let utf8_path = std::path::PathBuf::from("x�.jpg");
    let lib = dimagine_core::Library {
        root: tmp.path().to_path_buf(),
        files: vec![
            dimagine_core::library::FileEntry {
                rel: "x�.jpg".into(),
                name: "x�.jpg".into(),
                path: raw_path,
                native_name: raw_name,
                class: dimagine_core::FileClass::Image,
            },
            dimagine_core::library::FileEntry {
                rel: "x�.jpg".into(),
                name: "x�.jpg".into(),
                path: utf8_path,
                native_name: "x�.jpg".into(),
                class: dimagine_core::FileClass::Image,
            },
        ],
        ignored: Default::default(),
        unreadable_dirs: Vec::new(),
    };
    assert_ne!(lib.files[0].path, lib.files[1].path);
    let report = run(&lib);
    assert!(report.has_non_utf8_paths);
}

#[test]
#[cfg(unix)]
fn special_files_are_skipped_with_incomplete_finding() {
    use std::ffi::CString;
    let tmp = tmp("special");
    let fifo = CString::new(tmp.path().join("wait.jpg").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let report = run(&open_library(tmp.path()));
    assert!(report
        .findings
        .iter()
        .any(|f| f.code == check::codes::SPECIAL_FILE));
    assert!(!report.read_complete);
    assert_eq!(report.exit_code(), 3);
}

#[test]
#[cfg(unix)]
fn unreadable_dir_makes_check_exit_3() {
    let tmp = tmp("exit3");
    write(tmp.path(), "fine.jpg", jpg());
    write(tmp.path(), "locked/x.jpg", jpg());
    let locked = tmp.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let lib = open_library(tmp.path());
    let report = run(&lib);
    assert!(!report.read_complete);
    assert_eq!(report.exit_code(), 3);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn finds_are_sorted_by_path() {
    let tmp = tmp("sorted");
    write(tmp.path(), "zz.png", b"not really");
    write(tmp.path(), "aa.md", "---\nid: nope\n---\n");
    let lib = open_library(tmp.path());
    let report = run(&lib);
    let paths: Vec<&str> = report.findings.iter().map(|f| f.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[test]
fn unknown_properties_do_not_break_checks() {
    let tmp = tmp("unknown-props");
    write(tmp.path(), "girl.jpg", jpg());
    write(
        tmp.path(),
        "girl.jpg.md",
        "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\nmood: rainy\nwidth: 1200\nnested:\n  - a: [1, 2]\n---\n\n![[girl.jpg]]\n",
    );
    let lib = open_library(tmp.path());
    let report = run(&lib);
    assert!(
        report.findings.is_empty(),
        "unknown properties stay acceptable, and the valid id+embed is fine: {:?}",
        report.findings
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn quoted_title_embed_satisfies_the_self_embed() {
    // Regression (R7b): a `(` or `<` in a quoted title used to hide the
    // embed entirely, so a valid note image with a title was reported as
    // missing its self-embed.
    let tmp = tmp("title-embed");
    write(tmp.path(), "girl.jpg", jpg());
    write(
        tmp.path(),
        "girl.jpg.md",
        "words\n\n![](girl.jpg \"opening (\")\n",
    );
    let lib = open_library(tmp.path());
    let report = run(&lib);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

#[test]
#[cfg(unix)]
fn backslash_file_names_do_not_alias_slash_paths() {
    // Regression (R7b): on Unix both the file `a\b.jpg` and the file
    // `a/b.jpg` shared the resolver key `a/b.jpg`, so a valid link was
    // ambiguous and the display path lied about the real name.
    // (Filesystem fixtures for non-UTF-8 folders are built only in
    // links_test, in memory: APFS refuses non-UTF-8 names with EILSEQ.)
    let tmp = tmp("backslash");
    write_os(tmp.path(), b"a\\b.jpg", jpg()); // one file named a\b.jpg
    write(tmp.path(), "a/b.jpg", jpg()); // a different file, a/b.jpg
    write(tmp.path(), "n.md", "![](a/b.jpg) and ![](a\\b.jpg)\n");
    let lib = open_library(tmp.path());
    let report = run(&lib);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    let entry = lib
        .files
        .iter()
        .find(|f| f.path == Path::new("a\\b.jpg"))
        .expect("native backslash-named file");
    assert_eq!(entry.rel, "a\\b.jpg", "display keeps the backslash");
}
