use dimagine_eagle::{import, ImportOptions, SkipReasonCode};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn fake_library(root: &Path) {
    fs::create_dir_all(root.join("images")).unwrap();
    fs::write(root.join("metadata.json"), serde_json::to_vec(&json!({
        "version": 4,
        "folders": [{"id":"f1", "name":"Deep [Blue] # ^ |", "children":[{"id":"f2", "name":"Cafe\u{0301}"}]}]
    })).unwrap()).unwrap();
    item(
        root,
        "a",
        json!({"id":"a", "name":"Same", "ext":"png", "folders":["f2"], "tags":["one"], "star":4, "width":3, "height":2, "annotation":"hello"}),
        Some(("Same.png", b"png-a")),
    );
    item(
        root,
        "b",
        json!({"id":"b", "name":"same", "ext":"PNG", "folders":["f2"]}),
        Some(("same.PNG", b"png-b")),
    );
    item(
        root,
        "c",
        json!({"id":"c", "name":"Untitled", "ext":"jpg", "folders":["gone"]}),
        Some(("Untitled.jpg", b"jpg-c")),
    );
    item(
        root,
        "trash",
        json!({"id":"trash", "name":"Trash", "ext":"jpg", "isDeleted":true}),
        Some(("Trash.jpg", b"trash")),
    );
    item(
        root,
        "movie",
        json!({"id":"movie", "name":"Clip", "ext":"mp4"}),
        Some(("Clip.mp4", b"video")),
    );
    fs::create_dir_all(root.join("images/missing.info")).unwrap();
}

fn item(root: &Path, id: &str, metadata: serde_json::Value, file: Option<(&str, &[u8])>) {
    let dir = root.join("images").join(format!("{id}.info"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    if let Some((name, bytes)) = file {
        fs::write(dir.join(name), bytes).unwrap();
    }
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), output);
            } else {
                output.push((
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output.sort_by(|a, b| a.0.cmp(&b.0));
    output
}

#[test]
fn imports_folder_tree_skips_and_preserves_raw_bytes() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    fake_library(&src);
    let source_before = tree(&src);
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(tree(&src), source_before);
    assert_eq!(report.imported.len(), 3);
    assert_eq!(report.renamed, 1);
    assert_eq!(report.dangling_folder_refs, 1);
    assert_eq!(report.folder_counts["Deep -Blue- - - -/Café"], 2);
    assert_eq!(report.folder_counts["(no Eagle folder)"], 1);
    assert!(report
        .skipped
        .iter()
        .any(|s| s.reason_code == SkipReasonCode::MissingMetadata));
    assert!(report
        .skipped
        .iter()
        .any(|s| s.reason_code == SkipReasonCode::Trash));
    assert!(report
        .skipped
        .iter()
        .any(|s| s.reason_code == SkipReasonCode::NotImage));
    assert_eq!(
        fs::read(dst.join("Eagle/Fixture/Deep -Blue- - - -/Café/Same.png")).unwrap(),
        b"png-a"
    );
    assert_eq!(
        fs::read(dst.join("Eagle/Fixture/Deep -Blue- - - -/Café/Same.png.eagle.json")).unwrap(),
        fs::read(src.join("images/a.info/metadata.json")).unwrap()
    );
    let note =
        fs::read_to_string(dst.join("Eagle/Fixture/Deep -Blue- - - -/Café/Same.png.md")).unwrap();
    assert!(note.contains("rating: 4"));
    assert!(note.contains("width: 3\nheight: 2"));
    assert!(note.ends_with("![[Same.png]]\n"));
}

#[test]
fn accepts_v2_v3_and_v4_headers_and_ignores_mtime_index() {
    let temp = TempDir::new().unwrap();
    for version in [2, 3, 4] {
        let src = temp.path().join(format!("v{version}.library"));
        let dst = temp.path().join(format!("out-{version}"));
        fake_library(&src);
        let mut header: serde_json::Value =
            serde_json::from_slice(&fs::read(src.join("metadata.json")).unwrap()).unwrap();
        header["version"] = json!(version);
        header["unknownHeaderField"] = json!({"preservedInOriginal": true});
        fs::write(
            src.join("metadata.json"),
            serde_json::to_vec(&header).unwrap(),
        )
        .unwrap();
        fs::write(src.join("mtime.json"), br#"{"all":5,"a":123456}"#).unwrap();
        assert_eq!(
            import(&src, &dst, ImportOptions::default())
                .unwrap()
                .imported
                .len(),
            3
        );
        assert_eq!(
            fs::read(src.join("mtime.json")).unwrap(),
            br#"{"all":5,"a":123456}"#
        );
    }
}

#[test]
fn refuses_visible_destination_content() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    fake_library(&src);
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("visible"), "x").unwrap();
    assert!(import(&src, &dst, ImportOptions::default()).is_err());
}

#[test]
fn python_prototype_golden_tree_and_note_content() {
    let temp = TempDir::new().unwrap();
    let rust_root = temp.path().join("rust");
    let python_root = temp.path().join("python");
    let src_a = rust_root.join("Fixture.library");
    let src_b = python_root.join("Fixture.library");
    let rust_dst = temp.path().join("rust-out");
    let python_dst = temp.path().join("python-out");
    fake_library(&src_a);
    fake_library(&src_b);
    import(
        &src_a,
        &rust_dst,
        ImportOptions {
            name: Some("Fixture".to_owned()),
        },
    )
    .unwrap();
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/prototype/eagle-import.py");
    let output = Command::new("python3")
        .arg(&script)
        .arg(&src_b)
        .arg(&python_dst)
        .arg("--name")
        .arg("Fixture")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rust_tree = normalize_tree(tree(&rust_dst));
    let python_tree = normalize_tree(tree(&python_dst));
    assert_eq!(rust_tree, python_tree);
}

fn normalize_tree(entries: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    entries
        .into_iter()
        .map(|(path, bytes)| {
            let path = path
                .split('/')
                .map(normalize_component)
                .collect::<Vec<_>>()
                .join("/");
            let content = match String::from_utf8(bytes.clone()) {
                Ok(text) => {
                    let normalized_lines = text
                        .split_inclusive('\n')
                        .map(|line| {
                            if line.starts_with("id: ") {
                                "id: <ID>\n".to_owned()
                            } else if line.starts_with("imported: ") {
                                "imported: <TIME>\n".to_owned()
                            } else if line.starts_with("    imported: ") {
                                "    imported: <TIME>\n".to_owned()
                            } else {
                                line.to_owned()
                            }
                        })
                        .collect::<String>();
                    normalize_generated_refs(&normalized_lines).into_bytes()
                }
                Err(_) => bytes,
            };
            (path, content)
        })
        .collect()
}

fn normalize_component(component: &str) -> String {
    let b = component.as_bytes();
    if b.len() >= 20
        && b[0..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'-'
    {
        format!("<GENERATED>{}", &component[20..])
    } else {
        component.to_owned()
    }
}

fn normalize_generated_refs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if index + 20 <= bytes.len() {
            let candidate = &bytes[index..index + 20];
            if candidate[0..8].iter().all(u8::is_ascii_digit)
                && candidate[8] == b'-'
                && candidate[9..15].iter().all(u8::is_ascii_digit)
                && candidate[15] == b'-'
            {
                output.push_str("<GENERATED>");
                index += 20;
                continue;
            }
        }
        let ch = text[index..].chars().next().unwrap();
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}
