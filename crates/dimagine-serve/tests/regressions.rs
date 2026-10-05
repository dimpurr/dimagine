use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{router, Catalog, FsCatalog, OriginalPreview, ServeConfig};
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png-data";

fn app_for(root: &TempDir, config: ServeConfig) -> axum::Router {
    router(
        FsCatalog::new(root.path()).unwrap(),
        OriginalPreview,
        config,
    )
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
async fn bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn sidecar_symlink_is_rejected_and_hidden_paths_are_inaccessible() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("photo.png"), PNG).unwrap();
    fs::write(root.path().join(".hidden.png"), PNG).unwrap();
    fs::create_dir(root.path().join(".hidden")).unwrap();
    fs::write(root.path().join(".hidden/secret.png"), PNG).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root.path().join(".hidden/secret.png"),
        root.path().join("visible-link.png"),
    )
    .unwrap();
    fs::write(root.path().join("outside.md"), "secret note").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        root.path().join("outside.md"),
        root.path().join("photo.png.md"),
    )
    .unwrap();
    let catalog = FsCatalog::new(root.path()).unwrap();
    let app = router(catalog.clone(), OriginalPreview, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    #[cfg(unix)]
    {
        let response = app
            .clone()
            .oneshot(request("/api/image/photo.png", Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    for route in [
        "/media/.hidden.png",
        "/media/.hidden/secret.png",
        "/api/folder/.hidden",
        "/collection/.hidden/set.md",
    ] {
        let response = app
            .clone()
            .oneshot(request(route, Some(&cookie)))
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::OK, "{route}");
    }
    #[cfg(unix)]
    assert!(catalog.resolve_path("visible-link.png").is_err());
}

#[tokio::test]
async fn rendered_markdown_sanitizes_unsafe_schemes_and_keeps_safe_links() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("photo.png"), PNG).unwrap();
    fs::write(root.path().join("photo.png.md"), "[js](javascript:alert(1)) [data](data:text/html,x) [vb](vbscript:alert(1)) [mixed](JaVaScRiPt:alert(1)) [entity](java&#x73;cript:alert(1)) [https](https://example.com) [mail](mailto:a@example.com) [relative](/folder)").unwrap();
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let response = app
        .clone()
        .oneshot(request("/image/photo.png", Some(&cookie)))
        .await
        .unwrap();
    let html = String::from_utf8(bytes(response).await).unwrap();
    assert!(html.contains("https://example.com"));
    assert!(html.contains("mailto:a@example.com"));
    assert!(html.contains("/folder"));
    for unsafe_scheme in ["javascript:", "data:", "vbscript:", "JaVaScRiPt:"] {
        assert!(!html.to_ascii_lowercase().contains(unsafe_scheme));
    }
    assert!(!html.contains("href=\"javascript"));
}

#[tokio::test]
async fn login_throttles_failures_and_success_remains_available() {
    let root = tempfile::tempdir().unwrap();
    let app = app_for(&root, ServeConfig::default());
    let mut wrong = Vec::new();
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("passcode=0000"))
                    .unwrap(),
            )
            .await
            .unwrap();
        wrong.push((response.status(), bytes(response).await));
    }
    assert_eq!(wrong[0], wrong[1]);
    let cookie = login(&app, "2333").await;
    assert!(cookie.starts_with("dimagine_session="));
}

