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
use dimagine_core::library::{FileClass, FileEntry, Library};
use dimagine_core::links::{extract_markdown_links, Outcome, Resolver};
use percent_encoding::percent_decode_str;
use pulldown_cmark::{html, Event, Options, Parser};
use saphyr::{LoadableYamlNode, Yaml};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path as FsPath, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use unicode_normalization::UnicodeNormalization;

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
    /// Embeds that did not resolve to exactly one image, shown on the
    /// collection page instead of being silently dropped (FORMAT §5.1).
    pub diagnostics: Vec<CollectionDiagnostic>,
}

/// Why one embed is not a member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum DiagnosticKind {
    /// The bare name matched several files; the viewer must not guess
    /// (FORMAT §5.1 rule 3).
    Ambiguous,
    /// The target claims an image but matches nothing (FORMAT §5.1).
    Missing,
}

/// One embed that resolved to no single image.
#[derive(Clone, Debug, Serialize)]
pub struct CollectionDiagnostic {
    /// The embed target as written between the brackets.
    pub target: String,
    /// 1-based line of the embed in the note file.
    pub line: usize,
    pub kind: DiagnosticKind,
    /// Candidate paths when [`DiagnosticKind::Ambiguous`].
    pub candidates: Vec<String>,
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
    pub properties: serde_json::Value,
    /// Front matter parsing error, if the note has malformed YAML.
    pub front_matter_error: Option<String>,
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
///
/// The library is walked once with `dimagine-core` at construction; the walk
/// feeds the link resolver and the collection listing. Note contents are read
/// live on every request, so edits to a note are visible without a restart.
#[derive(Clone, Debug)]
pub struct FsCatalog {
    root: PathBuf,
    library: Library,
    resolver: Arc<Resolver>,
}

impl FsCatalog {
    /// Open a catalog rooted at an existing directory.
    pub fn new(root: impl AsRef<FsPath>) -> Result<Self, CatalogError> {
        let root = fs::canonicalize(root).map_err(|_| CatalogError::NotFound)?;
        if !root.is_dir() {
            return Err(CatalogError::NotFound);
        }
        let library = Library::open(&root).map_err(|_| CatalogError::NotFound)?;
        let resolver = Arc::new(Resolver::new(&library.files));
        Ok(Self {
            root,
            library,
            resolver,
        })
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
            if ty.is_file() && is_image(&path) {
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
        if !folder_path.is_dir() {
            return Err(CatalogError::NotFound);
        }
        let folder_rel = folder_path
            .strip_prefix(&self.root)
            .unwrap_or(&folder_path)
            .to_path_buf();
        let mut items = Vec::new();
        for entry in self.library.files.iter().filter(|entry| {
            matches!(entry.class, FileClass::Note | FileClass::ImageNote)
                && entry.path.parent() == Some(folder_rel.as_path())
        }) {
            let text = fs::read_to_string(self.root.join(&entry.path))
                .map_err(|_| CatalogError::Unreadable)?;
            let parsed = parse_note(&text);
            let (members, diagnostics) =
                collect_collection(&self.resolver, &self.library.files, entry, &parsed, &text);
            if !is_collection(&parsed, &members, &diagnostics) {
                continue;
            }
            let title = yaml_string(&parsed.properties, "title")
                .unwrap_or_else(|| entry.name.trim_end_matches(".md").to_string());
            items.push(Collection {
                path: entry.rel.clone(),
                title,
                members,
                diagnostics,
            });
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
        let native_name = safe.file_name().unwrap_or_default().to_os_string();
        let name = native_name.to_string_lossy().into_owned();
        let class = dimagine_core::library::classify(&name);
        if !matches!(class, FileClass::Note | FileClass::ImageNote) {
            return Err(CatalogError::NotFound);
        }
        let rel = safe
            .strip_prefix(&self.root)
            .map_err(|_| CatalogError::Forbidden)?
            .to_path_buf();
        let text = fs::read_to_string(&safe).map_err(|_| CatalogError::Unreadable)?;
        let parsed = parse_note(&text);
        let note = FileEntry {
            rel: rel.to_string_lossy().into_owned(),
            name: name.clone(),
            path: rel,
            native_name,
            class,
        };
        let (members, diagnostics) =
            collect_collection(&self.resolver, &self.library.files, &note, &parsed, &text);
        if !is_collection(&parsed, &members, &diagnostics) {
            return Err(CatalogError::NotFound);
        }
        Ok(Collection {
            path: path.to_owned(),
            title: yaml_string(&parsed.properties, "title")
                .unwrap_or_else(|| name.trim_end_matches(".md").to_string()),
            members,
            diagnostics,
        })
    }

    fn image_detail(&self, path: &str) -> Result<ImageDetail, CatalogError> {
        let image = self.resolve_path(path)?;
        if !image.is_file() || !is_image(&image) {
            return Err(CatalogError::NotFound);
        }
        let note_path = PathBuf::from(format!("{}.md", image.to_string_lossy()));
        let note_meta = match fs::symlink_metadata(&note_path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(CatalogError::Unreadable),
        };
        let (properties, body, front_matter_error) = if let Some(note_meta) = note_meta {
            if note_meta.file_type().is_symlink() {
                return Err(CatalogError::Forbidden);
            }
            let canonical_note =
                fs::canonicalize(&note_path).map_err(|_| CatalogError::Unreadable)?;
            let note_relative = canonical_note
                .strip_prefix(&self.root)
                .map_err(|_| CatalogError::Forbidden)?;
            if note_relative
                .components()
                .any(|c| ignored_name(&c.as_os_str().to_string_lossy()))
            {
                return Err(CatalogError::Forbidden);
            }
            let text = fs::read_to_string(canonical_note).map_err(|_| CatalogError::Unreadable)?;
            let parsed = parse_note(&text);
            (parsed.properties, parsed.body, parsed.error)
        } else {
            (serde_json::Value::Null, String::new(), None)
        };
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
            front_matter_error,
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
        let mut candidate = self.root.clone();
        for component in relative.components() {
            if let Component::Normal(part) = component {
                let name = part.to_string_lossy();
                if ignored_name(&name) {
                    return Err(CatalogError::Forbidden);
                }
                candidate.push(part);
                let metadata = match fs::symlink_metadata(&candidate) {
                    Ok(m) => m,
                    Err(_) => {
                        let parent = candidate.parent().ok_or(CatalogError::NotFound)?;
                        let mut matched = None;
                        if let Ok(entries) = fs::read_dir(parent) {
                            let part_str = part.to_string_lossy();
                            let part_nfc: String = part_str.nfc().collect();
                            for entry in entries.flatten() {
                                let ename = entry.file_name();
                                let ename_str = ename.to_string_lossy();
                                if ename_str.nfc().eq(part_nfc.chars()) {
                                    candidate.pop();
                                    candidate.push(&ename);
                                    if let Ok(m) = fs::symlink_metadata(&candidate) {
                                        matched = Some(m);
                                        break;
                                    }
                                }
                            }
                        }
                        matched.ok_or(CatalogError::NotFound)?
                    }
                };
                if metadata.file_type().is_symlink() {
                    return Err(CatalogError::Forbidden);
                }
            }
        }
        let canonical = fs::canonicalize(&candidate).map_err(|_| CatalogError::NotFound)?;
        let relative = canonical
            .strip_prefix(&self.root)
            .map_err(|_| CatalogError::Forbidden)?;
        if relative
            .components()
            .any(|c| ignored_name(&c.as_os_str().to_string_lossy()))
        {
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

/// Bounded concurrency limiter for CPU-intensive preview generation.
struct ConcurrencyLimiter {
    active: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl ConcurrencyLimiter {
    fn new(max: usize) -> Self {
        Self {
            active: Mutex::new(0),
            cvar: Condvar::new(),
            max,
        }
    }

    fn acquire(&self) -> ConcurrencyGuard<'_> {
        let mut count = self.active.lock().unwrap();
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap();
        }
        *count += 1;
        ConcurrencyGuard { limiter: self }
    }
}

struct ConcurrencyGuard<'a> {
    limiter: &'a ConcurrencyLimiter,
}

impl<'a> Drop for ConcurrencyGuard<'a> {
    fn drop(&mut self) {
        let mut count = self.limiter.active.lock().unwrap();
        *count -= 1;
        self.limiter.cvar.notify_one();
    }
}

/// Real preview provider backed by `dimagine_preview::ensure`.
pub struct CachedPreview {
    root: PathBuf,
    concurrency: Arc<ConcurrencyLimiter>,
    logged: Arc<Mutex<HashSet<String>>>,
}

impl CachedPreview {
    /// Create a new preview provider for a library root with bounded concurrency (default 4).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_concurrency(root, 4)
    }

    /// Create a new preview provider with a custom concurrency limit.
    pub fn with_concurrency(root: impl Into<PathBuf>, max_concurrency: usize) -> Self {
        let root = root.into();
        let root = fs::canonicalize(&root).unwrap_or(root);
        Self {
            root,
            concurrency: Arc::new(ConcurrencyLimiter::new(max_concurrency.max(1))),
            logged: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl PreviewProvider for CachedPreview {
    fn preview_path(&self, image: &str, kind: PreviewKind) -> Option<PathBuf> {
        let relative = FsPath::new(image);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return None;
        }
        let candidate = self.root.join(relative);
        let source = match fs::canonicalize(&candidate) {
            Ok(c) if c.starts_with(&self.root) && c.is_file() => c,
            _ => {
                // Try resolving with NFC/NFD normalization if exact byte match failed
                let mut matched = self.root.clone();
                for component in relative.components() {
                    if let Component::Normal(part) = component {
                        matched.push(part);
                        if fs::symlink_metadata(&matched).is_err() {
                            let parent = matched.parent()?;
                            let mut resolved = None;
                            if let Ok(entries) = fs::read_dir(parent) {
                                let part_str = part.to_string_lossy();
                                let part_nfc: String = part_str.nfc().collect();
                                for entry in entries.flatten() {
                                    let ename = entry.file_name();
                                    let ename_str = ename.to_string_lossy();
                                    if ename_str.nfc().eq(part_nfc.chars()) {
                                        matched.pop();
                                        matched.push(&ename);
                                        if fs::symlink_metadata(&matched).is_ok() {
                                            resolved = Some(());
                                            break;
                                        }
                                    }
                                }
                            }
                            resolved?;
                        }
                    }
                }
                let Ok(c) = fs::canonicalize(&matched) else {
                    return None;
                };
                if c.starts_with(&self.root) && c.is_file() {
                    c
                } else {
                    return None;
                }
            }
        };
        let preview_kind = match kind {
            PreviewKind::Thumb => dimagine_preview::Kind::Thumb,
            PreviewKind::View => dimagine_preview::Kind::View,
        };
        let _guard = self.concurrency.acquire();
        match dimagine_preview::ensure(&self.root, &source, &[preview_kind]) {
            Ok(renditions) => renditions
                .into_iter()
                .find(|r| r.kind == preview_kind)
                .map(|r| r.path),
            Err(err) => {
                let mut logged = self.logged.lock().unwrap();
                if logged.insert(image.to_string()) {
                    eprintln!("preview generation failed for {image}: {err}");
                }
                None
            }
        }
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
    /// Set this when the viewer is served directly over HTTPS.
    pub https: bool,
}
impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            passcode: "2333".to_string(),
            cookie_name: "dimagine_session".to_string(),
            https: false,
        }
    }
}

#[derive(Clone)]
struct AppState {
    catalog: Arc<dyn Catalog>,
    previews: Arc<dyn PreviewProvider>,
    config: ServeConfig,
    sessions: Arc<Mutex<HashMap<String, u64>>>,
    throttles: Arc<Mutex<ThrottleState>>,
}

#[derive(Default)]
struct ThrottleState {
    clients: HashMap<String, (u32, u64)>,
    global_failures: u32,
}

impl ThrottleState {
    /// Record one failed attempt, returning the escalating delay step.
    /// The client map stays bounded by evicting the least recently seen
    /// client; failure counts saturate instead of overflowing.
    fn record_failure(&mut self, client_key: &str, now: u64) -> u32 {
        self.global_failures = self.global_failures.saturating_add(1);
        if self.clients.len() >= 1024 && !self.clients.contains_key(client_key) {
            if let Some(oldest) = self
                .clients
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(key, _)| key.clone())
            {
                self.clients.remove(&oldest);
            }
        }
        let entry = self
            .clients
            .entry(client_key.to_owned())
            .or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now;
        self.global_failures.max(entry.0)
    }
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
        throttles: Arc::new(Mutex::new(ThrottleState::default())),
    };
    Router::new()
        .route("/login", get(login_page).post(login))
        .route("/", get(folder_page))
        .route("/folder/*path", get(folder_page))
        .route("/collection/*path", get(collection_page))
        .route("/image/*path", get(image_page))
        .route("/media/*path", get(media))
        .route("/thumb/*path", get(media))
        .route("/raw/*path", get(media))
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
        match sessions.get(&token).copied() {
            Some(expiry) if expiry > now => true,
            Some(_) => {
                sessions.remove(&token);
                false
            }
            None => false,
        }
    } else {
        false
    };
    if authenticated {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("private, no-cache"));
        return response;
    }
    if request.uri().path().starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    Redirect::to("/login").into_response()
}

