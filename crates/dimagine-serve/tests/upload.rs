//! The upload endpoint (ADR-017 §11, ADR-018 §3 §5).
//!
//! These drive the router directly against a throwaway library and a
//! throwaway state directory — no real library, no real account, no real
//! photos: every uploaded byte here is generated. They pin the security
//! bar (default-off, owner-only, origin and CSRF, multipart-only), the
//! per-file cap enforced while the body streams against a real local
//! server (a file over the cap is refused while the client is still
//! mid-write), the batch refusal before anything is written, and the
//! round-trip of a landing upload.

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
const BOUNDARY: &str = "w66boundary";

/// A minimal stand-in for an uploaded image: the PNG magic, then noise.
/// Nothing in the upload path decodes the file, and nothing here is a
/// photo — the bytes exist only to be the format their name claims.
fn png(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[seed; 48]);
    bytes
}

/// A library with a collection note and a state directory, both under one
/// tempdir. No images: every image in these tests arrives by upload.
fn library() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("library")).unwrap();
    std::fs::write(
        root.path().join("library/mine.md"),
        b"---\nkind: collection\ntitle: Mine\n---\n\nHello.\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("state")).unwrap();
    root
}

fn config(root: &TempDir, write: bool, passcode: Option<&str>) -> ServeConfig {
    ServeConfig {
        data_dir: root.path().join("state"),
        auth: AuthMode::Account,
        passcode: passcode.map(str::to_owned),
        write,
        public_origin: write.then(|| ORIGIN.to_owned()),
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

fn json_body(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
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
    assert_eq!(status, StatusCode::SEE_OTHER, "login: {}", text(&body));
    cookie_of(&headers)
}

/// Sign an owner in and fetch their CSRF token.
async fn owner_and_csrf(app: &axum::Router) -> (String, String) {
    let cookie = login(app, "email=owner%40example.com&password=secret123").await;
    let (status, _, body) = send(app, get("/api/write/csrf", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let csrf = json_body(&body)["csrf"].as_str().unwrap().to_owned();
    (cookie, csrf)
}

fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

/// One part of a multipart body: (field name, file name, content).
struct Part<'a> {
    name: &'static str,
    file_name: Option<&'a str>,
    content: Vec<u8>,
}

fn multipart_body(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part.file_name {
            Some(file_name) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{file_name}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n",
                    part.name
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    part.name
                )
                .as_bytes(),
            ),
        }
        body.extend_from_slice(&part.content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// A POST to the upload endpoint, multipart by construction, carrying the
/// session cookie, the CSRF header and the origin like a browser form would.
fn upload_request(body: Vec<u8>, cookie: &str, csrf: &str, origin: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/write/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .header("origin", origin)
        .body(Body::from(body))
        .unwrap()
}

fn library_file(root: &TempDir, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.path().join("library").join(rel)).ok()
}

// ---------------------------------------------------------------------------
// The bar (ADR-018 §1–§3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_write_the_upload_route_is_absent() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, false, None);
    let cookie = login(&app, "email=owner%40example.com&password=secret123").await;
    let (status, _, _) = send(&app, get("/api/write/upload", Some(&cookie))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A POST to the absent route is the same 404: the write surface is
    // never mounted, so an upgraded binary cannot quietly open an upload
    // door (ADR-018 §1).
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset.png"),
        content: png(1),
    }]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(library_file(&root, "inbox/sunset.png").is_none());
}

#[tokio::test]
async fn an_anonymous_upload_is_refused_and_writes_nothing() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset.png"),
        content: png(1),
    }]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("origin", ORIGIN)
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(library_file(&root, "inbox/sunset.png").is_none());
}

#[tokio::test]
async fn a_passcode_session_cannot_upload() {
    let root = library();
    let app = app_for(&root, true, Some("passcode123"));
    let cookie = login(&app, "passcode=passcode123").await;
    let (status, _, body) = send(&app, get("/api/write/csrf", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = json_body(&body)["csrf"].as_str().unwrap().to_owned();
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset.png"),
        content: png(1),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&body));
    assert!(text(&body).contains("no owner account"), "{}", text(&body));
    assert!(library_file(&root, "inbox/sunset.png").is_none());
}

