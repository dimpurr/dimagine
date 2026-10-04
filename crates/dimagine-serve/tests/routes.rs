use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{router, CachedPreview, Catalog, FsCatalog, OriginalPreview, ServeConfig};
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

fn library() -> (TempDir, axum::Router) {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("art")).unwrap();
    fs::write(
        temp.path().join("art/猫.png"),
        b"\x89PNG\r\n\x1a\nimage bytes",
    )
    .unwrap();
    fs::write(
        temp.path().join("art/猫.png.md"),
        "---\ntitle: 猫\nrating: 5\n---\nA **quiet** cat.\n<script>alert(1)</script>\n",
    )
    .unwrap();
    fs::write(temp.path().join("art/second.jpg"), b"\xff\xd8\xffimage two").unwrap();
    fs::create_dir_all(temp.path().join("art/nested")).unwrap();
    fs::write(temp.path().join("set.md"), "---\nkind: collection\ntitle: Ordered set\n---\n![[art/second.jpg]]\nSecond caption\n![[art/猫.png]]\nFirst cat.\n").unwrap();
    fs::write(
        temp.path().join("art/猫.png.source.json"),
        "{\"origin\":true}",
    )
    .unwrap();
    fs::write(temp.path().join(".ignored.png"), b"ignored").unwrap();
    let catalog = FsCatalog::new(temp.path()).unwrap();
    let app = router(catalog, OriginalPreview, ServeConfig::default());
    (temp, app)
}

