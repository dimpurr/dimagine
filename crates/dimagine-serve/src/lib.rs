//! Read-only, passcode-protected web viewer for a dimagine library.
use axum::{
    body::Body,
    extract::{Form, Path, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use pulldown_cmark::{html, Event, Options, Parser};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Component, Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp",
];
const SESSION_SECONDS: u64 = 60 * 60 * 24 * 30;

/// A library item shown by the folder view.
#[derive(Clone, Debug, Serialize)]
pub struct ImageEntry {
    /// Path relative to the library root, using forward slashes.
    pub path: String,
    /// Display name.
    pub name: String,
}

/// A collection note and its image members in document order.
#[derive(Clone, Debug, Serialize)]
pub struct Collection {
    /// Collection note path relative to the library root.
    pub path: String,
    /// Display title.
    pub title: String,
    /// Ordered image paths.
    pub members: Vec<CollectionMember>,
}

/// One image and its adjacent caption in a collection.
#[derive(Clone, Debug, Serialize)]
pub struct CollectionMember {
    /// Image path relative to the library root.
    pub path: String,
    /// Caption line following its embed, when present.
    pub caption: String,
}

/// Parsed image note details.
#[derive(Clone, Debug, Serialize)]
pub struct ImageDetail {
    /// Image path relative to the library root.
    pub path: String,
    /// YAML front matter properties.
    pub properties: serde_yaml::Value,
    /// Markdown note body rendered to safe HTML by the page handler.
    pub body: String,
    /// Adjacent raw metadata filenames.
    pub raw_files: Vec<String>,
}

/// Read-only library catalog contract.
pub trait Catalog: Send + Sync + 'static {
    /// List images in a folder, with an empty path meaning the library root.
    fn list_folder(&self, folder: &str) -> Result<Vec<ImageEntry>, CatalogError>;
    /// List visible child folders beneath a folder.
    fn list_subfolders(&self, folder: &str) -> Result<Vec<String>, CatalogError>;
    /// List collection notes in a folder.
    fn list_collections(&self, folder: &str) -> Result<Vec<Collection>, CatalogError>;
    /// Return a collection by note path.
    fn collection(&self, path: &str) -> Result<Collection, CatalogError>;
    /// Return image detail by image path.
    fn image_detail(&self, path: &str) -> Result<ImageDetail, CatalogError>;
    /// Resolve and validate a path beneath the library root.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, CatalogError>;
    /// Return the library root.
    fn root(&self) -> &FsPath;
}

/// Preview path provider contract. The fallback uses the original image.
pub trait PreviewProvider: Send + Sync + 'static {
    /// Find a rendition for an image, or return `None` to use the original.
    fn preview_path(&self, image: &str, kind: PreviewKind) -> Option<PathBuf>;
}

/// Requested image rendition.
#[derive(Clone, Copy, Debug)]
pub enum PreviewKind {
    /// Grid thumbnail.
    Thumb,
    /// Detail view image.
    View,
}

/// Filesystem catalog that follows FORMAT §2.2 while scanning.
#[derive(Clone, Debug)]
pub struct FsCatalog {
    root: PathBuf,
}

impl FsCatalog {
    /// Open a catalog rooted at an existing directory.
    pub fn new(root: impl AsRef<FsPath>) -> Result<Self, CatalogError> {
        let root = fs::canonicalize(root).map_err(|_| CatalogError::NotFound)?;
        if !root.is_dir() {
            return Err(CatalogError::NotFound);
        }
        Ok(Self { root })
    }