#[tokio::test]
async fn a_upload_without_the_csrf_token_or_the_right_origin_is_refused() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let attempt = |csrf: &str, origin: &str| {
        let body = multipart_body(&[Part {
            name: "file",
            file_name: Some("sunset.png"),
            content: png(1),
        }]);
        upload_request(body, &cookie, csrf, origin)
    };

    // No token at all: the session has one, so an empty header is the 403
    // of a wrong token, not the 401 of no session.
    let (status, _, _) = send(&app, attempt("", ORIGIN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A wrong token.
    let (status, _, _) = send(&app, attempt("guessed", ORIGIN)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A look-alike origin.
    let (status, _, _) = send(&app, attempt(&csrf, "http://example.test.evil")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // No origin and no referer.
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset.png"),
        content: png(1),
    }]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(library_file(&root, "inbox/sunset.png").is_none());
}

#[tokio::test]
async fn the_upload_route_takes_multipart_only_and_json_is_its_own_refusal() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // JSON at the upload endpoint: the ADR excepts the upload route from
    // the JSON rule and this route from nothing else — 415 either way.
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/upload")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .header("origin", ORIGIN)
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        text(&body)
    );

    // Multipart at a JSON endpoint: refused the same way, from the other side.
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset.png"),
        content: png(1),
    }]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/write/move")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .header("origin", ORIGIN)
        .body(Body::from(body))
        .unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        text(&body)
    );
    assert!(text(&body).contains("application/json"), "{}", text(&body));
}

// ---------------------------------------------------------------------------
// The round-trip (ADR-017 §11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_lands_in_the_default_folder_with_its_note() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let bytes = png(7);
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("sunset-blue.png"),
        content: bytes.clone(),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let answer = json_body(&body);
    assert!(answer["op"].as_str().is_some(), "{answer}");
    assert_eq!(answer["files"][0]["image"], "inbox/sunset-blue.png");
    assert_eq!(answer["files"][0]["note"], "inbox/sunset-blue.png.md");
    assert_eq!(answer["files"][0]["duplicate"], false);
    assert_eq!(answer["files"][0]["renamed"], false);
    // The image and its note are on disk, and the note is the upload note.
    assert_eq!(library_file(&root, "inbox/sunset-blue.png").unwrap(), bytes);
    let note = String::from_utf8(
        library_file(&root, "inbox/sunset-blue.png.md").expect("the note lands beside the image"),
    )
    .unwrap();
    assert!(note.contains("  - type: upload\n"), "{note}");
    assert!(
        note.contains("original_name: \"sunset-blue.png\""),
        "{note}"
    );
    assert!(note.contains("![[inbox/sunset-blue.png]]"), "{note}");
    assert!(note.contains("id: "), "{note}");
}

