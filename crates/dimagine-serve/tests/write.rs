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
    for uri in ["/api/write/settings", "/api/write/trash", "/api/write/csrf"] {
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
