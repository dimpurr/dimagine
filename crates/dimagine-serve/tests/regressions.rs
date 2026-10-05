use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{
    router, Catalog, CatalogError, Collection, FsCatalog, ImageDetail, ImageEntry, OriginalPreview,
    ServeConfig, LOGIN_CONCURRENCY_LIMIT, LOGIN_FAILURE_BUDGET,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tower::ServiceExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png-data";

fn app_for(root: &TempDir, mut config: ServeConfig) -> axum::Router {
    // Keep account state inside the test's own directory: no test may read or
    // write the real user state directory.
    config.data_dir = root.path().join("state");
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
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    let app = router(catalog.clone(), OriginalPreview, config);
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
async fn concurrent_wrong_guesses_are_bounded_before_comparison() {
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
            let status = response.status();
            let retry_after = response.headers().get("retry-after").cloned();
            (status, retry_after, bytes(response).await)
        }));
    }
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.unwrap());
    }
    // All four guesses are in flight at once, but only
    // LOGIN_CONCURRENCY_LIMIT may be: the excess is refused before the
    // passcode is compared, so a wave cannot test more passcodes than that.
    let compared = outcomes
        .iter()
        .filter(|(status, _, _)| *status == StatusCode::UNAUTHORIZED)
        .count();
    assert_eq!(compared, LOGIN_CONCURRENCY_LIMIT);
    for (status, retry_after, body) in &outcomes {
        if *status != StatusCode::TOO_MANY_REQUESTS {
            continue;
        }
        assert!(
            retry_after.is_some(),
            "a refused login attempt must say when to retry"
        );
        assert!(body.is_empty(), "a refused attempt reveals nothing");
    }
    // The compared guesses paid the escalating delay, computed before the
    // passcode was read, so a parallel burst is slowed instead of free.
    assert!(start.elapsed() >= std::time::Duration::from_millis(200));
    // The delay is paid before comparing, so a correct passcode is delayed
    // too, and success stays available after the burst (no lockout).
    let start = std::time::Instant::now();
    let cookie = login(&app, "2333").await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(800));
    assert!(cookie.starts_with("dimagine_session="));
}

