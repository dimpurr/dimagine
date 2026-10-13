//! Collection body edits (ADR-017 §4, §5, §6): the byte diffs are pinned by
//! the fixtures under `tests/fixtures/body/`, one case per rule, and asserted
//! exactly — both the new bytes and the ranges they were taken from.

use std::path::{Path, PathBuf};

use dimagine_core::body_edit::{
    self, append_member, is_member, link_text_for, member_spans, new_collection_body,
    remove_member, safe_file_name, verify_points_to, BodyCtx, EditError, Embed, FolderSource,
};
use dimagine_core::library::{classify, FileEntry};
use dimagine_core::links::Resolver;
use serde::Deserialize;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(rel: &str) -> Vec<u8> {
    let path = fixtures().join(rel);
    std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

#[derive(Deserialize)]
struct Case {
    name: String,
    op: String,
    library: Vec<String>,
    note: String,
    image: String,
    #[serde(rename = "in")]
    in_file: String,
    #[serde(default)]
    out: Option<String>,
    #[serde(default)]
    member: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    deleted: Vec<String>,
    #[serde(default)]
    added_text: Option<String>,
    #[serde(default)]
    desc: String,
}

fn cases() -> Vec<Case> {
    serde_json::from_slice(&read("body/cases.json")).expect("cases.json")
}

/// Library entries built from library-relative paths, the way a walk would
/// classify them.
fn entries(rels: &[String]) -> Vec<FileEntry> {
    rels.iter()
        .map(|rel| {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            FileEntry {
                rel: rel.clone(),
                name,
                path: PathBuf::from(rel),
                native_name: std::ffi::OsString::from(
                    rel.rsplit('/').next().unwrap_or(rel).to_string(),
                ),
                class: classify(rel),
            }
        })
        .collect()
}

/// Delete the reported ranges from the original bytes.
fn apply_removals(body: &[u8], removed: &[std::ops::Range<usize>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0usize;
    for range in removed {
        assert!(range.start >= cursor, "reported ranges must be ordered");
        assert!(range.end <= body.len(), "reported range out of bounds");
        out.extend_from_slice(&body[cursor..range.start]);
        cursor = range.end;
    }
    out.extend_from_slice(&body[cursor..]);
    out
}

#[test]
fn body_cases_match_their_expected_bytes() {
    for case in cases() {
        let files = entries(&case.library);
        let resolver = Resolver::new(&files);
        let ctx = BodyCtx::new(&files, &resolver);
        let note = ctx
            .index_of(&case.note)
            .unwrap_or_else(|| panic!("{}: note {} is not in the library", case.name, case.note));
        let image = ctx
            .index_of(&case.image)
            .unwrap_or_else(|| panic!("{}: image {} is not in the library", case.name, case.image));
        let body = read(&format!("body/{}", case.in_file));
        let label = || format!("{} ({})", case.name, case.desc);
        match case.op.as_str() {
            "append" => match append_member(&body, note, image, &ctx) {
                Err(err) => assert_eq!(
                    Some(err.to_string()),
                    case.error,
                    "{}: unexpected refusal",
                    label()
                ),
                Ok(appended) => {
                    assert_eq!(case.error, None, "{}: expected a refusal", label());
                    let want = read(&format!(
                        "body/{}",
                        case.out.as_deref().unwrap_or(&case.in_file)
                    ));
                    assert_eq!(appended.bytes, want, "{}: bytes", label());
                    match &case.added_text {
                        Some(text) => {
                            let range = appended
                                .added
                                .clone()
                                .unwrap_or_else(|| panic!("{}: no added range", label()));
                            assert_eq!(
                                String::from_utf8(appended.bytes[range].to_vec()).unwrap(),
                                *text,
                                "{}: the added range must hold exactly the new embed line",
                                label()
                            );
                        }
                        None => assert_eq!(appended.added, None, "{}: expected no change", label()),
                    }
                    assert!(
                        appended.added.is_none()
                            || verify_points_to(&appended.bytes, note, image, &ctx)
                                .unwrap_or(false),
                        "{}: written bytes must re-resolve to the image",
                        label()
                    );
                }
            },
            "remove" => {
                let removed = remove_member(&body, note, image, &ctx)
                    .unwrap_or_else(|err| panic!("{}: {err}", label()));
                let want = read(&format!(
                    "body/{}",
                    case.out.as_deref().unwrap_or(&case.in_file)
                ));
                assert_eq!(removed.bytes, want, "{}: bytes", label());
                let taken: Vec<String> = removed
                    .removed
                    .iter()
                    .map(|range| String::from_utf8(body[range.clone()].to_vec()).unwrap())
                    .collect();
                let mut got = taken.clone();
                let mut want_taken = case.deleted.clone();
                got.sort();
                want_taken.sort();
                assert_eq!(got, want_taken, "{}: bytes taken out", label());
                assert_eq!(
                    apply_removals(&body, &removed.removed),
                    removed.bytes,
                    "{}: the reported ranges must account for the whole diff",
                    label()
                );
            }
            "member" => {
                let want = case
                    .member
                    .unwrap_or_else(|| panic!("{}: no member value", label()));
                assert_eq!(
                    is_member(&body, note, image, &ctx).unwrap(),
                    want,
                    "{}: membership",
                    label()
                );
            }
            other => panic!("{}: unknown op {other}", case.name),
        }
    }
}

#[test]
fn member_spans_distinguish_a_member_line_from_an_inline_embed() {
    let files = entries(&[
        "c.md".to_string(),
        "girl.jpg".to_string(),
        "curl.jpg".to_string(),
    ]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    let body = b"![[girl.jpg]] ![[curl.jpg]]\n![[girl.jpg|the girl]]\n";
    let hits = member_spans(body, 0, 1, &ctx).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(!hits[0].whole_line, "two embeds share the line");
    assert!(hits[1].whole_line, "one embed alone makes a member line");
    assert_eq!(hits[1].line_range, 28..51);
    assert!(!hits[0].range.is_empty());
    // Prose on the embed's own line is the user's: the link is not the whole
    // line, so the line itself must never be taken out (ADR-017 §5).
    let with_caption = b"![[girl.jpg]] a caption\n";
    let hits = member_spans(with_caption, 0, 1, &ctx).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].whole_line, "the caption shares the line");
}

#[test]
fn a_plain_wikilink_is_not_membership() {
    let files = entries(&["c.md".to_string(), "girl.jpg".to_string()]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    assert!(!is_member(b"[[girl.jpg]]\n", 0, 1, &ctx).unwrap());
    assert!(is_member(b"![[girl.jpg]]\n", 0, 1, &ctx).unwrap());
}

/// Run `f` against a library made of these library-relative paths.
fn with_ctx<T>(rels: &[&str], f: impl Fn(&BodyCtx<'_>) -> T) -> T {
    let strings: Vec<String> = rels.iter().map(|rel| rel.to_string()).collect();
    let files = entries(&strings);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    f(&ctx)
}

#[test]
fn link_text_names_the_image_unambiguously_or_refuses() {
    // A nested image is named by its full library path, so a namesake added
    // later cannot make the link ambiguous (ADR-017 §4).
    assert_eq!(
        with_ctx(&["c.md", "refs/a/x.jpg"], |ctx| link_text_for(
            ctx.index_of("refs/a/x.jpg").unwrap(),
            ctx
        )
        .unwrap()),
        Embed::Text("![[refs/a/x.jpg]]".to_string())
    );
    // A root image is named by its bare name, which is also its full path.
    assert_eq!(
        with_ctx(&["c.md", "x.jpg"], |ctx| link_text_for(
            ctx.index_of("x.jpg").unwrap(),
            ctx
        )
        .unwrap()),
        Embed::Text("![[x.jpg]]".to_string())
    );
    // Two files answer to that bare name: refuse, never guess (FORMAT §5.1 r3).
    assert_eq!(
        with_ctx(&["c.md", "x.jpg", "refs/x.jpg"], |ctx| link_text_for(
            ctx.index_of("x.jpg").unwrap(),
            ctx
        )
        .unwrap()),
        Embed::RenameOrMove {
            name: "x.jpg".to_string(),
            matches: vec!["x.jpg".to_string(), "refs/x.jpg".to_string()],
        }
    );
    assert_eq!(
        with_ctx(&["c.md", "x.jpg", "refs/x.jpg"], |ctx| link_text_for(
            ctx.index_of("refs/x.jpg").unwrap(),
            ctx
        )
        .unwrap()),
        Embed::Text("![[refs/x.jpg]]".to_string())
    );
    // Names that break wikilinks are reported, never renamed here (FORMAT §2.1).
    assert_eq!(
        with_ctx(&["c.md", "na#me.jpg"], |ctx| link_text_for(
            ctx.index_of("na#me.jpg").unwrap(),
            ctx
        )),
        Err(EditError::UnlinkableName {
            path: "na#me.jpg".to_string(),
            characters: vec!['#'],
        })
    );
}

#[test]
fn removing_from_the_images_own_note_refuses() {
    let files = entries(&["girl.jpg".to_string(), "girl.jpg.md".to_string()]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    let body = b"---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n\n![[girl.jpg]]\n";
    assert!(is_member(body, 1, 0, &ctx).unwrap());
    assert_eq!(
        remove_member(body, 1, 0, &ctx).err(),
        Some(EditError::SelfEmbed)
    );
    // Another image in the same note is a normal collection member.
    let files = entries(&[
        "girl.jpg".to_string(),
        "girl.jpg.md".to_string(),
        "curl.jpg".to_string(),
    ]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    let body = b"---\nkind: collection\n---\n\n![[girl.jpg]]\n\n![[curl.jpg]]\n";
    let removed = remove_member(body, 1, 2, &ctx).unwrap();
    assert_eq!(
        String::from_utf8(removed.bytes).unwrap(),
        "---\nkind: collection\n---\n\n![[girl.jpg]]\n\n"
    );
}

#[test]
fn new_collection_body_is_front_matter_then_one_embed_per_image() {
    let files = entries(&[
        "c.md".to_string(),
        "refs/a/x.jpg".to_string(),
        "y.jpg".to_string(),
    ]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    let images = vec![
        ctx.index_of("refs/a/x.jpg").unwrap(),
        ctx.index_of("y.jpg").unwrap(),
    ];
    assert_eq!(
        String::from_utf8(new_collection_body("Sleep PV", &images, &ctx).unwrap()).unwrap(),
        "---\nkind: collection\ntitle: Sleep PV\n---\n\n![[refs/a/x.jpg]]\n![[y.jpg]]\n"
    );
    assert_eq!(
        String::from_utf8(new_collection_body("", &images, &ctx).unwrap()).unwrap(),
        "---\nkind: collection\n---\n\n![[refs/a/x.jpg]]\n![[y.jpg]]\n"
    );
    // A title YAML could misread is quoted, not rewritten.
    assert_eq!(
        String::from_utf8(new_collection_body("no: 4", &[], &ctx).unwrap()).unwrap(),
        "---\nkind: collection\ntitle: \"no: 4\"\n---\n\n"
    );
    // The whole list is refused when one image cannot be named.
    let clash = entries(&[
        "c.md".to_string(),
        "y.jpg".to_string(),
        "refs/y.jpg".to_string(),
    ]);
    let resolver = Resolver::new(&clash);
    let ctx = BodyCtx::new(&clash, &resolver);
    assert!(new_collection_body("x", &[1, 2], &ctx).is_err());
}

#[test]
fn safe_file_name_strips_what_breaks_links() {
    assert_eq!(safe_file_name("Sleep PV — shot 1"), "Sleep PV — shot 1.md");
    assert_eq!(safe_file_name("a[b]c#d^e|f"), "abcdef.md");
    assert_eq!(safe_file_name("refs/deep/name"), "refsdeepname.md");
    assert_eq!(safe_file_name("..hidden"), "hidden.md");
    assert_eq!(safe_file_name("   "), "untitled.md");
    assert_eq!(safe_file_name("[]#^|"), "untitled.md");
    assert_eq!(safe_file_name("tab\tsep"), "tabsep.md");
    assert_eq!(safe_file_name("trailing. "), "trailing.md");
}

#[derive(Deserialize)]
struct FolderCase {
    file: String,
    current: String,
    collection: String,
    collection_source: String,
    upload: String,
    upload_source: String,
}

fn source_name(source: FolderSource) -> &'static str {
    match source {
        FolderSource::Setting => "setting",
        FolderSource::Current => "current",
        FolderSource::RootFallback => "root-fallback",
        FolderSource::InboxFallback => "inbox-fallback",
    }
}

fn folder_cases() -> Vec<FolderCase> {
    let text = std::fs::read_to_string(fixtures().join("body/folders.json"))
        .expect("read body/folders.json");
    serde_json::from_str(&text).expect("folders.json")
}

#[test]
fn app_json_folder_spellings_resolve_to_known_answers() {
    let files = entries(&[
        "girl.jpg".to_string(),
        "refs/deep/x.jpg".to_string(),
        "assets/keep/y.jpg".to_string(),
    ]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    for case in folder_cases() {
        let text = std::fs::read_to_string(fixtures().join("app").join(&case.file))
            .unwrap_or_else(|err| panic!("read app/{}: {err}", case.file));
        let label = || format!("{}/current={}", case.file, case.current);
        let collection = body_edit::collection_folder(Some(&text), &case.current, &ctx);
        assert_eq!(
            (collection.folder.as_str(), source_name(collection.source)),
            (case.collection.as_str(), case.collection_source.as_str()),
            "newFileLocation: {}",
            label()
        );
        let upload = body_edit::upload_folder(Some(&text), &case.current, &ctx);
        assert_eq!(
            (upload.folder.as_str(), source_name(upload.source)),
            (case.upload.as_str(), case.upload_source.as_str()),
            "attachmentFolderPath: {}",
            label()
        );
    }
    // No file at all: Obsidian's own defaults.
    let collection = body_edit::collection_folder(None, "refs", &ctx);
    assert_eq!(
        (collection.folder.as_str(), collection.source),
        ("", FolderSource::RootFallback)
    );
    let upload = body_edit::upload_folder(None, "refs", &ctx);
    assert_eq!(
        (upload.folder.as_str(), upload.source),
        ("inbox", FolderSource::InboxFallback)
    );
    assert_eq!(upload.join("z.jpg"), "inbox/z.jpg");
    assert_eq!(collection.join("c.md"), "c.md");
}

#[test]
fn bytes_that_cannot_be_edited_are_refused_by_name() {
    let files = entries(&["c.md".to_string(), "girl.jpg".to_string()]);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    assert_eq!(
        append_member(b"not \xff utf8", 0, 1, &ctx).err(),
        Some(EditError::NotUtf8)
    );
    assert_eq!(
        remove_member(b"\xff".as_slice(), 0, 1, &ctx).err(),
        Some(EditError::NotUtf8)
    );
    // A canvas is not a Markdown note: its links are JSON, not embed lines.
    let with_canvas = entries(&[
        "c.canvas".to_string(),
        "girl.jpg".to_string(),
        "x.md".to_string(),
    ]);
    let resolver = Resolver::new(&with_canvas);
    let ctx = BodyCtx::new(&with_canvas, &resolver);
    assert_eq!(
        is_member(b"{}\n".as_slice(), 0, 1, &ctx).err(),
        Some(EditError::NotANote)
    );
    assert_eq!(
        append_member(b"hi\n".as_slice(), 99, 1, &ctx).err(),
        Some(EditError::NotInLibrary)
    );
    // An image entry, not a note, cannot be the collection.
    assert_eq!(
        is_member(b"hi\n".as_slice(), 1, 1, &ctx).err(),
        Some(EditError::NotANote)
    );
}

#[cfg(unix)]
#[test]
fn an_image_under_a_non_utf8_folder_cannot_be_named() {
    use std::os::unix::ffi::OsStrExt;
    let native = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"d\xff"));
    let mut image = entries(&["placeholder".to_string()]);
    image[0] = FileEntry {
        rel: "d\u{FFFD}/x.jpg".to_string(),
        name: "x.jpg".to_string(),
        path: native.join("x.jpg"),
        native_name: std::ffi::OsString::from("x.jpg"),
        class: classify("x.jpg"),
    };
    image.push(FileEntry {
        rel: "c.md".to_string(),
        name: "c.md".to_string(),
        path: PathBuf::from("c.md"),
        native_name: std::ffi::OsString::from("c.md"),
        class: classify("c.md"),
    });
    let resolver = Resolver::new(&image);
    let ctx = BodyCtx::new(&image, &resolver);
    assert_eq!(
        link_text_for(0, &ctx).err(),
        Some(EditError::NonUtf8Path),
        "a lossy display path could alias a different real folder"
    );
}
