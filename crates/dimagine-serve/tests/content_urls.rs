//! Content-addressed preview URLs (ADR-020), end to end: the URL is the
//! content, so it can be cached for as long as the content exists, and the old
//! path routes keep working by pointing at it.
//!
//! Every fixture here is synthetic: a one-pixel PNG from the repository's test
//! fixtures and a provider that writes a marker file instead of decoding
//! anything. No image is rendered by a real encoder and nothing leaves the
//! machine.

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use dimagine_serve::{router, AuthMode, FsCatalog, PreviewKind, PreviewProvider, ServeConfig};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use tower::ServiceExt;

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");
const OTHER_PNG: &[u8] = include_bytes!("../../../tests/fixtures/exif.png");

/// What a rendition's bytes look like when this file's provider made them: the
/// source bytes with a tail, so a test can tell a rendition from the original
/// without decoding anything.
const RENDITION_TAIL: &[u8] = b"\nrendition";

/// A preview provider with the cache's *shape* and none of its work: renditions
/// are keyed by the SHA-256 of the source, live under the library root, and are
/// recorded as they are asked for.
///
/// It exists so a test can see the two things ADR-020 is about: which paths the
/// routes asked to generate (an unknown digest must ask for nothing), and which
/// bytes came back (a rendition is not the original).
struct TestPreview {
    root: PathBuf,
    /// Every `preview_path` call, as the path and the size class it named.
    /// Recorded as text because `PreviewKind` is the route's own vocabulary and
    /// a test's log reads better in the words the URL uses.
    asked: Arc<Mutex<Vec<(String, &'static str)>>>,
    /// When false, `preview_path` answers `None`: no rendition can be made
    /// from the content, which is the fallback-to-the-original case.
    renderable: bool,
}

impl TestPreview {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            asked: Arc::new(Mutex::new(Vec::new())),
            renderable: true,
        }
    }

    fn unrenderable(root: &Path) -> Self {
        Self {
            renderable: false,
            ..Self::new(root)
        }
    }

    fn rendition_path(&self, hash: &str, kind: PreviewKind) -> PathBuf {
        self.root
            .join(".test-renditions")
            .join(format!("{hash}-{}", kind_name(kind)))
    }
}

/// The size class the way a URL spells it.
fn kind_name(kind: PreviewKind) -> &'static str {
    match kind {
        PreviewKind::Thumb => "thumb",
        PreviewKind::View => "view",
    }
}

impl PreviewProvider for TestPreview {
    fn preview_path(&self, image: &str, kind: PreviewKind) -> Option<PathBuf> {
        self.asked
            .lock()
            .unwrap()
            .push((image.to_owned(), kind_name(kind)));
        if !self.renderable {
            return None;
        }
        let source = self.root.join(image);
        let hash = dimagine_preview::sha256_file(&source).ok()?;
        let rendition = self.rendition_path(&hash, kind);
        if !rendition.is_file() {
            fs::create_dir_all(rendition.parent()?).ok()?;
            let mut bytes = fs::read(&source).ok()?;
            bytes.extend_from_slice(RENDITION_TAIL);
            fs::write(&rendition, &bytes).ok()?;
        }
        Some(rendition)
    }

    fn existing_preview(&self, hash: &str, kind: PreviewKind) -> Option<PathBuf> {
        let rendition = self.rendition_path(hash, kind);
        (self.renderable && rendition.is_file()).then_some(rendition)
    }
}

fn library(files: &[(&str, &[u8])]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, bytes) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    dir
}

fn app_with(root: &TempDir, previews: TestPreview, config: ServeConfig) -> axum::Router {
    router(
        FsCatalog::new(root.path()).unwrap(),
        previews,
        ServeConfig {
            // Account state inside the test's own directory: no test may read
            // or write the real user state directory.
            data_dir: root.path().join("state"),
            ..config
        },
    )
}

