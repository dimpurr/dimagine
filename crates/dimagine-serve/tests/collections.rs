use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use dimagine_serve::{
    router, Catalog, CatalogError, DiagnosticKind, FsCatalog, OriginalPreview, ServeConfig,
};
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png-data";

fn write_file(root: &TempDir, rel: &str, bytes: impl AsRef<[u8]>) {
    let path = root.path().join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn library() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    write_file(&root, "refs/target.png", PNG);
    write_file(
        &root,
        "sub/relative.md",
        "![[../refs/target.png]]\nA relative target.\n",
    );
    write_file(
        &root,
        "no_kind.md",
        "![[refs/target.png]]\nNo kind marker.\n",
    );
    write_file(
        &root,
        "kind_only.md",
        "---\nkind: collection\n---\nNo embeds here.\n",
    );
    write_file(&root, "dup_a/photo.png", PNG);
    write_file(&root, "dup_b/photo.png", PNG);
    write_file(&root, "ambiguous.md", "![[photo.png]]\n");
    write_file(&root, "missing.md", "![[refs/nope.png]]\n");
    write_file(&root, "solo/pic.png", PNG);
    write_file(&root, "solo/pic.png.md", "![[pic.png]]\n");
    write_file(&root, "uni/雨.png", PNG);
    write_file(&root, "unicode.md", "![[uni/雨.png]]\n");
    root
}

fn catalog(root: &TempDir) -> FsCatalog {
    FsCatalog::new(root.path()).unwrap()
}

fn request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

async fn login(app: &axum::Router) -> String {
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
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
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

#[test]
fn dotdot_relative_embed_resolves_against_the_note_folder() {
    let root = library();
    let collection = catalog(&root).collection("sub/relative.md").unwrap();
    assert_eq!(collection.members.len(), 1);
    assert_eq!(collection.members[0].path, "refs/target.png");
    assert_eq!(collection.members[0].caption, "A relative target.");
    assert!(collection.diagnostics.is_empty());
}

#[test]
fn collection_without_kind_is_a_collection() {
    let root = library();
    let catalog = catalog(&root);
    let collection = catalog.collection("no_kind.md").unwrap();
    assert_eq!(collection.members.len(), 1);
    assert_eq!(collection.members[0].path, "refs/target.png");
    let listed = catalog.list_collections("").unwrap();
    assert!(listed.iter().any(|c| c.path == "no_kind.md"));
}

#[test]
fn kind_marker_only_affects_listing() {
    let root = library();
    let collection = catalog(&root).collection("kind_only.md").unwrap();
    assert!(collection.members.is_empty());
    assert!(collection.diagnostics.is_empty());
}

#[test]
fn ambiguous_bare_name_is_reported_with_candidates() {
    let root = library();
    let collection = catalog(&root).collection("ambiguous.md").unwrap();
    assert!(collection.members.is_empty());
    assert_eq!(collection.diagnostics.len(), 1);
    let diagnostic = &collection.diagnostics[0];
    assert_eq!(diagnostic.kind, DiagnosticKind::Ambiguous);
    assert_eq!(diagnostic.target, "photo.png");
    let mut candidates = diagnostic.candidates.clone();
    candidates.sort();
    assert_eq!(candidates, ["dup_a/photo.png", "dup_b/photo.png"]);
}

#[test]
fn missing_member_is_reported_not_dropped() {
    let root = library();
    let collection = catalog(&root).collection("missing.md").unwrap();
    assert!(collection.members.is_empty());
    assert_eq!(collection.diagnostics.len(), 1);
    assert_eq!(collection.diagnostics[0].kind, DiagnosticKind::Missing);
    assert_eq!(collection.diagnostics[0].target, "refs/nope.png");
}

#[test]
fn self_embed_does_not_make_an_image_note_a_collection() {
    let root = library();
    let catalog = catalog(&root);
    assert!(matches!(
        catalog.collection("solo/pic.png.md"),
        Err(CatalogError::NotFound)
    ));
    let listed = catalog.list_collections("solo").unwrap();
    assert!(listed.is_empty());
}

#[test]
fn unicode_embed_resolves_and_round_trips() {
    let root = library();
    let collection = catalog(&root).collection("unicode.md").unwrap();
    assert_eq!(collection.members.len(), 1);
    assert_eq!(collection.members[0].path, "uni/雨.png");
}

#[tokio::test]
async fn collection_page_shows_ambiguous_and_missing_diagnostics() {
    let root = library();
    let app = router(catalog(&root), OriginalPreview, ServeConfig::default());
    let cookie = login(&app).await;
    let ambiguous = app
        .clone()
        .oneshot(request("/collection/ambiguous.md", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(ambiguous.status(), StatusCode::OK);
    let html = body(ambiguous).await;
    assert!(html.contains("matches several files"), "{html}");
    assert!(html.contains("dup_a/photo.png"));
    assert!(html.contains("dup_b/photo.png"));
    let missing = app
        .oneshot(request("/collection/missing.md", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
    assert!(body(missing).await.contains("matches no file"));
}
