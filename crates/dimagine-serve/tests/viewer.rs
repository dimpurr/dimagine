//! The viewer's own routes, end to end: what each one answers, and what the
//! query string means (spec §6).

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{router, FsCatalog, OriginalPreview, ServeConfig};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

/// Enough of a PNG for the catalog to accept it as an image. The viewer never
/// decodes it: the thumbnails are the original bytes in these tests.
const PNG_BYTES: &str = "\u{89}PNG\r\n\u{1a}\nsynthetic-png-data";

/// A library with nested folders, a `refs` vs `refs2` prefix trap, a
/// collection, tags, and a note with an explicit `added` beside notes without
/// one (spec §6).
fn library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    write("refs/landscape.png", PNG_BYTES);
    write(
        "refs/landscape.png.md",
        "---\ntitle: Landscape\ntags:\n  - nature\n  - wide\nadded: 2026-01-02T03:04:05+00:00\n\
         source: https://example.com/landscape\n---\nA wide **landscape**.\n![[landscape.png]]\n",
    );
    write("refs/ui/button.png", PNG_BYTES);
    write(
        "refs/ui/button.png.md",
        "---\ntitle: Button\ntags:\n  - ui\nadded: 2026-02-03T04:05:06+00:00\n---\nA button.\n",
    );
    write("refs2/trap.png", PNG_BYTES);
    write("plain.png", PNG_BYTES);
    write(
        "browse.md",
        "---\ntitle: Browse\n---\n![[refs/landscape.png]]\nFirst.\n![[plain.png]]\nSecond.\n",
    );
    dir
}

fn app(dir: &TempDir) -> axum::Router {
    router(
        FsCatalog::new(dir.path()).unwrap(),
        OriginalPreview,
        ServeConfig {
            // Account state inside the test's own directory: no test
            // may read or write the real user state directory.
            data_dir: dir.path().join("state"),
            ..ServeConfig::default()
        },
    )
}

/// A library shaped like the real one that flooded the collection list
/// (W27f): a `kind: collection` note, a plain note that embeds images, and an
/// IMAGE note that embeds a sibling beside its own preview. All three are
/// valid collections (FORMAT §5); the list carries the first two only.
fn flood_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    write("refs/page-01.png", PNG_BYTES);
    write("refs/page-02.png", PNG_BYTES);
    write(
        "refs/page-01.png.md",
        "---\ntitle: Page one\n---\nPreviewing the shot beside a sibling.\n\n\
         ![[page-02.png]]\n![[page-01.png]]\n",
    );
    write(
        "guide.md",
        "---\nkind: collection\ntitle: Guide\n---\nPlanned shots go here.\n",
    );
    write(
        "roundup.md",
        "---\ntitle: Roundup\n---\n![[refs/page-01.png]]\n![[refs/page-02.png]]\n",
    );
    dir
}

/// 205 images without notes: enough that a lens which stops at 200 has
/// something to stop at (W34 audit #10).
fn big_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("refs")).unwrap();
    for index in 0..205 {
        fs::write(root.join(format!("refs/rec-{index:03}.jpg")), PNG_BYTES).unwrap();
    }
    dir
}

/// `big_library` with one more thing: a collection note that embeds every image
/// in its own order, so the collection view is longer than the page size while
/// being the one view that has no pages to show (`/?c=` lists all members).
fn big_collection_library() -> TempDir {
    let dir = big_library();
    let mut note = String::from("---\nkind: collection\ntitle: Album\n---\n");
    for index in 0..205 {
        note.push_str(&format!("![[refs/rec-{index:03}.jpg]]\n"));
    }
    fs::write(dir.path().join("album.md"), note).unwrap();
    dir
}

/// 210 images whose `added` times straddle the Recent lens's 200-image
/// window: `oldtag` sits on the ten oldest images — every one of them
/// outside the newest 200 — and `newtag` on the ten newest, all inside
/// it. The 190 between carry no note, so they are untagged and their
/// `added` time is the scan's own, which lands between the two tagged
/// tens. `refs` also holds a subfolder with ten images, so a `sub=0`
/// view has direct members to count beside the recursive one, and
/// `album.md` collects two old and three middle images, so a collection
/// row can disagree with its link the same way the tag and folder rows
/// can.
fn window_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    // The ten oldest: outside the lens's window, tagged `oldtag`.
    for index in 0..10 {
        let name = format!("old-{index:02}.jpg");
        write(&format!("refs/{name}"), PNG_BYTES);
        write(
            &format!("refs/{name}.md"),
            &format!(
                "---\ntags:\n  - oldtag\nadded: 2020-01-01T00:{index:02}:00+00:00\n---\nOld.\n",
            ),
        );
    }
    // The 190 the window is full of: no note, so untagged, and an
    // `added` time of the scan's own — between the two tagged tens.
    for index in 0..180 {
        write(&format!("refs/mid-{index:03}.jpg"), PNG_BYTES);
    }
    for index in 0..10 {
        write(&format!("refs/sub/deep-{index:02}.jpg"), PNG_BYTES);
    }
    // The ten newest: inside the lens's window, tagged `newtag`.
    for index in 0..10 {
        let name = format!("new-{index:02}.jpg");
        write(&format!("refs/{name}"), PNG_BYTES);
        write(
            &format!("refs/{name}.md"),
            &format!(
                "---\ntags:\n  - newtag\nadded: 2030-01-01T00:{index:02}:00+00:00\n---\nNew.\n",
            ),
        );
    }
    // A collection straddling the window's edge: two members the lens
    // does not reach and three it does.
    write(
        "album.md",
        "---\nkind: collection\ntitle: Album\n---\n\
         ![[refs/old-00.jpg]]\n![[refs/old-01.jpg]]\n\
         ![[refs/mid-000.jpg]]\n![[refs/mid-001.jpg]]\n![[refs/mid-002.jpg]]\n",
    );
    dir
}

async fn login(router: axum::Router) -> (axum::Router, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("passcode=2333"))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    (router, cookie)
}

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn text(app: &axum::Router, uri: &str, cookie: &str) -> String {
    let response = get(app, uri, cookie).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn json(app: &axum::Router, uri: &str, cookie: &str) -> Value {
    let response = get(app, uri, cookie).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

/// The script the page actually serves — the one it names by hash, so a test
/// cannot pass against a stale asset. This repository has no JavaScript harness,
/// so a claim about what the client does is a claim about these bytes.
async fn served_script(app: &axum::Router, cookie: &str, page: &str) -> String {
    let hash = page
        .split("src=\"/assets/app-")
        .nth(1)
        .expect("the page loads the script")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let response = get(app, &format!("/assets/app-{hash}"), cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

/// The stylesheet the page actually serves — the one it names by hash, so a
/// test cannot pass against a stale asset. There is no CSS harness either:
/// a claim about what the stylesheet does is a claim about these bytes.
async fn served_stylesheet(app: &axum::Router, cookie: &str, page: &str) -> String {
    let hash = page
        .split("href=\"/assets/app-")
        .nth(1)
        .expect("the page loads the stylesheet")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let response = get(app, &format!("/assets/app-{hash}"), cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/css; charset=utf-8"
    );
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

/// Every `@media (hover: none)` block of the stylesheet, as text — brace
/// counted, so a rule hidden inside one is found wherever it sits.
fn hover_none_blocks(css: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut at = 0;
    while let Some(found) = css[at..].find("@media (hover: none)") {
        let start = at + found;
        let Some(open_rel) = css[start..].find('{') else {
            break;
        };
        let open = start + open_rel;
        let mut depth = 0usize;
        let mut close = None;
        for (offset, character) in css[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            break;
        };
        blocks.push(&css[start..=close]);
        at = close + 1;
    }
    blocks
}

/// The paths `/api/view` lists for a query, in the order it lists them.
async fn json_paths(app: &axum::Router, query: &str, cookie: &str) -> Vec<String> {
    let uri = if query.is_empty() {
        "/api/view".to_owned()
    } else {
        format!("/api/view?{query}")
    };
    json(app, &uri, cookie).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap().to_owned())
        .collect()
}

/// The paths a page lists as tiles, in the order it lists them.
async fn page_paths(app: &axum::Router, query: &str, cookie: &str) -> Vec<String> {
    let uri = if query.is_empty() {
        "/".to_owned()
    } else {
        format!("/?{query}")
    };
    html_paths(&text(app, &uri, cookie).await)
}

/// The image paths a page links to as tiles, in the order it links them.
fn html_paths(html: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("class=\"tile\" data-path=\"") {
        rest = &rest[start + "class=\"tile\" data-path=\"".len()..];
        let end = rest.find('"').expect("a closed attribute");
        paths.push(rest[..end].replace("&amp;", "&"));
        rest = &rest[end..];
    }
    paths
}

/// A library for the note-body renderer (FORMAT §5.1): a
/// note that links to and embeds other images, with the
/// tricky targets — spaces, unicode, brackets — a note
/// link, and the XSS shapes.
fn link_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    write("refs/landscape.png", PNG_BYTES);
    write("refs/ui/button.png", PNG_BYTES);
    write("refs/my photo.png", PNG_BYTES);
    write("refs/猫.png", PNG_BYTES);
    write("refs/a[b].png", PNG_BYTES);
    write("guide.md", "---\ntitle: Guide\n---\nA guide.\n");
    write(
        "refs/landscape.png.md",
        "---\ntitle: Landscape\ntags:\n  - nature\nrating: 4\nsource: https://example.com/landscape\nauthor: Example Artist\nlicense: unknown\ncreated: 2021-10-15\nimported: 2026-10-04T14:30:12+01:00\nadded: 2023-07-12T20:54:07+01:00\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\nheight: 3429\n---\n\
         A wide shot.\n\
         See [[refs/ui/button.png]] and [[refs/ui/button.png|the button]].\n\
         ![[refs/my photo.png]]\n\
         ![[landscape.png]]\n\
         [[missing.png]] and [[refs/猫.png]] and [[refs/a[b].png]].\n\
         [[guide]] and [[evil.png|<script>alert(1)</script>]]\n",
    );
    write(
        "refs/ui/button.png.md",
        "---\ntitle: Button\n---\nA button.\n",
    );
    dir
}

/// The rendered note body of an image page, without
/// the markup around it.
fn note_body(html: &str) -> &str {
    let start = html
        .find("<article class=\"note-body\">")
        .expect("a note body")
        + "<article class=\"note-body\">".len();
    let end = html[start..]
        .find("</article>")
        .expect("a closed note body");
    &html[start..start + end]
}

#[tokio::test]
async fn the_note_body_renders_links_and_embeds_per_format_5_1() {
    let dir = link_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/refs/landscape.png", &cookie).await;
    let body = note_body(&html);

    // A resolved link to another image: its page, with the
    // file name as the label.
    assert!(
        body.contains(
            "<a href=\"/image/refs/ui/button.png\" rel=\"noopener noreferrer\">button.png</a>"
        ),
        "{body}"
    );
    // An aliased link uses the alias.
    assert!(
        body.contains(
            "<a href=\"/image/refs/ui/button.png\" rel=\"noopener noreferrer\">the button</a>"
        ),
        "{body}"
    );
    // An embed of another image: its view rendition, linked
    // to its page.
    assert!(
        body.contains(
            "<a href=\"/image/refs/my%20photo.png\" rel=\"noopener noreferrer\"><img src=\"/media/refs/my%20photo.png\" alt=\"my photo.png\"></a>"
        ),
        "{body}"
    );
    // A link to a note: the note view.
    assert!(
        body.contains("<a href=\"/?c=guide.md\" rel=\"noopener noreferrer\">guide</a>"),
        "{body}"
    );
    // Unicode and brackets in a target are encoded.
    assert!(
        body.contains("<a href=\"/image/refs/%E7%8C%AB.png\" rel=\"noopener noreferrer\">"),
        "{body}"
    );
    assert!(
        body.contains("<a href=\"/image/refs/a%5Bb%5D.png\" rel=\"noopener noreferrer\">"),
        "{body}"
    );
    // A link that resolves to nothing stays plain text,
    // marked as missing.
    assert!(
        body.contains("<span class=\"missing-link\">[[missing.png]]</span>"),
        "{body}"
    );
    // The note's own embed (FORMAT §3.2) is not repeated:
    // the page already shows the image.
    assert!(!body.contains("landscape.png</a>"), "{body}");
    assert!(
        !body.contains("<img src=\"/media/refs/landscape.png\""),
        "{body}"
    );
    // XSS shapes stay escaped text, never markup. This is the note fragment,
    // not the whole page (W43's theme boot is the frame's one script tag).
    assert!(!body.contains("<script>"), "{body}");
    assert!(
        body.contains("<span class=\"missing-link\">[[evil.png|&lt;script&gt;alert(1)&lt;/script&gt;]]</span>"),
        "{body}"
    );
}

#[tokio::test]
async fn the_image_page_properties_are_a_definition_list() {
    let dir = link_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/refs/landscape.png", &cookie).await;

    // Known FORMAT §3.1 properties, in show order.
    assert!(html.contains("<dl class=\"property-list\">"), "{html}");
    assert!(html.contains("<dt>Title</dt><dd>Landscape</dd>"), "{html}");
    assert!(
        html.contains(
            "<dt>Tags</dt><dd><a class=\"inspector-tag\" href=\"/?tag=nature\">nature</a></dd>"
        ),
        "{html}"
    );
    assert!(html.contains("<dt>Rating</dt><dd>4</dd>"), "{html}");
    assert!(
        html.contains(
            "<dt>Source</dt><dd><a class=\"source-link\" href=\"https://example.com/landscape\" rel=\"noreferrer noopener\">https://example.com/landscape</a></dd>"
        ),
        "{html}"
    );
    assert!(
        html.contains("<dt>Author</dt><dd>Example Artist</dd>"),
        "{html}"
    );
    assert!(html.contains("<dt>License</dt><dd>unknown</dd>"), "{html}");
    assert!(
        html.contains("<dt>Created</dt><dd><span title=\"2021-10-15\">15 Oct 2021</span></dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Imported</dt><dd><span title=\"2026-10-04T14:30:12+01:00\">04 Oct 2026, 14:30 (+01:00)</span></dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Added</dt><dd><span title=\"2023-07-12T20:54:07+01:00\">12 Jul 2023, 20:54 (+01:00)</span></dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>ID</dt><dd><code>01JA8X3Q7K2M9V4T6R1B5N0C3Q</code></dd>"),
        "{html}"
    );
    // An unknown property follows the known ones.
    assert!(html.contains("<dt>height</dt><dd>3429</dd>"), "{html}");
    let title_at = html.find("<dt>Title</dt>").unwrap();
    let height_at = html.find("<dt>height</dt>").unwrap();
    assert!(title_at < height_at, "{html}");
    // The raw front matter stays reachable, collapsed.
    assert!(
        html.contains("<details class=\"properties-raw\"><summary>Raw properties</summary>"),
        "{html}"
    );
    assert!(html.contains("<pre class=\"properties\">"), "{html}");
}

#[tokio::test]
async fn the_image_api_returns_the_rendered_note() {
    let dir = link_library();
    let (app, cookie) = login(app(&dir)).await;
    let detail = json(&app, "/api/image/refs/landscape.png", &cookie).await;

    // The raw body and the rendered body are both there.
    let body = detail["body"].as_str().unwrap();
    assert!(body.contains("[[refs/ui/button.png]]"), "{body}");
    let body_html = detail["body_html"].as_str().unwrap();
    assert!(
        body_html.contains(
            "<a href=\"/image/refs/ui/button.png\" rel=\"noopener noreferrer\">button.png</a>"
        ),
        "{body_html}"
    );
    assert!(
        body_html.contains("<img src=\"/media/refs/my%20photo.png\""),
        "{body_html}"
    );
    // The self-embed is not repeated in the rendered note.
    assert!(
        !body_html.contains("<img src=\"/media/refs/landscape.png\""),
        "{body_html}"
    );
    // An image without a note has no rendered body.
    let plain = json(&app, "/api/image/refs/%E7%8C%AB.png", &cookie).await;
    assert!(plain["body_html"].is_null(), "{plain}");
}

#[tokio::test]
async fn the_library_is_all_images_newest_added_first() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/", &cookie).await;

    let paths = html_paths(&html);
    assert_eq!(
        paths.len(),
        4,
        "every image is on the library page: {paths:?}"
    );
    // Added descending (spec §2): the February note before the January one.
    let button = paths
        .iter()
        .position(|path| path == "refs/ui/button.png")
        .expect("the button is listed");
    let landscape = paths
        .iter()
        .position(|path| path == "refs/landscape.png")
        .expect("the landscape is listed");
    assert!(button < landscape, "newest added first: {paths:?}");
    assert!(html.contains("refs2/trap.png"));
    assert!(html.contains("plain.png"));
}

#[tokio::test]
async fn api_view_answers_the_same_query_in_the_same_order() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    for uri in [
        "/",
        "/?sort=name",
        "/?sort=name&dir=desc",
        "/?sort=size",
        "/?sort=rating",
        "/?in=refs",
        "/?in=refs&sub=0",
        "/?tag=nature",
        "/?tag=nature&tag=wide",
        "/?tag=nature&tag=ui",
        "/?q=landscape",
        "/?untagged=1",
        "/?recent=1",
        "/?c=browse.md",
        "/?size=l",
    ] {
        let query = uri.strip_prefix("/?").unwrap_or("");
        let from_html = page_paths(&app, query, &cookie).await;
        let from_json = json_paths(&app, query, &cookie).await;
        assert_eq!(from_html, from_json, "/?{query} disagrees with /api/view");
    }
}

#[tokio::test]
async fn sort_direction_and_paging_are_honoured() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let ascending = json_paths(&app, "sort=name&dir=asc", &cookie).await;
    let descending = json_paths(&app, "sort=name&dir=desc", &cookie).await;
    let mut reversed = descending.clone();
    reversed.reverse();
    assert_eq!(ascending, reversed, "dir flips the name order");

    // Every page reports the whole total, not the page's own length.
    let page = json(&app, "/api/view?p=1", &cookie).await;
    assert_eq!(page["total"], 4);
    assert_eq!(page["page"], 1);
    assert_eq!(page["page_size"], 120);
    assert_eq!(page["items"].as_array().unwrap().len(), 4);

    let page = json(&app, "/api/view?p=7", &cookie).await;
    assert_eq!(page["total"], 4, "an empty page still states the total");
    assert_eq!(page["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_folder_does_not_reach_a_folder_that_only_shares_its_prefix() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    // The order is Added descending, so compare the set, not the sequence.
    let mut paths = json_paths(&app, "in=refs", &cookie).await;
    paths.sort();
    assert_eq!(paths, vec!["refs/landscape.png", "refs/ui/button.png"]);

    // Without subfolders, only the images the folder holds itself.
    let paths = json_paths(&app, "in=refs&sub=0", &cookie).await;
    assert_eq!(paths, vec!["refs/landscape.png"]);

    // And `refs2` is its own view, with one image.
    let paths = json_paths(&app, "in=refs2", &cookie).await;
    assert_eq!(paths, vec!["refs2/trap.png"]);
}

#[tokio::test]
async fn tags_are_combined_with_and() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let paths = json_paths(&app, "tag=nature", &cookie).await;
    assert_eq!(paths, vec!["refs/landscape.png"]);
    // Both tags are on the same note, so AND keeps it.
    let paths = json_paths(&app, "tag=nature&tag=wide", &cookie).await;
    assert_eq!(paths, vec!["refs/landscape.png"]);
    // One tag from each of two notes matches nothing.
    let paths = json_paths(&app, "tag=nature&tag=ui", &cookie).await;
    assert!(paths.is_empty());
}

#[tokio::test]
async fn a_collection_lists_its_members_in_the_notes_own_order() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let paths = json_paths(&app, "c=browse.md", &cookie).await;
    assert_eq!(
        paths,
        vec!["refs/landscape.png", "plain.png"],
        "the embed order wins over the added order"
    );
}

