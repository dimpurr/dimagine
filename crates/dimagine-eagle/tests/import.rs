use dimagine_eagle::{import, ImportError, ImportOptions, SkipReasonCode};
use saphyr::{LoadableYamlNode, Yaml};
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
fn rejects_canonical_source_destination_overlap_and_absolute_library_label() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    fake_library(&src);
    let inside = src.join("nested-output");
    let error = import(&src, &inside, ImportOptions::default()).unwrap_err();
    assert!(error
        .to_string()
        .contains("overlapping source and destination"));

    let parent = temp.path().join("parent");
    fs::create_dir_all(parent.join("source.library")).unwrap();
    fake_library(&parent.join("source.library"));
    let error = import(
        &parent.join("source.library"),
        &parent,
        ImportOptions::default(),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("overlapping source and destination"));

    let outside = temp.path().join("absolute-label-output");
    let report = import(
        &src,
        &outside,
        ImportOptions {
            name: Some(src.to_string_lossy().into_owned()),
        },
    )
    .unwrap();
    assert_eq!(report.imported.len(), 3);
    assert!(outside.join("Eagle").exists());
    assert!(src.join("images").exists());
}

#[test]
fn allocates_case_insensitive_sibling_folder_names_without_overwrites() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Aliases.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(
        src.join("metadata.json"),
        serde_json::to_vec(
            &json!({"folders":[{"id":"upper","name":"A"},{"id":"lower","name":"a"}]}),
        )
        .unwrap(),
    )
    .unwrap();
    item(
        &src,
        "1",
        json!({"name":"x","ext":"png","folders":["upper"]}),
        Some(("x.png", b"first")),
    );
    item(
        &src,
        "2",
        json!({"name":"x","ext":"png","folders":["lower"]}),
        Some(("x.png", b"second")),
    );
    item(
        &src,
        "3",
        json!({"name":"x","ext":"png","folders":["upper"]}),
        Some(("x.png", b"third")),
    );
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.imported.len(), 3);
    assert_eq!(
        fs::read(dst.join("Eagle/Aliases/A/x.png")).unwrap(),
        b"first"
    );
    assert_eq!(
        fs::read(dst.join("Eagle/Aliases/a-2/x.png")).unwrap(),
        b"second"
    );
    assert_eq!(
        fs::read(dst.join("Eagle/Aliases/A/x-2.png")).unwrap(),
        b"third"
    );
}

#[test]
fn accepts_only_settings_directories_and_preserves_existing_viewer_settings() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    fake_library(&src);
    fs::create_dir_all(dst.join(".obsidian/snippets")).unwrap();
    fs::create_dir_all(dst.join(".dimagine")).unwrap();
    fs::write(
        dst.join(".obsidian/snippets/dimagine-gallery.css"),
        "user css",
    )
    .unwrap();
    fs::write(dst.join(".dimagine/settings.json"), "user settings").unwrap();
    assert_eq!(
        import(&src, &dst, ImportOptions::default())
            .unwrap()
            .imported
            .len(),
        3
    );
    assert_eq!(
        fs::read_to_string(dst.join(".obsidian/snippets/dimagine-gallery.css")).unwrap(),
        "user css"
    );
    assert_eq!(
        fs::read_to_string(dst.join(".dimagine/settings.json")).unwrap(),
        "user settings"
    );

    let other = temp.path().join("other");
    fs::create_dir_all(other.join(".other-hidden")).unwrap();
    assert!(import(&src, &other, ImportOptions::default()).is_err());

    let hidden_file = temp.path().join("hidden-file");
    fs::create_dir_all(&hidden_file).unwrap();
    fs::write(hidden_file.join(".keep"), "hidden user file").unwrap();
    assert!(import(&src, &hidden_file, ImportOptions::default()).is_err());
}