fn app(root: &TempDir, previews: TestPreview) -> axum::Router {
    app_with(root, previews, ServeConfig::default())
}

fn request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

async fn login(app: &axum::Router, passcode: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(format!("passcode={passcode}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> axum::response::Response {
    app.clone()
        .oneshot(request(uri, Some(cookie)))
        .await
        .unwrap()
}

async fn bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

/// One response header as text, or `""` when the answer carries none.
fn header_of(response: &axum::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

/// The content digest one image is addressed by, as the JSON states it —
/// `None` while the index does not hold one (a file it has not hashed yet).
async fn digest_now(app: &axum::Router, cookie: &str, path: &str) -> Option<String> {
    let response = get(app, &format!("/api/image/{path}"), cookie).await;
    if response.status() != StatusCode::OK {
        return None;
    }
    let document: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    document["sha256"].as_str().map(str::to_owned)
}

/// The content digest one image is addressed by, which the tests below assert
/// must exist.
async fn digest_of(app: &axum::Router, cookie: &str, path: &str) -> String {
    digest_now(app, cookie, path)
        .await
        .unwrap_or_else(|| panic!("{path} has no digest"))
}

/// The rendition URL a tile for this image asks, read off the page itself.
async fn tile_src_of(app: &axum::Router, cookie: &str, path: &str) -> String {
    let html = String::from_utf8(bytes(get(app, "/", cookie).await).await).unwrap();
    // `data-path` is the tile's own name for the image, and it is the prefix
    // the viewer's tests read a page's paths back out of.
    let marker = format!("data-path=\"{path}\"");
    let tile = html
        .split(&marker)
        .nth(1)
        .unwrap_or_else(|| panic!("no tile for {path} in {html}"));
    let src = tile.split("src=\"").nth(1).expect("a tile carries an img");
    src.split('"').next().unwrap().to_owned()
}

fn generation() -> &'static str {
    dimagine_preview::RENDITION_GENERATION
}

/// The headline of ADR-020 item 2: a content URL answers with the rendition,
/// promises it for as long as the URL exists, states its ETag from the URL's own
/// three facts, and revalidates to a 304.
#[tokio::test]
async fn w71_a_content_url_is_immutable_and_revalidates_with_its_own_etag() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;
    // The generation the URL is spelled with, as the cache names it.
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;
    assert_eq!(hash.len(), 64, "a digest is 64 hex characters");

    let url = format!("/p/{hash}/thumb.{generation}");
    let response = get(&app, &url, &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "cache-control"),
        "private, max-age=31536000, immutable",
        "the URL is the content, so the answer can be kept for as long as it exists"
    );
    assert_eq!(
        header_of(&response, "etag"),
        format!("\"{hash}.thumb.{generation}\""),
        "the ETag is the digest, the size class and the generation"
    );
    let etag = header_of(&response, "etag");
    let served = bytes(response).await;
    let mut expected = PNG.to_vec();
    expected.extend_from_slice(RENDITION_TAIL);
    assert_eq!(served, expected, "the rendition, not the original");

    // The same URL again, with the ETag it just gave: nothing changed, because
    // nothing can.
    let mut revalidate = request(&url, Some(&cookie));
    revalidate
        .headers_mut()
        .insert(header::IF_NONE_MATCH, etag.parse().unwrap());
    let response = app.clone().oneshot(revalidate).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(bytes(response).await.is_empty(), "a 304 carries no body");

    // The other size class is a different URL with a different ETag.
    let response = get(&app, &format!("/p/{hash}/view.{generation}"), &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, "etag"),
        format!("\"{hash}.view.{generation}\"")
    );
    assert!(bytes(response).await.len() > PNG.len());

    // The digest is case-insensitive in the URL and lower-case in the answer:
    // the same bytes are the same address.
    let response = get(
        &app,
        &format!("/p/{}/thumb.{generation}", hash.to_uppercase()),
        &cookie,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "etag"), etag);

    // The generation is part of the address: another one is a hop to this one,
    // never the answer itself (item 8).
    let response = get(&app, &format!("/p/{hash}/thumb.g1"), &cookie).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        header_of(&response, "location"),
        format!("/p/{hash}/thumb.{generation}")
    );
    let response = get(&app, &format!("/p/{hash}/thumb"), &cookie).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        header_of(&response, "location"),
        format!("/p/{hash}/thumb.{generation}")
    );
}