/// W27f: FORMAT §5 lets every note that embeds images be a collection, and
/// image notes embedding their siblings buried the deliberate ones on a real
/// library (1,175 collections, 1,170 of them image notes). The list a person
/// browses — the sidebar section, `/collections`, the `/api/sidebar`
/// `collections` array — carries a collection only when the note means to
/// collect: `kind: collection`, or a non-image note with an image embed
/// (`dimagine_index::collection_is_listed`, FORMAT §3.2/§5).
#[tokio::test]
async fn the_collection_list_carries_only_notes_that_mean_to_collect() {
    let dir = flood_library();
    let (app, cookie) = login(app(&dir)).await;

    // The API list: the `kind: collection` note and the plain note, in path
    // order, every item carrying the same three fields and a numeric count.
    let sidebar = json(&app, "/api/sidebar", &cookie).await;
    let collections = sidebar["collections"].as_array().unwrap();
    let paths: Vec<&str> = collections
        .iter()
        .map(|collection| collection["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec!["guide.md", "roundup.md"],
        "the image note stays off the list"
    );
    for collection in collections {
        let mut keys: Vec<&str> = collection
            .as_object()
            .unwrap()
            .keys()
            .map(|key| key.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["count", "path", "title"],
            "one shape for every item: {collection}"
        );
        assert!(
            collection["count"].is_u64(),
            "a count is always a number, never null: {collection}"
        );
    }
    assert_eq!(collections[0]["count"], 0, "no embeds yet");
    assert_eq!(collections[1]["count"], 2);

    // The sidebar section and the /collections page say the same thing.
    let library_page = text(&app, "/", &cookie).await;
    assert!(library_page.contains("href=\"/?c=guide.md\""));
    assert!(library_page.contains("href=\"/?c=roundup.md\""));
    assert!(
        !library_page.contains("/?c=refs/page-01.png.md"),
        "the image note is a collection, but not one the list carries"
    );
    let collections_page = text(&app, "/collections", &cookie).await;
    assert!(collections_page.contains("href=\"/?c=guide.md\""));
    assert!(collections_page.contains("href=\"/?c=roundup.md\""));
    assert!(!collections_page.contains("/?c=refs/page-01.png.md"));
}

/// W27f: leaving the list changes only the list. The image note that embeds
/// its sibling still works as a collection: `/?c=<image note>` shows its
/// members, and "appears in" on the image page still names it (FORMAT §5; the
/// self-embed stays a preview, FORMAT §3.2).
#[tokio::test]
async fn an_image_note_off_the_list_is_still_a_collection_for_the_rest() {
    let dir = flood_library();
    let (app, cookie) = login(app(&dir)).await;

    let paths = json_paths(&app, "c=refs/page-01.png.md", &cookie).await;
    assert_eq!(paths, vec!["refs/page-02.png"], "the sibling, not itself");
    let page = page_paths(&app, "c=refs/page-01.png.md", &cookie).await;
    assert_eq!(
        page,
        json_paths(&app, "c=refs/page-01.png.md", &cookie).await
    );

    // The image page names the image note in "appears in", listed or not.
    let html = text(&app, "/image/refs/page-02.png", &cookie).await;
    assert!(html.contains("Appears in"));
    assert!(html.contains("href=\"/?c=refs/page-01.png.md\""));
    assert!(html.contains("href=\"/?c=roundup.md\""));

    // W27f review L-1: the legacy pretty URL resolves too. "Off the list" is
    // not "not a collection", so `/collection/<note>` must not answer 404 for
    // the one collection the list leaves off.
    let response = get(&app, "/collection/refs/page-01.png.md", &cookie).await;
    assert_eq!(
        response.status(),
        StatusCode::PERMANENT_REDIRECT,
        "an unlisted collection still redirects"
    );
    assert_eq!(response.headers()["location"], "/?c=refs/page-01.png.md");

    // A note that is not a collection at all is still not redirected.
    let response = get(&app, "/collection/refs/page-03.png.md", &cookie).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn untagged_and_recent_are_lenses_with_counts() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let untagged = json_paths(&app, "untagged=1", &cookie).await;
    let mut sorted = untagged.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["plain.png", "refs2/trap.png"],
        "only the images whose note carries no tag, got {untagged:?}"
    );

    // W34 audit #10: Recent is a count, not a window — the 200 most recently
    // added images, however old. On this four-image library that is every
    // image, in the same newest-first order as All.
    let recent = json_paths(&app, "recent=1", &cookie).await;
    assert_eq!(recent, json_paths(&app, "", &cookie).await);

    let sidebar = json(&app, "/api/sidebar", &cookie).await;
    assert_eq!(sidebar["total"], 4);
    assert_eq!(sidebar["untagged"], 2);
    assert_eq!(sidebar["recent"], 4, "a small library is all recent");
}

/// W34 audit #10: "Recent" is the 200 most recently added images — not a
/// 30-day window that quietly covered 95% of a library imported inside it.
/// On 205 images the lens, the sidebar row, the page count and the paging all
/// agree: 200, and not one image more. The URL stays `/?recent=1`.
#[tokio::test]
async fn recent_is_the_last_two_hundred_not_the_whole_library() {
    let dir = big_library();
    let (app, cookie) = login(app(&dir)).await;

    let sidebar = json(&app, "/api/sidebar", &cookie).await;
    assert_eq!(sidebar["total"], 205);
    assert_eq!(sidebar["recent"], 200);
    assert!(
        text(&app, "/", &cookie)
            .await
            .contains("Recent — last 200 added</span><span class=\"sidebar-row-count\">200"),
        "the sidebar row states the definition beside the size"
    );

    let recent = json(&app, "/api/view?recent=1", &cookie).await;
    assert_eq!(
        recent["total"], 200,
        "the view states the lens's own size, 200 or fewer"
    );

    // The lens is the first 200 images of the library's own newest-first
    // order: page one carries 120, page two the last 80, and nothing follows
    // the 200th.
    let all_page_one = json_paths(&app, "", &cookie).await;
    let all_page_two = json_paths(&app, "p=2", &cookie).await;
    let mut expected = all_page_one.clone();
    expected.extend_from_slice(&all_page_two[..80]);
    let recent_page_one = json_paths(&app, "recent=1", &cookie).await;
    let recent_page_two = json_paths(&app, "recent=1&p=2", &cookie).await;
    assert_eq!(recent_page_one.len(), 120);
    assert_eq!(recent_page_two.len(), 80, "page two stops at the 200th");
    let listed: Vec<String> = recent_page_one.into_iter().chain(recent_page_two).collect();
    assert_eq!(listed, expected, "the lens is the 200 newest by Added");

    let page = json(&app, "/api/view?recent=1&p=3", &cookie).await;
    assert_eq!(page["total"], 200, "an empty page still states the size");
    assert!(page["items"].as_array().unwrap().is_empty());

    // The page says all of it: the label states the definition, the count
    // the size, and "Load more" ends on page two.
    let html = text(&app, "/?recent=1", &cookie).await;
    assert!(html.contains("<title>Recent — last 200 added · dimagine</title>"));
    assert!(html.contains("<h1>Recent — last 200 added</h1>"));
    assert!(html.contains("<span class=\"result-count\">200 items</span>"));
    assert!(html.contains("class=\"load-more-btn\" data-next-page=\"2\""));
    let html = text(&app, "/?recent=1&p=2", &cookie).await;
    assert!(
        !html.contains("load-more-btn"),
        "the 200th image is the last one the lens offers"
    );

    // W37 review, Medium #1 (W39 review M1/L1): the lens's order is not a
    // choice, so the toolbar states it — a focusable arrow whose sentence is an
    // attribute, rather than a sort the grid ignores, a disabled control a
    // keyboard cannot reach, or a wide hint that overflows the phone toolbar.
    let html = text(&app, "/?recent=1", &cookie).await;
    assert!(
        html.contains("role=\"note\" tabindex=\"0\""),
        "the Recent order is a focusable label: {html}"
    );
    // W39b review L1: the sentence is real text in the bubble the label
    // shows on hover or focus, so a sighted keyboard user reaches it too,
    // and the row still carries only the arrow (W39 review M1).
    assert!(
        html.contains(
            ">↓<span class=\"sort-tip\">Recent is always the last 200 added, newest first.</span>"
        ),
        "the order and its sentence in the bubble: {html}"
    );
    assert!(
        !html.contains("disabled"),
        "nothing on the Recent lens is unreachable: {html}"
    );
    // The sentence rides in `title`, `aria-label` and the bubble — three
    // copies, none of them text on the toolbar row itself.
    assert_eq!(
        html.matches("Recent is always the last 200 added, newest first.")
            .count(),
        3,
        "the sentence stays out of the row: {html}"
    );
    assert!(
        html.contains("title=\"Recent is always the last 200 added, newest first.\""),
        "and it says why: {html}"
    );
    // A URL that asks for a sort the lens cannot honour shows the same label
    // and says, in the notice box, that the sort was ignored.
    let html = text(&app, "/?recent=1&sort=name-asc", &cookie).await;
    assert!(
        html.contains(">↓<span class=\"sort-tip\">"),
        "the label shows the order in force, not the URL's: {html}"
    );
    assert!(
        html.contains("Ignored sort: Recent is always the last 200 added, newest first"),
        "{html}"
    );

    // W37 review, Medium #2: a page past the lens end says so, states the size
    // it still reports, and offers the way back — it never claims the library
    // is empty beside a count of 200.
    let html = text(&app, "/?recent=1&p=3", &cookie).await;
    assert!(html.contains("Nothing on this page"), "{html}");
    assert!(!html.contains("The library is empty"), "{html}");
    assert!(!html.contains("Nothing in Recent"), "{html}");
    assert!(
        html.contains("href=\"/?recent=1\">Back to the first page</a>"),
        "the way back to the images: {html}"
    );
    assert!(
        html.contains("<span class=\"result-count\">200 items</span>"),
        "the count and the state agree: {html}"
    );
}