    fn scan_images(&self, directory: &FsPath) -> Result<Vec<ImageEntry>, CatalogError> {
        let mut out = Vec::new();
        for entry in fs::read_dir(directory).map_err(|_| CatalogError::NotFound)? {
            let entry = entry.map_err(|_| CatalogError::Unreadable)?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if ignored_name(&name)
                || entry
                    .file_type()
                    .map_err(|_| CatalogError::Unreadable)?
                    .is_symlink()
            {
                continue;
            }
            let ty = entry.file_type().map_err(|_| CatalogError::Unreadable)?;
            if ty.is_dir() {
                out.extend(self.scan_images(&path)?);
            } else if ty.is_file() && is_image(&path) {
                let relative = path
                    .strip_prefix(&self.root)
                    .map_err(|_| CatalogError::Forbidden)?;
                out.push(ImageEntry {
                    path: relative.to_string_lossy().replace('\\', "/"),
                    name,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn collections_in(&self, folder: &str) -> Result<Vec<Collection>, CatalogError> {
        let folder_path = self.resolve_path(folder)?;
        let mut items = Vec::new();
        for entry in fs::read_dir(folder_path).map_err(|_| CatalogError::NotFound)? {
            let entry = entry.map_err(|_| CatalogError::Unreadable)?;
            let meta = entry.file_type().map_err(|_| CatalogError::Unreadable)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if ignored_name(&name) || meta.is_symlink() || !meta.is_file() || !name.ends_with(".md")
            {
                continue;
            }
            let text = fs::read_to_string(entry.path()).map_err(|_| CatalogError::Unreadable)?;
            let (props, body) = parse_note(&text);
            if yaml_string(&props, "kind").as_deref() == Some("collection") {
                let entry_path = entry.path();
                let rel = entry_path
                    .strip_prefix(&self.root)
                    .map_err(|_| CatalogError::Forbidden)?;
                let path = rel.to_string_lossy().replace('\\', "/");
                let title = yaml_string(&props, "title")
                    .unwrap_or_else(|| name.trim_end_matches(".md").to_string());
                items.push(Collection {
                    path: path.clone(),
                    title,
                    members: collect_members(&self.root, &path, body),
                });
            }
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(items)
    }
}

impl Catalog for FsCatalog {
    fn list_folder(&self, folder: &str) -> Result<Vec<ImageEntry>, CatalogError> {
        let directory = self.resolve_path(folder)?;
        if !directory.is_dir() {
            return Err(CatalogError::NotFound);
        }
        self.scan_images(&directory)
    }

    fn list_subfolders(&self, folder: &str) -> Result<Vec<String>, CatalogError> {
        let directory = self.resolve_path(folder)?;
        let mut folders = Vec::new();
        for entry in fs::read_dir(directory).map_err(|_| CatalogError::NotFound)? {
            let entry = entry.map_err(|_| CatalogError::Unreadable)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().map_err(|_| CatalogError::Unreadable)?;
            if kind.is_dir() && !kind.is_symlink() && !ignored_name(&name) {
                folders.push(if folder.is_empty() {
                    name
                } else {
                    format!("{}/{name}", folder.trim_end_matches('/'))
                });
            }
        }
        folders.sort();
        Ok(folders)
    }

    fn list_collections(&self, folder: &str) -> Result<Vec<Collection>, CatalogError> {
        self.collections_in(folder)
    }

    fn collection(&self, path: &str) -> Result<Collection, CatalogError> {
        let safe = self.resolve_path(path)?;
        if !safe.is_file() {
            return Err(CatalogError::NotFound);
        }
        let text = fs::read_to_string(&safe).map_err(|_| CatalogError::Unreadable)?;
        let (props, body) = parse_note(&text);
        if yaml_string(&props, "kind").as_deref() != Some("collection") {
            return Err(CatalogError::NotFound);
        }
        Ok(Collection {
            path: path.to_owned(),
            title: yaml_string(&props, "title").unwrap_or_else(|| {
                safe.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .trim_end_matches(".md")
                    .to_string()
            }),
            members: collect_members(&self.root, path, body),
        })
    }

    fn image_detail(&self, path: &str) -> Result<ImageDetail, CatalogError> {
        let image = self.resolve_path(path)?;
        if !image.is_file() || !is_image(&image) {
            return Err(CatalogError::NotFound);
        }
        let note = PathBuf::from(format!("{}.md", image.to_string_lossy()));
        let body = if note.is_file() {
            let text = fs::read_to_string(note).map_err(|_| CatalogError::Unreadable)?;
            let (properties, body) = parse_note(&text);
            (properties, body.to_owned())
        } else {
            (serde_yaml::Value::Null, String::new())
        };
        let (properties, body) = body;
        let parent = image.parent().ok_or(CatalogError::NotFound)?;
        let base = image.file_name().unwrap_or_default().to_string_lossy();
        let mut raw_files = Vec::new();
        for entry in fs::read_dir(parent).map_err(|_| CatalogError::Unreadable)? {
            let entry = entry.map_err(|_| CatalogError::Unreadable)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{base}."))
                && name.ends_with(".json")
                && !entry
                    .file_type()
                    .map_err(|_| CatalogError::Unreadable)?
                    .is_symlink()
            {
                raw_files.push(name);
            }
        }
        raw_files.sort();
        Ok(ImageDetail {
            path: path.to_owned(),
            properties,
            body,
            raw_files,
        })
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf, CatalogError> {
        let relative = FsPath::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(CatalogError::Forbidden);
        }
        let candidate = self.root.join(relative);
        let canonical = fs::canonicalize(&candidate).map_err(|_| CatalogError::NotFound)?;
        if !canonical.starts_with(&self.root) {
            return Err(CatalogError::Forbidden);
        }
        Ok(canonical)
    }

    fn root(&self) -> &FsPath {
        &self.root
    }
}

/// Original-image preview fallback.
#[derive(Clone, Debug, Default)]
pub struct OriginalPreview;
impl PreviewProvider for OriginalPreview {
    fn preview_path(&self, image: &str, _kind: PreviewKind) -> Option<PathBuf> {
        Some(PathBuf::from(image))
    }
}

/// Catalog lookup error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogError {
    NotFound,
    Forbidden,
    Unreadable,
}
impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotFound => "path not found",
            Self::Forbidden => "path is outside the library or invalid",
            Self::Unreadable => "path could not be read",
        })
    }
}
impl std::error::Error for CatalogError {}

/// Viewer configuration.
#[derive(Clone)]
pub struct ServeConfig {
    /// Shared passcode.
    pub passcode: String,
    /// Cookie name used for the in-memory session.
    pub cookie_name: String,
}
impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            passcode: "2333".to_string(),
            cookie_name: "dimagine_session".to_string(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    catalog: Arc<dyn Catalog>,
    previews: Arc<dyn PreviewProvider>,
    config: ServeConfig,
    sessions: Arc<Mutex<HashMap<String, u64>>>,
}

/// Build the read-only viewer router.
pub fn router<C: Catalog, P: PreviewProvider>(
    catalog: C,
    previews: P,
    config: ServeConfig,
) -> Router {
    router_from(Arc::new(catalog), Arc::new(previews), config)
}

/// Build a router from trait objects for later CLI integration.
pub fn router_from(
    catalog: Arc<dyn Catalog>,
    previews: Arc<dyn PreviewProvider>,
    config: ServeConfig,
) -> Router {
    let state = AppState {
        catalog,
        previews,
        config,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/login", get(login_page).post(login))
        .route("/", get(folder_page))
        .route("/folder/*path", get(folder_page))
        .route("/collection/*path", get(collection_page))
        .route("/image/*path", get(image_page))
        .route("/media/*path", get(media))
        .route("/thumb/*path", get(media))
        .route("/api/folder", get(folder_root_json))
        .route("/api/folder/*path", get(folder_json))
        .route("/api/collection/*path", get(collection_json))
        .route("/api/image/*path", get(image_json))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

async fn auth(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    if request.uri().path() == "/login" {
        return next.run(request).await;
    }
    let token = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| cookie_value(v, &state.config.cookie_name));
    let authenticated = if let Some(token) = token {
        let now = now_seconds();
        let mut sessions = state.sessions.lock().unwrap();
        sessions.retain(|_, expiry| *expiry > now);
        sessions.contains_key(&token)
    } else {
        false
    };
    if authenticated {
        return next.run(request).await;
    }
    if request.uri().path().starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    Redirect::to("/login").into_response()
}

async fn login_page() -> Html<String> {
    Html(layout("Sign in", "<form method=\"post\"><label>Passcode <input name=\"passcode\" type=\"password\" autofocus></label><button>Sign in</button></form>"))
}
#[derive(Deserialize)]
struct LoginForm {
    passcode: String,
}
async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if form
        .passcode
        .as_bytes()
        .ct_eq(state.config.passcode.as_bytes())
        .unwrap_u8()
        != 1
    {
        return (StatusCode::UNAUTHORIZED, Html(layout("Sign in", "<p>Incorrect passcode.</p><form method=\"post\"><label>Passcode <input name=\"passcode\" type=\"password\"></label><button>Sign in</button></form>"))).into_response();
    }
    let token = uuid::Uuid::new_v4().to_string();
    state
        .sessions
        .lock()
        .unwrap()
        .insert(token.clone(), now_seconds() + SESSION_SECONDS);
    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={};",
        state.config.cookie_name, token, SESSION_SECONDS
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn folder_page(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let folder = route_tail(uri.path(), "/folder/");
    render_folder(&state, &folder, false)
}
async fn folder_root_json(State(state): State<AppState>) -> Response {
    json_result(render_folder_data(&state, ""))
}
async fn folder_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    json_result(render_folder_data(&state, &path))
}
async fn collection_page(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    match state.catalog.collection(&path) {
        Ok(c) => Html(layout(&c.title, &collection_html(&c))).into_response(),
        Err(e) => error_response(e),
    }
}
async fn collection_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    json_result(state.catalog.collection(&path))
}
async fn image_page(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    match state.catalog.image_detail(&path) {
        Ok(detail) => {
            let props = escape_html(&serde_yaml::to_string(&detail.properties).unwrap_or_default());
            let body = markdown_html(&detail.body);
            let raws = detail
                .raw_files
                .iter()
                .map(|f| format!("<li>{}</li>", escape_html(f)))
                .collect::<String>();
            let content = format!("<img class=\"detail\" src=\"/media/{}\" alt=\"{}\"><h2>Properties</h2><pre>{props}</pre><h2>Note</h2><article>{body}</article><h2>Raw source files</h2><ul>{raws}</ul>", encode_path(&detail.path), escape_html(&detail.path));
            Html(layout(&detail.path, &content)).into_response()
        }
        Err(e) => error_response(e),
    }
}
async fn image_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    match state.catalog.image_detail(&path) {
        Ok(d) => Json(d).into_response(),
        Err(e) => error_response(e),
    }
}
async fn media(
    State(state): State<AppState>,
    Path(path): Path<String>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let original = match state.catalog.resolve_path(&path) {
        Ok(p) => p,
        Err(e) => return error_response(e),
    };
    if !is_image(&original) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let kind = if uri.path().starts_with("/thumb/") {
        PreviewKind::Thumb
    } else {
        PreviewKind::View
    };
    let served = state
        .previews
        .preview_path(&path, kind)
        .and_then(|p| fs::canonicalize(p).ok())
        .filter(|p| p.starts_with(state.catalog.root()) && p.is_file())
        .unwrap_or(original);
    serve_image(&served, &headers)
}

