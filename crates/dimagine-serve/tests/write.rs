//! The write API and settings page (ADR-018, ADR-019 §5).
//!
//! These drive the router directly against a throwaway library and a
//! throwaway state directory, so no real library or account is touched.
//! They pin the security contract (the default-off, the CSRF bar, the
//! origin rule, the owner-only rule) and the round-trip of each write.

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
};
use dimagine_serve::{
    accounts::AccountsStore, router, AuthMode, FsCatalog, OriginalPreview, ServeConfig,
};
use dimagine_write::Fingerprint;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "http://example.test";
const PIXEL: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");

/// A library with one image and a state directory, both under one tempdir.
fn library() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("library")).unwrap();
    std::fs::write(root.path().join("library/one.png"), PIXEL).unwrap();
    std::fs::create_dir_all(root.path().join("state")).unwrap();
    root
}

fn config(root: &TempDir, write: bool, passcode: Option<&str>) -> ServeConfig {
    ServeConfig {
        data_dir: root.path().join("state"),
        auth: AuthMode::Account,
        passcode: passcode.map(str::to_owned),
        write,
        public_origin: write.then(|| ORIGIN.to_string()),
        ..ServeConfig::default()
    }
}

fn app_for(root: &TempDir, write: bool, passcode: Option<&str>) -> axum::Router {
    router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        config(root, write, passcode),
    )
}