/// W34 audit #12: a narrowed view says its own name, in the tab title and in
/// a heading above the grid, instead of the generic "Search" that never said
/// what was on screen.
#[tokio::test]
async fn a_narrowed_view_names_itself_in_the_title_and_the_heading() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let html = text(&app, "/?tag=nature", &cookie).await;
    assert!(
        html.contains("<title>Tag: nature · dimagine</title>"),
        "{html}"
    );
    assert!(html.contains("<h1>Tag: nature</h1>"));

    // Several tags: every one is named, in the order they narrow.
    let html = text(&app, "/?tag=wide&tag=nature", &cookie).await;
    assert!(html.contains("<title>Tag: wide, nature · dimagine</title>"));
    assert!(html.contains("<h1>Tag: wide, nature</h1>"));

    let html = text(&app, "/?in=refs", &cookie).await;
    assert!(html.contains("<title>Folder: refs · dimagine</title>"));
    assert!(html.contains("<h1>Folder: refs</h1>"));

    // A collection is named by what its note calls it.
    let html = text(&app, "/?c=browse.md", &cookie).await;
    assert!(html.contains("<title>Collection: Browse · dimagine</title>"));
    assert!(html.contains("<h1>Collection: Browse</h1>"));

    let html = text(&app, "/?q=landscape", &cookie).await;
    assert!(html.contains("<title>Search: landscape · dimagine</title>"));
    assert!(html.contains("<h1>Search: landscape</h1>"));

    // The lenses name themselves too…
    let html = text(&app, "/?untagged=1", &cookie).await;
    assert!(html.contains("<title>Untagged · dimagine</title>"));
    assert!(html.contains("<h1>Untagged</h1>"));

    // …and the whole library stays quietly "Library": it names itself.
    let html = text(&app, "/", &cookie).await;
    assert!(html.contains("<title>Library · dimagine</title>"));
    assert!(
        !html.contains("<h1"),
        "the home needs no heading above its pictures: {html}"
    );
}

#[tokio::test]
async fn the_sidebar_counts_match_the_index_counts() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let sidebar = json(&app, "/api/sidebar", &cookie).await;

    // Every count is the number of images the matching view returns.
    assert_eq!(sidebar["total"], 4);
    assert_eq!(
        sidebar["total"],
        json(&app, "/api/view", &cookie).await["total"]
    );
    // Recent states its own size, never more (W34 audit #10).
    assert_eq!(
        sidebar["recent"],
        json(&app, "/api/view?recent=1", &cookie).await["total"],
        "the Recent row and the Recent view say the same size"
    );

    for folder in sidebar["folders"].as_array().unwrap() {
        let path = folder["path"].as_str().unwrap();
        let view = json(&app, &format!("/api/view?in={}", encode(path)), &cookie).await;
        assert_eq!(folder["count"], view["total"], "folder {path}");
    }
    for collection in sidebar["collections"].as_array().unwrap() {
        let path = collection["path"].as_str().unwrap();
        let view = json(&app, &format!("/api/view?c={}", encode(path)), &cookie).await;
        assert_eq!(collection["count"], view["total"], "collection {path}");
    }
    for tag in sidebar["tags"].as_array().unwrap() {
        let name = tag["tag"].as_str().unwrap();
        let view = json(&app, &format!("/api/view?tag={}", encode(name)), &cookie).await;
        assert_eq!(tag["count"], view["total"], "tag {name}");
    }
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn old_links_are_permanent_redirects_to_the_new_form() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let response = get(&app, "/folder/refs", &cookie).await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()["location"], "/?in=refs");

    let response = get(&app, "/collection/browse.md", &cookie).await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()["location"], "/?c=browse.md");

    // The rest of a shared link survives the redirect.
    let response = get(&app, "/folder/refs?tag=nature", &cookie).await;
    assert_eq!(response.headers()["location"], "/?in=refs&tag=nature");
}

/// A redirect to a thing that is not there would show an empty library, which
/// reads as "this folder is empty" rather than "there is no such folder".
#[tokio::test]
async fn a_path_that_is_not_in_the_library_is_not_redirected() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    for uri in [
        "/folder/nowhere",
        "/folder/refs/deeper/still",
        "/collection/absent.md",
        "/folder/../escape",
    ] {
        let response = get(&app, uri, &cookie).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn every_page_carries_the_navigation_and_the_frame() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    for uri in ["/", "/folders", "/collections", "/search", "/?in=refs"] {
        let html = text(&app, uri, &cookie).await;
        // The four destinations, in the order spec §3 lists them.
        let tab_bar = html
            .split("<nav class=\"bottom-tab-bar\"")
            .nth(1)
            .expect("a phone tab bar");
        for href in ["/", "/folders", "/collections", "/search"] {
            assert!(
                tab_bar.contains(&format!("href=\"{href}\"")),
                "{uri}: {href}"
            );
        }
        assert_eq!(
            tab_bar.matches("class=\"tab-item").count(),
            4,
            "the tab bar has four entries: {uri}"
        );
        // The current one is marked for the stylesheet and for a screen reader.
        assert_eq!(tab_bar.matches("aria-current=\"page\"").count(), 1);
        assert_eq!(tab_bar.matches(" active").count(), 1);
        // One layout, three widths.
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/assets/app-"));
        assert!(html.contains("<script src=\"/assets/app-"));
    }
}

#[tokio::test]
async fn the_sidebar_lists_four_sections_with_counts() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/", &cookie).await;
    let sidebar = html
        .split("<aside class=\"desktop-sidebar\">")
        .nth(1)
        .expect("a desktop sidebar")
        .split("</aside>")
        .next()
        .unwrap();
    for title in ["Views", "Folders", "Collections", "Tags"] {
        assert!(sidebar.contains(title), "missing the {title} section");
    }
    // Every row shows a count (principle 3).
    let rows = sidebar.matches("sidebar-row-count").count();
    let labels = sidebar.matches("sidebar-row-label").count();
    assert!(rows >= labels, "every row carries its count");
    assert!(sidebar.contains("All"));
    assert!(sidebar.contains("Untagged"));
    assert!(sidebar.contains("Recent"));
    assert!(sidebar.contains(">nature</span>"));
    assert!(sidebar.contains(">Browse</span>"));
}

#[tokio::test]
async fn scope_chips_render_with_a_way_to_drop_each_one() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?in=refs&tag=nature&q=landscape", &cookie).await;
    // W43: the chips live inside the search capsule, on one line, and name
    // their kind with a glyph — the tooltip carries the word and the value.
    let chips = html
        .split("<span class=\"scope-chips\">")
        .nth(1)
        .expect("chips")
        .split("</div>")
        .next()
        .unwrap();
    assert!(chips.contains("title=\"Folder: refs\""));
    assert!(chips.contains("title=\"Tag: nature\""));
    assert!(chips.contains("title=\"Search: landscape\""));
    assert!(!chips.contains("in: refs") && !chips.contains("tag: nature"));
    assert_eq!(chips.matches("chip-remove").count(), 3);
    // Dropping the tag keeps the folder.
    assert!(chips.contains("href=\"/?in=refs&amp;q=landscape\""));
}

#[tokio::test]
async fn an_invalid_parameter_is_reported_and_never_a_server_error() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    for uri in [
        "/?sort=nonexistent",
        "/?dir=sideways",
        "/?size=huge",
        "/?p=0",
        "/?p=-3",
        "/?p=abc",
        "/?sub=maybe",
        "/?untagged=yes",
        "/?recent=sometimes",
        "/?in=/etc/passwd",
        "/?in=refs/../escape",
        "/?wat=1",
    ] {
        let response = get(&app, uri, &cookie).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{uri} must not be a server error"
        );
        let html = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            html.contains("notice-box"),
            "{uri} ignored a parameter without saying so: {html}"
        );
    }

    // The values that were not understood fall back to the defaults.
    let html = text(&app, "/?sort=nonexistent", &cookie).await;
    assert!(html.contains("Ignored unknown sort"));
    assert!(html.contains("Added ↓"), "the default sort is still shown");
}

/// A collection has no second page to ask for, so a page number asked of one
/// is ignored in the same voice the page already uses for a sort — and the
/// members it does list are still the whole of the note.
#[tokio::test]
async fn a_collection_page_says_so_about_a_page_number_it_cannot_take() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?c=tour.md&p=2", &cookie).await;
    assert!(
        html.contains("Ignored page number: a collection lists all of its members at once"),
        "{html}"
    );
    assert_eq!(
        html_paths(&html),
        vec!["omega/wide.png", "alpha/zen.png", "shrine/torii.png"],
        "asking for a second page takes nothing away"
    );
    // The ordinary view, where a page number means something, still says
    // nothing about it.
    let plain = text(&app, "/?p=2", &cookie).await;
    assert!(!plain.contains("Ignored page number"), "{plain}");
}

#[tokio::test]
async fn a_tile_remembers_the_view_it_came_from() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?in=refs&sort=name", &cookie).await;
    assert!(
        html.contains("?v=in%3Drefs%26sort%3Dname"),
        "a tile carries the view: {html}"
    );

    // And the image page offers the way back — landing on the tile it left,
    // the anchor the page's own tiles carry.
    let html = text(
        &app,
        "/image/refs/ui/button.png?v=in%3Drefs%26sort%3Dname",
        &cookie,
    )
    .await;
    assert!(
        html.contains("href=\"/?in=refs&amp;sort=name#img-refs/ui/button.png\""),
        "{html}"
    );
    assert!(html.contains("Back to view"));
}

#[tokio::test]
async fn the_image_page_shows_the_provenance_of_the_picture() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/refs/landscape.png", &cookie).await;

    // Path, copyable.
    assert!(html.contains("data-copy-text=\"refs/landscape.png\""));
    // Source link.
    assert!(html.contains("href=\"https://example.com/landscape\""));
    assert!(html.contains("rel=\"noreferrer noopener\""));
    // Tags as library links.
    assert!(html.contains("href=\"/?tag=nature\""));
    assert!(html.contains("href=\"/?tag=wide\""));
    // The note file, and the note itself.
    assert!(html.contains("refs/landscape.png.md"));
    assert!(html.contains("<strong>landscape</strong>"));
    // The raw metadata an import kept.
    assert!(html.contains("landscape.png.md"));
}

#[tokio::test]
async fn the_image_page_lists_the_collections_an_image_appears_in() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/refs/landscape.png", &cookie).await;
    assert!(html.contains("Appears in"));
    assert!(html.contains("href=\"/?c=browse.md\""));
    // The collection is named by its own title, and the path is the tooltip.
    assert!(html.contains("<a href=\"/?c=browse.md\" title=\"browse.md\">Browse</a>"));

    // An image in no collection says so in one line, not in silence.
    let html = text(&app, "/image/refs2/trap.png", &cookie).await;
    assert!(html.contains("Appears in"), "{html}");
    assert!(html.contains("Not in any collection."), "{html}");
    assert!(
        !html.contains("<ul class=\"appears-in\">"),
        "no list for an empty state: {html}"
    );
}

#[tokio::test]
async fn an_image_with_no_note_still_shows_its_path() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/plain.png", &cookie).await;
    assert!(html.contains("data-copy-text=\"plain.png\""));
    assert!(
        html.contains("alt=\"plain.png\""),
        "the file name is the alt text"
    );
    assert!(!html.contains("Tags"));
}

/// The `?v=` a tile or a hand writes, percent-encoded the way the page writes
/// it back on its arrows (`NON_ALPHANUMERIC`, the tiles' own spelling).
fn v_param(query: &str) -> String {
    percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// `<span class="image-nav-position" title="…">n / t</span>`, as the page
/// draws it, so a test states what it expects rather than scraping it. The
/// view names itself in the tooltip HTML-escaped, the way the page writes a
/// title attribute (`&` of a two-part query becomes `&amp;`).
fn position(position: u64, total: u64, view: &str) -> String {
    let view = view.replace('&', "&amp;");
    format!("<span class=\"image-nav-position\" title=\"{view}\">{position} / {total}</span>")
}

/// One arrow as the page draws it: a link (carrying the view, or none at all
/// when the plain library is the whole context), or the quiet stub at a
/// view's end.
fn prev_link(target: Option<&str>, view: &str) -> String {
    arrow("image-nav-prev", "Previous image", target, view)
}

fn next_link(target: Option<&str>, view: &str) -> String {
    arrow("image-nav-next", "Next image", target, view)
}

fn arrow(kind: &str, label: &str, target: Option<&str>, view: &str) -> String {
    match target {
        Some(path) => {
            let suffix = if view.is_empty() {
                String::new()
            } else {
                format!("?v={view}")
            };
            format!(
                "<a class=\"{kind}\" href=\"/image/{path}{suffix}\" \
                 title=\"{}\" aria-label=\"{label}\">",
                path.rsplit('/').next().unwrap_or(path)
            )
        }
        None => format!("<span class=\"{kind} nav-end\" aria-hidden=\"true\">"),
    }
}

/// Four images whose every order is written in their notes: distinct `added`
/// instants, a tag two of them share, and words only one body holds. Files
/// one to four in folders a to d, so no sort agrees with another and a
/// wrong walk cannot pass by accident.
fn order_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    for (image, added, body) in [
        (
            "a/one.png",
            "2026-01-01T00:00:00+00:00",
            "the first picture",
        ),
        (
            "b/two.png",
            "2026-01-02T00:00:00+00:00",
            "the second picture",
        ),
        (
            "c/three.png",
            "2026-01-03T00:00:00+00:00",
            "the third picture",
        ),
        (
            "d/four.png",
            "2026-01-04T00:00:00+00:00",
            "the fourth picture",
        ),
    ] {
        write(image, PNG_BYTES);
        let tags = if body.contains("second") || body.contains("third") {
            "tags:\n  - cat\n".to_owned()
        } else {
            String::new()
        };
        write(
            &format!("{image}.md"),
            &format!("---\ntitle: {added}\n{tags}added: {added}\n---\n{body}\n"),
        );
    }
    // Embed order is deliberately none of the sort orders: one, four, two, three.
    write(
        "set.md",
        "---\ntitle: Set\n---\n![[a/one.png]]\n![[d/four.png]]\n![[b/two.png]]\n![[c/three.png]]\n",
    );
    dir
}