async fn login_page() -> Response {
    let mut response = Html(layout("Sign in", "<form method=\"post\"><label>Passcode <input name=\"passcode\" type=\"password\" autofocus></label><button>Sign in</button></form>")).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
#[derive(Deserialize)]
struct LoginForm {
    passcode: String,
}
async fn login(
    State(state): State<AppState>,
    client: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let client_key = client
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    // Throttle before comparing so parallel guesses cannot bypass the
    // back-off and response timing never reveals whether a guess was
    // correct: every attempt pays the same escalating delay.
    let delay = {
        let mut throttle = state.throttles.lock().unwrap();
        throttle.record_failure(&client_key, now_seconds())
    };
    let millis =
        (100u64.saturating_mul(1u64.checked_shl(delay.min(6)).unwrap_or(u64::MAX))).min(5000);
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
    if form
        .passcode
        .as_bytes()
        .ct_eq(state.config.passcode.as_bytes())
        .unwrap_u8()
        != 1
    {
        let mut response = (StatusCode::UNAUTHORIZED, Html(layout("Sign in", "<p>Incorrect passcode.</p><form method=\"post\"><label>Passcode <input name=\"passcode\" type=\"password\"></label><button>Sign in</button></form>"))).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    {
        let mut throttles = state.throttles.lock().unwrap();
        throttles.clients.remove(&client_key);
        throttles.global_failures = 0;
    }
    let token = uuid::Uuid::new_v4().to_string();
    store_session(
        &mut state.sessions.lock().unwrap(),
        token.clone(),
        now_seconds(),
    );
    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        state.config.cookie_name,
        token,
        SESSION_SECONDS,
        if state.config.https { "; Secure" } else { "" }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn folder_page(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let raw = route_tail(uri.path(), "/folder/");
    let folder = match percent_decode_str(&raw).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || {
        render_folder_data(&AppState { catalog, ..state }, &folder)
    })
    .await
    {
        Ok(Ok(data)) => render_folder_data_html(data),
        Ok(Err(error)) => error_response(error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn folder_root_json(State(state): State<AppState>) -> Response {
    folder_data_blocking(state, String::new()).await
}
async fn folder_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    folder_data_blocking(state, path).await
}
async fn collection_page(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.collection(&path)).await {
        Ok(Ok(c)) => Html(layout(&c.title, &collection_html(&c))).into_response(),
        Ok(Err(e)) => error_response(e),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn collection_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.collection(&path)).await {
        Ok(result) => json_result(result),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn image_page(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.image_detail(&path)).await {
        Ok(Ok(detail)) => {
            let props = escape_html(&yaml_json_string(&detail.properties));
            let body = markdown_html(&detail.body);
            let raws = detail
                .raw_files
                .iter()
                .map(|f| format!("<li>{}</li>", escape_html(f)))
                .collect::<String>();
            let diagnostic = detail
                .front_matter_error
                .as_ref()
                .map(|e| {
                    format!(
                        "<p class=\"error\">Front matter error: {}</p>",
                        escape_html(e)
                    )
                })
                .unwrap_or_default();
            let content = format!("<p><a class=\"original-link\" href=\"/raw/{}\">View original</a></p><img class=\"detail\" src=\"/media/{}\" alt=\"{}\"><h2>Properties</h2><pre>{props}</pre>{diagnostic}<h2>Note</h2><article>{body}</article><h2>Raw source files</h2><ul>{raws}</ul>", encode_path(&detail.path), encode_path(&detail.path), escape_html(&detail.path));
            Html(layout(&detail.path, &content)).into_response()
        }
        Ok(Err(e)) => error_response(e),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn image_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.image_detail(&path)).await {
        Ok(Ok(d)) => Json(d).into_response(),
        Ok(Err(e)) => error_response(e),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn media(
    State(state): State<AppState>,
    Path(path): Path<String>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let kind = if uri.path().starts_with("/thumb/") {
        Some(PreviewKind::Thumb)
    } else if uri.path().starts_with("/raw/") {
        None
    } else {
        Some(PreviewKind::View)
    };
    let catalog = state.catalog.clone();
    let previews = state.previews.clone();
    let root = state.catalog.root().to_path_buf();
    let headers = headers.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let original = catalog
            .resolve_path(&path)
            .map_err(ServeImageError::Catalog)?;
        if !is_image(&original) {
            return Err(ServeImageError::Catalog(CatalogError::NotFound));
        }
        let served = match kind {
            Some(k) => previews
                .preview_path(&path, k)
                .and_then(|p| fs::canonicalize(p).ok())
                .filter(|p| p.starts_with(&root) && p.is_file())
                .unwrap_or_else(|| original.clone()),
            None => original.clone(),
        };
        let is_preview = served != original;
        open_hashed_image(&served).map(|r| (r, is_preview))
    })
    .await;
    match prepared {
        Ok(Ok(((file, metadata, etag, mime, mismatch), is_preview))) => {
            stream_image(file, metadata, etag, mime, mismatch, is_preview, &headers).await
        }
        Ok(Err(ServeImageError::Catalog(error))) => error_response(error),
        Ok(Err(ServeImageError::Io)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

enum ServeImageError {
    Catalog(CatalogError),
    Io,
}
fn open_hashed_image(
    path: &FsPath,
) -> Result<(fs::File, fs::Metadata, String, &'static str, bool), ServeImageError> {
    let mut file = fs::File::open(path).map_err(|_| ServeImageError::Io)?;
    let metadata = file.metadata().map_err(|_| ServeImageError::Io)?;
    let mut hasher = Sha256::new();
    let mut signature = [0u8; 32];
    let mut prefix = Vec::with_capacity(32);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| ServeImageError::Io)?;
        if read == 0 {
            break;
        }
        if prefix.len() < 32 {
            prefix.extend_from_slice(&buffer[..read.min(32 - prefix.len())]);
        }
        hasher.update(&buffer[..read]);
    }
    signature.copy_from_slice(&hasher.finalize());
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ServeImageError::Io)?;
    let etag = format!(
        "\"{}\"",
        signature
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let mime = detected_image_mime(&prefix).unwrap_or("application/octet-stream");
    let extension_mime = mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream");
    Ok((file, metadata, etag, mime, mime != extension_mime))
}

async fn stream_image(
    file: fs::File,
    metadata: fs::Metadata,
    etag: String,
    mime: &'static str,
    mismatch: bool,
    is_preview: bool,
    request_headers: &HeaderMap,
) -> Response {
    let file = tokio::fs::File::from_std(file);
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);
    let modified = metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut response = Response::new(body);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let cache_control = if is_preview {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if mismatch {
        h.insert(
            "x-dimagine-extension-mismatch",
            HeaderValue::from_static("true"),
        );
    }
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
        .is_some_and(|value| etag_header_matches(value, &etag))
    {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.body_mut() = Body::empty();
    }
    response
}

fn etag_header_matches(header: &str, etag: &str) -> bool {
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

fn detected_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        match &bytes[8..12] {
            b"avif" | b"avis" => Some("image/avif"),
            b"heic" | b"heix" | b"hevc" | b"hevx" => Some("image/heic"),
            b"mif1" => Some("image/heif"),
            _ => None,
        }
    } else {
        None
    }
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
async fn folder_data_blocking(state: AppState, folder: String) -> Response {
    match tokio::task::spawn_blocking(move || render_folder_data(&state, &folder)).await {
        Ok(result) => json_result(result),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
fn render_folder_data_html(data: FolderData) -> Response {
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
        if data.folder.is_empty() {
            "Library"
        } else {
            &data.folder
        },
        &format!(
            "<nav>{crumbs}</nav><ul>{cols}</ul><section class=\"grid\">{folders}{images}</section>"
        ),
    ))
    .into_response()
}
fn collection_html(c: &Collection) -> String {
    let figures: String = c.members.iter().map(|m| format!("<figure><a href=\"/image/{}\"><img loading=\"lazy\" src=\"/media/{}\" alt=\"{}\"></a><figcaption>{}</figcaption></figure>", encode_path(&m.path), encode_path(&m.path), escape_html(&m.path), escape_html(&m.caption))).collect();
    let diagnostics: String = c.diagnostics.iter().map(|d| match d.kind {
        DiagnosticKind::Ambiguous => format!(
            "<p class=\"error\">Line {}: <code>{}</code> matches several files, not guessing: {}</p>",
            d.line,
            escape_html(&d.target),
            d.candidates
                .iter()
                .map(|candidate| escape_html(candidate))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DiagnosticKind::Missing => format!(
            "<p class=\"error\">Line {}: <code>{}</code> matches no file</p>",
            d.line,
            escape_html(&d.target)
        ),
    }).collect();
    format!("{figures}{diagnostics}")
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
    format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>:root{{color-scheme:light dark;font:16px system-ui}}body{{max-width:1100px;margin:auto;padding:1rem}}a{{color:inherit}}.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(145px,1fr));gap:12px}}.tile{{display:flex;flex-direction:column;text-decoration:none}}.tile img{{width:100%;aspect-ratio:1;object-fit:cover;border-radius:8px}}.tile span{{margin-top:4px;font-size:0.85rem;overflow-wrap:break-word}}.tile.folder{{aspect-ratio:1;display:flex;align-items:center;justify-content:center;background:rgba(128,128,128,0.15);border-radius:8px;padding:0.5rem;text-align:center;box-sizing:border-box}}.detail{{max-width:100%;height:auto}}figure{{margin:0 0 1.4rem}}figure img{{max-width:100%;height:auto;border-radius:8px}}pre{{overflow:auto}}@media(max-width:420px){{.grid{{grid-template-columns:repeat(2,minmax(0,1fr))}}}}</style><h1>{}</h1>{}</html>", escape_html(title), escape_html(title), body)
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
    let mut sanitizer = ammonia::Builder::default();
    sanitizer.url_schemes(["http", "https", "mailto"].into_iter().collect());
    sanitizer.clean(&out).to_string()
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
struct ParsedNote {
    properties: serde_json::Value,
    body: String,
    body_line: usize,
    error: Option<String>,
}
fn parse_note(text: &str) -> ParsedNote {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return ParsedNote {
            properties: serde_json::Value::Null,
            body: String::new(),
            body_line: 1,
            error: None,
        };
    };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return ParsedNote {
            properties: serde_json::Value::Null,
            body: text.to_owned(),
            body_line: 1,
            error: None,
        };
    }
    let mut offset = first.len();
    let yaml_start = offset;
    let mut line_no = 1usize;
    for line in lines {
        line_no += 1;
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let yaml = &text[yaml_start..offset];
            let body = text[offset + line.len()..].to_owned();
            let body_line = line_no + 1;
            return match Yaml::load_from_str(yaml) {
                Ok(documents) if documents.len() == 1 => ParsedNote {
                    properties: yaml_to_json(&documents[0]),
                    body,
                    body_line,
                    error: None,
                },
                Ok(_) => ParsedNote {
                    properties: serde_json::Value::Null,
                    body,
                    body_line,
                    error: Some("front matter must contain exactly one YAML document".to_owned()),
                },
                Err(error) => ParsedNote {
                    properties: serde_json::Value::Null,
                    body,
                    body_line,
                    error: Some(error.to_string()),
                },
            };
        }
        offset += line.len();
    }
    ParsedNote {
        properties: serde_json::Value::Null,
        body: text.to_owned(),
        body_line: 2,
        error: Some("front matter has no closing delimiter".to_owned()),
    }
}
fn yaml_to_json(yaml: &Yaml<'_>) -> serde_json::Value {
    if let Some(value) = yaml.as_str() {
        return serde_json::Value::String(value.to_owned());
    }
    if let Some(value) = yaml.as_bool() {
        return serde_json::Value::Bool(value);
    }
    if let Some(value) = yaml.as_integer() {
        return serde_json::Value::Number(value.into());
    }
    if let Some(value) = yaml.as_floating_point() {
        return serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    if yaml.is_null() {
        return serde_json::Value::Null;
    }
    if let Some(sequence) = yaml.as_vec() {
        return serde_json::Value::Array(sequence.iter().map(yaml_to_json).collect());
    }
    if let Some(mapping) = yaml.as_mapping() {
        let mut object = serde_json::Map::new();
        for (key, value) in mapping {
            let key = key
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{key:?}"));
            object.insert(key, yaml_to_json(value));
        }
        return serde_json::Value::Object(object);
    }
    serde_json::Value::Null
}
fn yaml_json_string(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_owned())
}
fn yaml_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}
/// A note is a collection when it embeds at least one image (FORMAT §5), when
/// an embed is reported as ambiguous or missing, or when it carries
/// `kind: collection`, which exists so tools list it (FORMAT §5). An image
/// note's self-embed never counts (FORMAT §3.2).
fn is_collection(
    parsed: &ParsedNote,
    members: &[CollectionMember],
    diagnostics: &[CollectionDiagnostic],
) -> bool {
    !members.is_empty()
        || !diagnostics.is_empty()
        || yaml_string(&parsed.properties, "kind").as_deref() == Some("collection")
}

