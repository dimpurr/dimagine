use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_preview::{sha256_file, RENDITION_GENERATION};
use dimagine_serve::{
    router, CachedPreview, Catalog, CatalogError, Collection, CollectionPage, FsCatalog,
    ImageDetail, ImageEntry, OriginalPreview, PreviewKind, PreviewProvider, ServeConfig,
    LOGIN_CONCURRENCY_LIMIT, LOGIN_FAILURE_BUDGET,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
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
        // The hint is the worst case, not a best case: a busy
        // slot holds an attempt for the whole escalating delay
        // and the argon2 check that follows it, so the refusal
        // rounds the whole hold up to the next whole second
        // (RW19b review Low; RW54 review Low #4 — the delay
        // alone let a client that came back at its end be
        // refused once more). The value is pinned as one
        // literal: lowering the delay below what a busy slot
        // can impose — or dropping the rounding — turns this
        // red (RW54 review Low #3).
        let retry = retry_after.as_ref().and_then(|value| value.to_str().ok());
        assert_eq!(
            retry,
            Some("6"),
            "a refused attempt waits out the longest slot hold"
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

/// A catalog whose listing is slow, so the admission bound has something to
/// bound. Its root is a real temporary library because the viewer builds its
/// index there when the router is created.
struct SlowListingCatalog {
    #[allow(dead_code)]
    root: TempDir,
}

impl SlowListingCatalog {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }
}

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
    fn collection_page(&self, _path: &str) -> Result<CollectionPage, CatalogError> {
        Err(CatalogError::NotFound)
    }
    fn image_detail(&self, _path: &str) -> Result<ImageDetail, CatalogError> {
        Err(CatalogError::NotFound)
    }
    fn resolve_path(&self, _path: &str) -> Result<PathBuf, CatalogError> {
        Err(CatalogError::Forbidden)
    }
    fn root(&self) -> &Path {
        self.root.path()
    }
}