/// 205 images with dates in their notes, a minute apart, so the Recent lens
/// has one picture past its 200 for every ordering under the sun.
fn recent_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("refs")).unwrap();
    for index in 0..205u64 {
        let image = format!("refs/rec-{index:03}.jpg");
        fs::write(root.join(&image), PNG_BYTES).unwrap();
        let added = format!("2026-05-01T{:02}:{:02}:00+00:00", index / 60, index % 60);
        fs::write(
            root.join(format!("{image}.md")),
            format!("---\nadded: {added}\n---\nrecorded {index}\n"),
        )
        .unwrap();
    }
    dir
}

#[tokio::test]
async fn prev_next_walk_the_view_the_page_was_reached_from() {
    let dir = order_library();
    let (app, cookie) = login(app(&dir)).await;

    // The plain library: added newest first — four, three, two, one.
    let html = text(&app, "/image/c/three.png", &cookie).await;
    assert!(
        html.contains(&prev_link(Some("d/four.png"), "")),
        "no v, so no ?v= on the arrows: {html}"
    );
    assert!(html.contains(&next_link(Some("b/two.png"), "")));
    assert!(html.contains(&position(2, 4, "All")));
    // The ends of the view answer stubs, not wraps.
    let html = text(&app, "/image/d/four.png", &cookie).await;
    assert!(html.contains(&prev_link(None, "")));
    assert!(html.contains(&position(1, 4, "All")));
    assert!(html.contains(&next_link(Some("c/three.png"), "")));
    let html = text(&app, "/image/a/one.png", &cookie).await;
    assert!(html.contains(&next_link(None, "")));
    assert!(html.contains(&position(4, 4, "All")));

    // A sort the reader picked: name ascending — four, one, three, two (the
    // names sort, not the folders). The arrows keep carrying the view,
    // spelled as the tiles spell it, and "Back to view" lands on the tile the
    // picture was left from — where `b/two.png` is the tail of the list.
    let view = "sort=name&dir=asc";
    let html = text(
        &app,
        &format!("/image/b/two.png?v={}", v_param(view)),
        &cookie,
    )
    .await;
    assert!(
        html.contains(&prev_link(Some("c/three.png"), &v_param(view))),
        "{html}"
    );
    assert!(html.contains(&next_link(None, &v_param(view))), "{html}");
    assert!(html.contains(&position(4, 4, view)));
    assert!(
        html.contains(&format!(
            "href=\"/?{}#img-b/two.png\">← Back to view",
            view.replace('&', "&amp;")
        )),
        "{html}"
    );
}

#[tokio::test]
async fn prev_next_walk_a_folder_a_tag_and_a_search() {
    let dir = order_library();
    let (app, cookie) = login(app(&dir)).await;

    // A folder of its own: one image, nothing on either side.
    let view = "in=b&sub=0";
    let html = text(
        &app,
        &format!("/image/b/two.png?v={}", v_param(view)),
        &cookie,
    )
    .await;
    assert!(html.contains(&prev_link(None, &v_param(view))));
    assert!(html.contains(&next_link(None, &v_param(view))));
    assert!(html.contains(&position(1, 1, view)));

    // A tag both middles carry: three, two in added order.
    let view = "tag=cat";
    let html = text(
        &app,
        &format!("/image/b/two.png?v={}", v_param(view)),
        &cookie,
    )
    .await;
    assert!(html.contains(&prev_link(Some("c/three.png"), &v_param(view))));
    assert!(html.contains(&next_link(None, &v_param(view))));
    assert!(html.contains(&position(2, 2, view)));
    let html = text(
        &app,
        &format!("/image/c/three.png?v={}", v_param(view)),
        &cookie,
    )
    .await;
    assert!(html.contains(&prev_link(None, &v_param(view))));
    assert!(html.contains(&next_link(Some("b/two.png"), &v_param(view))));

    // A search whose words only one body holds.
    let view = "q=first";
    let html = text(
        &app,
        &format!("/image/a/one.png?v={}", v_param(view)),
        &cookie,
    )
    .await;
    assert!(html.contains(&position(1, 1, view)));
    assert!(html.contains(&prev_link(None, &v_param(view))));
    assert!(html.contains(&next_link(None, &v_param(view))));
}

/// A collection's own embed order is the only order its members have
/// (FORMAT §5): the walk follows the note, here deliberately not any sort.
#[tokio::test]
async fn prev_next_walk_a_collection_in_its_embed_order() {
    let dir = order_library();
    let (app, cookie) = login(app(&dir)).await;
    let view = v_param("c=set.md");

    // Set order: one, four, two, three.
    let html = text(&app, &format!("/image/b/two.png?v={view}"), &cookie).await;
    assert!(html.contains(&prev_link(Some("d/four.png"), &view)));
    assert!(html.contains(&next_link(Some("c/three.png"), &view)));
    assert!(html.contains(&position(3, 4, "c=set.md")));
    let html = text(&app, &format!("/image/a/one.png?v={view}"), &cookie).await;
    assert!(html.contains(&prev_link(None, &view)));
    assert!(html.contains(&next_link(Some("d/four.png"), &view)));
    assert!(html.contains(&position(1, 4, "c=set.md")));
}

/// The Recent lens reaches the last 200 added, no matter how large the
/// library: the image at the lens's end has no next even though 199-odd more
/// exist, and an image the lens never held walks nowhere at all.
#[tokio::test]
async fn the_recent_lens_walks_only_the_two_hundred_it_shows() {
    let dir = recent_library();
    let (app, cookie) = login(app(&dir)).await;
    let view = v_param("recent=1");

    // rec-204 is the newest, rec-005 the last the lens holds.
    let html = text(&app, &format!("/image/refs/rec-204.jpg?v={view}"), &cookie).await;
    assert!(html.contains(&prev_link(None, &view)));
    assert!(html.contains(&next_link(Some("refs/rec-203.jpg"), &view)));
    assert!(html.contains(&position(1, 200, "recent=1")));

    let html = text(&app, &format!("/image/refs/rec-005.jpg?v={view}"), &cookie).await;
    assert!(html.contains(&prev_link(Some("refs/rec-006.jpg"), &view)));
    assert!(
        html.contains(&next_link(None, &view)),
        "rec-004 exists but the lens never showed it: {html}"
    );
    assert!(html.contains(&position(200, 200, "recent=1")));

    let html = text(&app, &format!("/image/refs/rec-004.jpg?v={view}"), &cookie).await;
    assert!(
        !html.contains("image-nav-position"),
        "an image the lens never held has no place in it: {html}"
    );
    assert!(!html.contains("image-nav-prev\" href="), "{html}");
    assert!(html.contains("Back to view"), "the way back stays: {html}");
}

/// A view that does not show this picture — a tag it does not carry — offers
/// no walk: the header keeps only the way back, which is exactly what a
/// stale `v=` means.
#[tokio::test]
async fn a_view_the_image_is_not_part_of_offers_no_walk() {
    let dir = order_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(
        &app,
        &format!("/image/d/four.png?v={}", v_param("tag=cat")),
        &cookie,
    )
    .await;
    assert!(!html.contains("image-nav"), "{html}");
    assert!(
        html.contains("href=\"/?tag=cat#img-d/four.png\">← Back to view"),
        "{html}"
    );
}

/// An unknown `v` is walked as the plain library and linked as written; an
/// unreadable `v` is the plain library entirely — either way, the arrows and
/// the back link never disagree about which view they mean, and neither can
/// leave the origin.
#[tokio::test]
async fn an_unknown_view_param_still_walks_and_gets_back_safely() {
    let dir = order_library();
    let (app, cookie) = login(app(&dir)).await;

    let html = text(&app, "/image/b/two.png?v=banana", &cookie).await;
    assert!(
        html.contains("href=\"/?banana#img-b/two.png\">← Back to view"),
        "{html}"
    );
    assert!(
        html.contains(&prev_link(Some("c/three.png"), &v_param("banana"))),
        "the arrows carry the view as written: {html}"
    );
    assert!(html.contains(&position(3, 4, "banana")));

    let html = text(&app, "/image/b/two.png?v=%FF", &cookie).await;
    assert!(
        html.contains("href=\"/#img-b/two.png\">← Back to view"),
        "{html}"
    );
    assert!(html.contains(&prev_link(Some("c/three.png"), "")), "{html}");
    assert!(html.contains(&position(3, 4, "All")), "{html}");
}

/// RW45 M-2: the arrows walk the whole view, while the `p` the `v` carries is
/// the page the reader arrived on. Past a page boundary the way back has to ask
/// for the page that holds the tile — an anchor into a page that is not there
/// leaves the reader at the top of the grid, which is the half of "lands on the
/// tile you left" that used to break after ~120 presses of `→`.
#[tokio::test]
async fn the_way_back_asks_for_the_page_the_tile_is_on() {
    let dir = big_library();
    let (app, cookie) = login(app(&dir)).await;

    // Name ascending: `rec-120` is the 121st image, the first tile of page two.
    let html = text(
        &app,
        &format!("/image/refs/rec-120.jpg?v={}", v_param("sort=name")),
        &cookie,
    )
    .await;
    assert!(html.contains(&position(121, 205, "sort=name")), "{html}");
    assert!(
        html.contains("href=\"/?sort=name&amp;p=2#img-refs/rec-120.jpg\">← Back to view"),
        "the view carries, the page comes from the position: {html}"
    );
    // The page it names really holds that tile, and the page the walk started
    // on really does not — which is the whole reason for rewriting `p`.
    let page_two = text(&app, "/?sort=name&p=2", &cookie).await;
    assert!(
        page_two.contains("id=\"img-refs/rec-120.jpg\""),
        "the anchor is on the page the link asks for"
    );
    let page_one = text(&app, "/?sort=name", &cookie).await;
    assert!(
        !page_one.contains("id=\"img-refs/rec-120.jpg\""),
        "and not on the page it arrived from"
    );

    // A `p` carried from further in is corrected in both directions: this image
    // is page two's, whatever the link said.
    let html = text(
        &app,
        &format!("/image/refs/rec-130.jpg?v={}", v_param("sort=name&p=3")),
        &cookie,
    )
    .await;
    assert!(
        html.contains(&position(131, 205, "sort=name&p=3")),
        "{html}"
    );
    assert!(
        html.contains("href=\"/?sort=name&amp;p=2#img-refs/rec-130.jpg\">← Back to view"),
        "{html}"
    );

    // And the first page of a view needs no page of its own.
    let html = text(
        &app,
        &format!("/image/refs/rec-005.jpg?v={}", v_param("sort=name&p=3")),
        &cookie,
    )
    .await;
    assert!(
        html.contains("href=\"/?sort=name&amp;p=1#img-refs/rec-005.jpg\">← Back to view"),
        "the page the reader arrived on stays stated: {html}"
    );
}

/// RW51 Medium-1: the page the position writes must not be written onto a view
/// that has no pages. A collection lists every member at once, and a `p` beside
/// `c` is reported to the reader as ignored — so the position-derived page made
/// the way back arrive with a `role="status"` line about a page nobody asked
/// for, on a library whose collection is longer than one page.
#[tokio::test]
async fn the_way_back_from_a_collection_never_asks_it_for_a_page() {
    let dir = big_collection_library();
    let (app, cookie) = login(app(&dir)).await;

    // Position 151 of the collection, well past the page boundary every
    // *paginated* view has: that is exactly the position that earned a `p=2`.
    let html = text(
        &app,
        &format!("/image/refs/rec-150.jpg?v={}", v_param("c=album.md")),
        &cookie,
    )
    .await;
    assert!(html.contains(&position(151, 205, "c=album.md")), "{html}");
    assert!(
        html.contains("href=\"/?c=album.md#img-refs/rec-150.jpg\">← Back to view"),
        "the carried view, as written, with no page bolted on: {html}"
    );

    // The page that link names really does hold the tile — every member is on
    // the one page — and it says nothing about pages.
    let collection = text(&app, "/?c=album.md", &cookie).await;
    assert!(
        collection.contains("id=\"img-refs/rec-150.jpg\""),
        "the anchor resolves on the page the link names"
    );
    assert!(
        !collection.contains("Ignored page number"),
        "the reader never asked: {collection}"
    );

    // And the half of RW45 M-2 this must not undo: a view that does page still
    // gets the page its position says the tile is on.
    let html = text(
        &app,
        &format!("/image/refs/rec-150.jpg?v={}", v_param("sort=name")),
        &cookie,
    )
    .await;
    assert!(
        html.contains("href=\"/?sort=name&amp;p=2#img-refs/rec-150.jpg\">← Back to view"),
        "{html}"
    );
}