#[tokio::test(start_paused = true)]
async fn parallel_guess_wave_cannot_brute_force_the_passcode() {
    let root = tempfile::tempdir().unwrap();
    let app = app_for(&root, ServeConfig::default());
    let wave = || {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/x-www-form-urlencoded")
    };
    // The attack: a wave of wrong guesses big enough to cover a whole
    // four-digit keyspace, with the right passcode in the middle of it.
    // The clock is virtual, so the escalating back-off is paid without the
    // test spending real seconds on it.
    let mut tasks = Vec::new();
    for guess in 0..1_000u32 {
        let app = app.clone();
        let request = wave()
            .body(Body::from(format!("passcode={guess:04}")))
            .unwrap();
        tasks.push(tokio::spawn(async move {
            let response = app.oneshot(request).await.unwrap();
            (
                response.status(),
                response.headers().contains_key("retry-after"),
                response.headers().contains_key("set-cookie"),
                bytes(response).await,
            )
        }));
    }
    let app_for_correct = app.clone();
    tasks.push(tokio::spawn(async move {
        let response = app_for_correct
            .oneshot(wave().body(Body::from("passcode=2333")).unwrap())
            .await
            .unwrap();
        (
            response.status(),
            response.headers().contains_key("retry-after"),
            response.headers().contains_key("set-cookie"),
            bytes(response).await,
        )
    }));
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.unwrap());
    }
    // The server compared at most the budgeted number of guesses, no matter
    // how many arrived at once.
    let compared = outcomes
        .iter()
        .filter(|(status, ..)| *status == StatusCode::UNAUTHORIZED)
        .count();
    assert!(
        compared <= LOGIN_FAILURE_BUDGET as usize,
        "{compared} passcodes compared, budget is {LOGIN_FAILURE_BUDGET}"
    );
    let refused: Vec<_> = outcomes
        .iter()
        .filter(|(status, ..)| *status == StatusCode::TOO_MANY_REQUESTS)
        .collect();
    assert_eq!(
        refused.len() + compared,
        outcomes.len(),
        "a guess is either compared or refused, never dropped: {outcomes:?}"
    );
    for (_, carries_retry_after, _, body) in &refused {
        assert!(
            *carries_retry_after,
            "a refused guess must say when to retry"
        );
        assert!(body.is_empty(), "a refused guess reveals nothing");
    }
    // The wave did not sign anybody in: every response came back without a
    // session cookie, including the one that carried the right passcode.
    assert_eq!(outcomes[1_000].0, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        outcomes.iter().all(|(_, _, session, _)| !session),
        "the wave handed out a session cookie"
    );

    // With nothing competing for the two login slots any more, only the
    // exhausted budget can refuse: keep guessing until it does.
    let mut compared = compared;
    let mut budget_ran_out = false;
    for guess in 0..LOGIN_FAILURE_BUDGET * 2 {
        let response = app
            .clone()
            .oneshot(
                wave()
                    .body(Body::from(format!("passcode=9{guess:03}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        match response.status() {
            StatusCode::UNAUTHORIZED => compared += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                assert!(response.headers().get("retry-after").is_some());
                budget_ran_out = true;
                break;
            }
            other => panic!("unexpected login status {other}"),
        }
    }
    assert!(budget_ran_out, "the attempt budget never ran out");
    assert!(
        compared <= LOGIN_FAILURE_BUDGET as usize,
        "{compared} passcodes compared in one window, budget is {LOGIN_FAILURE_BUDGET}"
    );

    // The right passcode is not accepted while the budget is spent: it is
    // refused without being compared, like any other over-budget guess.
    let response = app
        .oneshot(wave().body(Body::from("passcode=2333")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get("retry-after").is_some());
    assert!(response.headers().get("set-cookie").is_none());
    assert!(bytes(response).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn login_budget_is_keyed_on_the_peer_ip_and_ignores_forwarded_for() {
    let root = tempfile::tempdir().unwrap();
    let app = app_for(&root, ServeConfig::default());
    let peer: std::net::SocketAddr = "198.51.100.7:40000".parse().unwrap();
    let guess = |passcode: &str, forwarded: &str| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-forwarded-for", forwarded)
            .body(Body::from(format!("passcode={passcode}")))
            .unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        request
    };
    // A fresh forwarded-for on every guess must not buy a fresh budget: the
    // attempts are keyed on the peer socket IP, which a client cannot change.
    let mut refused = None;
    for attempt in 0..=LOGIN_FAILURE_BUDGET {
        let response = app
            .clone()
            .oneshot(guess("0000", &format!("203.0.113.{attempt}")))
            .await
            .unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(response);
            break;
        }
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(
        refused.is_some(),
        "a spoofed forwarded-for must not reset the peer ip's budget"
    );
    let response = app.oneshot(guess("2333", "203.0.113.250")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().get("set-cookie").is_none());
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
    let state = tempfile::tempdir().unwrap();
    let config = ServeConfig {
        data_dir: state.path().join("state"),
        ..ServeConfig::default()
    };
    let app = router(SlowListingCatalog, OriginalPreview, config);
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

/// Log in from a given TCP peer with extra headers, returning the response.
async fn login_from(
    app: &axum::Router,
    peer: std::net::SocketAddr,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from("passcode=2333")).unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    app.clone().oneshot(request).await.unwrap()
}

fn set_cookie(response: &axum::response::Response) -> String {
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(start_paused = true)]
async fn secure_cookie_follows_forwarded_proto_from_trusted_proxy_only() {
    let root = tempfile::tempdir().unwrap();
    let mut config = ServeConfig {
        passcode: Some("2333".to_owned()),
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    config.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
    let app = app_for(&root, config);
    let proxy: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let direct: std::net::SocketAddr = "198.51.100.7:40000".parse().unwrap();

    // A trusted proxy that terminated TLS: the cookie is Secure even though
    // this request arrived over plaintext HTTP.
    let response = login_from(&app, proxy, &[("x-forwarded-proto", "https")]).await;
    let cookie = set_cookie(&response);
    assert!(cookie.contains("Secure"), "{cookie}");

    // The same proxy on a plaintext leg to this server: no Secure.
    let response = login_from(&app, proxy, &[]).await;
    let cookie = set_cookie(&response);
    assert!(!cookie.contains("Secure"), "{cookie}");

    // A direct client cannot turn the flag on by spoofing the header.
    let response = login_from(&app, direct, &[("x-forwarded-proto", "https")]).await;
    let cookie = set_cookie(&response);
    assert!(!cookie.contains("Secure"), "{cookie}");
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