/// ADR-020 item 1, the other half: the pages this server renders — the grid, a
/// collection's covers, the image page's stage — and the JSON the script reads
/// all ask for the content. No page asks for a rendition by path any more.
#[tokio::test]
async fn w71_pages_and_json_ask_for_content_and_never_a_path_rendition() {
    let root = library(&[
        ("refs/one.png", PNG),
        ("refs/two.png", OTHER_PNG),
        ("plain.png", PNG),
        (
            "browse.md",
            b"---\ntitle: Browse\n---\n![[refs/one.png]]\n![[plain.png]]\n",
        ),
    ]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;
    // The generation the URL is spelled with, as the cache names it.
    let generation = generation();
    let one = digest_of(&app, &cookie, "refs/one.png").await;

    // The grid: every tile asks its own content, and none asks a path.
    let html = String::from_utf8(bytes(get(&app, "/", &cookie).await).await).unwrap();
    assert!(
        html.contains(&format!("src=\"/p/{one}/thumb.{generation}\"")),
        "{html}"
    );
    assert!(
        !html.contains("src=\"/thumb/"),
        "a page that knows the digest must not send a picture to a path URL: {html}"
    );
    assert_eq!(
        tile_src_of(&app, &cookie, "refs/one.png").await,
        format!("/p/{one}/thumb.{generation}")
    );

    // The image page's stage asks the view rendition by content.
    let page =
        String::from_utf8(bytes(get(&app, "/image/refs/one.png", &cookie).await).await).unwrap();
    assert!(
        page.contains(&format!("src=\"/p/{one}/view.{generation}\"")),
        "{page}"
    );
    assert!(!page.contains("src=\"/media/"), "{page}");

    // A collection's cover, and its member tiles, are addressed the same way.
    let collections =
        String::from_utf8(bytes(get(&app, "/collections", &cookie).await).await).unwrap();
    assert!(
        collections.contains(&format!("src=\"/p/{one}/thumb.{generation}\"")),
        "{collections}"
    );
    assert!(!collections.contains("src=\"/thumb/"), "{collections}");

    // The JSON an agent (or the script's own "Load more") reads carries the
    // digest each row's URL is built from, and never a URL of its own making.
    let response = get(&app, "/api/view", &cookie).await;
    let document: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    let items = document["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "{document}");
    for item in items {
        let digest = item["sha256"].as_str().unwrap_or_else(|| panic!("{item}"));
        assert_eq!(digest.len(), 64, "{item}");
    }
    // The same bytes in two places are one digest: the URL does not depend on
    // which path asked for it (ADR-020 item 7b).
    let plain = digest_of(&app, &cookie, "plain.png").await;
    assert_eq!(plain, one, "the same bytes are the same content");
}

/// ADR-020 item 3: the old path routes still work, by pointing at the content
/// URL — and the hop itself is never cached, because a path can be reused for
/// other bytes and a cached 301 would pin it to one digest.
#[tokio::test]
async fn w71_the_path_routes_redirect_to_the_content_url_without_caching_the_hop() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;
    let hash = digest_of(&app, &cookie, "refs/one.png").await;

    for (route, variant) in [
        ("/thumb/refs/one.png", "thumb"),
        ("/media/refs/one.png", "view"),
    ] {
        let response = get(&app, route, &cookie).await;
        assert_eq!(response.status(), StatusCode::FOUND, "{route}");
        let target = format!("/p/{hash}/{variant}.{}", generation());
        assert_eq!(header_of(&response, "location"), target, "{route}");
        let cache_control = header_of(&response, "cache-control");
        assert_eq!(cache_control, "private, no-cache", "{route}");
        assert!(
            !cache_control.contains("immutable"),
            "the hop must never be immutable: {route}"
        );
        // Following the hop lands on the immutable answer.
        let response = get(&app, &target, &cookie).await;
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert!(header_of(&response, "cache-control").contains("immutable"));
    }

    // `/raw/` is out of scope (ADR-020): the original as written, answered by
    // the route itself, revalidated, and never a redirect.
    let response = get(&app, "/raw/refs/one.png", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, PNG);
}

/// ADR-020 item 4: the content URL is behind the same gate as the path routes —
/// the digest proves a content exists, it never grants a look at it — and with
/// `--auth none` both are public, as everything is.
#[tokio::test]
async fn w71_a_content_url_answers_to_the_same_gate_as_a_path_route() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;
    let hash = digest_of(&app, &cookie, "refs/one.png").await;
    let content = format!("/p/{hash}/thumb.{}", generation());

    let signed_out = app.clone().oneshot(request(&content, None)).await.unwrap();
    let path_route = app
        .clone()
        .oneshot(request("/thumb/refs/one.png", None))
        .await
        .unwrap();
    assert_ne!(signed_out.status(), StatusCode::OK);
    assert_eq!(
        signed_out.status(),
        path_route.status(),
        "the content route is gated exactly like the path route"
    );
    assert_eq!(
        header_of(&signed_out, "location"),
        header_of(&path_route, "location")
    );

    // An open server: no session exists to have, and the content is served.
    // It reads a library of its own, because a process holds each library's
    // index once and this test already has one open over the first — and the
    // bytes are the same bytes, so they are the same digest and the same URL.
    let open_root = library(&[("refs/one.png", PNG)]);
    let open = app_with(
        &open_root,
        TestPreview::new(open_root.path()),
        ServeConfig {
            auth: AuthMode::None,
            ..ServeConfig::default()
        },
    );
    let response = get(&open, &content, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header_of(&response, "cache-control").contains("immutable"));
}