/// "Appears in" with many collections: each named by what the collection
/// calls itself — a title, or its file name when it has none — linked to its
/// view, and a hostile title reaches the page only escaped (RW37's rule,
/// applied to the back-link list).
#[tokio::test]
async fn appears_in_names_many_collections_and_escapes_hostile_titles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    write("hero.png", PNG_BYTES);
    write("side.png", PNG_BYTES);
    write(
        "browse.md",
        "---\ntitle: Browse\n---\n![[hero.png]]\nA browse.\n",
    );
    write(
        "sets/round.md",
        format!(
            "---\ntitle: {}\n---\n![[hero.png]]\nA round.\n",
            "Set <img onerror=alert(1)>\" quotes"
        )
        .as_str(),
    );
    write("deep.md", "---\n---\n![[hero.png]]\nNo title of its own.\n");

    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/hero.png", &cookie).await;
    assert!(
        html.contains("<ul class=\"appears-in\">"),
        "three collections, one list: {html}"
    );
    assert!(
        html.contains("<a href=\"/?c=browse.md\" title=\"browse.md\">Browse</a>"),
        "{html}"
    );
    // A title even with quotes and markup stays a label; the link keeps the
    // path, percent-encoded.
    assert!(
        html.contains(
            "<a href=\"/?c=sets/round.md\" title=\"sets/round.md\">\
             Set &lt;img onerror=alert(1)&gt;&quot; quotes</a>"
        ),
        "{html}"
    );
    assert!(
        !html.contains("Set <img onerror"),
        "the raw title rendered as markup: {html}"
    );
    // No title: the file names the collection.
    assert!(
        html.contains("<a href=\"/?c=deep.md\" title=\"deep.md\">deep.md</a>"),
        "{html}"
    );

    let html = text(&app, "/image/side.png", &cookie).await;
    assert!(html.contains("Not in any collection."), "{html}");
}

#[tokio::test]
async fn the_folders_page_is_the_same_tree_the_sidebar_shows() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let page = text(&app, "/folders", &cookie).await;
    let sidebar = text(&app, "/", &cookie).await;

    for folder in ["refs", "refs/ui", "refs2"] {
        let link = format!("href=\"/?in={folder}\"");
        assert!(page.contains(&link), "/folders is missing {folder}");
        assert!(sidebar.contains(&link), "the sidebar is missing {folder}");
    }
    // Recursive counts: refs holds one image and one below it.
    assert!(page.contains(">refs</span><span class=\"sidebar-row-count\">2"));
}

#[tokio::test]
async fn the_collections_page_shows_a_title_a_count_and_a_cover() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/collections", &cookie).await;
    assert!(html.contains("href=\"/?c=browse.md\""));
    assert!(html.contains(">Browse</span>"));
    assert!(html.contains("2 items"));
    // The cover is the first member, not the newest.
    assert!(html.contains("src=\"/thumb/refs/landscape.png\""));
}

#[tokio::test]
async fn the_search_page_has_a_field_tags_and_recent_views() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/search", &cookie).await;
    assert!(html.contains("action=\"/\""));
    assert!(html.contains("name=\"q\""));
    assert!(html.contains("for=\"q\""), "the field is labelled");
    // Tapping a tag narrows the library.
    assert!(html.contains("href=\"/?tag=nature\""));
    assert!(html.contains("sidebar-row-count\">1"));
    // The recent list is filled in by the script, on this device only.
    assert!(html.contains("data-recent-views"));
}

#[tokio::test]
async fn the_assets_are_served_once_with_a_content_hash() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/", &cookie).await;

    let css = html
        .split("href=\"/assets/app-")
        .nth(1)
        .expect("a stylesheet link")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let response = get(&app, &format!("/assets/app-{css}"), &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/css; charset=utf-8"
    );
    assert_eq!(
        response.headers()["cache-control"],
        "public, max-age=31536000, immutable",
        "the URL carries the hash, so it can be cached for a year"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("aspect-ratio: 1"), "square tiles");
    assert!(
        body.contains("object-fit: contain"),
        "pictures are never cropped"
    );
    assert!(
        body.contains("@media (min-width: 768px)"),
        "the tablet breakpoint"
    );
    assert!(
        body.contains("@media (min-width: 1200px)"),
        "the desktop breakpoint"
    );
    assert!(body.contains("prefers-color-scheme: dark"), "a dark theme");

    // A stale hash is not served, so a cached page cannot load the wrong file.
    let stale = get(&app, "/assets/app-000000000000.css", &cookie).await;
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
}

/// RW54 review Medium #1: the order bubble opens inside the viewport it
/// opens in. Above ~550 px the label sits at the toolbar's right end and
/// `right: 0` grows the bubble leftwards into the row; below that the
/// anchor cannot hold — the row wraps at 520 px and `space-between` parks
/// the label mid-row (its right edge measured at x 102 of a 390 px
/// viewport), where a 280 px bubble opened ~178 px outside the viewport's
/// left edge. The narrow band anchors the bubble at the label's left
/// instead. There is no CSS harness: the claim is about the bytes the page
/// links, as the script pins are.
#[tokio::test]
async fn the_order_bubble_is_anchored_where_the_viewport_can_show_it() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let page = text(&app, "/?recent=1", &cookie).await;
    let css = served_stylesheet(&app, &cookie, &page).await;
    assert!(
        css.contains(
            "@media (max-width: 550px) {\n  .sort-tip {\n    right: auto;\n    left: 0;\n  }\n}"
        ),
        "below ~550 px the bubble grows rightwards from the label's left edge: {css}"
    );
}

/// RW54 review Low #2: nothing hides the bubble on touch. The old
/// `@media (hover: none)` rule answered "no hover" with `display: none`,
/// which takes the sentence out of the accessibility tree with the bubble —
/// a hidden subtree is not read, so the comment that promised a screen
/// reader still would was false — and left a sighted keyboard user on a
/// touch device with no visible sentence at all, which is the reader W39b
/// review L1 was for. The bubble is absolutely positioned, so revealing it
/// costs the toolbar row no width.
#[tokio::test]
async fn the_order_bubble_is_not_hidden_on_touch() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let page = text(&app, "/?recent=1", &cookie).await;
    let css = served_stylesheet(&app, &cookie, &page).await;
    let blocks = hover_none_blocks(&css);
    assert!(
        !blocks.is_empty(),
        "the stylesheet's other touch rules exist, so they are scanned: {css}"
    );
    let hiding: Vec<&str> = blocks
        .iter()
        .copied()
        .filter(|block| block.contains(".sort-tip"))
        .collect();
    assert!(
        hiding.is_empty(),
        "no touch rule takes the bubble out of the accessibility tree: {}",
        hiding.join("\n")
    );
}

/// RW45 M-1: the pull-up sheet's open/closed state is a disclosure, and a
/// disclosure only reaches a screen reader through the control that changes it.
/// There is no JavaScript harness in this repository, so the pin is on the
/// script the page actually serves: the sheet state is set on the handle, with
/// the label that goes with it, and never on the panel the handle opens.
#[tokio::test]
async fn the_pull_up_sheet_states_itself_on_its_own_button() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let page = text(&app, "/image/refs/landscape.png", &cookie).await;
    assert!(
        page.contains(
            "<button type=\"button\" class=\"sheet-handle\" aria-expanded=\"false\" \
             aria-controls=\"image-panel\" aria-label=\"Show details\">"
        ),
        "the markup starts collapsed: {page}"
    );

    let script = served_script(&app, &cookie, &page).await;
    assert!(
        script.contains("handle.setAttribute('aria-expanded'"),
        "the button carries the state"
    );
    assert!(
        script.contains("handle.setAttribute('aria-label'")
            && script.contains("'Hide details'")
            && script.contains("'Show details'"),
        "and says which way it is"
    );
    assert!(
        !script.contains("panel.setAttribute('aria-expanded'"),
        "the region it opens is not the control"
    );
}

#[tokio::test]
async fn everything_stays_behind_the_passcode() {
    let dir = library();
    let app = app(&dir);
    for uri in [
        "/",
        "/folders",
        "/collections",
        "/search",
        "/api/view",
        "/api/sidebar",
        "/?in=refs",
        "/folder/refs",
        "/collection/browse.md",
        "/image/refs/landscape.png",
        // The source view of a note and the bytes of an image are routes of
        // their own, added beside the pages rather than under them, and both
        // answer straight from the file — so both have to be named here.
        "/raw/browse.md",
        "/media/refs/landscape.png",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        if uri.starts_with("/api/") {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        } else {
            assert_eq!(
                response.status(),
                StatusCode::SEE_OTHER,
                "{uri} must ask for the passcode"
            );
            assert_eq!(response.headers()["location"], "/login");
        }
    }
}

#[tokio::test]
async fn an_empty_library_says_so_rather_than_looking_broken() {
    let dir = tempfile::tempdir().unwrap();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/", &cookie).await;
    assert!(html.contains("The library is empty"));

    let sidebar = json(&app, "/api/sidebar", &cookie).await;
    assert_eq!(sidebar["total"], 0);
    let view = json(&app, "/api/view", &cookie).await;
    assert_eq!(view["total"], 0);
    assert!(view["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_folder_with_no_images_offers_the_way_out() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("empty")).unwrap();
    let (app, cookie) = login(app(&dir)).await;

    let html = text(&app, "/?in=empty&sub=0", &cookie).await;
    assert!(html.contains("Show subfolders"), "{html}");
    assert!(html.contains("href=\"/?in=empty\""));
    // W43: one way out per state — the chip already offers the way to
    // drop the folder, so the state itself does not repeat it.
    assert!(!html.contains("Clear all filters"), "{html}");
}

/// RW37: the scope chips used to paste the user's own words into the page
/// unescaped. Every hostile scope value must reach the body escaped.
#[tokio::test]
async fn hostile_scope_values_reach_the_library_page_only_escaped() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let html = text(
        &app,
        "/?q=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E",
        &cookie,
    )
    .await;
    assert!(
        html.contains("Search: &lt;img src=x onerror=alert(1)&gt;"),
        "{html}"
    );
    assert!(
        !html.contains("<img src=x"),
        "the raw form rendered: {html}"
    );

    let html = text(&app, "/?tag=%3Cscript%3E%22x%22", &cookie).await;
    assert!(html.contains("Tag: &lt;script&gt;&quot;x&quot;"), "{html}");
    // W43: the whole page legitimately ships one bare `<script>` — the theme
    // boot in <head>, which the frame itself writes. Hostile input pays for
    // nothing more than escaped text.
    assert_eq!(
        html.matches("<script>").count(),
        1,
        "only the theme boot: {html}"
    );
    assert!(html.contains("<script>try{var t=localStorage.getItem('dimagine.theme')"));

    let html = text(&app, "/?in=%22%3E%3Cimg%20onerror%3E", &cookie).await;
    assert!(
        html.contains("Folder: &quot;&gt;&lt;img onerror&gt;"),
        "{html}"
    );
    assert!(
        !html.contains("\"><img onerror"),
        "the raw form rendered: {html}"
    );

    let html = text(&app, "/?c=%22%3E%3Csvg%3E.md", &cookie).await;
    assert!(
        html.contains("Collection: &quot;&gt;&lt;svg&gt;.md"),
        "{html}"
    );
    assert!(
        !html.contains("\"><svg>.md"),
        "the raw form rendered: {html}"
    );
}

/// A collection's title is library content, not the page author's words: it
/// reaches the sidebar and the collections page only escaped.
#[tokio::test]
async fn a_collection_title_carrying_markup_reaches_the_page_only_escaped() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.png"), PNG_BYTES).unwrap();
    fs::write(
        dir.path().join("set.md"),
        "---\ntitle: Set <img onerror=x>\n---\n![[a.png]]\n",
    )
    .unwrap();
    let (app, cookie) = login(app(&dir)).await;

    let sidebar = text(&app, "/", &cookie).await;
    assert!(sidebar.contains("Set &lt;img onerror=x&gt;"), "{sidebar}");
    assert!(
        !sidebar.contains("<img onerror"),
        "the raw form rendered: {sidebar}"
    );

    let cards = text(&app, "/collections", &cookie).await;
    assert!(cards.contains("Set &lt;img onerror=x&gt;"), "{cards}");
    assert!(
        !cards.contains("<img onerror"),
        "the raw form rendered: {cards}"
    );
}

// =========================== W46 · the collection page ====================
//
// A collection is a note that embeds images (FORMAT §5), and `/?c=<note>` is
// the collection page: the note's header — its title, the text the note keeps
// for itself, the number of items, the way to the note's source — above its
// members in embed order, each with the caption line its embed gave it
// (FORMAT §5: "a line directly after an embed is that member's note").

/// A curated collection whose embed order no sort would produce (name, added
/// and size each disagree with it), one member embedded twice with two
/// captions, one member without a caption, a hostile caption line, a note
/// with a hostile title, and an empty collection: every fact the collection
/// page states is somewhere in this library.
fn page_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };

    write("omega/wide.png", PNG_BYTES);
    write("alpha/zen.png", PNG_BYTES);
    write("shrine/torii.png", PNG_BYTES);
    write(
        "tour.md",
        &(["---", "title: Tour", "---", "", "The plan for the trip.", ""]
            .join("\n")
            + // Prose a note author could have written to find out what the
              // page does with it: the same shape as the hostile caption
              // below, but where a reader's own words go.
            "<script>alert(1)</script> and <img src=x onerror=alert(2)>.\n\n"
            + "![[omega/wide.png]]\n"
            + "The gate that opens the day.\n"
            + "![[alpha/zen.png]]\n"
            + "![[shrine/torii.png]]\n"
            + "Torii, <img src=x onerror=alert(1)> at dawn.\n"
            // The same member again, through its bare name: a different
            // line, a different caption, and the same image exactly once.
            + "![[zen.png]]\n"
            + "Seen again on the way back.\n"),
    );
    // A title with markup: the page shows it, the browser must read it.
    write(
        "hostile.md",
        "---\ntitle: Set <img onerror=x>\n---\n![[shrine/torii.png]]\n",
    );
    // `kind: collection` with no embeds yet: a collection with nothing to
    // list, which is a different fact than not being a collection.
    write(
        "empty.md",
        "---\nkind: collection\ntitle: Empty Days\n---\nNothing collected yet.\n",
    );
    dir
}

/// The collection page's header, without anything after it.
fn collection_header(html: &str) -> &str {
    let start = html
        .find("class=\"search-section collection-header\"")
        .expect("a collection header")
        + "class=\"search-section collection-header\"".len();
    let end = html[start..].find("</section>").expect("a closed header");
    &html[start..start + end]
}

/// The member captions a page shows, in the order their tiles appear.
fn member_captions(html: &str) -> Vec<String> {
    html.split("class=\"member-caption\" title=\"")
        .skip(1)
        .map(|tail| {
            tail[..tail.find("\">").expect("a closed attribute")]
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
        })
        .collect()
}

