use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{router, Catalog, FsCatalog, OriginalPreview, ServeConfig};
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
