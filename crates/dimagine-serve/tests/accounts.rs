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
    accounts::AccountsStore, remind_until_owner_exists, router, router_from, AuthMode, FsCatalog,
    OriginalPreview, ServeConfig, LOGIN_FAILURE_BUDGET, NO_LOGIN_BANNER, NO_OWNER_REMINDER,
    NO_OWNER_REMINDER_INTERVAL,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

fn app_for(root: &TempDir, passcode: Option<&str>, auth: AuthMode) -> axum::Router {
    let config = ServeConfig {
        passcode: passcode.map(str::to_owned),
        data_dir: root.path().join("state"),
        auth,
        ..ServeConfig::default()
    };
    router(
        FsCatalog::new(root.path()).unwrap(),
        OriginalPreview,
        config,
    )
}

/// The default `--auth account` router: a login exists unless a passcode or an
/// account says otherwise.
fn app_with(root: &TempDir, passcode: Option<&str>) -> axum::Router {
    app_for(root, passcode, AuthMode::Account)
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
async fn first_run_setup_needs_no_code_and_is_gone_afterwards() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);

    // Every page redirects to /setup while no account exists.
    let (status, headers, _) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/setup");
    let api = send(&app, get("/api/folder", None)).await;
    assert_eq!(api.0, StatusCode::SEE_OTHER);

    let (status, html) = body_text(&app, get("/setup", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Initial Setup"), "{html}");
    // There is no code any more: nothing to print, copy or paste.
    assert!(!html.contains("setup_code"), "{html}");
    assert!(!html.contains("one-time"), "{html}");

    // Mismatched passwords are refused, and nothing is written.
    let (status, _) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=other-password",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.path().join("state/accounts.json").exists());

    // Email + password + confirmation is all the form asks for.
    let (status, headers, _) = send(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
    let cookie = session_cookie(&headers);

    let (status, _) = body_text(&app, get("/", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);

    // The setup page is gone for good, and a second POST cannot add an owner.
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=second%40example.com&password=secret123456&confirm_password=secret123456",
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

/// The sign-in and setup pages link the stylesheet and script. Those carry no
/// library data, so they are served before the auth gate; gating them would
/// redirect the sign-in page's own stylesheet back to the sign-in page, which
/// then renders unstyled.
#[tokio::test]
async fn auth_pages_load_the_stylesheet_without_a_session() {
    let routes = [
        dimagine_serve::assets::css_route(),
        dimagine_serve::assets::js_route(),
    ];

    // Setup gate: no owner exists yet.
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);
    for route in &routes {
        let (status, _, body) = send(&app, get(route, None)).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert!(!body.is_empty(), "{route}");
    }

    // Login gate: an owner exists but this client has no session.
    let root = tempfile::tempdir().unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_with(&root, None);
    for route in &routes {
        let (status, _, body) = send(&app, get(route, None)).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert!(!body.is_empty(), "{route}");
    }
}

#[tokio::test]
async fn setup_needs_the_checkbox_for_a_weak_password() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);

    // The form warns about a short password and offers the checkbox.
    let (status, html) = body_text(&app, get("/setup", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Use this weak password anyway"), "{html}");
    assert!(html.contains("fewer than 8 characters"), "{html}");
    assert!(!html.contains("minlength"), "no length minimum: {html}");

    // A weak password without the box is refused, and nothing is written.
    let (status, html) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=short&confirm_password=short",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains("Use this weak password anyway"), "{html}");
    assert!(!root.path().join("state/accounts.json").exists());

    // The server enforces it: an empty value for the box is not a yes.
    let (status, _) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=short&confirm_password=short&allow_weak=",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.path().join("state/accounts.json").exists());

    // An empty password is refused even with the box ticked.
    let (status, html) = body_text(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=&confirm_password=&allow_weak=yes",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains("cannot be empty"), "{html}");
    assert!(!root.path().join("state/accounts.json").exists());

    // With the box ticked, the same weak password is accepted.
    let (status, headers, _) = send(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=short&confirm_password=short&allow_weak=yes",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
    let store = AccountsStore::new(&root.path().join("state"));
    assert_eq!(store.load().unwrap().users.len(), 1);
}

#[tokio::test]
async fn setup_accepts_a_password_of_eight_characters_without_the_box() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);

    // Eight characters is the weak threshold, not a minimum length: this is
    // accepted with no confirmation at all.
    let (status, headers, _) = send(
        &app,
        post(
            "/setup",
            "email=owner%40example.com&password=12345678&confirm_password=12345678",
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

    // A fresh process (a new router) with the same data dir: /setup is a 404.
    let app = app_with(&root, None);
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=evil%40example.com&password=secret123&confirm_password=secret123",
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
    let app = app_with(&root, None);

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
    let app = app_with(&root, None);
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
    let app = app_with(&root, Some("open-sesame"));

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
    let app = app_with(&root, Some("open-sesame"));
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

    // Building the viewer refuses outright: no router, so no setup page.
    let err = try_router(&root).unwrap_err();
    assert!(err.to_string().contains("users"), "{err}");
}

#[tokio::test]
async fn a_store_that_loses_its_users_key_while_running_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::OK,
        "the store is absent, so this is a first run"
    );

    // The file appears with no `users` key under the live server.
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("accounts.json"), r#"{"schema":1}"#).unwrap();

    // The setup form is never rendered and the POST never creates an account.
    for request in [
        get("/setup", None),
        post(
            "/setup",
            "email=owner%40example.com&password=secret123456&confirm_password=secret123456",
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
    let app = app_with(&root, None);
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
async fn concurrent_setup_requests_still_create_exactly_one_owner() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with(&root, None);

    let request = |email: &str| {
        post(
            "/setup",
            &format!("email={email}&password=secret123456&confirm_password=secret123456"),
        )
    };

    // Two requests race for the owner; exactly one may complete setup.
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
        "exactly one request may create the owner: {outcomes:?}"
    );
    for (status, headers) in &outcomes {
        if *status == StatusCode::SEE_OTHER && headers["location"] == "/" {
            continue;
        }
        // The loser is sent to the login page: it finds an owner already
        // exists, so its account is not created.
        assert!(
            *status == StatusCode::SEE_OTHER && headers["location"] == "/login",
            "the losing request must be redirected to login: {status} {headers:?}"
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

    // Setup stays closed: a follow-up request finds the owner and is a 404.
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=third%40example.com&password=secret123456&confirm_password=secret123456",
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn no_auth_mode_serves_every_page_without_a_login() {
    let root = tempfile::tempdir().unwrap();
    // A library with something on it, so the pages below exist.
    std::fs::create_dir_all(root.path().join("sub")).unwrap();
    std::fs::write(
        root.path().join("girl.jpg"),
        b"\xff\xd8\xff\xe0not-a-real-jpeg",
    )
    .unwrap();
    // Even with an account in the store, `--auth none` asks for nothing.
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, Some("open-sesame"), AuthMode::None);

    // The library is reachable anonymously, with no cookie and no redirect.
    for path in ["/", "/?in=sub", "/image/girl.jpg"] {
        let (status, _headers, body) = send(&app, get(path, None)).await;
        assert_eq!(status, StatusCode::OK, "{path} must be public");
        let html = String::from_utf8_lossy(&body).into_owned();
        assert!(
            html.contains(NO_LOGIN_BANNER),
            "{path} must carry the banner: {html}"
        );
    }

    // The API answers too, with no session at all.
    assert_eq!(send(&app, get("/api/folder", None)).await.0, StatusCode::OK);

    // No login exists: /login and /logout lead to the library, and owner
    // creation is not offered here.
    let (status, headers, _) = send(&app, get("/login", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
    assert_eq!(
        send(&app, get("/setup", None)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            post(
                "/setup",
                "email=intruder%40example.com&password=secret123456&confirm_password=secret123456",
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Posting a login is a no-op, not a way in or a way to see a form.
    let (status, headers, _) = send(&app, post("/login", "passcode=open-sesame")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/");
}

#[tokio::test]
async fn no_auth_mode_does_not_need_the_accounts_store() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    // A store a login would have to refuse: no `users` key at all.
    std::fs::write(state.join("accounts.json"), r#"{"schema":1}"#).unwrap();

    // Account mode still refuses to build a router from it (RW25 F-1).
    assert!(try_router(&root).is_err());

    // No-login mode never reads it, so the same store is not an obstacle.
    let app = router_from(
        Arc::new(FsCatalog::new(root.path()).unwrap()),
        Arc::new(OriginalPreview),
        ServeConfig {
            data_dir: state.clone(),
            auth: AuthMode::None,
            ..ServeConfig::default()
        },
    )
    .expect("--auth none does not read the accounts store");
    let (status, _, body) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains(NO_LOGIN_BANNER));
    // The refused store is left exactly as it was.
    assert_eq!(
        std::fs::read_to_string(state.join("accounts.json")).unwrap(),
        r#"{"schema":1}"#
    );
}

#[tokio::test]
async fn the_account_mode_never_shows_the_no_login_banner() {
    let root = tempfile::tempdir().unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_with(&root, None);

    // The login page is the one page an anonymous client can reach, and it
    // must not claim that there is no login.
    let (status, _, body) = send(&app, get("/login", None)).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body).into_owned();
    assert!(!html.contains("No login:"), "{html}");
    assert!(!html.contains("anyone who can reach"), "{html}");

    // And an anonymous library request is still a redirect to the login page.
    let (status, headers, _) = send(&app, get("/", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");
}

#[tokio::test]
async fn the_owner_reminder_repeats_until_an_account_exists() {
    let root = tempfile::tempdir().unwrap();
    let store = AccountsStore::new(&root.path().join("state"));

    let logged = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = {
        let logged = Arc::clone(&logged);
        move |message: &str| logged.lock().unwrap().push(message.to_string())
    };
    // A short interval stands in for the ten production minutes.
    let reminder = tokio::spawn(remind_until_owner_exists(
        store.clone(),
        Duration::from_millis(20),
        sink,
    ));

    tokio::time::sleep(Duration::from_millis(90)).await;
    assert!(
        logged.lock().unwrap().len() >= 2,
        "the reminder must repeat, not fire once"
    );

    // Creating the owner stops it, once it next looks.
    store
        .create_user("owner@example.com", "secret123456", "owner")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reminder)
        .await
        .expect("the reminder must stop once an owner exists")
        .unwrap();

    let logged = logged.lock().unwrap();
    assert!(logged.len() >= 2);
    assert!(
        logged.iter().all(|message| message == NO_OWNER_REMINDER),
        "only the reminder message is logged: {logged:?}"
    );
    // It names the setup page and the claim risk, and carries no secret:
    // there is no code to leak any more.
    assert_eq!(
        NO_OWNER_REMINDER,
        "No owner account yet: open /setup to create it. \
         Until then the first visitor can claim this server."
    );
    assert_eq!(NO_OWNER_REMINDER_INTERVAL, Duration::from_secs(600));
}

#[tokio::test]
async fn the_owner_reminder_is_silent_when_an_account_exists() {
    let root = tempfile::tempdir().unwrap();
    let store = AccountsStore::new(&root.path().join("state"));
    create_owner(&root, "owner@example.com", "secret123");

    let logged = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = {
        let logged = Arc::clone(&logged);
        move |message: &str| logged.lock().unwrap().push(message.to_string())
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        remind_until_owner_exists(store, Duration::from_millis(10), sink),
    )
    .await
    .expect("an existing owner must end the reminder at once");
    assert!(logged.lock().unwrap().is_empty());
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
