//! Account and first-run setup flow tests (ADR-014).
//!
//! These drive the router directly and keep every account file in the test's
//! own temporary state directory, so no real user state is ever read or
//! written.

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use dimagine_serve::{
    accounts::AccountsStore, router, router_from, FsCatalog, OriginalPreview, ServeConfig,
    LOGIN_FAILURE_BUDGET,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn app_with(root: &TempDir, setup_code: Option<&str>, passcode: Option<&str>) -> axum::Router {
    let config = ServeConfig {
        passcode: passcode.map(str::to_owned),
        data_dir: root.path().join("state"),
        setup_code: setup_code.map(str::to_owned),
        ..ServeConfig::default()
    };
    router(
        FsCatalog::new(root.path()).unwrap(),
        OriginalPreview,
        config,
    )
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

async fn body_text(app: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let (status, _, body) = send(app, request).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn session_cookie(headers: &HeaderMap) -> String {
    headers
        .get("set-cookie")
        .expect("login sets a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn create_owner(root: &TempDir, email: &str, password: &str) {
    let store = AccountsStore::new(&root.path().join("state"));
    store.create_user(email, password, "owner").unwrap();
}

#[tokio::test]
async fn first_run_setup_flow_wrong_code_refused_and_code_single_use() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("setup-code-abcdef"), None);

    // Every page redirects to /setup while no account exists.
    let (status, headers, _) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/setup");
    let api = send(&app, get("/api/folder", None)).await;
    assert_eq!(api.0, StatusCode::SEE_OTHER);

    let (status, html) = body_text(&app, get("/setup", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Initial Setup"), "{html}");

    // A wrong one-time code is refused and creates nothing.
    let (status, _) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456&setup_code=wrong-code",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.path().join("state/accounts.json").exists());

    // Mismatched passwords are refused even with the right code.
    let (status, _) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=other-password&setup_code=setup-code-abcdef",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The right code creates the owner and signs them in.
    let (status, headers, _) = send(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
    let cookie = session_cookie(&headers);

    let (status, _) = body_text(&app, get("/", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);

    // The setup page is gone for good, and the code is dead.
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=second%40example.com&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef",
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // The store holds exactly one owner with a mode-0600 file.
    let store = AccountsStore::new(&root.path().join("state"));
    let doc = store.load().unwrap();
    assert_eq!(doc.users.len(), 1);
    assert_eq!(doc.users[0].email, "owner@example.com");
    assert_eq!(doc.users[0].role, "owner");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.file_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "accounts.json must be 0600, got {mode:o}");
    }
}

#[tokio::test]
async fn setup_refuses_short_and_empty_passwords() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("setup-code-abcdef"), None);

    // The form tells the operator about the minimum up front.
    let (status, html) = body_text(&app, get("/setup", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("minlength=\"12\""), "{html}");

    // A password below the minimum is refused with a clear message, even
    // when both fields match and the setup code is valid.
    let (status, html) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=short&confirm_password=short&setup_code=setup-code-abcdef",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains("at least 12 characters"), "{html}");
    assert!(!root.path().join("state/accounts.json").exists());

    // An empty password is refused too (the two empty fields match).
    let (status, _) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=&confirm_password=&setup_code=setup-code-abcdef",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.path().join("state/accounts.json").exists());

    // A password of exactly 12 characters is accepted.
    let (status, headers, _) = send(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
}

#[tokio::test]
async fn setup_is_gone_after_restart_and_once_a_user_exists() {
    let root = tempfile::tempdir().unwrap();
    create_owner(&root, "owner@example.com", "secret123");

    // A fresh process (a new router) with the same data dir: no setup code is
    // generated and /setup is a 404.
    let app = app_with(&root, Some("would-be-code"), None);
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=evil%40example.com&password=secret123&confirm_password=secret123&setup_code=would-be-code",
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Anonymous pages now go to /login, not /setup.
    let (status, headers, _) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");
}

#[tokio::test]
async fn account_login_success_failure_and_logout() {
    let root = tempfile::tempdir().unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_with(&root, None, None);

    let (status, html) = body_text(&app, get("/login", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Email"), "{html}");

    // Unknown email and wrong password are both refused without a session.
    let (status, _, _) = send(
        &app,
        post("/login", "email=owner%40example.com&password=wrong"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(
        &app,
        post("/login", "email=nobody%40example.com&password=secret123"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The right password signs in and the session reaches the library.
    let (status, headers, _) = send(
        &app,
        post("/login", "email=OWNER%40example.com&password=secret123"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = session_cookie(&headers);
    assert!(cookie.starts_with("dimagine_session="), "{cookie}");
    assert_eq!(send(&app, get("/", Some(&cookie))).await.0, StatusCode::OK);

    // Logout clears the session: the old cookie is worthless afterwards.
    let (status, headers, _) = send(&app, get("/logout", Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");
    let cleared = headers["set-cookie"].to_str().unwrap();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let (status, headers, _) = send(&app, get("/", Some(&cookie))).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");
}

#[tokio::test(start_paused = true)]
async fn account_login_throttling_still_applies() {
    let root = tempfile::tempdir().unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_with(&root, None, None);
    let peer: SocketAddr = "203.0.113.7:40000".parse().unwrap();

    let wrong = || {
        let mut request = post("/login", "email=owner%40example.com&password=wrong");
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    };

    // The first budget's worth of guesses are compared and refused...
    for _ in 0..LOGIN_FAILURE_BUDGET {
        assert_eq!(send(&app, wrong()).await.0, StatusCode::UNAUTHORIZED);
    }
    // ...and then even the correct password is refused before it is compared.
    let mut correct = post("/login", "email=owner%40example.com&password=secret123");
    correct.extensions_mut().insert(ConnectInfo(peer));
    let (status, headers, body) = send(&app, correct).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
    assert!(body.is_empty());
}

#[tokio::test]
async fn passcode_compat_mode_when_no_user_and_passcode_set() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("ignored-setup-code"), Some("open-sesame"));

    // Passcode mode wins: no setup page, and the login form asks for a passcode.
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, html) = body_text(&app, get("/login", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Passcode"), "{html}");

    let (status, _, _) = send(&app, post("/login", "passcode=wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, _) = send(&app, post("/login", "passcode=open-sesame")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = session_cookie(&headers);
    assert_eq!(send(&app, get("/", Some(&cookie))).await.0, StatusCode::OK);

    // No account file was created by passcode mode.
    assert!(!root.path().join("state/accounts.json").exists());
}

#[tokio::test(start_paused = true)]
async fn untrusted_peer_ignores_spoofed_forwarded_for() {
    let root = tempfile::tempdir().unwrap();
    // No trusted proxies configured: the TCP peer is the only key.
    let app = app_with(&root, None, Some("open-sesame"));
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();

    for i in 0..LOGIN_FAILURE_BUDGET {
        let mut request = post("/login", "passcode=0000");
        request
            .headers_mut()
            .insert("x-forwarded-for", format!("203.0.113.{i}").parse().unwrap());
        request.extensions_mut().insert(ConnectInfo(peer));
        assert_eq!(send(&app, request).await.0, StatusCode::UNAUTHORIZED);
    }
    // A brand-new spoofed hop does not buy a fresh budget: the peer IP is
    // still the key, so the next attempt is refused.
    let mut request = post("/login", "passcode=0000");
    request
        .headers_mut()
        .insert("x-forwarded-for", "203.0.113.250".parse().unwrap());
    request.extensions_mut().insert(ConnectInfo(peer));
    assert_eq!(send(&app, request).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn malformed_and_unknown_schema_stores_are_errors_not_empty_stores() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let store = AccountsStore::new(&state);

    // Truncated mid-record.
    std::fs::write(
        store.file_path(),
        r#"{"schema":1,"users":[{"id":"01J9XEXAMPLEULID0000000000","#,
    )
    .unwrap();
    let err = store.load().unwrap_err();
    assert!(err.to_string().contains("malformed accounts.json"), "{err}");
    assert!(store.has_users().is_err());

    // Empty file.
    std::fs::write(store.file_path(), "").unwrap();
    assert!(store.load().is_err());
    assert!(store.has_users().is_err());

    // Unknown schema version.
    std::fs::write(store.file_path(), r#"{"schema":999,"users":[]}"#).unwrap();
    let err = store.load().unwrap_err();
    assert!(err.to_string().contains("schema 999"), "{err}");
    assert!(store.has_users().is_err());

    // A readable, current-schema store still loads and reports its users.
    std::fs::write(store.file_path(), r#"{"schema":1,"users":[]}"#).unwrap();
    assert!(!store.has_users().unwrap());
    let user = store.create_user("owner@example.com", "secret123", "owner");
    assert!(user.is_ok());
    assert!(store.has_users().unwrap());
}

#[cfg(unix)]
#[test]
fn unreadable_store_is_an_error_not_an_empty_store() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses file permissions; skipping keeps the test honest there.
    if running_as_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let store = AccountsStore::new(&state);
    std::fs::write(store.file_path(), r#"{"schema":1,"users":[]}"#).unwrap();
    std::fs::set_permissions(store.file_path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(store.load().is_err());
    let err = store.has_users().unwrap_err();
    assert!(
        err.to_string().contains("account storage I/O error"),
        "{err}"
    );
    let _ = std::fs::set_permissions(store.file_path(), std::fs::Permissions::from_mode(0o600));
}

fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Build the router the way the CLI does, so a store that cannot be read is a
/// returned error instead of a panic.
fn try_router(root: &TempDir) -> Result<axum::Router, dimagine_serve::accounts::AccountsError> {
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    router_from(
        Arc::new(FsCatalog::new(root.path()).unwrap()),
        Arc::new(OriginalPreview),
        config,
    )
}

#[tokio::test]
async fn a_store_without_the_users_key_is_an_error_not_a_first_run() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    // The `users` key renamed by another build: the file is present, so this
    // is unambiguously not a first run, but it must not read as "no users".
    std::fs::write(
        state.join("accounts.json"),
        r#"{"schema":1,"accts":[{"id":"01J9XEXAMPLEULID0000000000","email":"owner@example.com","password_hash":"$argon2id$v=19$dummy","role":"owner","created":"2026-10-05T12:00:00Z"}]}"#,
    )
    .unwrap();

    let err = AccountsStore::new(&state).has_users().unwrap_err();
    let text = err.to_string();
    assert!(text.contains("malformed accounts.json"), "{text}");
    assert!(
        text.contains("users"),
        "the error must name the key: {text}"
    );

    // Building the viewer refuses outright: no router, so no setup page and no
    // one-time code.
    let err = try_router(&root).unwrap_err();
    assert!(err.to_string().contains("users"), "{err}");
}

#[tokio::test]
async fn a_store_that_loses_its_users_key_while_running_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("setup-code-abcdef"), None);
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::OK,
        "the store is absent, so this is a first run"
    );

    // The file appears with no `users` key under the live server.
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("accounts.json"), r#"{"schema":1}"#).unwrap();

    // The setup form is never rendered and the code never creates an account.
    for request in [
        get("/setup", None),
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef",
        ),
    ] {
        let (status, html) = body_text(&app, request).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!html.contains("Initial Setup"), "{html}");
        assert!(!html.contains("password="), "{html}");
    }
    let store = AccountsStore::new(&state);
    assert!(store.load().is_err());
    assert_eq!(
        std::fs::read_to_string(store.file_path()).unwrap(),
        r#"{"schema":1}"#,
        "a refused setup must not rewrite the store"
    );
}

#[test]
fn unknown_top_level_fields_still_load_and_survive_a_write() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let store = AccountsStore::new(&state);
    std::fs::write(
        store.file_path(),
        r#"{"schema":1,"server_note":"keep me","users":[]}"#,
    )
    .unwrap();

    // Requiring `users` must not turn into rejecting unknown fields: the file
    // loads with its extra top-level key intact.
    let doc = store.load().unwrap();
    assert!(doc.users.is_empty());
    assert_eq!(
        doc.extra.get("server_note").unwrap(),
        &serde_json::Value::String("keep me".into())
    );

    // A write keeps it.
    store
        .create_user("owner@example.com", "secret123", "owner")
        .unwrap();
    let raw = std::fs::read_to_string(store.file_path()).unwrap();
    assert!(raw.contains("server_note"), "{raw}");
    assert!(raw.contains("owner@example.com"), "{raw}");
}

#[test]
fn an_absent_accounts_file_is_a_first_run_and_opens_setup() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    assert!(!state.exists());

    // No file at all is the one shape that still means "no users yet".
    let store = AccountsStore::new(&state);
    assert!(!store.has_users().unwrap());
    assert!(try_router(&root).is_ok());
}

#[tokio::test]
async fn a_first_run_without_an_accounts_file_opens_the_setup_page() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("setup-code-abcdef"), None);
    assert!(!root.path().join("state/accounts.json").exists());

    let (status, headers, _) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/setup");
    let (status, html) = body_text(&app, get("/setup", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Initial Setup"), "{html}");
    assert!(!root.path().join("state/accounts.json").exists());
}

#[cfg(unix)]
#[test]
fn save_creates_data_dir_with_0700() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let store = AccountsStore::new(&state);
    assert!(!state.exists());

    store
        .create_user("owner@example.com", "secret123", "owner")
        .unwrap();

    let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the data dir must be 0700, got {mode:o}");
}

#[cfg(unix)]
#[test]
fn permissions_warning_reports_group_or_world_readable_store() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let store = AccountsStore::new(&state);

    // No file: no warning.
    assert!(store.permissions_warning().is_none());

    // 0600: no warning.
    std::fs::write(store.file_path(), r#"{"schema":1,"users":[]}"#).unwrap();
    std::fs::set_permissions(store.file_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(store.permissions_warning().is_none());

    // Group- or world-readable: warning naming the mode.
    for mode in [0o640, 0o604, 0o644] {
        std::fs::set_permissions(store.file_path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let warning = store
            .permissions_warning()
            .unwrap_or_else(|| panic!("mode {mode:o} must warn"));
        assert!(warning.contains(&format!("{mode:04o}")), "{warning}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_setup_requests_consume_the_code_only_once() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, Some("setup-code-abcdef"), None);

    let request = |email: &str| {
        post(
            "/setup",
            &format!(
                "email={email}&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef"
            ),
        )
    };

    // Two requests race the one-time code; exactly one may complete setup.
    let app_a = app.clone();
    let app_b = app.clone();
    let task_a =
        tokio::spawn(async move { app_a.oneshot(request("owner%40example.com")).await.unwrap() });
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(request("second%40example.com"))
            .await
            .unwrap()
    });
    let (response_a, response_b) = (task_a.await.unwrap(), task_b.await.unwrap());

    let outcomes = [
        (response_a.status(), response_a.headers().clone()),
        (response_b.status(), response_b.headers().clone()),
    ];
    let winners: Vec<_> = outcomes
        .iter()
        .filter(|(status, headers)| *status == StatusCode::SEE_OTHER && headers["location"] == "/")
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one request may complete setup: {outcomes:?}"
    );
    for (status, headers) in &outcomes {
        if *status == StatusCode::SEE_OTHER && headers["location"] == "/" {
            continue;
        }
        // The loser is refused: 404 because the code is gone (or setup is
        // unreachable now that a user exists).
        assert_eq!(
            *status,
            StatusCode::NOT_FOUND,
            "the losing request must be refused: {status} {headers:?}"
        );
    }

    // The store holds exactly one owner — the second account was not created
    // and the first was not overwritten.
    let store = AccountsStore::new(&root.path().join("state"));
    let doc = store.load().unwrap();
    assert_eq!(doc.users.len(), 1, "{}", {
        let emails: Vec<_> = doc.users.iter().map(|u| u.email.as_str()).collect();
        emails.join(", ")
    });
    assert_eq!(doc.users[0].role, "owner");

    // The code is dead: a follow-up request with the same code is refused.
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=third%40example.com&password=secret123456&confirm_password=secret123456&setup_code=setup-code-abcdef",
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn trusted_proxy_keys_throttle_on_last_forwarded_hop() {
    let root = tempfile::tempdir().unwrap();
    let mut config = ServeConfig {
        passcode: Some("open-sesame".to_owned()),
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    config.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
    let app = router(
        FsCatalog::new(root.path()).unwrap(),
        OriginalPreview,
        config,
    );

    let trusted: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let guess = |xff: &str, peer: SocketAddr| {
        let mut request = post("/login", "passcode=0000");
        request
            .headers_mut()
            .insert("x-forwarded-for", xff.parse().unwrap());
        request.extensions_mut().insert(ConnectInfo(peer));
        request
    };

    // Exhaust the budget keyed on the last hop of the header.
    for _ in 0..LOGIN_FAILURE_BUDGET {
        assert_eq!(
            send(&app, guess("10.0.0.1, 203.0.113.9", trusted)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        send(&app, guess("10.0.0.1, 203.0.113.9", trusted)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A different forwarded client has its own budget.
    assert_eq!(
        send(&app, guess("10.0.0.1, 203.0.113.10", trusted)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // An untrusted peer's header is ignored, so its key is the peer IP.
    let untrusted: SocketAddr = "198.51.100.7:40000".parse().unwrap();
    assert_eq!(
        send(&app, guess("203.0.113.9", untrusted)).await.0,
        StatusCode::UNAUTHORIZED
    );
}