async fn body(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn login(app: axum::Router) -> (axum::Router, String) {
    let response = app
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
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    (app, cookie)
}

#[tokio::test]
async fn html_pages_and_json_routes_render_unicode_notes_and_collection_order() {
    let (_temp, app) = library();
    let (app, cookie) = login(app).await;
    for route in [
        "/",
        "/folder/art",
        "/collection/set.md",
        "/image/art/%E7%8C%AB.png",
        "/api/folder",
        "/api/folder/art",
        "/api/collection/set.md",
        "/api/image/art/%E7%8C%AB.png",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(route)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{route}");
    }
    let root_page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(body(root_page).await.contains("folder"));
    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/collection/set.md")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body(page).await;
    assert!(html.find("Second caption").unwrap() < html.find("First cat.").unwrap());
    assert!(html.contains("art/second.jpg"));
    let detail = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/image/art/%E7%8C%AB.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body(detail).await;
    assert!(json.contains("quiet"));
    assert!(json.contains("猫.png.source.json"));
    let page = app
        .oneshot(
            Request::builder()
                .uri("/image/art/%E7%8C%AB.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let rendered = body(page).await;
    assert!(rendered.contains("&lt;script&gt;"));
    assert!(!rendered.contains("<script>"));
}

#[tokio::test]
async fn passcode_gate_and_bad_passcode() {
    let (_temp, app) = library();
    let gated = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(gated.status(), StatusCode::SEE_OTHER);
    assert_eq!(gated.headers()["location"], "/login");
    let bad = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("passcode=no"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn traversal_and_outside_symlink_are_rejected() {
    let (temp, app) = library();
    let outside = tempfile::NamedTempFile::new().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), temp.path().join("leak.png")).unwrap();
    let catalog = FsCatalog::new(temp.path()).unwrap();
    assert!(catalog.resolve_path("../outside.png").is_err());
    #[cfg(unix)]
    assert!(catalog.resolve_path("leak.png").is_err());
    let (app, cookie) = login(app).await;
    for path in ["/media/..%2F..%2Fetc%2Fpasswd", "/media/leak.png"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn image_response_has_type_etag_and_last_modified() {
    let (_temp, app) = library();
    let (app, cookie) = login(app).await;
    let response = app
        .oneshot(
            Request::builder()
                .uri("/media/art/%E7%8C%AB.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert!(response.headers().contains_key("etag"));
    assert!(response.headers().contains_key("last-modified"));
}

#[test]
fn scanner_skips_format_ignored_paths_and_unicode_is_preserved() {
    let (_temp, _app) = library();
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join(".hidden")).unwrap();
    fs::write(temp.path().join("Thumbs.db"), b"not image").unwrap();
    fs::write(temp.path().join("._fork.png"), b"ignore").unwrap();
    fs::write(temp.path().join("桌面.png"), b"image").unwrap();
    let entries = FsCatalog::new(temp.path())
        .unwrap()
        .list_folder("")
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "桌面.png");
}

#[tokio::test]
async fn special_character_and_unicode_urls_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    // Folders with special characters
    let folder_pixiv = root.join("pixiv & twitter");
    fs::create_dir_all(&folder_pixiv).unwrap();

    let folder_ab = root.join("a+b");
    fs::create_dir_all(&folder_ab).unwrap();

    // Images with special characters
    fs::write(
        folder_pixiv.join("50% off #1?.png"),
        b"\x89PNG\r\n\x1a\nfake png 1",
    )
    .unwrap();
    fs::write(
        folder_ab.join("雨の日 ☔.png"),
        b"\x89PNG\r\n\x1a\nfake png 2",
    )
    .unwrap();

    // Decomposed NFD file
    let nfd_name = "touche\u{301}.png";
    fs::write(root.join(nfd_name), b"\x89PNG\r\n\x1a\nfake png 3").unwrap();

    // Collections named with special characters
    fs::write(
        root.join("pixiv & twitter.md"),
        "---\nkind: collection\ntitle: pixiv & twitter\n---\n![[pixiv & twitter/50% off #1?.png]]\n",
    )
    .unwrap();
    fs::write(
        root.join("雨の日 ☔.md"),
        "---\nkind: collection\ntitle: 雨の日 ☔\n---\n![[a+b/雨の日 ☔.png]]\n",
    )
    .unwrap();

    let catalog = FsCatalog::new(root).unwrap();
    let app = router(catalog, OriginalPreview, ServeConfig::default());
    let (app, cookie) = login(app).await;

    // 1. Root page generates links to subfolders and collections
    let root_page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(root_page.status(), StatusCode::OK);
    let root_html = body(root_page).await;

    // Verify phone tile layout styles
    assert!(root_html.contains("aspect-ratio:1"));
    assert!(root_html.contains("object-fit:cover"));
    assert!(root_html
        .contains("@media(max-width:420px){.grid{grid-template-columns:repeat(2,minmax(0,1fr))}}"));

    // Verify folder links in root
    assert!(root_html.contains("/folder/pixiv%20%26%20twitter"));
    assert!(root_html.contains("/folder/a%2Bb"));
    assert!(root_html.contains("/collection/pixiv%20%26%20twitter.md"));
    assert!(root_html.contains("/collection/%E9%9B%A8%E3%81%AE%E6%97%A5%20%E2%98%94.md"));

    // 2. Request folder via generated link
    let folder_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/folder/pixiv%20%26%20twitter")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(folder_resp.status(), StatusCode::OK);
    let folder_html = body(folder_resp).await;

    // Check breadcrumbs and image links inside folder
    assert!(
        folder_html.contains("<a href=\"/folder/pixiv%20%26%20twitter\">pixiv &amp; twitter</a>")
    );
    assert!(folder_html.contains("/image/pixiv%20%26%20twitter/50%25%20off%20%231%3F.png"));
    assert!(folder_html.contains("/thumb/pixiv%20%26%20twitter/50%25%20off%20%231%3F.png"));

    // Request breadcrumb link
    let crumb_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/folder/pixiv%20%26%20twitter")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(crumb_resp.status(), StatusCode::OK);

    // Request image page via generated link
    let image_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/image/pixiv%20%26%20twitter/50%25%20off%20%231%3F.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(image_resp.status(), StatusCode::OK);

    // Request thumb and media for special character image
    let thumb_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/thumb/pixiv%20%26%20twitter/50%25%20off%20%231%3F.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(thumb_resp.status(), StatusCode::OK);

    let media_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/pixiv%20%26%20twitter/50%25%20off%20%231%3F.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(media_resp.status(), StatusCode::OK);

    // Request collection via generated link
    let coll_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/collection/pixiv%20%26%20twitter.md")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(coll_resp.status(), StatusCode::OK);

    // Folder and image with a+b and emoji
    let ab_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/folder/a%2Bb")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ab_resp.status(), StatusCode::OK);

    let emoji_img_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/image/a%2Bb/%E9%9B%A8%E3%81%AE%E6%97%A5%20%E2%98%94.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(emoji_img_resp.status(), StatusCode::OK);

    // NFC request for NFD file
    let nfc_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/image/touch%C3%A9.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(nfc_resp.status(), StatusCode::OK);

    // Traversal protections: decoded '..' must still be rejected
    for bad_uri in [
        "/folder/%2e%2e",
        "/folder/pixiv%20%26%20twitter/%2e%2e",
        "/image/%2e%2e%2fsecret.png",
        "/media/..%2fsecret.png",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(bad_uri)
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "expected rejection for {bad_uri}"
        );
    }
}

#[tokio::test]
async fn real_preview_generation_and_caching_and_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let art = root.join("art");
    fs::create_dir_all(&art).unwrap();

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
        0xcf, 0xc0, 0x50, 0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fs::write(art.join("cat.png"), TINY_PNG).unwrap();
    fs::write(art.join("photo.heic"), b"fake heic bytes").unwrap();

    let catalog = FsCatalog::new(root).unwrap();
    let previews = CachedPreview::new(root);
    let app = router(catalog, previews, ServeConfig::default());
    let (app, cookie) = login(app).await;

    // 1. Image page has link to original and view rendition
    let page_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/image/art/cat.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_resp.status(), StatusCode::OK);
    let page_html = body(page_resp).await;
    assert!(page_html.contains("href=\"/raw/art/cat.png\""));
    assert!(page_html.contains("src=\"/media/art/cat.png\""));

    // 2. Request thumb rendition -> generated on demand, cached with long cache headers
    let thumb_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/thumb/art/cat.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(thumb_resp.status(), StatusCode::OK);
    assert_eq!(
        thumb_resp.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert!(thumb_resp.headers().contains_key("etag"));

    // Cache entry exists under .dimagine/cache/
    let cache_dir = root.join(".dimagine/cache/previews");
    assert!(cache_dir.is_dir());

    // 3. Request view rendition -> generated on demand with long cache headers
    let view_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/art/cat.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view_resp.status(), StatusCode::OK);
    assert_eq!(
        view_resp.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );

    // 4. Request raw original -> serves original bytes with private, no-cache
    let raw_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/raw/art/cat.png")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(raw_resp.status(), StatusCode::OK);
    assert_eq!(raw_resp.headers()["cache-control"], "private, no-cache");
    assert_eq!(
        to_bytes(raw_resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        TINY_PNG
    );

    // 5. Unsupported format (HEIC) falls back to original image
    let heic_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/thumb/art/photo.heic")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(heic_resp.status(), StatusCode::OK);
    assert_eq!(heic_resp.headers()["cache-control"], "private, no-cache");
    assert_eq!(
        to_bytes(heic_resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        b"fake heic bytes"
    );

    // 6. Verify nothing is written outside .dimagine/cache/
    let dot_dimagine = root.join(".dimagine");
    for entry in fs::read_dir(&dot_dimagine).unwrap() {
        let entry = entry.unwrap();
        assert_eq!(
            entry.file_name(),
            "cache",
            "only cache directory should be created in .dimagine"
        );
    }
}