/// Extract the image members of one note with the shared core parser and
/// resolver, and collect the embeds that resolved to no single image.
fn collect_collection(
    resolver: &Resolver,
    files: &[FileEntry],
    note: &FileEntry,
    parsed: &ParsedNote,
    text: &str,
) -> (Vec<CollectionMember>, Vec<CollectionDiagnostic>) {
    let links = extract_markdown_links(&parsed.body, parsed.body_line);
    let lines: Vec<&str> = text.lines().collect();
    let mut members = Vec::new();
    let mut diagnostics = Vec::new();
    for link in links.iter().filter(|link| link.syntax.is_strong_image()) {
        match resolver.resolve(&link.target, note, link.syntax) {
            Outcome::Resolved(idx) if files[idx].class == FileClass::Image => {
                let image = &files[idx];
                if is_self_embed(note, image) {
                    continue;
                }
                members.push(CollectionMember {
                    path: image.rel.clone(),
                    caption: caption_after(&lines, link.line),
                });
            }
            Outcome::Ambiguous(hits) => diagnostics.push(CollectionDiagnostic {
                target: link.target.clone(),
                line: link.line,
                kind: DiagnosticKind::Ambiguous,
                candidates: hits.iter().map(|&idx| files[idx].rel.clone()).collect(),
            }),
            Outcome::NotFound => diagnostics.push(CollectionDiagnostic {
                target: link.target.clone(),
                line: link.line,
                kind: DiagnosticKind::Missing,
                candidates: Vec::new(),
            }),
            Outcome::NotImageTarget | Outcome::Resolved(_) => {}
        }
    }
    (members, diagnostics)
}