fn serve_image(path: &FsPath, request_headers: &HeaderMap) -> Response {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let modified = metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let etag = format!("\"{:x}-{:x}\"", metadata.len(), modified);
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .as_ref()
            .parse()
            .unwrap(),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    h.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&httpdate::fmt_http_date(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified),
        ))
        .unwrap(),
    );
    if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.body_mut() = Body::empty();
    }
    response
}

#[derive(Serialize)]
struct FolderData {
    folder: String,
    breadcrumbs: Vec<(String, String)>,
    folders: Vec<String>,
    images: Vec<ImageEntry>,
    collections: Vec<Collection>,
}
fn render_folder_data(state: &AppState, folder: &str) -> Result<FolderData, CatalogError> {
    let folders = state.catalog.list_subfolders(folder)?;
    let images_all = state.catalog.list_folder(folder)?;
    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{}/", folder.trim_end_matches('/'))
    };
    let images = images_all
        .into_iter()
        .filter(|i| {
            i.path
                .strip_prefix(&prefix)
                .is_some_and(|tail| !tail.contains('/'))
        })
        .collect();
    let collections = state.catalog.list_collections(folder)?;
    let breadcrumbs = breadcrumbs(folder);
    Ok(FolderData {
        folder: folder.to_owned(),
        breadcrumbs,
        folders,
        images,
        collections,
    })
}
fn render_folder(state: &AppState, folder: &str, _json: bool) -> Response {
    match render_folder_data(state, folder) {
        Ok(data) => {
            let crumbs = data
                .breadcrumbs
                .iter()
                .map(|(label, path)| {
                    if path.is_empty() {
                        format!("<a href=\"/\">{}</a>", escape_html(label))
                    } else {
                        format!(
                            "<a href=\"/folder/{}\">{}</a>",
                            encode_path(path),
                            escape_html(label)
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let folders = data
                .folders
                .iter()
                .map(|folder| {
                    let name = folder.rsplit('/').next().unwrap_or(folder);
                    format!(
                        "<a class=\"tile folder\" href=\"/folder/{}\">📁 {}</a>",
                        encode_path(folder),
                        escape_html(name)
                    )
                })
                .collect::<String>();
            let images = data.images.iter().map(|i| format!("<a class=\"tile\" href=\"/image/{}\"><img loading=\"lazy\" src=\"/thumb/{}\" alt=\"{}\"><span>{}</span></a>", encode_path(&i.path), encode_path(&i.path), escape_html(&i.name), escape_html(&i.name))).collect::<String>();
            let cols = data
                .collections
                .iter()
                .map(|c| {
                    format!(
                        "<li><a href=\"/collection/{}\">{}</a></li>",
                        encode_path(&c.path),
                        escape_html(&c.title)
                    )
                })
                .collect::<String>();
            Html(layout(
                if folder.is_empty() { "Library" } else { folder },
                &format!(
                    "<nav>{crumbs}</nav><ul>{cols}</ul><section class=\"grid\">{folders}{images}</section>"
                ),
            ))
            .into_response()
        }
        Err(e) => error_response(e),
    }
}
fn collection_html(c: &Collection) -> String {
    c.members.iter().map(|m| format!("<figure><a href=\"/image/{}\"><img loading=\"lazy\" src=\"/media/{}\" alt=\"{}\"></a><figcaption>{}</figcaption></figure>", encode_path(&m.path), encode_path(&m.path), escape_html(&m.path), escape_html(&m.caption))).collect::<String>()
}
fn json_result<T: Serialize>(result: Result<T, CatalogError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => error_response(e),
    }
}
fn error_response(e: CatalogError) -> Response {
    match e {
        CatalogError::NotFound => StatusCode::NOT_FOUND.into_response(),
        CatalogError::Forbidden => StatusCode::FORBIDDEN.into_response(),
        CatalogError::Unreadable => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
fn layout(title: &str, body: &str) -> String {
    format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>:root{{color-scheme:light dark;font:16px system-ui}}body{{max-width:1100px;margin:auto;padding:1rem}}a{{color:inherit}}.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(145px,1fr));gap:12px}}.tile img,figure img{{width:100%;height:180px;object-fit:cover;border-radius:8px}}.detail{{max-width:100%;height:auto}}figure{{margin:0 0 1.4rem}}pre{{overflow:auto}}@media(max-width:420px){{.grid{{grid-template-columns:repeat(2,minmax(0,1fr))}}}}</style><h1>{}</h1>{}</html>", escape_html(title), escape_html(title), body)
}
fn markdown_html(markdown: &str) -> String {
    let mut out = String::new();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let safe_events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    html::push_html(&mut out, safe_events);
    out
}
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn ignored_name(s: &str) -> bool {
    s.starts_with('.')
        || s.starts_with("._")
        || s == "Thumbs.db"
        || s.eq_ignore_ascii_case("desktop.ini")
}
fn is_image(path: &FsPath) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| IMAGE_EXTENSIONS.contains(&x.to_ascii_lowercase().as_str()))
}
fn parse_note(text: &str) -> (serde_yaml::Value, &str) {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let yaml = &rest[..end];
            let body = rest[end + 4..].trim_start_matches(['\r', '\n']);
            return (
                serde_yaml::from_str(yaml).unwrap_or(serde_yaml::Value::Null),
                body,
            );
        }
    }
    (serde_yaml::Value::Null, text)
}
fn yaml_string(value: &serde_yaml::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}
fn collect_members(root: &FsPath, note_path: &str, body: &str) -> Vec<CollectionMember> {
    let mut out = Vec::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let target = if let Some(start) = line.find("![[") {
            line[start + 3..]
                .split("]]")
                .next()
                .map(|s| s.split('|').next().unwrap_or(s).trim().to_string())
        } else if let Some(start) = line.find("](") {
            line[start + 2..]
                .split(')')
                .next()
                .map(|s| s.trim_matches(['<', '>']).to_string())
        } else {
            None
        };
        if let Some(target) = target {
            if let Some(path) = resolve_embed(root, note_path, &target) {
                let caption = match lines.peek() {
                    Some(next)
                        if !next.trim().is_empty() && !next.trim_start().starts_with("![[") =>
                    {
                        lines.next().unwrap_or("").to_string()
                    }
                    _ => String::new(),
                };
                out.push(CollectionMember { path, caption });
            }
        }
    }
    out
}
fn resolve_embed(root: &FsPath, note: &str, target: &str) -> Option<String> {
    let note_parent = FsPath::new(note).parent().unwrap_or(FsPath::new(""));
    let candidates = [root.join(target), root.join(note_parent).join(target)];
    for candidate in candidates {
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        if canonical.starts_with(root) && canonical.is_file() && is_image(&canonical) {
            return canonical
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}
fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("Library".to_string(), String::new())];
    let mut acc = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push((part.to_string(), acc.clone()));
    }
    out
}
fn route_tail(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or("").to_string()
}
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|part| {
            part.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}
fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Run the viewer on a pre-bound TCP listener. Callers choose the bind address.
pub async fn serve(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    axum::serve(listener, app).await
}