/// W46 §1: the members arrive in the order the note embeds them — an order
/// no sort control would produce — and a duplicate embed appears once, at
/// its first position: the row the note gave it first, caption included.
#[tokio::test]
async fn a_collection_lists_its_members_in_embed_order_and_a_duplicate_once() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?c=tour.md", &cookie).await;

    assert_eq!(
        html_paths(&html),
        vec!["omega/wide.png", "alpha/zen.png", "shrine/torii.png"],
        "the embed order wins over every sort, and the fourth embed — \
         alpha again — is the same member again"
    );
    // The header and the toolbar say the size of that list: three members,
    // once each, never the four link rows the index counts for this note.
    let header = collection_header(&html);
    assert!(header.contains("3 items"), "{header}");
    assert!(
        html.contains("<span class=\"result-count\">3 items</span>"),
        "{html}"
    );
    assert_eq!(html.matches("grid-item").count(), 3, "{html}");
    // The order the toolbar names the note's, not a sort's — with the
    // sentence one Tab away in the bubble (W39b review L1).
    assert!(
        html.contains(
            ">Note order<span class=\"sort-tip\">A collection is ordered by the embeds in its note.</span>"
        ),
        "{html}"
    );
}

/// W46 §2: a member shows the line directly after its embed (FORMAT §5) —
/// a duplicated member keeps the caption of its first row — and a member
/// whose embed is followed by another has no caption, because the note gave
/// it none: absent is not inventable.
#[tokio::test]
async fn a_member_shows_its_own_caption_and_a_member_without_one_shows_none() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?c=tour.md", &cookie).await;

    let captions = member_captions(&html);
    assert_eq!(
        captions,
        vec![
            "The gate that opens the day.".to_owned(),
            "Torii, <img src=x onerror=alert(1)> at dawn.".to_owned(),
        ],
        "the captions in tile order; the second member's embed sits \
         directly before the next one, so the note gave it no caption"
    );
    // The caption is the note's own words, so hostile markup in it is
    // rendered as text: escaped in the element and the tooltip, the raw
    // form never appears.
    assert!(
        html.contains("title=\"Torii, &lt;img src=x onerror=alert(1)&gt; at dawn.\""),
        "{html}"
    );
    assert!(
        html.contains("&lt;img src=x onerror=alert(1)&gt; at dawn."),
        "the caption rendered: {html}"
    );
    assert!(
        !html.contains("<img src=x"),
        "a hostile caption must never become markup: {html}"
    );
}

/// W46 §3: the header names the collection the tab names (the title from the
/// note's own properties), shows the text the note keeps for itself — the
/// prose, without the member rows the grid below lists — states the item
/// count, and links to the note's source view.
#[tokio::test]
async fn the_collection_header_shows_the_note_itself_and_the_way_to_its_source() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?c=tour.md", &cookie).await;
    let header = collection_header(&html);

    // The same words the tab shows, once (W34 audit #12).
    assert!(
        html.contains("<title>Collection: Tour · dimagine</title>"),
        "{html}"
    );
    assert!(header.contains("<h1>Collection: Tour</h1>"), "{header}");
    // The note's own text: the prose, rendered as Markdown.
    assert!(
        header.contains("<div class=\"collection-note note-body\">"),
        "{header}"
    );
    assert!(header.contains("<p>The plan for the trip.</p>"), "{header}");
    // And the hostile prose of the same note: a collection's own text is
    // written by whoever owns the library, so the page has to arrive at the
    // browser with it read as words — the sanitiser's work is stated here at
    // the route that people actually reach, not only where it is unit-tested.
    assert!(
        header.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the note's prose should be on the page as text: {header}"
    );
    assert!(
        !header.contains("<script>"),
        "raw script reached the page: {header}"
    );
    assert!(
        !header.contains("<img src=x onerror"),
        "a live error handler reached the page: {header}"
    );
    // The member rows belong to the grid, not to the note's own text: no
    // caption line and no embed target appears in the header.
    assert!(
        !header.contains("The gate that opens the day"),
        "a member's caption is the member's, shown under its tile: {header}"
    );
    assert!(
        !header.contains("Seen again on the way back"),
        "the second caption belongs to the row the page does not list: {header}"
    );
    assert!(
        !header.contains("wide.png") && !header.contains("zen.png"),
        "the members are tiles below, not text above: {header}"
    );
    // The count of what is listed, and the way to the note's source.
    assert!(header.contains("3 items"), "{header}");
    assert!(
        header.contains("<a class=\"open-note\" href=\"/raw/tour.md\">Open note</a>"),
        "{header}"
    );

    // The source view serves the note as it is written: raw Markdown, shown
    // as text — never a page of this origin that a note could rig.
    let response = get(&app, "/raw/tour.md", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        body.contains("![[omega/wide.png]]"),
        "the note's source, as written: {body}"
    );
}

/// A hostile title renders as text on the collection page, as it already
/// does on the sidebar and the cards (W38 rules).
#[tokio::test]
async fn a_hostile_collection_title_reaches_the_page_only_escaped() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?c=hostile.md", &cookie).await;
    let header = collection_header(&html);

    assert!(
        header.contains("<h1>Collection: Set &lt;img onerror=x&gt;</h1>"),
        "{header}"
    );
    assert!(!header.contains("<img onerror"), "the raw form: {header}");
}

/// W46 §3, the empty case: a `kind: collection` note with nothing embedded
/// is a collection with no items — the header still names it, its own text
/// still shows, the count still tells the truth, and the empty state below
/// says what is empty rather than a collection page that looks broken.
#[tokio::test]
async fn an_empty_collection_keeps_its_header_and_says_what_is_empty() {
    let dir = page_library();
    let (empty_app, cookie) = login(app(&dir)).await;
    let html = text(&empty_app, "/?c=empty.md", &cookie).await;
    let header = collection_header(&html);

    assert!(
        html.contains("<title>Collection: Empty Days · dimagine</title>"),
        "{html}"
    );
    assert!(
        header.contains("<h1>Collection: Empty Days</h1>"),
        "{header}"
    );
    assert!(header.contains("<p>Nothing collected yet.</p>"), "{header}");
    assert!(header.contains("0 items"), "{header}");
    assert!(
        header.contains("<a class=\"open-note\" href=\"/raw/empty.md\">Open note</a>"),
        "{header}"
    );
    // No tiles, and the empty state's own words for a collection.
    assert!(html_paths(&html).is_empty(), "{html}");
    assert!(html.contains("empty.md has no images"), "{html}");
    assert!(
        html.contains("Its embeds resolve to no image that is still here."),
        "{html}"
    );
    // A note with no text of its own has no text block: the header is the
    // heading, the count and the link, not an empty rendered div.
    let bare = page_library();
    fs::write(
        bare.path().join("bare.md"),
        "---\nkind: collection\ntitle: Bare\n---\n",
    )
    .unwrap();
    let (bare_app, cookie) = login(app(&bare)).await;
    let html = text(&bare_app, "/?c=bare.md", &cookie).await;
    let header = collection_header(&html);
    assert!(!header.contains("collection-note"), "{header}");
}

/// A narrowing beside the collection keeps the page's header and picks which
/// members show — in the note's order, with the count of what is listed —
/// and when it matches none, the empty state blames the narrowing, not the
/// note's embeds.
#[tokio::test]
async fn a_narrowing_picks_members_but_the_note_keeps_the_page() {
    let dir = page_library();
    let (app, cookie) = login(app(&dir)).await;

    let html = text(&app, "/?c=tour.md&in=alpha", &cookie).await;
    assert_eq!(
        html_paths(&html),
        vec!["alpha/zen.png"],
        "the folder narrows which members show, in the note's order: {html}"
    );
    let header = collection_header(&html);
    assert!(
        header.contains("<h1>Collection: Tour</h1>"),
        "the collection is the page; the folder is a narrowing of it: {header}"
    );
    assert!(header.contains("1 item"), "{header}");
    assert!(
        html.contains("<span class=\"result-count\">1 item</span>"),
        "{html}"
    );
    // The narrowed-away members are not the page's list.
    assert!(!header.contains("3 items"), "{header}");

    let html = text(&app, "/?c=tour.md&in=nowhere", &cookie).await;
    assert!(
        html.contains("No member of this collection matches the filters beside it."),
        "{html}"
    );
}

/// The diagnostics of a note's broken embeds stay on its collection page
/// (FORMAT §5.1: reported, and the line is kept), beside the note's own
/// text — the missing embed's line still shows in that text.
#[tokio::test]
async fn a_broken_embed_is_reported_and_its_line_is_kept_in_the_note_text() {
    let root = tempfile::tempdir().unwrap();
    let write = |rel: &str, bytes: &str| {
        let path = root.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };
    write("a.png", PNG_BYTES);
    write(
        "gone.md",
        "---\ntitle: Gone\n---\nBefore it went away.\n![[a.png]]\nFirst.\n![[missing.png]]\nTail line.\n",
    );
    write("twin-a/photo.png", PNG_BYTES);
    write("twin-b/photo.png", PNG_BYTES);
    write("twin.md", "---\ntitle: Twin\n---\n![[photo.png]]\n");
    let (app, cookie) = login(app(&root)).await;

    let html = text(&app, "/?c=gone.md", &cookie).await;
    assert!(html.contains("matches no file"), "{html}");
    assert_eq!(html_paths(&html), vec!["a.png"]);
    let header = collection_header(&html);
    // §5.1: the line a missing embed sits on is kept; its caption "First."
    // belongs to the member above, and the tail prose to the note.
    assert!(
        header.contains("Before it went away."),
        "the prose: {header}"
    );
    assert!(
        header.contains("[[missing.png]]"),
        "the broken embed's line is kept in the note's text: {header}"
    );
    assert!(
        header.contains("Tail line."),
        "the line after the missing embed is the note's, not a caption: {header}"
    );

    // The ambiguous embed: reported with both candidates.
    let html = text(&app, "/?c=twin.md", &cookie).await;
    assert!(html.contains("matches several files"), "{html}");
    assert!(html.contains("twin-a/photo.png"), "{html}");
    assert!(html.contains("twin-b/photo.png"), "{html}");
}

// === W47: the filter panel beside the grid ===================================

/// One panel row as the reader sees it: a label, a number, and whether it links
/// (a dimmed row shows a number and offers no click; an active row links to the
/// view with that filter let go).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelRow {
    label: String,
    href: Option<String>,
    count: u64,
    active: bool,
}

impl PanelRow {
    /// The row as a test writes it: `label count state`, with `on` for the filter
    /// that is on, `…` for a dimmed zero and an empty state for a plain link.
    fn shape(&self) -> (String, u64, &'static str) {
        (
            self.label.clone(),
            self.count,
            if self.active {
                "on"
            } else if self.href.is_none() {
                "dim"
            } else {
                "link"
            },
        )
    }
}

/// The panel's groups, in the order the page puts them: title, then its rows in
/// the order it ranks them.
fn panel_groups(html: &str) -> Vec<(String, Vec<PanelRow>)> {
    let open = html
        .find("<aside class=\"filter-aside\"")
        .expect("the library page carries a filter panel");
    let close = html[open..]
        .find("</aside>")
        .map(|at| open + at + "</aside>".len())
        .expect("a closed panel");
    let panel = &html[open..close];

    panel
        .split("<h3 class=\"filter-group-title\">")
        .skip(1)
        .map(|chunk| {
            let title = chunk[..chunk.find('<').unwrap()].to_owned();
            let rows = chunk
                .split("<li>")
                .skip(1)
                .map(|cell| {
                    let cell = &cell[..cell.find("</li>").unwrap()];
                    let label = between(cell, "<span class=\"filter-row-label\">");
                    let count = between(cell, "<span class=\"filter-row-count\">")
                        .parse()
                        .expect("a count is a number");
                    PanelRow {
                        href: cell
                            .starts_with("<a class=\"filter-row")
                            .then(|| between(cell, "href=\"").replace("&amp;", "&")),
                        active: cell.starts_with("<a class=\"filter-row is-active\""),
                        label,
                        count,
                    }
                })
                .collect();
            (title, rows)
        })
        .collect()
}

/// The text between an opening attribute/tag and the next quote or tag.
fn between(haystack: &str, open: &str) -> String {
    let after = &haystack[haystack.find(open).unwrap() + open.len()..];
    let stop = after.find(['"', '<']).unwrap();
    after[..stop].to_owned()
}

/// Every row of one group, as `(label, count, state)`, in the page's own order.
fn shapes(html: &str, group: &str) -> Vec<(String, u64, &'static str)> {
    panel_groups(html)
        .into_iter()
        .find(|(title, _)| title == group)
        .unwrap_or_else(|| panic!("no {group} group in the panel"))
        .1
        .iter()
        .map(PanelRow::shape)
        .collect()
}

/// The same rows, sorted, for when a test is about the numbers and not the order.
fn sorted_shapes(html: &str, group: &str) -> Vec<(String, u64, &'static str)> {
    let mut shapes = shapes(html, group);
    shapes.sort();
    shapes
}

/// What `/api/view` says a view holds — the promise read back from the endpoint
/// that serves it.
async fn view_total(app: &axum::Router, href: &str, cookie: &str) -> u64 {
    let uri = match href.find('?') {
        Some(_) => href.replacen('/', "/api/view", 1),
        None => "/api/view".to_owned(),
    };
    json(app, &uri, cookie).await["total"].as_u64().unwrap()
}