/// FORMAT §3.2: an image note's embed of its own image is a preview, not a
/// membership.
fn is_self_embed(note: &FileEntry, image: &FileEntry) -> bool {
    note.class == FileClass::ImageNote
        && note
            .paired_image_path()
            .is_some_and(|paired| paired == image.path)
}

/// The caption is the line directly after an embed, when it is neither empty
/// nor itself an embed (FORMAT §5).
fn caption_after(lines: &[&str], link_line: usize) -> String {
    let Some(next) = lines.get(link_line) else {
        return String::new();
    };
    let trimmed = next.trim_start();
    if trimmed.is_empty() || trimmed.starts_with("![") {
        return String::new();
    }
    next.to_string()
}

fn store_session(sessions: &mut HashMap<String, u64>, token: String, now: u64) {
    sessions.retain(|_, expiry| *expiry > now);
    if sessions.len() >= 10_000 {
        if let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, expiry)| *expiry)
            .map(|(token, _)| token.clone())
        {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(token, now + SESSION_SECONDS);
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
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    config: &ServeConfig,
) -> std::io::Result<()> {
    let address = listener.local_addr()?;
    if default_passcode_warning(config, address) {
        eprintln!("WARNING: the default passcode is active on a non-loopback address");
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
}

fn default_passcode_warning(config: &ServeConfig, address: std::net::SocketAddr) -> bool {
    config.passcode == "2333" && !address.ip().is_loopback()
}

#[cfg(test)]
mod regression_unit_tests {
    use super::*;

    #[test]
    fn session_storage_prunes_expired_tokens_and_stays_bounded() {
        let mut sessions = (0..10_000)
            .map(|index| (format!("session-{index}"), 100 + index))
            .collect::<HashMap<_, _>>();
        store_session(&mut sessions, "fresh".to_owned(), 50);
        assert_eq!(sessions.len(), 10_000);
        assert!(sessions.contains_key("fresh"));
        assert!(!sessions.contains_key("session-0"));

        sessions.insert("expired".to_owned(), 1);
        store_session(&mut sessions, "after-expiry".to_owned(), 200);
        assert!(!sessions.contains_key("expired"));
        assert!(sessions.contains_key("after-expiry"));
        assert!(sessions.len() <= 10_000);
    }

    #[test]
    fn throttle_state_stays_bounded_and_saturates() {
        let mut throttle = ThrottleState::default();
        for index in 0..2000u64 {
            throttle.record_failure(&format!("client-{index}"), index);
        }
        assert!(throttle.clients.len() <= 1024);
        assert_eq!(throttle.global_failures, 2000);
        let mut client_only = ThrottleState::default();
        let mut last = 0;
        for index in 0..10u64 {
            last = client_only.record_failure("one", index);
            assert_eq!(last, index as u32 + 1);
        }
        assert_eq!(last, 10);
        client_only.global_failures = u32::MAX;
        client_only.record_failure("one", 99);
        assert_eq!(client_only.global_failures, u32::MAX);
    }

    #[test]
    fn default_passcode_warning_only_applies_to_non_loopback_binds() {
        let config = ServeConfig::default();
        assert!(!default_passcode_warning(
            &config,
            "127.0.0.1:3000".parse().unwrap()
        ));
        assert!(default_passcode_warning(
            &config,
            "0.0.0.0:3000".parse().unwrap()
        ));
        let custom = ServeConfig {
            passcode: "different".to_owned(),
            ..config
        };
        assert!(!default_passcode_warning(
            &custom,
            "0.0.0.0:3000".parse().unwrap()
        ));
    }
}