/// ADR-020 item 5: signing out is also forgetting the library. A preview is
/// cached for a year, so the next person at this machine must not read one back
/// out of the browser's disk cache.
#[tokio::test]
async fn w71_logout_clears_the_site_data_cache() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header("cookie", cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        header_of(&response, "clear-site-data"),
        "\"cache\"",
        "only the cache: the cookie is cleared by its own header"
    );
    assert_eq!(header_of(&response, "cache-control"), "no-store");
    let cleared = header_of(&response, "set-cookie");
    assert!(
        cleared.contains("Max-Age=0") || cleared.contains("max-age=0"),
        "{cleared}"
    );
}

/// The image paths `/api/view` lists — what the viewer's own index says the
/// library holds right now.
async fn view_paths(app: &axum::Router, cookie: &str) -> Vec<String> {
    let response = get(app, "/api/view", cookie).await;
    let document: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    document["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap().to_owned())
        .collect()
}

/// Poll `check` until it holds, or fail the test naming what never happened.
///
/// The library the viewer reads is resynced on a second, and the background
/// preview backfill runs on its own; a test that changed a file therefore waits
/// for the viewer to have seen the change instead of sleeping a guessed number
/// of milliseconds and hoping.
async fn wait_until<F, Fut>(mut check: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("waited five seconds and never saw {what}");
}

/// A library whose index is resynced every second, so a test can change a file
/// and wait for the viewer to have picked it up.
fn live_config() -> ServeConfig {
    ServeConfig {
        rescan_interval: 1,
        ..ServeConfig::default()
    }
}

/// ADR-020 item 7b: a move changes a path and nothing else. The content URL —
/// and every cached answer behind it — survives a rename, which is the whole
/// point of addressing content rather than locations.
#[tokio::test]
async fn w71_a_rename_keeps_the_content_url_and_its_cached_rendition() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app_with(&root, TestPreview::new(root.path()), live_config());
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;
    let url = format!("/p/{hash}/thumb.{generation}");
    let served = bytes(get(&app, &url, &cookie).await).await;
    assert!(!served.is_empty());

    fs::rename(
        root.path().join("refs/one.png"),
        root.path().join("refs/moved.png"),
    )
    .unwrap();
    wait_until(
        || async {
            view_paths(&app, &cookie)
                .await
                .iter()
                .any(|path| path == "refs/moved.png")
        },
        "the move in the index",
    )
    .await;

    // Same bytes, same digest, same URL: the tile at the new path asks exactly
    // where the old one did.
    assert_eq!(
        digest_of(&app, &cookie, "refs/moved.png").await,
        hash,
        "a move does not change content"
    );
    assert_eq!(tile_src_of(&app, &cookie, "refs/moved.png").await, url);
    // The URL a page cached a year ago still answers, from the cache alone.
    let response = get(&app, &url, &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, served);
    // The path that no longer exists is a 404, and the new path redirects to
    // the same content URL.
    assert_eq!(
        get(&app, "/thumb/refs/one.png", &cookie).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = get(&app, "/thumb/refs/moved.png", &cookie).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(header_of(&response, "location"), url);
}

/// ADR-020 item 7c: changed bytes are a different digest, so they are a
/// different URL — the page moves to it — and the old URL keeps answering the
/// pixels it always did, as long as some file still has those bytes. When
/// nothing does, the old URL is a 404 that generates nothing.
#[tokio::test]
async fn w71_changed_bytes_are_a_new_url_and_the_old_one_keeps_its_pixels() {
    let root = library(&[("refs/one.png", PNG), ("keep.png", PNG)]);
    let previews = TestPreview::new(root.path());
    let asked = previews.asked.clone();
    let app = app_with(&root, previews, live_config());
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let before = digest_of(&app, &cookie, "refs/one.png").await;
    let old_url = format!("/p/{before}/thumb.{generation}");
    let old_bytes = bytes(get(&app, &old_url, &cookie).await).await;
    assert!(!old_bytes.is_empty());

    // The bytes under the path change; `keep.png` still holds the old ones.
    fs::write(root.path().join("refs/one.png"), OTHER_PNG).unwrap();
    wait_until(
        || async {
            digest_now(&app, &cookie, "refs/one.png")
                .await
                .is_some_and(|digest| digest != before)
        },
        "the new bytes to be indexed",
    )
    .await;
    let after = digest_of(&app, &cookie, "refs/one.png").await;
    assert_ne!(after, before, "changed bytes are a different content");
    assert_eq!(
        tile_src_of(&app, &cookie, "refs/one.png").await,
        format!("/p/{after}/thumb.{generation}"),
        "the page follows the content"
    );
    // The old URL still answers the old pixels: the copy is what it addresses
    // now, and the cached rendition it serves was never touched.
    let response = get(&app, &old_url, &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, old_bytes);

    // With nothing left carrying those bytes, the URL is a 404 — and not a
    // request to render anything from a digest no path names.
    fs::remove_file(root.path().join("keep.png")).unwrap();
    wait_until(
        || async {
            !view_paths(&app, &cookie)
                .await
                .iter()
                .any(|path| path == "keep.png")
        },
        "the copy to leave the index",
    )
    .await;
    let asked_before = asked.lock().unwrap().len();
    assert_eq!(
        get(&app, &old_url, &cookie).await.status(),
        StatusCode::NOT_FOUND,
        "a digest no file carries names no content"
    );
    assert_eq!(
        asked.lock().unwrap().len(),
        asked_before,
        "an unknown digest is never a request to generate"
    );
}

/// ADR-020's attack review, as a test: a digest the index does not know is a
/// 404, and it is never a request to render anything — a URL that carries no
/// path cannot be an authorised request to spend decode time, and answering it
/// from the cache would be an existence oracle.
#[tokio::test]
async fn w71_an_unknown_digest_is_a_404_that_generates_nothing() {
    let root = library(&[("refs/one.png", PNG)]);
    let previews = TestPreview::new(root.path());
    let asked = previews.asked.clone();
    let app = app(&root, previews);
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;

    // The background pass fills the index's own gap (item 6) with no request
    // involved; wait for it to have asked about the one image there is, so the
    // counts below are a request's doing and nobody else's.
    wait_until(
        || async {
            asked
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path == "refs/one.png")
        },
        "the background backfill to ask about the indexed image",
    )
    .await;

    let unknown = "f".repeat(64);
    let settled = asked.lock().unwrap().len();
    for url in [
        format!("/p/{unknown}/thumb.{generation}"),
        format!("/p/{unknown}/view.{generation}"),
    ] {
        assert_eq!(
            get(&app, &url, &cookie).await.status(),
            StatusCode::NOT_FOUND,
            "{url}"
        );
    }
    assert_eq!(
        asked.lock().unwrap().len(),
        settled,
        "an unknown digest is not a request to generate"
    );

    // The control, so the counter is known to be one that can move: a digest
    // the index *does* hold, whose rendition is gone, is a request that
    // generates exactly once.
    fs::remove_file(root.path().join(format!(".test-renditions/{hash}-thumb"))).unwrap();
    let before = asked.lock().unwrap().len();
    let response = get(&app, &format!("/p/{hash}/thumb.{generation}"), &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        asked.lock().unwrap().len(),
        before + 1,
        "a known digest with no rendition is generated, once"
    );
}