#[tokio::test]
async fn concurrent_wrong_guesses_are_throttled_before_comparison() {
    let root = tempfile::tempdir().unwrap();
    let app = app_for(&root, ServeConfig::default());
    let wrong = || {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("passcode=0000"))
            .unwrap()
    };
    let start = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let response = app.oneshot(wrong()).await.unwrap();
            (response.status(), bytes(response).await)
        }));
    }
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.unwrap());
    }
    // The back-off is computed from the shared failure count before the
    // passcode is compared, so a parallel burst pays the escalating delay
    // (100ms, 200ms, 400ms, 800ms) instead of sleeping only 100ms each.
    assert!(start.elapsed() >= std::time::Duration::from_millis(800));
    assert!(outcomes
        .iter()
        .all(|(status, _)| *status == StatusCode::UNAUTHORIZED));
    assert!(outcomes.iter().all(|(_, body)| *body == outcomes[0].1));
    // The delay is paid before comparing, so a correct passcode is delayed
    // too, and success stays available after failures (no lockout).
    let start = std::time::Instant::now();
    let cookie = login(&app, "2333").await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(1600));
    assert!(cookie.starts_with("dimagine_session="));
}

#[tokio::test]
async fn secure_cookie_and_cache_policies_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let config = ServeConfig {
        https: true,
        ..ServeConfig::default()
    };
    let app = app_for(&root, config);
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
    assert!(response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Secure"));
    assert_eq!(response.headers()["cache-control"], "no-store");
    let login_page = app.clone().oneshot(request("/login", None)).await.unwrap();
    assert_eq!(login_page.headers()["cache-control"], "no-store");
    fs::write(root.path().join("p.png"), PNG).unwrap();
    let cookie = login(&app, "2333").await;
    let response = app
        .oneshot(request("/media/p.png", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "private, no-cache");
}

#[tokio::test]
async fn direct_children_only_and_streamed_original_has_byte_derived_etag_and_mime() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("root.png"), PNG).unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(root.path().join("nested/deep.png"), PNG).unwrap();
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let catalog = FsCatalog::new(root.path()).unwrap();
    assert_eq!(catalog.list_folder("").unwrap().len(), 1);
    let response = app
        .clone()
        .oneshot(request("/media/root.png", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "image/png");
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(bytes(response).await, PNG);
    fs::write(
        root.path().join("root.png"),
        b"\x89PNG\r\n\x1a\nsynthetic-png-datx",
    )
    .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/root.png")
                .header("cookie", &cookie)
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let new_etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/root.png")
                .header("cookie", &cookie)
                .header("if-none-match", format!("W/{new_etag}, \"other\""))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/media/root.png")
                .header("cookie", &cookie)
                .header("if-none-match", "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn front_matter_handles_crlf_reports_errors_and_saphyr_preserves_yaml_values() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("good.png"), PNG).unwrap();
    fs::write(
        root.path().join("good.png.md"),
        "---\r\ntitle: CRLF note\r\nrating: 4\r\ntags: [one, two]\r\n---\r\nBody\r\n",
    )
    .unwrap();
    fs::write(root.path().join("bad.png"), PNG).unwrap();
    fs::write(
        root.path().join("bad.png.md"),
        "---\ntitle: [broken\n---\nBody",
    )
    .unwrap();
    let catalog = FsCatalog::new(root.path()).unwrap();
    let good = catalog.image_detail("good.png").unwrap();
    assert_eq!(good.properties["title"], "CRLF note");
    assert_eq!(good.properties["rating"], 4);
    assert_eq!(good.properties["tags"][1], "two");
    assert_eq!(good.body, "Body\r\n");
    let bad = catalog.image_detail("bad.png").unwrap();
    assert!(bad.front_matter_error.is_some());
    assert_eq!(bad.properties, serde_json::Value::Null);
}

#[tokio::test]
async fn image_note_self_embed_is_not_collection_membership_and_extension_mismatch_is_reported() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("photo.png"), PNG).unwrap();
    fs::write(
        root.path().join("photo.png.md"),
        "---\nkind: collection\n---\n![[photo.png]]",
    )
    .unwrap();
    fs::write(root.path().join("misnamed.jpg"), PNG).unwrap();
    let catalog = FsCatalog::new(root.path()).unwrap();
    assert!(catalog
        .collection("photo.png.md")
        .unwrap()
        .members
        .is_empty());
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let response = app
        .oneshot(request("/media/misnamed.jpg", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["x-dimagine-extension-mismatch"], "true");
}
