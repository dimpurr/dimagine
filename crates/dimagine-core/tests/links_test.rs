//! Link extraction and FORMAT §5.1 resolution.

mod support;

use dimagine_core::library::{FileClass, FileEntry};
use dimagine_core::links::{
    extract_canvas_refs, extract_markdown_links, key, target_class, LinkSyntax, Outcome, Resolver,
    TargetClass,
};

fn files(rels: &[&str]) -> Vec<FileEntry> {
    rels.iter()
        .map(|rel| {
            let name = rel.rsplit('/').next().unwrap();
            FileEntry {
                rel: rel.to_string(),
                name: name.to_string(),
                path: std::path::PathBuf::from(rel),
                native_name: std::ffi::OsString::from(name),
                class: dimagine_core::library::classify(name),
            }
        })
        .collect()
}

#[test]
fn extracts_wikilinks_embeds_and_markdown_images() {
    let text = "See [[girl.jpg]] and ![[girl.jpg]]\n![alt text](refs/girl.png)\n";
    let links = extract_markdown_links(text, 1);
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].syntax, LinkSyntax::WikiLink);
    assert_eq!(links[0].target, "girl.jpg");
    assert_eq!(links[0].raw, "girl.jpg");
    assert_eq!(links[0].line, 1);
    assert_eq!(links[1].syntax, LinkSyntax::WikiEmbed);
    assert_eq!(links[2].syntax, LinkSyntax::MarkdownImage);
    assert_eq!(links[2].target, "refs/girl.png");
    assert_eq!(links[2].line, 2);
}

#[test]
fn alias_heading_and_block_parts_are_stripped() {
    let links = extract_markdown_links(
        "[[girl.jpg|the girl]] [[girl.jpg#closeup]] ![[girl.jpg#^block-id|big]]",
        1,
    );
    assert_eq!(links[0].target, "girl.jpg");
    assert_eq!(links[0].raw, "girl.jpg|the girl");
    assert_eq!(links[1].target, "girl.jpg");
    assert_eq!(links[2].target, "girl.jpg");
}

#[test]
fn markdown_paths_angle_brackets_titles_and_percent_encoding() {
    let links = extract_markdown_links(
        "![](<my file.jpg> \"title\")\n![](my%20file.jpg)\n![](space%20shot.jpg \"Photo\")\n",
        1,
    );
    assert_eq!(links[0].target, "my file.jpg");
    assert_eq!(links[0].raw, "my file.jpg");
    assert_eq!(links[1].target, "my file.jpg");
    assert_eq!(links[2].target, "space shot.jpg");
}

#[test]
fn markdown_destinations_accept_balanced_parentheses() {
    let links = extract_markdown_links(
        "![](<shot(1).jpg>)\n![](shot(1).jpg)\n![](shot\\(2\\).jpg)\n",
        1,
    );
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].target, "shot(1).jpg");
    assert_eq!(links[1].target, "shot(1).jpg");
    assert_eq!(links[2].target, "shot(2).jpg");
}

#[test]
fn external_urls_and_empty_targets_are_not_library_links() {
    let links = extract_markdown_links(
        "![](https://example.com/pic.jpg)\n![](data:image/png;base64,AAAA)\n[[#heading]]\n[[|alias]]\n",
        1,
    );
    assert_eq!(links.len(), 4, "extracted but skipped at resolve time");
    for link in &links {
        let outcome = Resolver::new(&files(&[])).resolve(
            &link.target,
            "",
            if link.syntax == LinkSyntax::WikiLink {
                LinkSyntax::WikiLink
            } else {
                LinkSyntax::MarkdownImage
            },
        );
        assert_eq!(outcome, Outcome::NotImageTarget, "{:?}", link.target);
    }
}

#[test]
fn fenced_code_blocks_are_skipped() {
    let text = "before\n```\n![[ghost.jpg]]\n[[ghost.jpg]]\n```\nafter ![[real.jpg]]\n";
    let links = extract_markdown_links(text, 1);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target, "real.jpg");
    assert_eq!(links[0].line, 6);
}

#[test]
fn tilde_fences_work_too() {
    let text = "~~~~\n![[ghost.jpg]]\n~~~~\n![[real.jpg]]\n";
    let links = extract_markdown_links(text, 1);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target, "real.jpg");
}

#[test]
fn inline_code_spans_are_skipped() {
    let links = extract_markdown_links("try `![[ghost.jpg]]` inline\n![](real.png)\n", 1);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target, "real.png");
}

#[test]
fn lines_count_the_whole_document() {
    // The extractor gets the body's first line; a front matter block before
    // it must still produce absolute line numbers.
    let body = "\nfirst\n\n![[girl.jpg]]\n";
    let links = extract_markdown_links(body, 7);
    assert_eq!(links[0].line, 10);
}