/// ADR-020 item 1: `<sha256>` is 64 hexadecimal characters and the variant is a
/// size class with a generation — and nothing else reaches the route. There is
/// no path parameter to escape with, so a spelling that is not a rendition is a
/// 404 before anything is looked up.
#[tokio::test]
async fn w71_a_spelling_that_is_not_a_rendition_is_a_404() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::new(root.path()));
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;
    // The control first: this spelling is served.
    assert_eq!(
        get(&app, &format!("/p/{hash}/thumb.{generation}"), &cookie)
            .await
            .status(),
        StatusCode::OK
    );

    let short = "a".repeat(63);
    let long = "a".repeat(65);
    let non_hex = "z".repeat(64);
    for spelling in [
        // Not a digest at all.
        format!("/p/not-a-digest/thumb.{generation}"),
        format!("/p/{short}/thumb.{generation}"),
        format!("/p/{long}/thumb.{generation}"),
        format!("/p/{non_hex}/thumb.{generation}"),
        format!("/p/%2e%2e/thumb.{generation}"),
        format!("/p/../thumb.{generation}"),
        // A digest, but not a rendition request.
        format!("/p/{hash}/raw.{generation}"),
        format!("/p/{hash}/thumb.jpg"),
        format!("/p/{hash}/thumb."),
        format!("/p/{hash}/thumb.g"),
        format!("/p/{hash}/thumb.gx"),
        format!("/p/{hash}/thumb.g-1"),
        format!("/p/{hash}/thumb.G2"),
        format!("/p/{hash}/Thumb.{generation}"),
        format!("/p/{hash}/view{generation}"),
        // More segments than the route has.
        format!("/p/{hash}/thumb.{generation}/extra"),
        format!("/p/{hash}%2Fthumb.{generation}"),
        format!("/p//thumb.{generation}"),
    ] {
        assert_eq!(
            get(&app, &spelling, &cookie).await.status(),
            StatusCode::NOT_FOUND,
            "{spelling} is not a rendition"
        );
    }
    // A generation the server does not serve is a hop to the one it does,
    // never a 404: a bookmarked URL from an older policy keeps working (item 8).
    let response = get(&app, &format!("/p/{hash}/thumb.g99"), &cookie).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        header_of(&response, "location"),
        format!("/p/{hash}/thumb.{generation}")
    );
}