#[tokio::test]
async fn a_meaningful_name_lands_kept_and_a_meaningless_one_is_renamed() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let body = multipart_body(&[
        Part {
            name: "file",
            file_name: Some("IMG_0001.png"),
            content: png(21),
        },
        Part {
            name: "file",
            file_name: Some("harbour-master.png"),
            content: png(22),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let answer = json_body(&body);
    // The camera name became `Upload <timestamp>` with the original in
    // the note's title; the meaningful name is kept (ADR-017 §11.2).
    assert_eq!(answer["files"][0]["renamed"], true);
    let renamed_image = answer["files"][0]["image"].as_str().unwrap();
    assert!(
        renamed_image.starts_with("inbox/Upload "),
        "{renamed_image}"
    );
    assert!(renamed_image.ends_with(".png"), "{renamed_image}");
    assert_eq!(answer["files"][1]["image"], "inbox/harbour-master.png");
    assert_eq!(answer["files"][1]["renamed"], false);
    let note = String::from_utf8(
        library_file(&root, &format!("{renamed_image}.md")).expect("the renamed image has a note"),
    )
    .unwrap();
    assert!(note.contains("title: \"IMG_0001.png\""), "{note}");
    assert!(note.contains("original_name: \"IMG_0001.png\""), "{note}");
}

#[tokio::test]
async fn a_folder_field_places_the_upload_there_and_a_collection_field_joins_it() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // The folder field is the endpoint's `--to`.
    let body = multipart_body(&[
        Part {
            name: "folder",
            file_name: None,
            content: b"refs/deep-sea".to_vec(),
        },
        Part {
            name: "file",
            file_name: Some("lantern.png"),
            content: png(3),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(
        json_body(&body)["files"][0]["image"],
        "refs/deep-sea/lantern.png"
    );
    assert!(library_file(&root, "refs/deep-sea/lantern.png").is_some());

    // The collection field is `--to-collection`: the default folder, plus
    // the join (ADR-017 §11.1).
    let body = multipart_body(&[
        Part {
            name: "collection",
            file_name: None,
            content: b"mine.md".to_vec(),
        },
        Part {
            name: "file",
            file_name: Some("harbour.png"),
            content: png(4),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(json_body(&body)["files"][0]["image"], "inbox/harbour.png");
    assert!(library_file(&root, "inbox/harbour.png").is_some());
    let note = std::fs::read_to_string(root.path().join("library/mine.md")).unwrap();
    assert!(note.contains("![[inbox/harbour.png]]"), "{note}");

    // A collection that does not exist refuses the upload outright.
    let body = multipart_body(&[
        Part {
            name: "collection",
            file_name: None,
            content: b"nope.md".to_vec(),
        },
        Part {
            name: "file",
            file_name: Some("later.png"),
            content: png(5),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", text(&body));
    assert!(library_file(&root, "inbox/later.png").is_none());
}

#[tokio::test]
async fn two_uploads_of_the_same_bytes_collide_with_the_suffix() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let bytes = png(31);
    for expected in ["inbox/dawn.png", "inbox/dawn 2.png"] {
        let body = multipart_body(&[Part {
            name: "file",
            file_name: Some("dawn.png"),
            content: bytes.clone(),
        }]);
        let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
        assert_eq!(status, StatusCode::OK, "{}", text(&body));
        assert_eq!(json_body(&body)["files"][0]["image"], expected);
    }
    // The second landed beside the first: the index has no digest row for
    // an image this server has not scanned yet, and a miss copies (the
    // fail-safe direction, ADR-017 §11.3).
    assert_eq!(
        library_file(&root, "inbox/dawn.png").unwrap(),
        library_file(&root, "inbox/dawn 2.png").unwrap()
    );
}

#[tokio::test]
async fn a_markup_upload_is_refused_by_name_and_by_bytes() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // By extension.
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("trap.svg"),
        content: b"<svg></svg>".to_vec(),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));

    // By magic, behind an image name.
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("trap.png"),
        content: b"<svg xmlns=\"urn:x\"></svg>".to_vec(),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));
    assert!(text(&body).contains("SVG"), "{}", text(&body));

    // A mismatch: a PNG name over not-PNG bytes.
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("trap.png"),
        content: b"definitely not a png".to_vec(),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));
    assert!(library_file(&root, "inbox/trap.svg").is_none());
    assert!(library_file(&root, "inbox/trap.png").is_none());
}

// ---------------------------------------------------------------------------
// The caps (ADR-018 §5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_file_over_the_cap_is_refused_and_lands_nothing() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let cap = 32usize;
    let app = router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        ServeConfig {
            max_upload_bytes: cap,
            ..config(&root, true, None)
        },
    );
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("big.png"),
        content: png(9),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", text(&body));
    assert!(text(&body).contains("32 bytes"), "{}", text(&body));
    assert!(library_file(&root, "inbox/big.png").is_none());
}

#[tokio::test]
async fn a_file_at_the_cap_still_lands() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let body_bytes = png(11);
    let cap = body_bytes.len();
    let app = router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        ServeConfig {
            max_upload_bytes: cap,
            ..config(&root, true, None)
        },
    );
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let body = multipart_body(&[Part {
        name: "file",
        file_name: Some("exact.png"),
        content: body_bytes.clone(),
    }]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(library_file(&root, "inbox/exact.png").unwrap(), body_bytes);
}

