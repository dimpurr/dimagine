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

    // `added` is in the past, so Recent is empty on this fixture; what matters
    // is that the lens filters rather than returning everything.
    let recent = json_paths(&app, "recent=1", &cookie).await;
    assert!(recent.len() < 4, "recent narrows: {recent:?}");

    let sidebar = json(&app, "/api/sidebar", &cookie).await;
    assert_eq!(sidebar["total"], 4);
    assert_eq!(sidebar["untagged"], 2);
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
    let chips = html
        .split("<div class=\"scope-chips\">")
        .nth(1)
        .expect("chips")
        .split("</div>")
        .next()
        .unwrap();
    assert!(chips.contains("in: refs"));
    assert!(chips.contains("tag: nature"));
    assert!(chips.contains("q: landscape"));
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
    assert!(html.contains("Show subfolders?"), "{html}");
    assert!(html.contains("href=\"/?in=empty\""));
    assert!(html.contains("Clear all filters"));
}