#[test]
fn emitted_frontmatter_round_trips_adversarial_strings_as_yaml() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Yaml.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(src.join("metadata.json"), br#"{"folders":[]}"#).unwrap();
    let title = "before\u{007f}\u{0085}\u{2028}\u{2029}: \"quoted\"";
    let url = "https://example.test/path?q=\u{0085}line\u{2028}break";
    let tags = vec![
        "plain".to_owned(),
        "tag\u{007f}end".to_owned(),
        "line\u{0085}break".to_owned(),
    ];
    item(
        &src,
        "adversarial",
        json!({"id":"item\u{2028}id","name":title,"ext":"png","url":url,"tags":tags}),
        Some(("adversarial.png", b"pixels")),
    );
    let display = "library\u{0085}name";
    let report = import(
        &src,
        &dst,
        ImportOptions {
            name: Some(display.to_owned()),
        },
    )
    .unwrap();
    let note_path = dst.join(format!("{}.md", report.imported[0].path));
    let note = fs::read_to_string(note_path).unwrap();
    let frontmatter = note
        .strip_prefix("---\n")
        .unwrap()
        .split_once("\n---\n")
        .unwrap()
        .0;
    let parsed = Yaml::load_from_str(frontmatter).unwrap();
    let doc = &parsed[0];
    assert_eq!(doc["title"].as_str(), Some(title));
    assert_eq!(doc["source"].as_str(), Some(url));
    let parsed_tags: Vec<_> = doc["tags"]
        .as_vec()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        parsed_tags,
        tags.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(doc["sources"][0]["library"].as_str(), Some(display));
    assert_eq!(doc["sources"][0]["item"].as_str(), Some("item\u{2028}id"));
}

#[test]
fn empty_tags_are_omitted_from_image_note() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("EmptyTags.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(src.join("metadata.json"), br#"{"folders":[]}"#).unwrap();
    item(
        &src,
        "tagless",
        json!({"name":"tagless","ext":"png","tags":[]}),
        Some(("tagless.png", b"x")),
    );
    import(&src, &dst, ImportOptions::default()).unwrap();
    let note = fs::read_to_string(dst.join("inbox/tagless.png.md")).unwrap();
    let frontmatter = note
        .strip_prefix("---\n")
        .unwrap()
        .split_once("\n---\n")
        .unwrap()
        .0;
    let parsed = Yaml::load_from_str(frontmatter).unwrap();
    assert!(parsed[0].as_mapping_get("tags").is_none());
}

#[test]
fn sanitizes_reserved_trailing_and_multibyte_components() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("SafeNames.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(
        src.join("metadata.json"),
        serde_json::to_vec(
            &json!({"folders":[{"id":"dots","name":format!("{}.", "a".repeat(120))}]}),
        )
        .unwrap(),
    )
    .unwrap();
    item(
        &src,
        "reserved",
        json!({"name":"CON","ext":"png"}),
        Some(("CON.png", b"reserved")),
    );
    item(
        &src,
        "long",
        json!({"name":"漢".repeat(120),"ext":"png","folders":["dots"]}),
        Some(("long.png", b"long")),
    );
    import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(fs::read(dst.join("inbox/_CON.png")).unwrap(), b"reserved");
    let imported = fs::read_dir(dst.join("Eagle/SafeNames"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    let folder_name = imported.file_name().unwrap().to_string_lossy();
    assert!(!folder_name.ends_with('.') && folder_name.len() <= 120);
    let image = fs::read_dir(imported)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
        .unwrap();
    assert!(image.file_name().to_string_lossy().len() < 255);
}

#[test]
fn rejects_duplicate_folder_ids_including_ancestor_repetition() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Duplicate.library");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(
        src.join("metadata.json"),
        serde_json::to_vec(
            &json!({"folders":[{"id":"f","name":"A","children":[{"id":"f","name":"B"}]}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let error = import(&src, &temp.path().join("out"), ImportOptions::default()).unwrap_err();
    assert!(error.to_string().contains("duplicate Eagle folder ID: f"));
}

#[test]
fn imports_deep_folder_metadata_beyond_json_default_recursion_limit() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Deep.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    let mut folders = json!([]);
    for index in (0..80).rev() {
        folders = json!([{"id":format!("f{index}"),"name":format!("d{index}"),"children":folders}]);
    }
    fs::write(
        src.join("metadata.json"),
        serde_json::to_vec(&json!({"folders":folders})).unwrap(),
    )
    .unwrap();
    item(
        &src,
        "deep",
        json!({"name":"deep","ext":"png","folders":["f79"]}),
        Some(("deep.png", b"deep")),
    );
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.imported.len(), 1);
    assert!(
        report.imported[0].path.matches('/').count() >= 80,
        "{}",
        report.imported[0].path
    );
}

#[test]
fn rejects_trailing_malformed_data_in_root_metadata() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Trailing.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(src.join("metadata.json"), b"{\"folders\":[]}garbage").unwrap();
    match import(&src, &dst, ImportOptions::default()) {
        Err(ImportError::InvalidSource(msg)) => {
            assert!(
                msg.contains("unreadable metadata.json"),
                "expected unreadable metadata.json, got: {msg}"
            );
        }
        other => panic!("expected InvalidSource, got {:?}", other),
    }
}

#[test]
fn rejects_deeply_nested_unused_fields_safely() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("DeepFields.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images")).unwrap();
    let mut nested = String::from("1");
    for _ in 0..600 {
        nested = format!("[{nested}]");
    }
    let json = format!("{{\"folders\":[],\"unused\":{nested}}}");
    fs::write(src.join("metadata.json"), json.as_bytes()).unwrap();
    match import(&src, &dst, ImportOptions::default()) {
        Err(ImportError::InvalidSource(msg)) => {
            assert!(
                msg.contains("nesting exceeds"),
                "expected nesting error, got: {msg}"
            );
        }
        other => panic!("expected InvalidSource, got {:?}", other),
    }
}