// ---------------------------------------------------------------- resolver

fn entry(rel: &str, class: FileClass) -> FileEntry {
    FileEntry {
        rel: rel.to_string(),
        name: rel.rsplit('/').next().unwrap().to_string(),
        path: std::path::PathBuf::from(rel),
        native_name: std::ffi::OsString::from(rel.rsplit('/').next().unwrap()),
        class,
    }
}

fn library_of(entries: &[(&str, FileClass)]) -> Vec<FileEntry> {
    entries
        .iter()
        .map(|(rel, class)| entry(rel, *class))
        .collect()
}

#[test]
fn bare_names_resolve_when_unique_and_ambiguous_when_not() {
    let files = library_of(&[
        ("a/girl.jpg", FileClass::Image),
        ("b/girl.jpg", FileClass::Image),
        ("c/solo.png", FileClass::Image),
    ]);
    let resolver = Resolver::new(&files);
    // FORMAT §5.1 rule 2: exactly one file with that name.
    assert!(matches!(
        resolver.resolve("solo.png", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(2)
    ));
    // Rule 3: two files with that name -> ambiguous, never guessed.
    match resolver.resolve("girl.jpg", "", LinkSyntax::WikiLink) {
        Outcome::Ambiguous(hits) => {
            assert_eq!(hits.len(), 2, "both same-named files are candidates");
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
}

#[test]
fn bare_name_resolution_is_nfc_and_case_insensitive() {
    // A decomposed file name (e + combining acute), as macOS can produce.
    let nfd = "touche\u{301}.jpg";
    // The same name in composed form, as a link is usually typed.
    let nfc = "touch\u{e9}.jpg";
    let files = vec![entry(nfd, FileClass::Image)];
    assert_eq!(
        files[0].name, nfd,
        "fixture setup writes the decomposed name"
    );
    let resolver = Resolver::new(&files);
    assert!(matches!(
        resolver.resolve(nfc, "", LinkSyntax::MarkdownImage),
        Outcome::Resolved(0)
    ));
    assert!(matches!(
        resolver.resolve("TOUCHÉ.JPG", "", LinkSyntax::MarkdownImage),
        Outcome::Resolved(0)
    ));
}

#[test]
fn path_links_try_root_then_note_folder() {
    let files = library_of(&[
        ("refs/root.jpg", FileClass::Image),
        ("x/note.jpg", FileClass::Image),
        ("x/leaf/in.jpg", FileClass::Image),
    ]);
    let resolver = Resolver::new(&files);
    // Root-relative hits first (FORMAT §5.1 rule 1).
    assert!(matches!(
        resolver.resolve("refs/root.jpg", "x", LinkSyntax::WikiEmbed),
        Outcome::Resolved(0)
    ));
    assert!(matches!(
        resolver.resolve("x/note.jpg", "x", LinkSyntax::WikiEmbed),
        Outcome::Resolved(1)
    ));
    assert_eq!(
        resolver.resolve("./x/note.jpg", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(1),
        "leading ./ is accepted"
    );
    // Root miss -> note-relative fallback.
    assert!(matches!(
        resolver.resolve("leaf/in.jpg", "x", LinkSyntax::MarkdownImage),
        Outcome::Resolved(2)
    ));
    // Bare names never use the note folder, only the global name index.
    assert_eq!(
        resolver.resolve("in.jpg", "refs", LinkSyntax::MarkdownImage),
        Outcome::Resolved(2)
    );
}

#[test]
fn parent_segments_resolve_against_the_note_folder() {
    let files = library_of(&[("a/c/x.jpg", FileClass::Image)]);
    let resolver = Resolver::new(&files);
    assert!(matches!(
        resolver.resolve("../c/x.jpg", "a/b", LinkSyntax::WikiEmbed),
        Outcome::Resolved(0)
    ));
    // Escaping the library root resolves to nothing.
    assert_eq!(
        resolver.resolve("../x.jpg", "", LinkSyntax::MarkdownImage),
        Outcome::NotFound
    );
    assert_eq!(
        resolver.resolve("../../escape.jpg", "a", LinkSyntax::MarkdownImage),
        Outcome::NotFound
    );
}

#[test]
fn image_extension_targets_report_missing_not_silent() {
    let files = library_of(&[("real.jpg", FileClass::Image)]);
    let resolver = Resolver::new(&files);
    assert_eq!(
        resolver.resolve("ghost.jpg", "", LinkSyntax::WikiLink),
        Outcome::NotFound
    );
    assert_eq!(
        resolver.resolve("refs/deep/ghost.jpg", "", LinkSyntax::MarkdownImage),
        Outcome::NotFound
    );
}

#[test]
fn note_like_targets_are_never_image_findings() {
    let files = library_of(&[
        ("real.jpg", FileClass::Image),
        ("ideas.md", FileClass::Note),
    ]);
    let resolver = Resolver::new(&files);
    // Missing .md links are note links, out of scope for findings.
    assert_eq!(
        resolver.resolve("ghost.md", "", LinkSyntax::WikiLink),
        Outcome::NotImageTarget
    );
    assert_eq!(
        resolver.resolve("ideas.md.missing", "", LinkSyntax::MarkdownImage),
        Outcome::NotImageTarget
    );
    assert_eq!(
        resolver.resolve("nope.json", "", LinkSyntax::WikiLink),
        Outcome::NotImageTarget
    );
}

#[test]
fn extensionless_targets_fall_back_to_image_stems() {
    let one = library_of(&[("dir/photo.jpg", FileClass::Image)]);
    let resolver = Resolver::new(&one);
    // ![[photo]] resolves like Obsidian would.
    assert!(matches!(
        resolver.resolve("photo", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(0)
    ));
    // A bare wikilink that matches nothing image-like is not reported (it
    // could be a note link).
    assert_eq!(
        resolver.resolve("ideas", "", LinkSyntax::WikiLink),
        Outcome::NotImageTarget
    );
    // An embed that matches no image could be a note embed: not reported.
    assert_eq!(
        resolver.resolve("wip-idea", "", LinkSyntax::WikiEmbed),
        Outcome::NotImageTarget
    );
    // But explicit Markdown image syntax can only mean an image.
    assert_eq!(
        resolver.resolve("wip-idea", "", LinkSyntax::MarkdownImage),
        Outcome::NotFound
    );
    // Ambiguity among stems is reported.
    let two = library_of(&[
        ("a/photo.jpg", FileClass::Image),
        ("b/photo.png", FileClass::Image),
    ]);
    let resolver = Resolver::new(&two);
    assert!(matches!(
        resolver.resolve("photo", "", LinkSyntax::WikiEmbed),
        Outcome::Ambiguous(_)
    ));
    // Path-like extensionless targets append image extensions.
    assert!(matches!(
        resolver.resolve("a/photo", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(0)
    ));
    assert!(matches!(
        resolver.resolve("b/photo", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(1)
    ));
    // An extless path that matches several candidate files stays ambiguous.
    let three = library_of(&[
        ("c/photo.jpg", FileClass::Image),
        ("c/photo.png", FileClass::Image),
    ]);
    let resolver = Resolver::new(&three);
    assert!(matches!(
        resolver.resolve("c/photo", "", LinkSyntax::WikiEmbed),
        Outcome::Ambiguous(_)
    ));
    assert!(matches!(
        resolver.resolve("d/nowhere", "", LinkSyntax::WikiEmbed),
        Outcome::NotImageTarget, // a note embed to a missing folder is not reported
    ));
}

#[test]
fn image_notes_do_not_satisfy_bare_image_names() {
    let files = library_of(&[
        ("girl.jpg", FileClass::Image),
        ("girl.jpg.md", FileClass::ImageNote),
    ]);
    let resolver = Resolver::new(&files);
    // `girl.jpg.md` is a different name; `girl.jpg` matches only the image.
    assert!(matches!(
        resolver.resolve("girl.jpg", "", LinkSyntax::WikiEmbed),
        Outcome::Resolved(0)
    ));
}

#[test]
fn canvas_file_nodes_are_extracted() {
    let canvas = serde_json::json!({
        "nodes": [
            {"id": "n1", "type": "file", "file": "refs/girl.jpg"},
            {"id": "n2", "type": "text", "text": "hello"},
            {"id": "n3", "type": "file", "file": "girl.png", "subpath": "#x"},
            {"id": "n4", "type": "link", "url": "https://example.com"}
        ]
    });
    let refs = extract_canvas_refs(&canvas);
    let targets: Vec<&str> = refs.iter().map(|r| r.file.as_str()).collect();
    assert_eq!(targets, vec!["refs/girl.jpg", "girl.png"]);
    assert_eq!(refs[0].node_id.as_deref(), Some("n1"));
}

#[test]
fn key_normalizes_nfc_and_case() {
    assert_eq!(key("Touché"), key("touche\u{301}"));
    assert_ne!(key("a b"), key("ab"));
}

#[test]
fn target_class_matches_expectations() {
    for (target, class) in [
        ("x.jpg", TargetClass::Image),
        ("x.JPG", TargetClass::Image),
        ("x.md", TargetClass::Text),
        ("x.canvas", TargetClass::Text),
        ("x.json", TargetClass::Text),
        ("x.pdf", TargetClass::Other),
        ("x", TargetClass::None),
    ] {
        assert_eq!(target_class(target), class, "{target}");
    }
}