/// A `folder` field over the control-field cap is refused while it streams and
/// nothing lands (H3): the placement fields carry a path, so the cap is
/// generous, but they are bounded like the file fields.
#[tokio::test]
async fn an_over_the_cap_control_field_is_refused_and_lands_nothing() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let body = multipart_body(&[
        Part {
            name: "folder",
            file_name: None,
            content: vec![b'a'; 8 * 1024],
        },
        Part {
            name: "file",
            file_name: Some("deep.png"),
            content: png(12),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", text(&body));
    assert!(text(&body).contains("folder field"), "{}", text(&body));
    assert!(library_file(&root, "inbox/deep.png").is_none());
}

/// The whole request's file bytes are bounded, not just each file (H3): a
/// batch whose files are each within the per-file cap but whose sum crosses
/// the total cap is refused before anything is written.
#[tokio::test]
async fn an_upload_over_the_total_cap_is_refused() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let one = png(13);
    let cap = one.len();
    let app = router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        ServeConfig {
            max_upload_bytes: cap,
            ..config(&root, true, None)
        },
    );
    let (cookie, csrf) = owner_and_csrf(&app).await;
    // Four files are the total cap (4 x the per-file cap); a fifth crosses it.
    let parts: Vec<Part<'_>> = (0..5)
        .map(|n| Part {
            name: "file",
            file_name: Some(["a.png", "b.png", "c.png", "d.png", "e.png"][n]),
            content: one.clone(),
        })
        .collect();
    let body = multipart_body(&parts);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", text(&body));
    assert!(text(&body).contains("in total"), "{}", text(&body));
    for name in ["a.png", "b.png", "c.png", "d.png", "e.png"] {
        assert!(
            library_file(&root, &format!("inbox/{name}")).is_none(),
            "{name}"
        );
    }
}

/// A request naming both a `folder` and a `collection` is a typed refusal, as
/// the CLI's `--to`/`--to-collection` conflict is (H6): neither silently wins,
/// and nothing lands.
#[tokio::test]
async fn naming_both_a_folder_and_a_collection_is_refused() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let body = multipart_body(&[
        Part {
            name: "folder",
            file_name: None,
            content: b"refs".to_vec(),
        },
        Part {
            name: "collection",
            file_name: None,
            content: b"mine.md".to_vec(),
        },
        Part {
            name: "file",
            file_name: Some("both.png"),
            content: png(14),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&body));
    assert!(
        text(&body).contains("a folder or a collection, not both"),
        "{}",
        text(&body)
    );
    assert!(library_file(&root, "inbox/both.png").is_none());
    assert!(library_file(&root, "refs/both.png").is_none());
}

/// The cap against a real local server on a real socket: the client
/// pushes a body sixty times the cap with no pause, and the test counts
/// what the socket actually took. A server that buffered the field first
/// would read the whole body before answering; this one answers 413 and
/// drops the connection while the client is still mid-write — the
/// "disconnect, no buffering" of ADR-018 §5. Nothing lands either way.
#[tokio::test]
async fn an_oversize_upload_is_cut_off_mid_stream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let cap = 4096usize;
    let app = router(
        FsCatalog::new(root.path().join("library")).unwrap(),
        OriginalPreview,
        ServeConfig {
            max_upload_bytes: cap,
            ..config(&root, true, None)
        },
    );
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // A multipart body whose one file field runs to sixty times the cap.
    let mut head = Vec::new();
    head.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"huge.png\"\r\n\r\n"
        )
        .as_bytes(),
    );
    head.extend_from_slice(&png(9));
    let filler = vec![0x7u8; 64 * 1024];
    let tail = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    let total = head.len() + filler.len() * 60 + tail.len();

    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let request_head = format!(
        "POST /api/write/upload HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Origin: {ORIGIN}\r\n\
         Cookie: {cookie}\r\n\
         x-csrf-token: {csrf}\r\n\
         Content-Type: multipart/form-data; boundary={BOUNDARY}\r\n\
         Content-Length: {total}\r\n\
         \r\n",
    );
    let writer_task = tokio::spawn(async move {
        let mut written = 0usize;
        if writer.write_all(request_head.as_bytes()).await.is_err() {
            return written;
        }
        for chunk in std::iter::once(head.as_slice())
            .chain(std::iter::repeat_n(filler.as_slice(), 60))
            .chain(std::iter::once(tail.as_slice()))
        {
            if writer.write_all(chunk).await.is_err() {
                return written;
            }
            written += chunk.len();
        }
        written
    });

    let status_line = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if reader.read(&mut byte).await.unwrap_or(0) == 0 {
                break;
            }
            buf.push(byte[0]);
            if buf.windows(2).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
                break;
            }
        }
        String::from_utf8_lossy(&buf)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    })
    .await
    .expect("the server answers");

    let written = tokio::time::timeout(std::time::Duration::from_secs(10), writer_task)
        .await
        .expect("the writer returns once the peer is gone")
        .unwrap();

    assert!(
        status_line.contains("413"),
        "the oversize upload answered: {status_line}"
    );
    assert!(
        written < total / 2,
        "the server read the whole body before refusing: {written} of {total}"
    );
    assert!(library_file(&root, "inbox/huge.png").is_none());
}