#[cfg(unix)]
#[test]
fn skips_symlinked_eagle_entries_without_copying_external_files() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Symlinks.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images/metadata-link.info")).unwrap();
    fs::create_dir_all(src.join("images/original-link.info")).unwrap();
    fs::write(src.join("metadata.json"), br#"{"folders":[]}"#).unwrap();
    let external = temp.path().join("outside.json");
    fs::write(&external, br#"{"name":"external","ext":"png"}"#).unwrap();
    symlink(
        &external,
        src.join("images/metadata-link.info/metadata.json"),
    )
    .unwrap();
    fs::write(
        src.join("images/original-link.info/metadata.json"),
        br#"{"name":"external","ext":"png"}"#,
    )
    .unwrap();
    let external_image = temp.path().join("outside.png");
    fs::write(&external_image, b"outside bytes").unwrap();
    symlink(
        &external_image,
        src.join("images/original-link.info/external.png"),
    )
    .unwrap();
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.imported.len(), 0);
    assert_eq!(
        report
            .skipped
            .iter()
            .filter(|skip| skip.reason_code == SkipReasonCode::Symlink)
            .count(),
        2
    );
    assert!(!dst.join("inbox/external.png").exists());
}

#[test]
fn rejects_root_or_prefix_components_in_source_item_metadata() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Escape.library");
    let dst = temp.path().join("out");
    fs::create_dir_all(src.join("images/root-name.info")).unwrap();
    fs::write(src.join("metadata.json"), br#"{"folders":[]}"#).unwrap();
    fs::write(
        src.join("images/root-name.info/metadata.json"),
        br#"{"name":"/","ext":"png"}"#,
    )
    .unwrap();
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.imported.len(), 0);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(
        report.skipped[0].reason_code,
        SkipReasonCode::OriginalMissing
    );
}

#[cfg(unix)]
#[test]
fn refuses_symlink_traversal_in_allowed_destination_settings() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    let external = temp.path().join("external");
    fake_library(&src);
    fs::create_dir_all(dst.join(".obsidian")).unwrap();
    fs::create_dir_all(&external).unwrap();
    symlink(&external, dst.join(".obsidian/snippets")).unwrap();
    assert!(import(&src, &dst, ImportOptions::default()).is_err());
    assert!(!external.join("dimagine-gallery.css").exists());
}