/// Create the owner account directly, the way the CLI would.
fn create_owner(root: &TempDir, email: &str, password: &str) {
    AccountsStore::new(&root.path().join("state"))
        .create_user(email, password, "owner")
        .unwrap();
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

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn cookie_of(headers: &HeaderMap) -> String {
    headers
        .get("set-cookie")
        .expect("a session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn login(app: &axum::Router, form_body: &str) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form_body.to_owned()))
        .unwrap();
    let (status, headers, body) = send(app, request).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "login redirects: {}",
        text(&body)
    );
    cookie_of(&headers)
}

/// Sign an owner in and fetch their CSRF token.
async fn owner_and_csrf(app: &axum::Router) -> (String, String) {
    let cookie = login(app, "email=owner%40example.com&password=secret123").await;
    let token = csrf_for(app, &cookie).await;
    (cookie, token)
}

/// The CSRF token of one already-signed-in session.
async fn csrf_for(app: &axum::Router, cookie: &str) -> String {
    let (status, _, body) = send(app, get("/api/write/csrf", Some(cookie))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the token endpoint is readable: {}",
        text(&body)
    );
    serde_json::from_str::<serde_json::Value>(&text(&body)).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

/// A well-formed write request: JSON body, correct origin, the CSRF header,
/// and the session cookie.
fn write_post(
    uri: &str,
    cookie: &str,
    csrf: &str,
    origin: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .header("x-csrf-token", csrf);
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

fn json_body(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// Default off (ADR-018 §1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_write_the_write_routes_are_absent_and_settings_is_read_only() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, false, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;

    // No write route exists at all — a 404, not a forbidden page.
    for uri in [
        "/api/write/settings",
        "/api/write/trash",
        "/api/write/move",
        "/api/write/csrf",
    ] {
        let (status, _, _) = send(&app, get(uri, Some(&cookie))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let (status, _, _) = send(&app, write_post(uri, &cookie, "x", Some(ORIGIN), "{}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} post");
    }

    // The settings page is still there, marked read-only.
    let (status, _, body) = send(&app, get("/settings", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&body);
    assert!(page.contains("trash.retentionDays"), "{page}");
    assert!(page.contains("--write"), "{page}");
}

// ---------------------------------------------------------------------------
// Owner-only (ADR-018 §2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_passcode_session_cannot_write_even_with_write_enabled() {
    let root = library();
    // No owner account, a passcode instead: login is possible, writing is not.
    let app = app_for(&root, true, Some("passcode123"));
    let cookie = login(&app, "passcode=passcode123").await;
    // In passcode mode the CSRF bootstrap is meaningless, but the guard is
    // what refuses: a fully well-formed write still gets 403.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            "whatever",
            Some(ORIGIN),
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&body));
    assert!(text(&body).contains("no owner account"), "{}", text(&body));
}

#[tokio::test]
async fn a_session_from_before_any_owner_existed_gains_no_write_access() {
    let root = library();
    // No owner account yet: a passcode session is possible, writing is not.
    let app = app_for(&root, true, Some("passcode123"));
    let cookie = login(&app, "passcode=passcode123").await;
    let csrf = csrf_for(&app, &cookie).await;
    let attempt = || {
        write_post(
            "/api/write/settings",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"reset":["trash.retentionDays"]}"#,
        )
    };

    // Before any owner exists the guard refuses the write...
    let (status, _, _) = send(&app, attempt()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // ...and creating an owner out-of-band must not promote that session:
    // it belongs to no account, so it stays refused.
    create_owner(&root, "owner@example.com", "secret123");
    let (status, _, body) = send(&app, attempt()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&body));
    assert!(
        text(&body).contains("not signed in to an account"),
        "{}",
        text(&body)
    );
    // The settings page agrees it is not an owner session.
    let (status, _, _) = send(&app, get("/settings", Some(&cookie))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Control: a fresh session of the new owner writes fine.
    let owner = login(&app, "email=owner%40example.com&password=secret123").await;
    let owner_csrf = csrf_for(&app, &owner).await;
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &owner,
            &owner_csrf,
            Some(ORIGIN),
            r#"{"reset":["trash.retentionDays"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));

    // ...and deleting that account out-of-band revokes the session again:
    // its account no longer exists, so its writes are refused. A second
    // owner stays so the server itself still has an owner.
    create_owner(&root, "second@example.com", "secret123");
    AccountsStore::new(&root.path().join("state"))
        .delete_user("owner@example.com")
        .unwrap();
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &owner,
            &owner_csrf,
            Some(ORIGIN),
            r#"{"reset":["trash.retentionDays"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&body));
    assert!(text(&body).contains("no longer exists"), "{}", text(&body));
}

#[tokio::test]
async fn anonymous_write_on_a_no_login_server_succeeds_with_the_origin() {
    let root = library();
    // `--auth none --allow-anonymous-write`: no session, no CSRF token. The
    // CLI confines this mode to a loopback bind; the origin check is what
    // the test pins here.
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        auth: AuthMode::None,
        write: true,
        allow_anonymous_write: true,
        public_origin: Some(ORIGIN.to_string()),
        ..ServeConfig::default()
    };
    let app = router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        config,
    );

    // No cookie, no token — a well-formed write still lands.
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/json")
        .header("origin", ORIGIN)
        .body(Body::from(r#"{"set":{"trash.retentionDays":90}}"#))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let on_disk =
        std::fs::read_to_string(root.path().join("library/.dimagine/settings.json")).unwrap();
    assert!(on_disk.contains("90"), "{on_disk}");

    // Without the origin it is still refused.
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"reset":["trash.retentionDays"]}"#))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// CSRF (ADR-018 §3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_write_without_the_origin_or_the_token_is_refused() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // Wrong origin.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            &csrf,
            Some("https://evil.example"),
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // `Origin: null` (a sandboxed form) is refused by name.
    let (status, _, _) = send(
        &app,
        write_post("/api/write/settings", &cookie, &csrf, Some("null"), "{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A missing token.
    let no_token = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = send(&app, no_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A wrong token.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            "deadbeef",
            Some(ORIGIN),
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A non-JSON body (even with origin and token) is refused.
    let not_json = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .header("x-csrf-token", &csrf)
        .body(Body::from("set=x"))
        .unwrap();
    let (status, _, _) = send(&app, not_json).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn get_on_a_post_only_route_changes_nothing() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // GET on a POST-only route never reaches a write: the path exists, so
    // axum answers 405 rather than running a handler.
    for uri in [
        "/api/write/settings",
        "/api/write/restore",
        "/api/write/collections",
        "/api/write/password",
        "/api/write/image/front-matter",
    ] {
        let (status, _, _) = send(&app, get(uri, Some(&cookie))).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
    }

    // The one GET on a mutating path — the undo toast — is a read: nothing
    // to undo before any write...
    let (status, _, body) = send(&app, get("/api/write/undo", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["op"], serde_json::Value::Null);

    // ...and reading it beside a write changes nothing: two reads agree,
    // and the toast still names the write's op.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/image/front-matter",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"image":"one.png","add":["cat"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let op = json_body(&body)["op"].as_str().unwrap().to_owned();
    let (status, _, first) = send(&app, get("/api/write/undo", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, second) = send(&app, get("/api/write/undo", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second, "the toast is a read, not a mutation");
    assert_eq!(json_body(&first)["op"], serde_json::json!(op));
}

#[tokio::test]
async fn referer_is_accepted_when_origin_is_absent_and_only_its_origin_matters() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // No Origin, but the Referer's origin is exactly the public origin.
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .header("referer", format!("{ORIGIN}/settings"))
        .body(Body::from(r#"{"reset":["trash.retentionDays"]}"#))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));

    // A Referer to a sibling subpath that only shares a prefix is refused.
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/settings")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .header("referer", format!("{ORIGIN}-other/settings"))
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_body_over_one_megabyte_is_cut_as_it_streams() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let pad = "a".repeat((1 << 20) + 1024);
    let body = format!("{{\"reset\":[],\"pad\":\"{pad}\"}}");
    let (status, _, _) = send(
        &app,
        write_post("/api/write/settings", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------
// Settings round-trip (ADR-019 §5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_save_writes_through_the_core_and_round_trips() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"set":{"trash.retentionDays":90}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let doc = json_body(&body);
    let written = doc["written"].as_str().unwrap();
    assert!(written.contains("90"), "{written}");
    let on_disk =
        std::fs::read_to_string(root.path().join("library/.dimagine/settings.json")).unwrap();
    assert!(on_disk.contains("90"), "{on_disk}");

    // A value the schema rejects is refused and changes nothing.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"set":{"trash.retentionDays":4000}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));

    // Reset brings it back to no file key.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/settings",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"reset":["trash.retentionDays"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(json_body(&body)["written"], serde_json::json!("{}"));
}

#[tokio::test]
async fn settings_page_is_owner_only() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    // Anonymous: the auth gate sends the page to login (an HTML 303).
    let (status, headers, _) = send(&app, get("/settings", None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");
    // Owner: the editable page.
    let (cookie, _) = owner_and_csrf(&app).await;
    let (status, _, body) = send(&app, get("/settings", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&body);
    assert!(page.contains("Restore default"), "{page}");
    assert!(page.contains("trash.retentionDays"), "{page}");
}

// ---------------------------------------------------------------------------
// Front-matter edit and undo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn front_matter_edit_writes_a_note_and_undo_undoes_it() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let note = root.path().join("library/one.png.md");

    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/image/front-matter",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"image":"one.png","add":["cat"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let doc = json_body(&body);
    assert_eq!(doc["changed"], serde_json::json!(true));
    let op = doc["op"].as_str().unwrap().to_owned();
    assert!(note.exists(), "the note landed");
    assert!(
        std::fs::read_to_string(&note).unwrap().contains("cat"),
        "the tag is in the note"
    );

    // Undo reverses the create (ADR-016 §8): the note leaves its path.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/undo",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &format!("{{\"op\":\"{op}\"}}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert!(!note.exists(), "undo moved the note out of the library");

    // The trash view then shows the group the undo handed back.
    let (status, _, body) = send(&app, get("/api/write/trash", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let doc = json_body(&body);
    let groups = doc["groups"].as_array().unwrap();
    assert!(!groups.is_empty(), "the undone note is in the trash view");
}

#[tokio::test]
async fn undo_records_the_peer_on_its_audit_lines() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // Trash the image (a move, so its undo has inverse steps of its own),
    // then undo the trash.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/trash",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let op = json_body(&body)["op"].as_str().unwrap().to_owned();
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/undo",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &format!("{{\"op\":\"{op}\"}}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));

    // The undo's own audit lines carry the peer like every other serve
    // write (ADR-018 §6).
    let log = std::fs::read_to_string(root.path().join("library/.dimagine/log/ops.jsonl")).unwrap();
    let undo_lines: Vec<&str> = log
        .lines()
        .filter(|line| line.contains(r#""source":"undo""#))
        .collect();
    assert!(
        !undo_lines.is_empty(),
        "the undo wrote its own audit lines: {log}"
    );
    for line in undo_lines {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            entry
                .get("client_addr")
                .and_then(|value| value.as_str())
                .is_some_and(|addr| !addr.is_empty()),
            "the undo line carries the peer: {line}"
        );
    }
}

#[tokio::test]
async fn front_matter_rejects_a_path_outside_the_library() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/image/front-matter",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"image":"../escape.png","add":["x"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", text(&body));
}

// ---------------------------------------------------------------------------
// Trash and restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trash_moves_a_group_out_and_restore_brings_it_back() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let image = root.path().join("library/one.png");
    assert!(image.exists());

    // Trash it.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/trash",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert!(!image.exists(), "the image left the library");
    // The trash view now lists the group.
    let (status, _, body) = send(&app, get("/api/write/trash", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let doc = json_body(&body);
    assert_eq!(doc["groups"].as_array().unwrap().len(), 1);

    // Restore it.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/restore",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert!(image.exists(), "the image is back");
    let (_, _, body) = send(&app, get("/api/write/trash", Some(&cookie))).await;
    assert!(json_body(&body)["groups"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn trash_names_each_group_once_when_an_image_is_repeated() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // A repeated name trashes one group, not a partially applied operation.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/trash",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png","one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(
        json_body(&body)["trashed"],
        serde_json::json!(["one.png"]),
        "{}",
        text(&body)
    );
    assert!(!root.path().join("library/one.png").exists());

    // The single group restores whole.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/restore",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert!(root.path().join("library/one.png").exists());
}

#[tokio::test]
async fn trash_refuses_a_path_outside_the_library_and_an_oversized_batch() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // A path that escapes the library is refused (the write core will not touch
    // it), and nothing is moved.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/trash",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["../escape.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(root.path().join("library/one.png").exists());

    // More than the batch limit is refused before any file is opened.
    let many: Vec<String> = (0..1001).map(|i| format!("x{i}.png")).collect();
    let body = format!("{{\"images\":{}}}", serde_json::to_string(&many).unwrap());
    let (status, _, _) = send(
        &app,
        write_post("/api/write/trash", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// ---------------------------------------------------------------------------
// Collections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collect_new_makes_a_collection_and_gives_the_image_an_id_note() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/collections",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"],"new":"My Set"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let doc = json_body(&body);
    assert_eq!(doc["created"], serde_json::json!(true));
    let collection = doc["collection"].as_str().unwrap();
    let collection_path = root.path().join("library").join(collection);
    assert!(collection_path.exists(), "the collection note landed");
    // Joining a collection gives the image's note an id (ADR-017 §3), creating
    // it if it was absent.
    assert!(
        root.path().join("library/one.png.md").exists(),
        "the image's note exists to carry its id"
    );

    // Removing it from the collection edits the collection back.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/collections",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &format!(r#"{{"images":["one.png"],"remove":"{collection}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
}

#[tokio::test]
async fn collect_requires_exactly_one_of_new_add_remove() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/collections",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"images":["one.png"]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Password change (the signed-in account's own credential)
// ---------------------------------------------------------------------------

/// A password the test invents at run time. Nothing here is a fixture and
/// nothing is written to a file or a report: the value exists for the length
/// of the test and is thrown away with its throwaway account.
fn generated_password(tag: &str) -> String {
    format!("{tag}-{}", uuid::Uuid::new_v4())
}

/// A short (seven character) password, for the weak-password rule.
fn generated_weak_password() -> String {
    let unique = uuid::Uuid::new_v4().to_string();
    format!("wk-{}", &unique[..4])
}

fn form_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Sign in with a generated password. `login` asserts the redirect, so this
/// is only for attempts that are expected to succeed.
async fn login_with(app: &axum::Router, email: &str, password: &str) -> String {
    let form = format!(
        "email={}&password={}",
        form_encode(email),
        form_encode(password)
    );
    login(app, &form).await
}

/// Attempt a sign-in and report the status, for the attempts that must fail.
async fn try_login(app: &axum::Router, email: &str, password: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "email={}&password={}",
            form_encode(email),
            form_encode(password)
        )))
        .unwrap();
    let (status, _, _) = send(app, request).await;
    status
}

/// The status of a page fetched with one session cookie.
async fn page_status(app: &axum::Router, uri: &str, cookie: &str) -> StatusCode {
    let (status, _, _) = send(app, get(uri, Some(cookie))).await;
    status
}

async fn change_password(
    app: &axum::Router,
    cookie: &str,
    csrf: &str,
    body: &serde_json::Value,
) -> (StatusCode, String) {
    let (status, _, response) = send(
        app,
        write_post(
            "/api/write/password",
            cookie,
            csrf,
            Some(ORIGIN),
            &body.to_string(),
        ),
    )
    .await;
    (status, text(&response))
}

#[tokio::test]
async fn a_password_change_verifies_the_old_password_and_signs_out_other_sessions() {
    let root = library();
    let email = "owner@example.com";
    let first = generated_password("w64-first");
    let second = generated_password("w64-second");
    create_owner(&root, email, &first);
    let app = app_for(&root, true, None);

    // Two sessions of the same account: the one that changes the password,
    // and one that the change must sign out.
    let changing = login_with(&app, email, &first).await;
    let other = login_with(&app, email, &first).await;
    assert_eq!(page_status(&app, "/", &other).await, StatusCode::OK);
    let csrf = csrf_for(&app, &changing).await;

    let (status, body) = change_password(
        &app,
        &changing,
        &csrf,
        &serde_json::json!({ "old": first, "new": second, "confirm": second }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    assert_eq!(json["changed"], serde_json::json!(true), "{body}");
    assert_eq!(json["signed_out"], serde_json::json!(1), "{body}");

    // The session that made the change is still signed in; the other is not.
    assert_eq!(page_status(&app, "/", &changing).await, StatusCode::OK);
    assert_eq!(page_status(&app, "/", &other).await, StatusCode::SEE_OTHER);

    // The old password no longer signs anyone in and the new one does.
    assert_eq!(
        try_login(&app, email, &first).await,
        StatusCode::UNAUTHORIZED
    );
    let _ = login_with(&app, email, &second).await;
}

#[tokio::test]
async fn a_password_change_refuses_bad_input_without_changing_anything() {
    let root = library();
    let email = "owner@example.com";
    let current = generated_password("w64-current");
    let wanted = generated_password("w64-wanted");
    let weak = generated_weak_password();
    create_owner(&root, email, &current);
    let app = app_for(&root, true, None);
    let cookie = login_with(&app, email, &current).await;
    let csrf = csrf_for(&app, &cookie).await;

    // The old password is wrong.
    let (status, body) = change_password(
        &app,
        &cookie,
        &csrf,
        &serde_json::json!({
            "old": generated_password("w64-wrong"),
            "new": wanted,
            "confirm": wanted,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("current password"), "{body}");

    // The two new passwords disagree: a mistyped confirmation sets nothing.
    let (status, body) = change_password(
        &app,
        &cookie,
        &csrf,
        &serde_json::json!({
            "old": current,
            "new": wanted,
            "confirm": generated_password("w64-typo"),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("do not match"), "{body}");

    // A new password shorter than eight characters needs the confirmation the
    // setup form asks for…
    let (status, body) = change_password(
        &app,
        &cookie,
        &csrf,
        &serde_json::json!({ "old": current, "new": weak, "confirm": weak }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("easy to guess"), "{body}");

    // …and an empty one is refused outright.
    let (status, body) = change_password(
        &app,
        &cookie,
        &csrf,
        &serde_json::json!({ "old": current, "new": "", "confirm": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("must not be empty"), "{body}");

    // None of those changed anything.
    let _ = login_with(&app, email, &current).await;
    assert_eq!(
        try_login(&app, email, &wanted).await,
        StatusCode::UNAUTHORIZED
    );

    // With the confirmation, the short password is accepted — the same rule
    // the setup form and `dimagine user passwd` apply.
    let cookie = login_with(&app, email, &current).await;
    let csrf = csrf_for(&app, &cookie).await;
    let (status, body) = change_password(
        &app,
        &cookie,
        &csrf,
        &serde_json::json!({
            "old": current,
            "new": weak,
            "confirm": weak,
            "allow_weak": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let _ = login_with(&app, email, &weak).await;
}

#[tokio::test]
async fn the_settings_page_offers_the_password_change_only_to_a_signed_in_owner() {
    let root = library();
    let email = "owner@example.com";
    let password = generated_password("w64-page");
    create_owner(&root, email, &password);

    // With `--write`, the owner's page carries the form and names the account
    // whose password it would change.
    let app = app_for(&root, true, None);
    let cookie = login_with(&app, email, &password).await;
    let (status, _, body) = send(&app, get("/settings", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let page = text(&body);
    assert!(page.contains("Change password"), "{page}");
    assert!(page.contains("pw-form"), "{page}");
    assert!(page.contains(email), "{page}");

    // Without `--write` the page is the read-only display it was: no form,
    // and the endpoint itself is absent (404, not a 403).
    let read_only = app_for(&root, false, None);
    let cookie = login_with(&read_only, email, &password).await;
    let (status, _, body) = send(&read_only, get("/settings", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&body);
    assert!(!page.contains("pw-form"), "{page}");
    assert!(!page.contains("Change password"), "{page}");
    let (status, body) = change_password(
        &read_only,
        &cookie,
        "whatever",
        &serde_json::json!({ "old": password, "new": password, "confirm": password }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn a_password_change_edits_only_the_sessions_own_account() {
    let root = library();
    let mine_email = "owner@example.com";
    let peer_email = "second@example.com";
    let mine_first = generated_password("w64-owner");
    let mine_second = generated_password("w64-owner-new");
    let peer = generated_password("w64-peer");
    create_owner(&root, mine_email, &mine_first);
    // A second account, the way `dimagine user create` adds one.
    AccountsStore::new(&root.path().join("state"))
        .create_user(peer_email, &peer, "owner")
        .unwrap();
    let app = app_for(&root, true, None);
    let mine = login_with(&app, mine_email, &mine_first).await;
    let peers = login_with(&app, peer_email, &peer).await;
    let csrf = csrf_for(&app, &mine).await;

    let (status, body) = change_password(
        &app,
        &mine,
        &csrf,
        &serde_json::json!({
            "old": mine_first,
            "new": mine_second,
            "confirm": mine_second,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    // No other session of *this* account existed, and the peer's session is
    // another account's, so nothing was signed out.
    assert_eq!(json["signed_out"], serde_json::json!(0), "{body}");

    // The peer's session and password are untouched…
    assert_eq!(page_status(&app, "/", &peers).await, StatusCode::OK);
    let _ = login_with(&app, peer_email, &peer).await;
    // …and the changed password is nobody else's.
    assert_eq!(
        try_login(&app, peer_email, &mine_second).await,
        StatusCode::UNAUTHORIZED
    );
}

// ---------------------------------------------------------------------------
// Move (ADR-017 §8)
// ---------------------------------------------------------------------------

/// A library with a note, a sidecar and a shelf that links the image three
/// ways, so a move has real work to do.
fn movable_library() -> TempDir {
    let root = library();
    let l = root.path().join("library");
    std::fs::write(l.join("one.png.xmp"), b"sidecar").unwrap();
    std::fs::write(
        l.join("one.png.md"),
        "---\ntags: [keep]\n---\nmine ![[one.png]]\n",
    )
    .unwrap();
    std::fs::write(l.join("shelf.md"), "a ![[one.png]] b ![[one|al]]\n").unwrap();
    root
}

#[tokio::test]
async fn move_renames_the_group_repairs_links_and_undoes_whole() {
    let root = movable_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/move",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"from":"one.png","to":"inbox/two.png"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let json = json_body(&body);
    assert_eq!(json["schema"], "dimagine.move/0.1");
    assert_eq!(json["dry_run"], false);
    assert!(json["op"].is_string());
    assert_eq!(json["renames"][0]["to"], "inbox/two.png");
    let l = root.path().join("library");
    assert!(!l.join("one.png").exists());
    assert_eq!(
        std::fs::read_to_string(l.join("shelf.md")).unwrap(),
        "a ![[inbox/two.png]] b ![[inbox/two.png|al]]\n"
    );
    let note = std::fs::read_to_string(l.join("inbox/two.png.md")).unwrap();
    assert!(note.contains("id: "), "{note}");

    let op = json["op"].as_str().unwrap().to_string();
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/undo",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &format!(r#"{{"op":"{op}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(
        std::fs::read_to_string(l.join("shelf.md")).unwrap(),
        "a ![[one.png]] b ![[one|al]]\n"
    );
    assert!(l.join("one.png").exists());
    assert!(!l.join("inbox/two.png.md").exists());
}

#[tokio::test]
async fn move_dry_run_answers_with_the_plan_and_touches_nothing() {
    let root = movable_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/move",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"from":"one.png","to":"inbox/two.png","dry_run":true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let json = json_body(&body);
    assert_eq!(json["dry_run"], true);
    assert!(json["op"].is_null());
    assert_eq!(json["edited"][1]["path"], "shelf.md");

    let l = root.path().join("library");
    assert!(l.join("one.png").exists());
    assert!(!l.join("inbox").exists());
    assert_eq!(
        std::fs::read_to_string(l.join("shelf.md")).unwrap(),
        "a ![[one.png]] b ![[one|al]]\n"
    );
}

#[tokio::test]
async fn move_refusals_are_400_and_security_bars_stay_up() {
    let root = movable_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // The taken destination refuses with the typed message, unchanged bytes.
    let (status, _, body) = send(
        &app,
        write_post(
            "/api/write/move",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"from":"one.png","to":"shelf.md"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));
    assert!(
        root.path().join("library/one.png").try_exists().unwrap(),
        "a refusal moves nothing"
    );

    // A missing source is a 404, like the trash's unknown image.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/move",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"from":"nobody.png","to":"inbox/two.png"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // No CSRF header: the guard refuses before the handler (ADR-018 §3).
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/move")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .body(Body::from(r#"{"from":"one.png","to":"inbox/two.png"}"#))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An anonymous request never reaches the handler: the auth gate closes
    // first (account mode, no session).
    let anon = Request::builder()
        .method("POST")
        .uri("/api/write/move")
        .header("content-type", "application/json")
        .header("origin", ORIGIN)
        .body(Body::from(r#"{"from":"one.png","to":"inbox/two.png"}"#))
        .unwrap();
    let (status, _, _) = send(&app, anon).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The wrong origin: refused, and nothing moved.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/move",
            &cookie,
            &csrf,
            Some("http://evil.test"),
            r#"{"from":"one.png","to":"inbox/two.png"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(root.path().join("library/one.png").exists());
}

// ---------------------------------------------------------------------------
// Collection reorder and caption lines (ADR-017 §9)
// ---------------------------------------------------------------------------

/// A library whose note is a three-member collection, each member with a
/// one-line caption.
fn collection_library() -> TempDir {
    let root = library();
    for name in ["two.png", "three.png"] {
        std::fs::write(root.path().join("library").join(name), PIXEL).unwrap();
    }
    std::fs::write(
        root.path().join("library/Picks.md"),
        "---\nkind: collection\ntitle: Picks\n---\n\n\
         ![[one.png]]\nfirst\n\n\
         ![[two.png]]\nsecond\n\n\
         ![[three.png]]\nthird\n",
    )
    .unwrap();
    root
}

/// The note's sha256 as the web client would carry it (the open-time hash).
fn note_hash(root: &TempDir, rel: &str) -> String {
    let bytes = std::fs::read(root.path().join("library").join(rel)).unwrap();
    Fingerprint::of_bytes(&bytes).hex()
}

fn read_note(root: &TempDir, rel: &str) -> Vec<u8> {
    std::fs::read(root.path().join("library").join(rel)).unwrap()
}

#[tokio::test]
async fn the_reorder_endpoint_permutes_for_the_owner() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let hash = note_hash(&root, "Picks.md");
    let body = format!(r#"{{"collection":"Picks.md","hash":"{hash}","order":[3,1,2]}}"#);
    let (status, _, bytes) = send(
        &app,
        write_post(
            "/api/write/collections/reorder",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let doc = json_body(&bytes);
    assert_eq!(doc["changed"], true);
    assert_eq!(
        doc["order"],
        serde_json::json!(["three.png", "one.png", "two.png"])
    );
    assert_eq!(
        read_note(&root, "Picks.md"),
        b"---\nkind: collection\ntitle: Picks\n---\n\n\
          ![[three.png]]\nthird\n\n\
          ![[one.png]]\nfirst\n\n\
          ![[two.png]]\nsecond\n"
    );
}

#[tokio::test]
async fn the_caption_endpoint_adds_edits_and_deletes_for_the_owner() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // Append a caption to block 1 (ordinal 1), one past its only line.
    let hash = note_hash(&root, "Picks.md");
    let body =
        format!(r#"{{"collection":"Picks.md","hash":"{hash}","block":1,"line":2,"text":"added"}}"#);
    let (status, _, bytes) = send(
        &app,
        write_post(
            "/api/write/collections/caption",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    assert_eq!(json_body(&bytes)["caption"], "added");
    assert!(text(&read_note(&root, "Picks.md")).contains("first\nadded\n"));

    // `text: null` deletes the line, with the hash the client now holds.
    let hash = note_hash(&root, "Picks.md");
    let body =
        format!(r#"{{"collection":"Picks.md","hash":"{hash}","block":1,"line":1,"text":null}}"#);
    let (status, _, bytes) = send(
        &app,
        write_post(
            "/api/write/collections/caption",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    assert_eq!(json_body(&bytes)["caption"], "deleted");
    let note = text(&read_note(&root, "Picks.md"));
    assert!(!note.contains("\nfirst\n"), "{note:?}");
    assert!(note.contains("![[one.png]]\nadded\n"), "{note:?}");
}

/// ADR-016 §3: a whole-replace op carries the open hash; a request without one
/// is refused, and one whose hash no longer matches is a 409 that names the
/// note's current ETag. Nothing moves in either case.
#[tokio::test]
async fn the_reorder_endpoint_refuses_a_missing_or_stale_hash() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let before = read_note(&root, "Picks.md");

    // No hash: a failure, never an overwrite.
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/collections/reorder",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"collection":"Picks.md","order":[3,1,2]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(read_note(&root, "Picks.md"), before);

    // A well-formed hash that is not the note's: 409 with the current ETag.
    let stale = "0".repeat(64);
    let body = format!(r#"{{"collection":"Picks.md","hash":"{stale}","order":[3,1,2]}}"#);
    let (status, headers, bytes) = send(
        &app,
        write_post(
            "/api/write/collections/reorder",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", text(&bytes));
    let doc = json_body(&bytes);
    let etag = doc["etag"].as_str().expect("a current etag");
    assert!(etag.starts_with("\"size-"), "{etag:?}");
    assert_eq!(headers["etag"], etag);
    // The 409 carries the note's current body so the browser can show it beside
    // the draft being kept (RW68 M1, ADR-016 §3); a reorder carries no body.
    assert!(doc.get("body").is_none(), "{doc}");
    assert_eq!(read_note(&root, "Picks.md"), before, "nothing changed");
}

/// The precondition is judged before the "nothing to change" shortcut: an edit
/// that would land as a no-op still reports a stale open hash as a conflict,
/// because the caller's version is not the note's (ADR-016 §3).
#[tokio::test]
async fn a_no_op_edit_with_a_stale_hash_still_conflicts() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let before = read_note(&root, "Picks.md");
    let stale = "0".repeat(64);

    // Block 1 line 1 already says "first", so writing it again changes nothing.
    let body = format!(
        r#"{{"collection":"Picks.md","hash":"{stale}","block":1,"line":1,"text":"first"}}"#
    );
    let (status, _, bytes) = send(
        &app,
        write_post(
            "/api/write/collections/caption",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", text(&bytes));
    assert_eq!(read_note(&root, "Picks.md"), before);

    // The identity permutation likewise.
    let body = format!(r#"{{"collection":"Picks.md","hash":"{stale}","order":[1,2,3]}}"#);
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/collections/reorder",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(read_note(&root, "Picks.md"), before);
}

/// The same security bar as every other write (ADR-018): off by default, and
/// past the CSRF guard or the auth gate it does not run.
#[tokio::test]
async fn the_reorder_endpoint_obeys_the_write_bar() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let body = r#"{"collection":"Picks.md","order":[2,1,3]}"#;

    // Without `--write` the route does not exist (once signed in, so the auth
    // gate is not what answers). The write router is unmounted, so there is no
    // CSRF token to fetch; a dummy header is fine because the guard never runs.
    let app = app_for(&root, false, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/collections/reorder",
            &cookie,
            "unused",
            Some(ORIGIN),
            body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // With `--write`: no CSRF header is refused, and an anonymous request never
    // reaches the handler.
    let app = app_for(&root, true, None);
    let (cookie, _) = owner_and_csrf(&app).await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/collections/reorder")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .body(Body::from(body.to_owned()))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let anon = Request::builder()
        .method("POST")
        .uri("/api/write/collections/reorder")
        .header("content-type", "application/json")
        .header("origin", ORIGIN)
        .body(Body::from(body.to_owned()))
        .unwrap();
    let (status, _, _) = send(&app, anon).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Whole-note body edit (ADR-017 §10, ADR-016 §3, FORMAT §5.3)
// ---------------------------------------------------------------------------

/// A library holding one image and its note: front matter, then a body whose
/// first line is the note's own self-embed (ADR-017 §2).
fn body_library() -> TempDir {
    let root = library();
    std::fs::write(
        root.path().join("library/one.png.md"),
        "---\ntitle: One\ntags: [a]\n---\n![[one.png]]\nold body line\n",
    )
    .unwrap();
    root
}

/// A whole-body replace whose new text keeps the self-embed: the front matter
/// is carried over untouched, only the body region moves, and nothing is
/// restored because the embed still resolves. The new hash is reported.
#[tokio::test]
async fn the_body_endpoint_replaces_the_body_for_the_owner() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let hash = note_hash(&root, "one.png.md");
    let body = serde_json::json!({
        "note": "one.png.md",
        "hash": hash,
        "body": "# Note\n\n![[one.png]]\nall about it\n",
    })
    .to_string();
    let (status, _, bytes) = send(
        &app,
        write_post("/api/write/note/body", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let doc = json_body(&bytes);
    assert_eq!(doc["changed"], true);
    assert_eq!(doc["self_embed_restored"], false);
    // The note now is the untouched front matter plus exactly the body sent.
    assert_eq!(
        read_note(&root, "one.png.md"),
        b"---\ntitle: One\ntags: [a]\n---\n# Note\n\n![[one.png]]\nall about it\n"
    );
    assert_eq!(doc["hash"], note_hash(&root, "one.png.md"));
    // The hash the response names is the precondition for the very next edit.
    let next = doc["hash"].as_str().unwrap();
    let again = serde_json::json!({
        "note": "one.png.md", "hash": next, "body": "![[one.png]]\ndone\n",
    })
    .to_string();
    let (status, _, _) = send(
        &app,
        write_post("/api/write/note/body", &cookie, &csrf, Some(ORIGIN), &again),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// ADR-016 §3: the open hash is required and enforced. A request without one is
/// refused (never an overwrite); one whose hash no longer matches is a 409 that
/// names the note's current hash and ETag, and no byte moves.
#[tokio::test]
async fn the_body_endpoint_refuses_a_missing_or_stale_hash() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let before = read_note(&root, "one.png.md");
    let current = note_hash(&root, "one.png.md");

    // No hash: a failure, never an overwrite.
    let no_hash = serde_json::json!({"note": "one.png.md", "body": "gone\n"}).to_string();
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/note/body",
            &cookie,
            &csrf,
            Some(ORIGIN),
            &no_hash,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A well-formed hash that is not the note's: 409 carrying the real one.
    let stale = "0".repeat(64);
    let body =
        serde_json::json!({"note": "one.png.md", "hash": stale, "body": "gone\n"}).to_string();
    let (status, headers, bytes) = send(
        &app,
        write_post("/api/write/note/body", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", text(&bytes));
    let doc = json_body(&bytes);
    assert_eq!(doc["hash"], current);
    let etag = doc["etag"].as_str().expect("a current etag");
    assert!(etag.starts_with("\"size-"), "{etag:?}");
    assert_eq!(headers["etag"], etag);
    // The 409 also carries the note's current body, the latest the browser can
    // show against the draft it is holding (RW68 M1, ADR-016 §3).
    assert_eq!(doc["body"], "![[one.png]]\nold body line\n");
    assert_eq!(read_note(&root, "one.png.md"), before);
}

/// The self-embed rule (ADR-017 §2): a new body that leaves the note's own
/// picture unembedded gets the embed appended, and the response says so.
#[tokio::test]
async fn the_body_endpoint_restores_a_missing_self_embed() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let hash = note_hash(&root, "one.png.md");
    let body = serde_json::json!({
        "note": "one.png.md", "hash": hash, "body": "just some words\n",
    })
    .to_string();
    let (status, _, bytes) = send(
        &app,
        write_post("/api/write/note/body", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    assert_eq!(json_body(&bytes)["self_embed_restored"], true);
    let note = read_note(&root, "one.png.md");
    assert_eq!(
        note,
        b"---\ntitle: One\ntags: [a]\n---\njust some words\n![[one.png]]\n"
    );
}

/// Collection notes are edited as text too (FORMAT §5.3): the whole-replace
/// accepts a plain note and keeps its front matter byte for byte.
#[tokio::test]
async fn the_body_endpoint_replaces_a_collection_note_as_text() {
    let root = collection_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let hash = note_hash(&root, "Picks.md");
    let body = serde_json::json!({
        "note": "Picks.md", "hash": hash, "body": "a note about picks\n",
    })
    .to_string();
    let (status, _, bytes) = send(
        &app,
        write_post("/api/write/note/body", &cookie, &csrf, Some(ORIGIN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let doc = json_body(&bytes);
    assert_eq!(doc["changed"], true);
    assert_eq!(doc["self_embed_restored"], false);
    assert_eq!(
        read_note(&root, "Picks.md"),
        b"---\nkind: collection\ntitle: Picks\n---\na note about picks\n"
    );
}

/// The endpoint is a write like any other: the route is absent without `--write`,
/// a request without the CSRF token is refused, and an anonymous one never
/// reaches the handler (ADR-018 §1, §2, §3).
#[tokio::test]
async fn the_body_endpoint_obeys_the_write_bar() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let body = serde_json::json!({"note": "one.png.md", "hash": "0", "body": "x"}).to_string();

    let app = app_for(&root, false, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let (status, _, _) = send(
        &app,
        write_post(
            "/api/write/note/body",
            &cookie,
            "unused",
            Some(ORIGIN),
            &body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let app = app_for(&root, true, None);
    let (cookie, _) = owner_and_csrf(&app).await;
    let no_token = Request::builder()
        .method("POST")
        .uri("/api/write/note/body")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .body(Body::from(body.clone()))
        .unwrap();
    let (status, _, _) = send(&app, no_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let anon = Request::builder()
        .method("POST")
        .uri("/api/write/note/body")
        .header("content-type", "application/json")
        .header("origin", ORIGIN)
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = send(&app, anon).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The page carries the editor for an owner with writes on: a textarea over the
/// body and the note's open-time hash the save posts back as its precondition.
#[tokio::test]
async fn the_image_page_offers_the_body_editor_to_the_owner() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let hash = note_hash(&root, "one.png.md");
    let (status, _, bytes) = send(&app, get("/image/one.png", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let page = text(&bytes);
    assert!(page.contains("class=\"note-body-edit\""), "{page}");
    assert!(
        page.contains(&format!("data-hash=\"{hash}\"")),
        "the open hash reaches the page: {page}"
    );
    assert!(page.contains("data-note=\"one.png.md\""), "{page}");
    assert!(page.contains("/api/write/note/body"), "{page}");
    // The raw body is shown in the textarea, its self-embed intact.
    assert!(page.contains("![[one.png]]"), "{page}");
    // The conflict prompt the script drives: what the note holds now, and the
    // two ways out of a 409 (RW68 M1/L4, ADR-016 §3).
    assert!(page.contains("function offerConflict"), "{page}");
    assert!(page.contains("'Show what the note holds now'"), "{page}");
    assert!(page.contains("'Load the latest'"), "{page}");
    assert!(page.contains("'Use my version anyway'"), "{page}");
    assert!(page.contains("result.data.body"), "{page}");
}

/// Without `--write` the note is still shown, but the editor is not offered —
/// an upgraded binary does not quietly open a write door (ADR-018 §1).
#[tokio::test]
async fn the_image_page_hides_the_body_editor_without_write() {
    let root = body_library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, false, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let (status, _, bytes) = send(&app, get("/image/one.png", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&bytes);
    assert!(!page.contains("note-body-edit"), "{page}");
    // The note itself is still there to read.
    assert!(page.contains("class=\"note-body\""), "{page}");
}

/// A signed-in non-owner (passcode viewer) on a write-enabled server is offered
/// no editor either: the body editor is an owner action, not merely a `--write`
/// one (RW68 L4, ADR-018 §1/§2). The note stays readable.
#[tokio::test]
async fn the_image_page_hides_the_body_editor_from_a_passcode_viewer() {
    let root = body_library();
    let app = app_for(&root, true, Some("passcode123"));
    let cookie = login(&app, "passcode=passcode123").await;
    let (status, _, bytes) = send(&app, get("/image/one.png", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let page = text(&bytes);
    assert!(!page.contains("note-body-edit"), "{page}");
    assert!(
        !page.contains("/api/write/note/body"),
        "no editor wiring for a non-owner: {page}"
    );
    // The note itself is still there to read.
    assert!(page.contains("class=\"note-body\""), "{page}");
}

#[tokio::test]
async fn front_matter_resolves_aliases_and_refuses_invalid_tables() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let path = root.path().join("library/_dimagine/tag-aliases.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"version":1,"aliases":{"old name":"middle","middle":"canonical/tag"}}"#,
    )
    .unwrap();
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let request = || {
        write_post(
            "/api/write/image/front-matter",
            &cookie,
            &csrf,
            Some(ORIGIN),
            r#"{"image":"one.png","add":["old name"]}"#,
        )
    };
    let (status, _, body) = send(&app, request()).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let note = root.path().join("library/one.png.md");
    let before = std::fs::read(&note).unwrap();
    assert!(text(&before).contains("canonical/tag"));
    assert!(!text(&before).contains("old name"));
    for bad in [
        r#"{"version":1,"aliases":{"a":"b","b":"a"}}"#,
        r#"{"version":1,"aliases":{"A":"x","a":"y"}}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        let (status, _, _) = send(&app, request()).await;
        assert!(!status.is_success());
        assert_eq!(std::fs::read(&note).unwrap(), before);
    }
}

#[tokio::test]
async fn front_matter_tag_field_validates_and_resolves_aliases() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let path = root.path().join("library/_dimagine/tag-aliases.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        r#"{"version":1,"aliases":{"old name":"canonical/tag"}}"#,
    )
    .unwrap();
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let post = |body| {
        write_post(
            "/api/write/image/front-matter",
            &cookie,
            &csrf,
            Some(ORIGIN),
            body,
        )
    };
    let (status, _, body) = send(
        &app,
        post(r#"{"image":"one.png","fields":{"tags":["old name"]}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let note = root.path().join("library/one.png.md");
    let before = std::fs::read(&note).unwrap();
    assert!(text(&before).contains("canonical/tag"));
    for value in [r#""bad""#, "42", "null", "[42]", r#"["bad tag"]"#] {
        let request = format!(r#"{{"image":"one.png","fields":{{"tags":{value}}}}}"#);
        let (status, _, _) = send(
            &app,
            write_post(
                "/api/write/image/front-matter",
                &cookie,
                &csrf,
                Some(ORIGIN),
                &request,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read(&note).unwrap(), before);
    }
}

#[tokio::test]
async fn tags_page_lists_notes_blocking_batches() {
    let root = library();
    std::fs::write(
        root.path().join("library/one.png.md"),
        "---\ntags: scalar\n---\n",
    )
    .unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    std::fs::write(root.path().join("library/broken.md"), [0xff]).unwrap();
    let (cookie, _) = owner_and_csrf(&app).await;
    let (status, _, body) = send(&app, get("/tags", Some(&cookie))).await;
    let html = text(&body);
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Notes blocking batch operations"), "{html}");
    assert!(html.contains("href=\"/image/one.png\""), "{html}");
    assert!(
        html.contains("tags: scalar") || html.contains("scalar, not a list"),
        "{html}"
    );
    assert!(html.contains("broken.md"), "{html}");
}

#[tokio::test]
async fn tags_page_shows_blockers_even_if_index_cannot_read_a_note() {
    let root = library();
    std::fs::write(root.path().join("library/broken.md"), [0xff]).unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let (status, _, body) = send(&app, get("/tags", Some(&cookie))).await;
    let html = text(&body);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{html}");
    assert!(html.contains("Notes blocking batch operations"), "{html}");
    assert!(html.contains("broken.md"), "{html}");
}

#[tokio::test]
async fn tags_scan_failure_does_not_disclose_root() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    std::fs::rename(
        root.path().join("library"),
        root.path().join("removed-library"),
    )
    .unwrap();
    let (_, _, body) = send(&app, get("/tags", Some(&cookie))).await;
    let html = text(&body);
    assert!(html.contains("Could not inspect batch blockers."), "{html}");
    assert!(!html.contains(&root.path().display().to_string()), "{html}");
    assert!(!html.contains("not a folder"), "{html}");
}

#[tokio::test]
async fn tag_progress_requires_owner() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, Some("visitor-pass"));
    let owner = login(&app, "email=owner%40example.com&password=secret123").await;
    let visitor_root = library();
    let visitor_app = app_for(&visitor_root, true, Some("visitor-pass"));
    let visitor = login(&visitor_app, "passcode=visitor-pass").await;
    assert_eq!(
        send(&app, get("/api/write/tags/progress", Some(&owner)))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &visitor_app,
            get("/api/write/tags/progress", Some(&visitor))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