/// ADR-020 item 6's second half: a rendition that cannot be made is answered by
/// the original — outside the URL's promise, so without `immutable` and with an
/// ETag of the bytes actually served.
#[tokio::test]
async fn w71_a_rendition_that_cannot_be_made_falls_back_to_the_original() {
    let root = library(&[("refs/one.png", PNG)]);
    let app = app(&root, TestPreview::unrenderable(root.path()));
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;

    let response = get(&app, &format!("/p/{hash}/view.{generation}"), &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cache_control = header_of(&response, "cache-control");
    assert_eq!(
        cache_control, "private, no-cache",
        "the URL promised this content's rendition, not its original"
    );
    assert!(!cache_control.contains("immutable"), "{cache_control}");
    let etag = header_of(&response, "etag");
    assert_ne!(
        etag,
        format!("\"{hash}.view.{generation}\""),
        "the fallback's ETag is what was served, not what the URL promised"
    );
    assert_eq!(bytes(response).await, PNG, "the image as written");
}

/// ADR-020 item 6's first half: the viewer works the index's gap in the
/// background, without any request, so a library that has never been opened
/// still fills in as it is browsed.
#[tokio::test]
async fn w71_the_background_pass_builds_what_the_index_says_is_missing() {
    let root = library(&[("a.png", PNG), ("refs/b.png", OTHER_PNG)]);
    let previews = TestPreview::new(root.path());
    let asked = previews.asked.clone();
    let app = app(&root, previews);

    wait_until(
        || async {
            let asked = asked.lock().unwrap();
            ["a.png", "refs/b.png"]
                .iter()
                .all(|path| asked.iter().any(|(seen, _)| seen == path))
        },
        "the background pass to reach both images",
    )
    .await;

    // Both size classes of both images, and nothing else: the pass works the
    // index's rows, not the folder.
    let asked = asked.lock().unwrap().clone();
    for path in ["a.png", "refs/b.png"] {
        for kind in ["thumb", "view"] {
            assert!(
                asked.contains(&(path.to_owned(), kind)),
                "the pass never asked for {path} {kind}: {asked:?}"
            );
        }
    }
    assert_eq!(asked.len(), 4, "two images, two renditions each: {asked:?}");
    // The app is only dropped here so the router lives as long as the pass.
    drop(app);
}

/// W70 review M1: the light/heavy classification must not re-read an image to
/// work out where its rendition lives. The digest comes from the index, keyed by
/// the size and mtime a `stat` gives — which is why every answer below still
/// works with the source file unreadable.
#[cfg(unix)]
#[tokio::test]
async fn w71_a_cached_preview_request_does_not_read_the_source_file() {
    use std::os::unix::fs::PermissionsExt;

    let root = library(&[("refs/one.png", PNG)]);
    let previews = TestPreview::new(root.path());
    let app = app(&root, previews);
    let cookie = login(&app, "2333").await;
    let generation = generation();
    let hash = digest_of(&app, &cookie, "refs/one.png").await;
    let url = format!("/p/{hash}/thumb.{generation}");
    let served = bytes(get(&app, &url, &cookie).await).await;
    assert!(
        !served.is_empty(),
        "the rendition exists before the lock-out"
    );

    let source = root.path().join("refs/one.png");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();

    // The path route resolves the digest without opening the image: reading it
    // would have failed on the permission and left the request to be answered
    // 200 from the heavy class, where a redirect is the answer only when the
    // index could name the content.
    let response = get(&app, "/thumb/refs/one.png", &cookie).await;
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "the digest is the index's, not a hash of a file nobody can read"
    );
    assert_eq!(header_of(&response, "location"), url);

    // And the content URL answers from the cache alone, byte for byte.
    let response = get(&app, &url, &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, served);

    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
}