#[cfg(unix)]
#[test]
fn does_not_write_through_leaf_symlinks_in_settings() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    fake_library(&src);
    fs::create_dir_all(dst.join(".obsidian/snippets")).unwrap();
    let external_css = temp.path().join("external.css");
    fs::write(&external_css, b"external css").unwrap();
    let victim = src.join("images/victim.txt");
    symlink(
        &external_css,
        dst.join(".obsidian/snippets/dimagine-gallery.css"),
    )
    .unwrap();
    symlink(&victim, dst.join(".obsidian/appearance.json")).unwrap();
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.imported.len(), 3);
    assert!(
        !victim.exists(),
        "settings writes must not follow dangling symlinks into the source"
    );
    assert_eq!(
        fs::read(temp.path().join("external.css")).unwrap(),
        b"external css"
    );
    assert!(
        fs::symlink_metadata(dst.join(".obsidian/snippets/dimagine-gallery.css"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::symlink_metadata(dst.join(".obsidian/appearance.json"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn post_item_failure_reports_partial_io_with_retained_artifacts() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Fixture.library");
    let dst = temp.path().join("dest");
    fake_library(&src);

    fs::create_dir_all(dst.join(".obsidian")).unwrap();
    fs::set_permissions(dst.join(".obsidian"), fs::Permissions::from_mode(0o555)).unwrap();

    let result = import(&src, &dst, ImportOptions::default());
    // Restore permissions so cleanup succeeds
    let _ = fs::set_permissions(dst.join(".obsidian"), fs::Permissions::from_mode(0o755));

    match result {
        Err(ImportError::PartialIo {
            progress,
            retained_artifacts,
            ..
        }) => {
            assert_eq!(progress.imported.len(), 3);
            assert!(
                !retained_artifacts.is_empty(),
                "retained artifacts must be reported"
            );
            for artifact in &retained_artifacts {
                assert!(
                    artifact.exists(),
                    "reported artifact {} must exist",
                    artifact.display()
                );
            }
        }
        other => panic!("expected PartialIo, got {:?}", other),
    }
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

#[test]
fn generic_name_uses_url_identifier() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Url.library");
    let dst = temp.path().join("dest");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(src.join("metadata.json"), b"{\"version\":4,\"folders\":[]}").unwrap();
    item(
        &src,
        "g",
        json!({"id":"g", "name":"image", "ext":"jpg",
               "url":"https://i.pximg.net/img-original/img/2023/01/02/03/04/05/12345678_p0.jpg"}),
        Some(("image.jpg", b"jpg-g")),
    );
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.renamed, 1);
    assert!(dst.join("inbox/pximg-12345678.jpg").is_file());
}

#[test]
fn ip_literal_host_falls_back_to_time_based_name() {
    let temp = TempDir::new().unwrap();
    let src = temp.path().join("Ip.library");
    let dst = temp.path().join("dest");
    fs::create_dir_all(src.join("images")).unwrap();
    fs::write(src.join("metadata.json"), b"{\"version\":4,\"folders\":[]}").unwrap();
    item(
        &src,
        "v4",
        json!({"id":"v4", "name":"image", "ext":"jpg",
               "url":"http://192.168.1.5:8080/img/1234567890"}),
        Some(("image.jpg", b"jpg-v4")),
    );
    item(
        &src,
        "v6",
        json!({"id":"v6", "name":"Screenshot", "ext":"jpg",
               "url":"https://[2001:db8::1]/photo/12345678"}),
        Some(("Screenshot.jpg", b"jpg-v6")),
    );
    let report = import(&src, &dst, ImportOptions::default()).unwrap();
    assert_eq!(report.renamed, 2);
    let mut names: Vec<String> = fs::read_dir(dst.join("inbox"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jpg"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "names: {names:?}");
    for name in &names {
        let stem = name.trim_end_matches(".jpg");
        let bytes = stem.as_bytes();
        assert!(
            bytes.len() == 8 + 1 + 6 + 1 + 4
                && bytes[0..8].iter().all(u8::is_ascii_digit)
                && bytes[8] == b'-'
                && bytes[9..15].iter().all(u8::is_ascii_digit)
                && bytes[15] == b'-'
                && bytes[16..]
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
            "expected a time-based name, got {name}"
        );
    }
}