/// The bound is exercised on the folder listing, which walks the library and
/// is the expensive kind of request the limit exists for. The library grid now
/// reads the index, so it is fast enough not to need bounding.
#[tokio::test]
async fn over_bound_requests_get_503_with_retry_after() {
    let state = tempfile::tempdir().unwrap();
    let config = ServeConfig {
        data_dir: state.path().join("state"),
        ..ServeConfig::default()
    };
    let app = router(SlowListingCatalog::new(), OriginalPreview, config);
    let cookie = login(&app, "2333").await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let app = app.clone();
        let cookie = cookie.clone();
        tasks.push(tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/folder")
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

/// The ETag of a response whose URL cannot promise its own content is the
/// SHA-256 of the very bytes it streams, hashed from the handle that is
/// streamed. A rendition *request* by path is a hop to its content URL now
/// (ADR-020), whose ETag is the URL's own facts instead, so this is pinned
/// where bytes are still served under a path: `/raw/`.
#[tokio::test]
async fn etag_and_streamed_bytes_come_from_the_same_opened_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    let first = app
        .clone()
        .oneshot(request("/raw/pic.png", Some(&cookie)))
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
                .uri("/raw/pic.png")
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
/// The streamed *original*: its MIME comes from its bytes, its ETag from a hash
/// of them, and a replacement under the same path does not validate against the
/// old one. `/raw/` is where the bytes as written are served — a rendition
/// request by path is a hop to its content URL (ADR-020), whose answer is the
/// rendition and whose ETag is the URL's own facts.
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
        .oneshot(request("/raw/root.png", Some(&cookie)))
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
                .uri("/raw/root.png")
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
                .uri("/raw/root.png")
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
                .uri("/raw/root.png")
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
/// A file whose bytes and whose extension disagree is reported as the mismatch
/// it is, by the route that serves the file as written (`/raw/`; a rendition by
/// path is a hop to its content URL, ADR-020).
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
        .oneshot(request("/raw/misnamed.jpg", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["x-dimagine-extension-mismatch"], "true");
}

/* ============================== W70: admission classes + client retries */

// A real 1x1 PNG: the preview generator must be able to decode it, so
// warm-ups really produce rendition files on disk (the fake synths the
// other tests use would make every /thumb a generation-then-fallback).
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xcf, 0xc0, 0x50,
    0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A gate preview rendering waits behind. Closed, every image request
/// that must generate parks inside `preview_path` — holding its
/// admission permit exactly as a real preview generation does. The
/// parks are on a `Condvar` inside `spawn_blocking`, so reopening the
/// gate drains the parked requests instead of cancelling them.
struct Gate {
    open: Mutex<bool>,
    resumed: Condvar,
}

impl Gate {
    fn new(open: bool) -> Self {
        Self {
            open: Mutex::new(open),
            resumed: Condvar::new(),
        }
    }

    fn close(&self) {
        *self.open.lock().unwrap() = false;
    }

    fn reopen(&self) {
        let mut open = self.open.lock().unwrap();
        *open = true;
        self.resumed.notify_all();
    }

    fn wait_open(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.resumed.wait(open).unwrap();
        }
    }
}

/// [`CachedPreview`] with its heavy half parked on a [`Gate`]: the
/// probe (`existing_preview`) stays the real one, so the light class
/// has real renditions to find — and a misclassified thumbnail becomes
/// fatal, because it parks behind the eight heavy permits for its
/// whole 10 s wait and comes back a 503.
struct GatedCachedPreview {
    inner: CachedPreview,
    gate: Arc<Gate>,
}

impl PreviewProvider for GatedCachedPreview {
    fn preview_path(&self, image: &str, kind: PreviewKind) -> Option<PathBuf> {
        self.gate.wait_open();
        self.inner.preview_path(image, kind)
    }

    fn existing_preview(&self, hash: &str, kind: PreviewKind) -> Option<PathBuf> {
        // A *view* rendition is never "already on disk" as far as this test is
        // concerned, so a view request always has to generate — and parks. The
        // thumbnails are the real cache's answer, which is what makes the
        // grid's storm a set of light hits. Without this the background preview
        // backfill (ADR-020 item 6), which runs at router build, would have
        // built the very rendition the eight parked requests are supposed to be
        // generating.
        match kind {
            PreviewKind::View => None,
            PreviewKind::Thumb => self.inner.existing_preview(hash, kind),
        }
    }
}

/// [`OriginalPreview`]'s shape (the original *is* the rendition) gated
/// the same way, for the tests that only need the heavy class busy:
/// every request generates, so every request is heavy.
struct GatedPreview {
    root: PathBuf,
    gate: Arc<Gate>,
}

impl PreviewProvider for GatedPreview {
    fn preview_path(&self, image: &str, _kind: PreviewKind) -> Option<PathBuf> {
        self.gate.wait_open();
        Some(self.root.join(image))
    }
}

/// The fix's own headline (W70): a grid's worth of thumbnails whose
/// previews are already on disk answers while every heavy permit sits
/// on a preview generation. Before the fix the ninth request onward
/// got an immediate 503 and `<img>`'s one-error handling marked the
/// tiles "Unavailable" — different ones on every reload, the previews
/// undamaged the whole time.
#[tokio::test]
async fn w70_a_grid_of_cached_thumbnails_answers_while_every_heavy_permit_is_busy() {
    const IMAGES: usize = 200;
    let root = tempfile::tempdir().unwrap();
    for index in 0..IMAGES {
        fs::write(root.path().join(format!("img-{index:03}.png")), TINY_PNG).unwrap();
    }
    let catalog = FsCatalog::new(root.path()).unwrap();
    let gate = Arc::new(Gate::new(true));
    let previews = GatedCachedPreview {
        inner: CachedPreview::new(root.path()),
        gate: gate.clone(),
    };
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    let app = router(catalog, previews, config);
    let cookie = login(&app, "2333").await;

    // Every image here holds the same bytes, so the grid asks one address —
    // and a tile's address is its content, not its path (ADR-020). The
    // rendition is warmed once: it now exists on disk.
    let digest = sha256_file(root.path().join("img-000.png")).unwrap();
    let thumb = format!("/p/{digest}/thumb.{RENDITION_GENERATION}");
    let view = format!("/p/{digest}/view.{RENDITION_GENERATION}");
    let response = app
        .clone()
        .oneshot(request(&thumb, Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Park every heavy permit: eight view renditions that do not exist
    // yet, each generating behind a closed gate.
    gate.close();
    let mut heavy = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let cookie = cookie.clone();
        let view = view.clone();
        heavy.push(tokio::spawn(async move {
            let response = app.oneshot(request(&view, Some(&cookie))).await.unwrap();
            response.status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The grid: two hundred thumbnails at once. Every one must answer
    // — the light class never queues behind a preview generator — and
    // none may come back a 503.
    let started = Instant::now();
    let mut storm = Vec::new();
    for _ in 0..IMAGES {
        let app = app.clone();
        let cookie = cookie.clone();
        let thumb = thumb.clone();
        storm.push(tokio::spawn(async move {
            let response = app.oneshot(request(&thumb, Some(&cookie))).await.unwrap();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            (response.status(), retry_after)
        }));
    }
    let mut answered = 0;
    let mut refused = 0;
    for task in storm {
        let (status, retry_after) = task.await.unwrap();
        match status {
            StatusCode::OK => {
                assert!(retry_after.is_none());
                answered += 1;
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                refused += 1;
                if let Some(hint) = retry_after {
                    eprintln!("a tile got the busy-server 503 with Retry-After: {hint}");
                }
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(refused, 0, "{refused} tiles got the busy-server 503");
    assert_eq!(answered, IMAGES);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered within the light class's own wait, not the heavy budget: {:?}",
        started.elapsed()
    );

    // Let the parked generations finish and confirm they all serve.
    gate.reopen();
    for task in heavy {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}

/// W70's heavy class, end to end: over the serving bound, requests
/// queue and are answered within the wait; past the queue's own bound,
/// the next request is refused without queueing at all, and the
/// refusal carries an honest `Retry-After` — the whole wait budget a
/// fresh attempt could still face, as the literal the test pins.
#[tokio::test]
async fn w70_heavy_queue_answers_within_the_wait_and_refuses_the_next_beyond_it() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("img.png"), PNG).unwrap();
    let catalog = FsCatalog::new(root.path()).unwrap();
    let gate = Arc::new(Gate::new(false));
    let previews = GatedPreview {
        root: root.path().to_path_buf(),
        gate: gate.clone(),
    };
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    let app = router(catalog, previews, config);
    let cookie = login(&app, "2333").await;

    // The address a page asks, which is the image's content (ADR-020). Every
    // request below has to generate — the provider reports no rendition on
    // disk — so every one of them is heavy.
    let digest = sha256_file(root.path().join("img.png")).unwrap();
    let url = format!("/p/{digest}/thumb.{RENDITION_GENERATION}");

    // Eight requests hold the permits (parked inside generation) and
    // a full queue waits behind them: 8 + 256, every one of them
    // within its wait budget.
    let serving = 8;
    let queue = 256;
    let mut parked = Vec::new();
    for _ in 0..serving + queue {
        let app = app.clone();
        let cookie = cookie.clone();
        let url = url.clone();
        parked.push(tokio::spawn(async move {
            let response = app.oneshot(request(&url, Some(&cookie))).await.unwrap();
            let status = response.status();
            let _ = to_bytes(response.into_body(), usize::MAX).await;
            status
        }));
    }
    // Give the whole wave time to reach the queue; the parked holders
    // cannot drain on their own, so the queue stays full until the
    // gate opens.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Request 8 + 256 + 1: refused without taking a queue place, and
    // honestly told to come back after the whole wait budget (the
    // literal, so lowering the constant fails this test — the RW54
    // lesson).
    let started = Instant::now();
    let probe = app
        .clone()
        .oneshot(request(&url, Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(probe.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        probe.headers()["retry-after"].to_str().unwrap(),
        "10",
        "the whole heavy wait budget, never a best case"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "refused without queueing: {:?}",
        started.elapsed()
    );

    // Reopen: the eight holders and the whole queue answer — every
    // waiter was served within the wait it entered (the drain is
    // millisecond-sized once the gate opens, and ~1.5 s of the ten
    // are already spent).
    gate.reopen();
    let mut served = 0;
    for task in parked {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
        served += 1;
    }
    assert_eq!(served, serving + queue);
}

/// The retry parameter the client (!r=N) adds is the client's own
/// bookkeeping: the routes answer the same rendition the same way with
/// or without it — including the terminal 404 a probe treats as
/// "never again" (W70: no retry loop on 404).
#[tokio::test]
async fn w70_the_retry_query_parameter_the_client_adds_is_ignored_by_the_media_routes() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("cat.png"), PNG).unwrap();
    let app = app_for(&root, ServeConfig::default());
    let cookie = login(&app, "2333").await;
    // The address a tile asks, and the one it retries: its content (ADR-020).
    let digest = sha256_file(root.path().join("cat.png")).unwrap();
    let url = format!("/p/{digest}/thumb.{RENDITION_GENERATION}");
    let base = app
        .clone()
        .oneshot(request(&url, Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(base.status(), StatusCode::OK);
    let base_etag = base.headers()["etag"].to_str().unwrap().to_owned();
    let base_bytes = bytes(base).await;

    for attempt in [1, 2, 3] {
        let response = app
            .clone()
            .oneshot(request(&format!("{url}?r={attempt}"), Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["etag"].to_str().unwrap(), base_etag);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            base_bytes
        );
    }

    // A missing image stays the one answer a retry cannot change.
    let missing = app
        .oneshot(request("/thumb/absent.png?r=1", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(missing.headers().get("retry-after").is_none());
}

/// "Load more" appended no tile with an ordinary shape: `tileFit` answers
/// `normal` for one, and the badge table only has the cropped shapes, so the
/// lookup threw and the button fell back to reloading the page. There is no
/// JavaScript harness in this repository, so the pin is on the script's bytes:
/// the badge is asked for only for the two shapes that have one.
#[test]
fn an_appended_tile_asks_for_a_badge_only_for_a_cropped_shape() {
    let script = include_str!("../src/assets/app.js");
    assert!(script.contains("function tileFit"));
    assert!(script.contains("return 'normal';"));
    assert!(
        script
            .contains("if (fit === 'tall' || fit === 'wide') tile.appendChild(badgeElement(fit));"),
        "tileElement must not ask TILE_BADGE for the `normal` shape"
    );
    assert!(!script.contains("        tile.appendChild(badgeElement(fit));\n      }"));
}