/// The strongest thing that can be said about a facet count: it is the
/// number of images the page it links to actually holds. Checked for
/// every row of every group, on views with no filters, one, and two —
/// and on the two shapes a real-size library adds: the Recent lens's
/// 200-image window cutting through the counts, and `sub=0` narrowing
/// a folder to its direct members.
///
/// A row that is off counts the view its own link opens, whatever
/// group it is in — a tag row's link adds its tag to the tags already
/// on, so its count is that AND, not the tag alone. A row that is on
/// counts the view as it stands — the filter that is on, beside the
/// other filters — which is the same number in every group: the row's
/// link is the way out, and the number beside a way-out row says where
/// the reader is, not where the link goes.
#[tokio::test]
async fn every_panel_number_is_the_total_of_the_view_its_row_promises() {
    for (dir, views) in [
        (
            library(),
            vec![
                String::new(),
                "in=refs".into(),
                "tag=nature".into(),
                "untagged=1".into(),
                "recent=1".into(),
                "c=browse.md".into(),
                "in=refs&tag=nature".into(),
                "in=refs&sort=title&tag=ui".into(),
                "untagged=1&recent=1".into(),
            ],
        ),
        (
            window_library(),
            vec![
                String::new(),
                "tag=oldtag".into(),
                "tag=newtag".into(),
                "tag=oldtag&tag=newtag".into(),
                "recent=1".into(),
                "recent=1&tag=oldtag".into(),
                "recent=1&tag=newtag".into(),
                "recent=1&untagged=1".into(),
                "recent=1&c=album.md".into(),
                "in=refs".into(),
                "in=refs&sub=0".into(),
                "in=refs&sub=0&recent=1".into(),
                "in=refs&sub=0&tag=oldtag".into(),
                "in=refs/sub&sub=0".into(),
                "untagged=1".into(),
                "c=album.md".into(),
                "c=album.md&tag=oldtag".into(),
            ],
        ),
    ] {
        let (app, cookie) = login(app(&dir)).await;
        let mut compared = 0;
        for view in views {
            let uri = if view.is_empty() {
                "/".to_owned()
            } else {
                format!("/?{view}")
            };
            let html = text(&app, &uri, &cookie).await;
            // The view on screen: what a chosen row's number says,
            // and the grid every other row's link opens beside.
            let on_screen = view_total(&app, &uri, &cookie).await;
            let mut seen = 0;
            for (group, rows) in panel_groups(&html) {
                for row in rows {
                    match (row.active, row.href.as_deref()) {
                        (true, _) => assert_eq!(
                            row.count, on_screen,
                            "on {uri} the {group} row {row:?} is on, \
                             and the view on screen holds {on_screen}"
                        ),
                        // A dimmed row promises the empty view, and
                        // says so as a number rather than a link.
                        (false, None) => {
                            assert_eq!(row.count, 0, "on {uri}: {row:?} in {group}")
                        }
                        (false, Some(href)) => {
                            let total = view_total(&app, href, &cookie).await;
                            assert_eq!(
                                row.count, total,
                                "on {uri} the {group} row {row:?} promises {} \
                                 and {href} holds {total}",
                                row.count
                            );
                        }
                    }
                    seen += 1;
                    compared += 1;
                }
            }
            // Every view has rows worth checking; a view whose panel had
            // nothing to compare would let the whole test pass without
            // having read anything.
            assert!(seen >= 2, "{uri} had only {seen} comparable rows");
        }
        assert!(
            compared >= 30,
            "only {compared} rows checked across all views"
        );
    }
}

#[tokio::test]
async fn the_panel_counts_what_the_grid_shows_on_a_library_larger_than_the_window() {
    let dir = window_library();
    let (app, cookie) = login(app(&dir)).await;

    // A tag only on old images: the grid holds the ten oldest, every
    // one of them outside the lens's 200-image window. The Recent row
    // is its own link (`tag=oldtag&recent=1`), which holds the same
    // ten — the library-wide window used to count it as 0 and dim it
    // away, and the `newtag` row used to count ten and link to the
    // empty view the two tags AND into.
    let old = text(&app, "/?tag=oldtag", &cookie).await;
    assert_eq!(view_total(&app, "/?tag=oldtag", &cookie).await, 10);
    assert_eq!(
        shapes(&old, "Views"),
        vec![("Recent".into(), 10, "link"), ("Untagged".into(), 0, "dim"),]
    );
    assert_eq!(
        shapes(&old, "Folders"),
        vec![("refs".into(), 10, "link"), ("sub".into(), 0, "dim"),]
    );
    assert_eq!(
        shapes(&old, "Tags"),
        vec![("oldtag".into(), 10, "on"), ("newtag".into(), 0, "dim"),]
    );
    assert_eq!(
        shapes(&old, "Collections"),
        vec![("Album".into(), 2, "link")]
    );

    // The lens on: the grid still shows those ten, so the panel says
    // ten — the Recent row (chosen, the view as it stands) and the
    // `refs` row, which the library-wide window used to dim at 0
    // although every image on screen is in `refs`.
    let lensed = text(&app, "/?recent=1&tag=oldtag", &cookie).await;
    assert_eq!(view_total(&app, "/?recent=1&tag=oldtag", &cookie).await, 10);
    assert_eq!(
        shapes(&lensed, "Views"),
        vec![("Recent".into(), 10, "on"), ("Untagged".into(), 0, "dim"),]
    );
    assert_eq!(
        shapes(&lensed, "Folders"),
        vec![("refs".into(), 10, "link"), ("sub".into(), 0, "dim"),]
    );

    // The lens alone: its window is the ten `newtag` and the 190
    // untagged images, so every row counts what its own link shows —
    // capped at the window, never at the library.
    let recent = text(&app, "/?recent=1", &cookie).await;
    assert_eq!(view_total(&app, "/?recent=1", &cookie).await, 200);
    assert_eq!(
        shapes(&recent, "Views"),
        vec![
            ("Recent".into(), 200, "on"),
            ("Untagged".into(), 190, "link"),
        ]
    );
    assert_eq!(
        shapes(&recent, "Folders"),
        vec![("refs".into(), 200, "link"), ("sub".into(), 10, "link"),]
    );
    assert_eq!(
        shapes(&recent, "Tags"),
        vec![("newtag".into(), 10, "link"), ("oldtag".into(), 10, "link"),]
    );
    assert_eq!(
        shapes(&recent, "Collections"),
        vec![("Album".into(), 5, "link")]
    );

    // `sub=0`: the grid shows `refs`'s 200 direct members, and the
    // folder rows count direct membership too — `refs` no longer
    // rolls its subfolder up into its own number.
    let direct = text(&app, "/?in=refs&sub=0", &cookie).await;
    assert_eq!(view_total(&app, "/?in=refs&sub=0", &cookie).await, 200);
    assert_eq!(
        shapes(&direct, "Folders"),
        vec![("refs".into(), 200, "on"), ("sub".into(), 10, "link"),]
    );
    assert_eq!(
        shapes(&direct, "Tags"),
        vec![("newtag".into(), 10, "link"), ("oldtag".into(), 10, "link"),]
    );
    assert_eq!(
        shapes(&direct, "Views"),
        vec![
            ("Recent".into(), 200, "link"),
            ("Untagged".into(), 180, "link"),
        ]
    );
}

#[tokio::test]
async fn the_panel_reads_the_library_and_narrows_as_the_filters_go_on() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    // Nothing on: four images, two in `refs`, one of them in `refs/ui`, one in
    // `refs2`; tags nature + wide on one, ui on another; two notes-less images;
    // `browse.md` collecting two of them. Ranked by count, and by label where the
    // counts tie.
    let plain = text(&app, "/", &cookie).await;
    assert_eq!(
        shapes(&plain, "Views"),
        vec![("Recent".into(), 4, "link"), ("Untagged".into(), 2, "link")]
    );
    assert_eq!(
        shapes(&plain, "Folders"),
        vec![
            ("refs".into(), 2, "link"),
            ("refs2".into(), 1, "link"),
            ("ui".into(), 1, "link"),
        ]
    );
    assert_eq!(
        shapes(&plain, "Collections"),
        vec![("Browse".into(), 2, "link")]
    );
    assert_eq!(
        shapes(&plain, "Tags"),
        vec![
            ("nature".into(), 1, "link"),
            ("ui".into(), 1, "link"),
            ("wide".into(), 1, "link"),
        ]
    );
    assert!(
        !plain.contains("Clear all filters") && !plain.contains("filter-fold-count"),
        "with nothing on, the panel says nothing about filters being on"
    );

    // One tag on. The folders and the collection honour it; the tag
    // rows count beside it, so a row says what adding it would show —
    // `wide` shares landscape.png with `nature`, while `ui` shares no
    // image with it and falls to the honest zero the panel shows as a
    // dimmed number instead of hiding it.
    let one = text(&app, "/?tag=nature", &cookie).await;
    assert_eq!(
        shapes(&one, "Views"),
        vec![("Recent".into(), 1, "link"), ("Untagged".into(), 0, "dim")]
    );
    assert_eq!(
        shapes(&one, "Folders"),
        vec![
            ("refs".into(), 1, "link"),
            ("refs2".into(), 0, "dim"),
            ("ui".into(), 0, "dim"),
        ]
    );
    assert_eq!(
        shapes(&one, "Collections"),
        vec![("Browse".into(), 1, "link")]
    );
    assert_eq!(
        shapes(&one, "Tags"),
        vec![
            ("nature".into(), 1, "on"),
            ("wide".into(), 1, "link"),
            ("ui".into(), 0, "dim"),
        ]
    );

    // Two filters on: everything but their own groups narrows to the one image.
    let both = text(&app, "/?in=refs&tag=nature", &cookie).await;
    assert_eq!(
        sorted_shapes(&both, "Folders"),
        vec![
            ("refs".into(), 1, "on"),
            ("refs2".into(), 0, "dim"),
            ("ui".into(), 0, "dim"),
        ]
    );
    assert_eq!(
        sorted_shapes(&both, "Tags"),
        vec![
            ("nature".into(), 1, "on"),
            ("ui".into(), 0, "dim"),
            ("wide".into(), 1, "link"),
        ]
    );
    assert_eq!(
        sorted_shapes(&both, "Views"),
        vec![("Recent".into(), 1, "link"), ("Untagged".into(), 0, "dim")]
    );

    // The untagged lens is the case the panel was argued for: it empties the tag
    // group completely (so every tag row dims) while `refs2` — the folder of
    // notes-less images — stays at its one image.
    let untagged = text(&app, "/?untagged=1", &cookie).await;
    assert_eq!(
        sorted_shapes(&untagged, "Tags"),
        vec![
            ("nature".into(), 0, "dim"),
            ("ui".into(), 0, "dim"),
            ("wide".into(), 0, "dim"),
        ]
    );
    assert_eq!(
        sorted_shapes(&untagged, "Folders"),
        vec![
            ("refs".into(), 0, "dim"),
            ("refs2".into(), 1, "link"),
            ("ui".into(), 0, "dim"),
        ]
    );
    assert_eq!(
        sorted_shapes(&untagged, "Views"),
        vec![("Recent".into(), 2, "link"), ("Untagged".into(), 2, "on")]
    );

    // A collection narrows the other groups to its members: `ui` (button.png) is
    // outside `browse.md`, so it dims, while the collection's own row stands open.
    let collected = text(&app, "/?c=browse.md", &cookie).await;
    assert_eq!(
        sorted_shapes(&collected, "Tags"),
        vec![
            ("nature".into(), 1, "link"),
            ("ui".into(), 0, "dim"),
            ("wide".into(), 1, "link"),
        ]
    );
    assert_eq!(
        sorted_shapes(&collected, "Folders"),
        vec![
            ("refs".into(), 1, "link"),
            ("refs2".into(), 0, "dim"),
            ("ui".into(), 0, "dim"),
        ]
    );
    assert_eq!(
        sorted_shapes(&collected, "Collections"),
        vec![("Browse".into(), 2, "on")]
    );
}

#[tokio::test]
async fn the_panel_acts_through_links_alone_and_folds_without_a_script() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    let page = text(&app, "/", &cookie).await;
    let open = page.find("<aside class=\"filter-aside\"").unwrap();
    let panel = &page[open..open + page[open..].find("</aside>").unwrap() + "</aside>".len()];
    assert!(!panel.contains("<script"), "{panel}");
    assert!(!panel.contains("onclick"), "{panel}");
    assert!(!panel.contains("style="), "{panel}");
    // The fold is a real `<details>`; the heading is the desktop's, and the
    // stylesheet keeps one of the two at a width.
    assert!(panel.contains("<details class=\"filter-fold\">"), "{panel}");
    assert!(
        panel.contains("<summary class=\"filter-fold-summary\">"),
        "{panel}"
    );
    assert!(
        panel.contains("<h2 class=\"filter-title\">Filters<"),
        "{panel}"
    );
    // A panel beside the grid adds and removes no tiles: the grid is the grid.
    assert_eq!(html_paths(panel), Vec::<String>::new());
    assert_eq!(
        page_paths(&app, "tag=ui", &cookie).await,
        json_paths(&app, "tag=ui", &cookie).await
    );

    // The fold's button is a phone's row and it carries the number of filters on
    // — the only part of the panel a collapsed phone can see.
    assert!(!panel.contains("filter-fold-count"), "nothing on: {panel}");
    let on = text(&app, "/?tag=ui", &cookie).await;
    assert!(
        on.contains("<span class=\"filter-fold-count\">1</span>"),
        "the fold says one filter is on"
    );
    assert_eq!(
        on.matches("filter-fold-count").count(),
        2,
        "heading and fold"
    );
    // One filter on: the chip beside the search field is already the way out, so
    // the panel does not offer a second one (the rule W43 settled).
    assert!(!on.contains("Clear all filters"), "{on}");
    // And the row that is on is the way back out.
    let ui = panel_groups(&on)
        .into_iter()
        .find(|(title, _)| title == "Tags")
        .unwrap()
        .1
        .into_iter()
        .find(|row| row.label == "ui")
        .unwrap();
    assert_eq!(ui.href.as_deref(), Some("/"));
    assert!(ui.active);
    // Two filters: the number grows, and one link clears both.
    // Two filters on: now the panel offers what no single chip can, which is to
    // let go of both at once.
    let two = text(&app, "/?tag=ui&in=refs", &cookie).await;
    assert!(
        two.contains(
            "<h2 class=\"filter-title\">Filters<span class=\"filter-fold-count\">2</span></h2>"
        ),
        "{two}"
    );
    assert!(
        two.contains("<a class=\"filter-clear\" href=\"/\">Clear all filters</a>"),
        "{two}"
    );
    // The same link where the view carries more than filters. The label
    // promises filters and nothing else, so the order, the tile size and the
    // page the reader is standing on travel with them — and `href="/"` above,
    // on a URL whose only parameters are filters, cannot tell that link apart
    // from one that dropped all three (RW47b Low-1).
    let ordered = text(
        &app,
        "/?tag=ui&in=refs&sort=name&dir=desc&size=l&p=2",
        &cookie,
    )
    .await;
    assert!(
        ordered.contains(
            "<a class=\"filter-clear\" href=\"/?sort=name&amp;dir=desc&amp;size=l&amp;p=2\">"
        ),
        "{ordered}"
    );
}