#[tokio::test]
async fn a_batch_over_the_limit_is_refused_before_anything_is_written() {
    let root = library();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    // One file past the limit: every file is refused with it, because a
    // refusal fails the whole upload before the first byte (ADR-018 §5).
    let limit = 1000;
    let mut parts = Vec::new();
    for index in 0..=limit {
        parts.push(Part {
            name: "file",
            file_name: Some("dawn.png"),
            content: png(index as u8),
        });
    }
    let body = multipart_body(&parts);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", text(&body));
    assert!(text(&body).contains("at most 1000"), "{}", text(&body));
    // The refusal happened before the library was opened for it.
    assert!(!root.path().join("library/inbox").exists());
}

/// On a collection page, `attachmentFolderPath` resolves against the
/// collection note's folder (ADR-017 §6, §11.1): with `./` the image lands
/// beside the note, not at the library root.
#[tokio::test]
async fn a_collection_page_upload_resolves_the_folder_against_the_note() {
    let root = library();
    std::fs::create_dir_all(root.path().join("library/.obsidian")).unwrap();
    std::fs::write(
        root.path().join("library/.obsidian/app.json"),
        b"{\"attachmentFolderPath\": \"./\"}",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("library/refs")).unwrap();
    std::fs::write(
        root.path().join("library/refs/shots.md"),
        b"---\nkind: collection\ntitle: Shots\n---\n\nHello.\n",
    )
    .unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;

    let body = multipart_body(&[
        Part {
            name: "collection",
            file_name: None,
            content: b"refs/shots.md".to_vec(),
        },
        Part {
            name: "file",
            file_name: Some("harbour.png"),
            content: png(9),
        },
    ]);
    let (status, _, body) = send(&app, upload_request(body, &cookie, &csrf, ORIGIN)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(json_body(&body)["files"][0]["image"], "refs/harbour.png");
    assert!(library_file(&root, "refs/harbour.png").is_some());
    assert!(library_file(&root, "harbour.png").is_none());
    let note = std::fs::read_to_string(root.path().join("library/refs/shots.md")).unwrap();
    assert!(note.contains("![[refs/harbour.png]]"), "{note}");
}

#[tokio::test]
async fn tag_batch_preview_apply_and_undo_require_csrf_and_reject_stale_plans() {
    let root = library();
    let note = root.path().join("library/synthetic.md");
    let original = b"---\ntags: [old]\n---\nSynthetic.\n";
    std::fs::write(&note, original).unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let request = |path: &str, body: &serde_json::Value, token: &str| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header("x-csrf-token", token)
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let mut proposal = serde_json::json!({"version":1,"entries":[{"source":"old","target":"new","action":"rename","selected":true,"image_count":1,"reason":"test","alternative_action":null}],"notes":[],"optional_operations":[]});
    let (status, _, _) = send(
        &app,
        request("/api/write/tags/validate-proposal", &proposal, &csrf),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Unselected entries must receive the same validation as CLI list input.
    proposal["entries"][0]["selected"] = serde_json::json!(false);
    proposal["entries"].as_array_mut().unwrap().push(serde_json::json!({"source":"other","target":"valid","action":"rename","selected":true,"image_count":1,"reason":"test","alternative_action":null}));
    proposal["entries"][0]["target"] = serde_json::json!("bad//target");
    let (status, _, _) = send(
        &app,
        request("/api/write/tags/validate-proposal", &proposal, &csrf),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut batch = serde_json::json!({"operations": [{"action":"rename", "source":"old", "target":"new"}], "remember_aliases":true});
    let (status, _, _) = send(&app, request("/api/write/tags/plan", &batch, "bad")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = send(&app, request("/api/write/tags/plan", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let preview = json_body(&body);
    assert_eq!(preview["changed_notes"], 1);
    batch["digest"] = preview["digest"].clone();
    std::fs::write(&note, b"---\ntags: [old]\n---\nChanged.\n").unwrap();
    let (status, _, _) = send(&app, request("/api/write/tags/apply", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = send(&app, request("/api/write/tags/start", &batch, "bad")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&app, request("/api/write/tags/start", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    std::fs::write(&note, original).unwrap();
    let (status, _, body) = send(&app, request("/api/write/tags/apply", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let op = json_body(&body)["op"].as_str().unwrap().to_owned();
    assert!(text(&std::fs::read(&note).unwrap()).contains("new"));
    let (status, _, body) = send(
        &app,
        request("/api/write/undo", &serde_json::json!({"op":op}), &csrf),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(std::fs::read(&note).unwrap(), original);
    let (status, _, body) = send(&app, request("/api/write/tags/plan", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::OK);
    batch["digest"] = json_body(&body)["digest"].clone();
    let (status, _, _) = send(&app, request("/api/write/tags/start", &batch, &csrf)).await;
    assert_eq!(status, StatusCode::OK);
    loop {
        let req = Request::builder()
            .uri("/api/write/tags/progress")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let progress = json_body(&body);
        if progress["state"] == "done" {
            assert_eq!(progress["completed"], progress["total"]);
            assert!(progress["total"].as_u64().unwrap() > 0);
            break;
        }
        assert_eq!(progress["state"], "running", "{progress}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn tag_form_requires_csrf_and_revalidates_before_apply_and_undo() {
    let root = library();
    let note = root.path().join("library/form-synthetic.md");
    let original = b"---\ntags: [old]\n---\nSynthetic.\n";
    std::fs::write(&note, original).unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let request = |path: &str, fields: &[(&str, &str)]| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap()
    };
    let fields = |token: &str, step: &str, digest: &str| {
        vec![
            ("csrf", token.to_owned()),
            ("action", "rename".into()),
            ("source", "old".into()),
            ("target", "new".into()),
            ("step", step.into()),
            ("digest", digest.into()),
        ]
    };
    let encode = |v: Vec<(&str, String)>| {
        v.into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect::<Vec<_>>()
    };
    let send_fields = |path: &str, fields: Vec<(String, String)>| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
            .unwrap()
    };
    let (status, _, _) = send(
        &app,
        send_fields("/api/write/tags/form", encode(fields("bad", "preview", ""))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = send(
        &app,
        send_fields("/api/write/tags/form", encode(fields(&csrf, "preview", ""))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    let html = text(&body);
    let digest = html
        .split("name=\"digest\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    std::fs::write(&note, b"---\ntags: [old]\n---\nChanged.\n").unwrap();
    let (status, _, _) = send(
        &app,
        send_fields(
            "/api/write/tags/form",
            encode(fields(&csrf, "apply", digest)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    std::fs::write(&note, original).unwrap();
    let (status, _, body) = send(
        &app,
        send_fields(
            "/api/write/tags/form",
            encode(fields(&csrf, "apply", digest)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert!(text(&body).contains("Apply running"));
    let op = loop {
        let req = Request::builder()
            .uri("/api/write/tags/progress")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        let progress = json_body(&body);
        if progress["state"] == "done" {
            break progress["op"].as_str().unwrap().to_owned();
        }
        assert_eq!(progress["state"], "running", "{progress}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let page = Request::builder()
        .uri("/tags")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = send(&app, page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&body).contains("Tag apply finished"));
    let (status, _, _) = send(
        &app,
        request("/api/write/tags/undo-form", &[("csrf", &csrf), ("op", &op)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(std::fs::read(&note).unwrap(), original);
}

#[tokio::test]
async fn tag_batch_cleanup_runs_last_and_operation_chains_are_refused() {
    let root = library();
    let note = root.path().join("library/parent-synthetic.md");
    std::fs::write(&note, b"---\ntags: [parent, parent/child, old]\n---\n").unwrap();
    create_owner(&root, "owner@example.com", "secret123");
    let app = app_for(&root, true, None);
    let (cookie, csrf) = owner_and_csrf(&app).await;
    let request = |body: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri("/api/write/tags/plan")
            .header("content-type", "application/json")
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header("x-csrf-token", &csrf)
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (status, _, body) = send(&app, request(serde_json::json!({"operations":[{"action":"rename","source":"old","target":"new"},{"action":"remove_redundant_parent"}]}))).await;
    assert_eq!(status, StatusCode::OK, "{}", text(&body));
    assert_eq!(json_body(&body)["changed_notes"], 1);
    let (status, _, _) = send(&app, request(serde_json::json!({"operations":[{"action":"remove_redundant_parent"},{"action":"remove","source":"old"}]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = send(&app, request(serde_json::json!({"operations":[{"action":"rename","source":"old","target":"middle"},{"action":"remove","source":"middle"}]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
