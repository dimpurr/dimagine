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
    assert!(html.contains(">↓</span>"), "showing the order: {html}");
    assert!(
        !html.contains("disabled"),
        "nothing on the Recent lens is unreachable: {html}"
    );
    assert!(
        !html.contains(">Recent is always"),
        "and the sentence is not visible text that widens the toolbar: {html}"
    );
    assert!(
        html.contains("title=\"Recent is always the last 200 added, newest first.\""),
        "and it says why: {html}"
    );
    // A URL that asks for a sort the lens cannot honour shows the same label
    // and says, in the notice box, that the sort was ignored.
    let html = text(&app, "/?recent=1&sort=name-asc", &cookie).await;
    assert!(
        html.contains(">↓</span>"),
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

#[tokio::test]
async fn a_tile_remembers_the_view_it_came_from() {
    let dir = library();
    let (app, cookie) = login(app(&dir)).await;
    let html = text(&app, "/?in=refs&sort=name", &cookie).await;
    assert!(
        html.contains("?v=in%3Drefs%26sort%3Dname"),
        "a tile carries the view: {html}"
    );

    // And the image page offers the way back.
    let html = text(
        &app,
        "/image/refs/ui/button.png?v=in%3Drefs%26sort%3Dname",
        &cookie,
    )
    .await;
    assert!(html.contains("href=\"/?in=refs&amp;sort=name\""), "{html}");
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

    // An image in no collection says nothing about it.
    let html = text(&app, "/image/refs2/trap.png", &cookie).await;
    assert!(!html.contains("Appears in"), "{html}");
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
    // The order the toolbar names the note's, not a sort's.
    assert!(html.contains(">Note order</span>"), "{html}");
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