#[tokio::test]
async fn a_facet_list_over_the_cap_opens_through_a_second_query_parameter() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;

    // The fixture has four tags and three folders, so nothing is capped; the
    // parameter still round-trips, and a page with it reads the same grid.
    let page = text(&app, "/?more=tags", &cookie).await;
    assert!(!page.contains(">Show "), "nothing to show: {page}");
    assert_eq!(
        html_paths(&page),
        html_paths(&text(&app, "/", &cookie).await)
    );

    // A facet list the URL does not know is ignored with a notice, the way every
    // unusable parameter is, and the panel still arrives.
    let odd = text(&app, "/?more=ratings", &cookie).await;
    assert!(odd.contains("Ignored unknown facet list"), "{odd}");
    assert!(odd.contains("<aside class=\"filter-aside\""), "{odd}");
}

/// ================================ W49 · justified rows and the Taken sort ===
///
/// The library these tests sort is built from the fixtures the index crate
/// reads its header facts from: real images with real dimensions, and real EXIF
/// dates. Nothing here is a fabricated number the server had to be told.
///
/// The taken times, in ns since the epoch, straight from the fixtures:
/// - `exif-date.jpg` 2023-07-12 20:54:07.123 +01:00 = 1_689_191_647_123_000_000
/// - `exif-naive.jpg` the same wall clock, read as UTC     = 1_689_195_247_000_000_000
/// - `rotated-exif.jpg` 2018-05-05, −05:00                   = 1_521_994_953_000_000_000
/// - `exif-zeros.jpg` the classic all-zero date, and `pixel.png`, which is a
///   PNG and carries no EXIF at all: both are an unknown instant.
const EXIF_DATE: &[u8] = include_bytes!("../../../tests/fixtures/exif-date.jpg");
const EXIF_NAIVE: &[u8] = include_bytes!("../../../tests/fixtures/exif-naive.jpg");
const EXIF_ROTATED: &[u8] = include_bytes!("../../../tests/fixtures/rotated-exif.jpg");
const EXIF_ZEROS: &[u8] = include_bytes!("../../../tests/fixtures/exif-zeros.jpg");
const PIXEL_PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");

/// A library whose images carry the header facts the index reads: three taken
/// times — one of them tied between two paths, because the two files are the
/// same content — and two images with no taken time at all.
fn header_library() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let write = |rel: &str, bytes: &[u8]| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    };
    write("refs/a-date.jpg", EXIF_DATE);
    write("refs/b-date.jpg", EXIF_DATE);
    write("refs/c-naive.jpg", EXIF_NAIVE);
    write("refs/d-rotated.jpg", EXIF_ROTATED);
    write("refs/e-zeros.jpg", EXIF_ZEROS);
    write("refs/f-pixel.png", PIXEL_PNG);
    // A tag on one dated image, so the sort can be combined with a filter.
    fs::write(
        root.join("refs/a-date.jpg.md"),
        "---\ntags:\n  - dated\n---\nA dated shot.\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn the_taken_sort_orders_by_the_time_a_camera_recorded() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;

    // Newest first: the naive date (later wall clock), then the tied pair in
    // path order, then the older rotated shot, then the two unknowns — last in
    // both directions, because an unknown instant is neither old nor young.
    assert_eq!(
        json_paths(&app, "sort=taken", &cookie).await,
        vec![
            "refs/c-naive.jpg",
            "refs/a-date.jpg",
            "refs/b-date.jpg",
            "refs/d-rotated.jpg",
            "refs/e-zeros.jpg",
            "refs/f-pixel.png",
        ],
        "newest taken first, ties by path, unknowns last"
    );

    // Oldest first is the same order with the known times reversed, and the
    // same two unknowns still last.
    assert_eq!(
        json_paths(&app, "sort=taken&dir=asc", &cookie).await,
        vec![
            "refs/d-rotated.jpg",
            "refs/a-date.jpg",
            "refs/b-date.jpg",
            "refs/c-naive.jpg",
            "refs/e-zeros.jpg",
            "refs/f-pixel.png",
        ],
        "an unknown is not an old time: it stays last"
    );

    // The HTML page and the JSON agree, as they do for every other sort.
    for query in ["sort=taken", "sort=taken&dir=asc"] {
        assert_eq!(
            page_paths(&app, query, &cookie).await,
            json_paths(&app, query, &cookie).await,
            "/?{query} disagrees with /api/view"
        );
    }
}

#[tokio::test]
async fn the_taken_sort_combines_with_the_filters_and_survives_in_the_url() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;

    // A tag narrows the same order, and the sort keeps its place in the URL.
    assert_eq!(
        json_paths(&app, "sort=taken&tag=dated", &cookie).await,
        vec!["refs/a-date.jpg"],
        "a filter narrows the taken order, it does not replace it"
    );
    assert_eq!(
        json_paths(&app, "sort=taken&dir=asc&in=refs&tag=dated", &cookie).await,
        vec!["refs/a-date.jpg"]
    );

    // The menu the page shows offers the sort in both directions, with the
    // scope it is filtering carried along, and the tile's way back keeps it.
    let html = text(&app, "/?in=refs&sort=taken", &cookie).await;
    assert!(
        html.contains("<option value=\"taken-desc\" selected>Taken ↓"),
        "{html}"
    );
    assert!(
        html.contains("<option value=\"taken-asc\">Taken ↑"),
        "{html}"
    );
    assert!(html.contains("name=\"in\" value=\"refs\""), "{html}");

    // Dropping a filter keeps the sort: the chip's link is the same view
    // without the one filter, so the order a person chose is still there.
    let html = text(&app, "/?in=refs&sort=taken&tag=dated", &cookie).await;
    assert!(
        html.contains("href=\"/?in=refs&amp;sort=taken\""),
        "dropping the tag keeps the taken sort: {html}"
    );
    assert!(
        html.contains("?v=in%3Drefs%26tag%3Ddated%26sort%3Dtaken"),
        "and the tile's back link keeps both: {html}"
    );
}

#[tokio::test]
async fn an_image_with_no_taken_time_says_so_where_the_order_shows_it() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;

    // In the taken order the two unknowns are marked, and only those two.
    let html = text(&app, "/?sort=taken", &cookie).await;
    let grid = grid_section(&html);
    assert_eq!(
        grid.matches("class=\"tile-notime\"").count(),
        2,
        "the images with no EXIF date are marked: {grid}"
    );
    assert!(grid.contains("No taken time"), "{grid}");
    assert!(html.contains("data-justify data-taken"), "{html}");

    // In every other order they are quiet, because "no taken time" would only
    // be noise: nothing on screen suggests the order is about dates.
    for query in ["", "sort=name", "sort=added", "recent=1"] {
        let html = text(&app, &format!("/?{query}"), &cookie).await;
        assert!(
            !html.contains("tile-notime"),
            "/?{query} should not mark tiles"
        );
    }

    // The sort the view cannot honour is reported rather than silently
    // dropped, exactly as the Recent lens and a collection report theirs.
    let recent = text(&app, "/?recent=1&sort=taken&dir=asc", &cookie).await;
    assert!(
        recent.contains("Ignored sort: Recent is always the last 200 added, newest first"),
        "{recent}"
    );
    assert!(!recent.contains("tile-notime"), "{recent}");
}

#[tokio::test]
async fn the_api_reports_the_header_facts_the_grid_laid_out_by() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;
    let page = json(&app, "/api/view?sort=taken", &cookie).await;
    let items = page["items"].as_array().unwrap();

    let by_path = |name: &str| {
        items
            .iter()
            .find(|item| item["path"] == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
            .clone()
    };

    // The 12×1 fixture, as its header says, with the taken time it carries.
    let dated = by_path("refs/a-date.jpg");
    assert_eq!(dated["width"], 12);
    assert_eq!(dated["height"], 1);
    assert_eq!(dated["taken_ns"], 1_689_191_647_123_000_000_i64);

    // The rotated fixture stores 12×1 and displays 1×12: the index reports the
    // displayed shape, and the grid lays it out by that.
    let rotated = by_path("refs/d-rotated.jpg");
    assert_eq!(rotated["width"], 1);
    assert_eq!(rotated["height"], 12);

    // A PNG and a JPEG with an all-zero date: an unknown is `null` here, never
    // a zero and never a made-up date.
    for name in ["refs/e-zeros.jpg", "refs/f-pixel.png"] {
        let unknown = by_path(name);
        assert!(unknown["taken_ns"].is_null(), "{name}: {unknown}");
    }
    assert_eq!(by_path("refs/f-pixel.png")["width"], 1);
    assert_eq!(by_path("refs/f-pixel.png")["height"], 1);

    // RW49 L-2: a `null` taken time is not one fact but five, and the agent's
    // view of the library keeps the distinction the index recorded — in the
    // same one word `dimagine scan` prints — rather than collapsing every
    // unknown into the same null (invariant 4). The image page says the same
    // pair of facts in words.
    assert_eq!(
        dated["taken_reason"],
        Value::Null,
        "a row that has an instant has nothing to explain"
    );
    assert_eq!(
        by_path("refs/e-zeros.jpg")["taken_reason"],
        "date-unreadable"
    );
    assert_eq!(by_path("refs/f-pixel.png")["taken_reason"], "no-exif");
}

#[tokio::test]
async fn a_page_carries_the_shape_of_a_known_image_and_a_square_for_an_unknown() {
    let dir = header_library();
    let (known_app, cookie) = login(app(&dir)).await;
    let html = text(&known_app, "/?sort=taken", &cookie).await;

    // A known image: its own dimensions on the thumbnail, so the browser holds
    // the space before the bytes arrive, and the ratio the row is laid out by.
    let grid = grid_section(&html);
    assert!(
        grid.contains("alt=\"a-date.jpg\" width=\"12\" height=\"1\""),
        "{grid}"
    );
    assert!(grid.contains("style=\"--ar:2.500\""), "{grid}");
    // The 1×12 fixture is below the tall threshold, so it is cropped to its
    // cell and badged, and its row ratio is clamped so the row cannot overflow.
    assert!(grid.contains("data-fit=\"tall\""), "{grid}");
    assert!(grid.contains("class=\"tile-badge\""), "{grid}");

    // A header nothing could read — the viewer never opens the file to find
    // out, so the tile falls back to the square cell the grid always had.
    let unknown = tempfile::tempdir().unwrap();
    fs::write(unknown.path().join("scratch.png"), PNG_BYTES).unwrap();
    let (square_app, cookie) = login(app(&unknown)).await;
    let html = text(&square_app, "/", &cookie).await;
    let grid = grid_section(&html);
    assert!(grid.contains("style=\"--ar:1.000\""), "{grid}");
    assert!(
        !grid.contains(" width=") && !grid.contains("data-fit"),
        "an unreadable header states no size and no crop: {grid}"
    );
    assert!(html.contains("data-justify"), "{html}");
}

/// The `<section class="grid …">` of a page, so an assertion about a tile is
/// not read against the chrome's own SVG attributes.
fn grid_section(html: &str) -> &str {
    let start = html.find("<section class=\"grid").expect("a grid section");
    let end = html[start..].find("</section>").expect("a closed grid") + start;
    &html[start..end]
}

#[tokio::test]
async fn the_image_page_shows_the_taken_time_and_the_dimensions() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/image/refs/a-date.jpg", &cookie).await;

    assert!(
        html.contains("<dt>Dimensions</dt><dd>12 × 1</dd>"),
        "{html}"
    );
    assert!(
        html.contains(
            "<dt>Taken</dt><dd><time datetime=\"2023-07-12T19:54:07.123Z\">\
                       12 Jul 2023, 19:54:07 UTC</time></dd>"
        ),
        "{html}"
    );

    // The rotated fixture: displayed 1×12, so the stage crops it tall and the
    // panel says the shape the file really has.
    let html = text(&app, "/image/refs/d-rotated.jpg", &cookie).await;
    assert!(
        html.contains("<dt>Dimensions</dt><dd>1 × 12</dd>"),
        "{html}"
    );
    assert!(
        html.contains("class=\"image-stage\" data-fit=\"tall\""),
        "{html}"
    );

    // An image with no EXIF date: the row is there and says both that there is
    // no taken time and which kind of unknown that is. The index recorded which
    // of its five kinds this is, and the page is where that distinction is kept
    // (invariant 4, RW49 L-2). No date is invented either way.
    let html = text(&app, "/image/refs/f-pixel.png", &cookie).await;
    assert!(
        html.contains("<dt>Taken</dt><dd>Unknown — the file carries no EXIF</dd>"),
        "{html}"
    );
    assert!(
        !html.contains("1970") && !html.contains("1 Jan"),
        "an epoch is not a taken time: {html}"
    );

    // A different kind of unknown off a real header: the date tag is there and
    // names no moment. It is not the same fact as "no EXIF", and does not read
    // as it.
    let html = text(&app, "/image/refs/e-zeros.jpg", &cookie).await;
    assert!(
        html.contains("<dt>Taken</dt><dd>Unknown — the date it carries is unreadable</dd>"),
        "{html}"
    );
}

/// RW49 M-1: the "no taken time" mark has to reach the tiles "Load more"
/// appends too, and say what the server's own mark says. There is no JavaScript
/// harness in this repository, so the pin is on the bytes the page serves: the
/// grid — never the tile still under construction, which has no parent yet and
/// so could not find one — is what the mark is decided against, and the words
/// are the ones the rendered tile carries.
#[tokio::test]
async fn a_tile_the_script_appends_is_marked_the_way_a_rendered_tile_is() {
    let dir = header_library();
    let (app, cookie) = login(app(&dir)).await;
    let page = text(&app, "/?sort=taken", &cookie).await;
    assert!(
        page.contains("data-justify data-taken"),
        "the grid states the order the mark depends on: {page}"
    );
    let mark = grid_section(&page)
        .split("<span class=\"tile-notime\" title=\"")
        .nth(1)
        .expect("the taken grid marks the images with no taken time")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert!(mark.starts_with("No taken time"), "{mark}");

    let script = served_script(&app, &cookie, &page).await;
    assert!(
        script.contains("appendChild(tileElement(item, grid))"),
        "the grid travels into the builder, so the tile is built knowing it"
    );
    assert!(
        script.contains("gridOrdersByTaken(inGrid)"),
        "and the mark is decided by that grid, not by the detached tile"
    );
    assert!(
        !script.contains("tile.closest('.grid')"),
        "a tile under construction has no ancestor to ask"
    );
    assert!(
        script.contains(&format!("'{mark}'")),
        "the script writes the server's own sentence, not a second wording"
    );
}
