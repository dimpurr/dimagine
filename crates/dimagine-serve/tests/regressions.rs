use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{
    router, Catalog, CatalogError, Collection, FsCatalog, ImageDetail, ImageEntry, OriginalPreview,
    ServeConfig,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
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

struct SlowListingCatalog;

impl Catalog for SlowListingCatalog {
    fn list_folder(&self, _folder: &str) -> Result<Vec<ImageEntry>, CatalogError> {
        std::thread::sleep(std::time::Duration::from_millis(600));
        Ok(Vec::new())
    }
    fn list_subfolders(&self, _folder: &str) -> Result<Vec<String>, CatalogError> {
        Ok(Vec::new())
    }
    fn list_collections(&self, _folder: &str) -> Result<Vec<Collection>, CatalogError> {
        Ok(Vec::new())
    }
    fn collection(&self, _path: &str) -> Result<Collection, CatalogError> {
        Err(CatalogError::NotFound)
    }
    fn image_detail(&self, _path: &str) -> Result<ImageDetail, CatalogError> {
        Err(CatalogError::NotFound)
    }
    fn resolve_path(&self, _path: &str) -> Result<PathBuf, CatalogError> {
        Err(CatalogError::Forbidden)
    }
    fn root(&self) -> &Path {
        Path::new(".")
    }
}

#[tokio::test]
async fn over_bound_requests_get_503_with_retry_after() {
    let app = router(SlowListingCatalog, OriginalPreview, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let app = app.clone();
        let cookie = cookie.clone();
        tasks.push(tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header("cookie", cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            (response.status(), retry_after)
        }));
    }
    let mut ok = 0;
    let mut denied = 0;
    for task in tasks {
        let (status, retry_after) = task.await.unwrap();
        match status {
            StatusCode::OK => {
                ok += 1;
                assert!(retry_after.is_none());
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                denied += 1;
                assert_eq!(retry_after.as_deref(), Some("1"));
            }
            other => panic!("unexpected status {other}"),
        }
    }
    // The admission bound (8) is respected exactly: 8 listings run
    // concurrently and the rest are rejected immediately instead of
    // queueing without a limit.
    assert_eq!(ok, 8);
    assert_eq!(denied, 4);
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn etag_and_streamed_bytes_come_from_the_same_opened_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let first = app
        .clone()
        .oneshot(request("/media/pic.png", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_etag = first.headers()["etag"].to_str().unwrap().to_owned();
    let first_bytes = bytes(first).await;
    // The digest is computed from the very handle that is streamed.
    assert_eq!(hex_sha256(&first_bytes), first_etag.trim_matches('"'));

    // Replace the file between requests: the stale digest must not
    // validate, and the new response must hash and stream the
    // replacement from one open.
    let replaced = b"\x89PNG\r\n\x1a\nreplaced-bytes";
    fs::write(root.path().join("pic.png"), replaced).unwrap();
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/pic.png")
                .header("cookie", &cookie)
                .header("if-none-match", &first_etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_etag = second.headers()["etag"].to_str().unwrap().to_owned();
    let second_bytes = bytes(second).await;
    assert_ne!(second_etag, first_etag);
    assert_eq!(second_bytes, replaced);
    assert_eq!(hex_sha256(&second_bytes), second_etag.trim_matches('"'));
}

#[tokio::test]
async fn etag_stays_consistent_while_the_file_is_swapped_concurrently() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("race.png");
    fs::write(&target, PNG).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let target = target.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut flip = false;
            while !stop.load(Ordering::SeqCst) {
                // Atomic replacement, like a cache regeneration:
                // readers that opened the old inode keep it.
                let staging = target.with_extension("swp");
                let bytes = if flip {
                    PNG
                } else {
                    b"\x89PNG\r\n\x1a\nswapped"
                };
                if fs::write(&staging, bytes).is_ok() {
                    let _ = fs::rename(&staging, &target);
                }
                flip = !flip;
            }
        })
    };
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let mut served = 0;
    for _ in 0..50 {
        let response = app
            .clone()
            .oneshot(request("/media/race.png", Some(&cookie)))
            .await
            .unwrap();
        if response.status() != StatusCode::OK {
            continue;
        }
        let etag = response.headers()["etag"]
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_owned();
        let body = bytes(response).await;
        // A hash-then-reopen implementation would sometimes pair the
        // old digest with the new bytes (or vice versa).
        assert_eq!(hex_sha256(&body), etag);
        served += 1;
    }
    assert!(served > 0);
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
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
