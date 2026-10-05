//! Read-only, passcode-protected web viewer for a dimagine library.

pub mod accounts;

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
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Shared passcode.
    pub passcode: String,
    /// Cookie name used for the in-memory session.
    pub cookie_name: String,
    /// Set this when the viewer is served directly over HTTPS.
    pub https: bool,
    /// Trusted reverse proxy IP addresses.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// State directory for accounts.json.
    pub data_dir: PathBuf,
}
impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            passcode: "2333".to_string(),
            cookie_name: "dimagine_session".to_string(),
            https: false,
            trusted_proxies: Vec::new(),
            data_dir: accounts::default_data_dir(),
        }
    }
}

/// Resolve client IP for rate limiting and logging.
///
/// When the direct TCP peer is a trusted reverse proxy, the client IP is extracted
/// from the LAST hop of the `X-Forwarded-For` header. For untrusted peers (or if the
/// header is absent/malformed), the TCP peer IP is used directly, ignoring the header.
pub fn resolve_client_ip(
    peer_addr: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
    trusted_proxies: &[std::net::IpAddr],
) -> String {
    let Some(peer) = peer_addr else {
        return "unknown".to_string();
    };
    let peer_ip = peer.ip();
    if trusted_proxies.contains(&peer_ip) {
        if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last_hop) = forwarded.split(',').next_back().map(str::trim) {
                if let Ok(ip) = last_hop.parse::<std::net::IpAddr>() {
                    return ip.to_string();
                }
            }
        }
    }
    peer_ip.to_string()
}

#[derive(Clone)]
struct AppState {
    catalog: Arc<dyn Catalog>,
    previews: Arc<dyn PreviewProvider>,
    config: ServeConfig,
    sessions: Arc<Mutex<HashMap<String, u64>>>,
    throttles: Arc<Mutex<ThrottleState>>,
    admission: Arc<tokio::sync::Semaphore>,
    login_slots: Arc<tokio::sync::Semaphore>,
}

/// At most this many login attempts may be in flight at once, across all
/// clients. Guesses are refused here — before the passcode is looked at — so
/// a brute-force wave cannot test many passcodes at the same time.
pub const LOGIN_CONCURRENCY_LIMIT: usize = 2;

/// Attempts one client may spend inside `LOGIN_BUDGET_WINDOW_SECS` before the
/// server stops comparing its guesses. Charged before the comparison, so the
/// passcode is never even looked at once the budget is gone.
pub const LOGIN_FAILURE_BUDGET: u32 = 10;

/// Attempts all clients together may spend inside `LOGIN_BUDGET_WINDOW_SECS`.
/// This is the backstop that keeps the per-client budget from being farmed out
/// across many source addresses, and it is not affected by client eviction.
pub const LOGIN_GLOBAL_FAILURE_BUDGET: u32 = 100;

/// Length of the login attempt budget window. Budgets are restored, never
/// permanently withdrawn, so there is no lockout.
pub const LOGIN_BUDGET_WINDOW_SECS: u64 = 15 * 60;

/// Clients kept in memory. Past this the least recently seen one is dropped;
/// forgetting a client only restores that client's own budget, never the
/// global one.
const MAX_TRACKED_CLIENTS: usize = 1024;

/// Bound on concurrently expensive requests: directory listings and
/// media streaming (which may generate previews). Excess requests get
/// a 503 with Retry-After instead of queueing without limit.
const ADMISSION_LIMIT: usize = 8;

fn acquire_admission(state: &AppState) -> Option<tokio::sync::OwnedSemaphorePermit> {
    state.admission.clone().try_acquire_owned().ok()
}

fn admission_denied() -> Response {
    let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// Attempts spent by one client (or by every client together) inside the
/// current budget window.
#[derive(Clone, Copy, Default)]
struct LoginBudget {
    spent: u32,
    window_start: u64,
    last_seen: u64,
}

impl LoginBudget {
    /// Attempts spent in the window containing `now`, starting a fresh window
    /// first if the previous one has passed. A clock that steps backwards keeps
    /// the current window rather than handing out a second budget.
    fn spent_in_window(&mut self, now: u64) -> u32 {
        if now.saturating_sub(self.window_start) >= LOGIN_BUDGET_WINDOW_SECS {
            self.window_start = now;
            self.spent = 0;
        }
        self.last_seen = now;
        self.spent
    }

    fn charge(&mut self, now: u64) {
        self.spent = self.spent.saturating_add(1);
        self.last_seen = now;
    }

    /// Seconds until this window rolls over and the budget is restored.
    fn retry_after(&self, now: u64) -> u64 {
        LOGIN_BUDGET_WINDOW_SECS
            .saturating_sub(now.saturating_sub(self.window_start))
            .max(1)
    }
}

#[derive(Default)]
struct ThrottleState {
    clients: HashMap<String, LoginBudget>,
    global: LoginBudget,
}

/// What the pre-comparison login gate decided about one attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoginGate {
    /// The attempt may be compared against the passcode. Carries the
    /// escalating delay step.
    Compare(u32),
    /// Refused without comparing; `retry_after` seconds until the budget is
    /// restored.
    Refused { retry_after: u64 },
}

impl ThrottleState {
    /// Decide whether one login attempt may be compared, charging it to both
    /// the per-client and the global budget when it may. Runs before the
    /// passcode is read, so a refused attempt costs the caller nothing and
    /// reveals nothing about the guess.
    fn admit(&mut self, client_key: &str, now: u64) -> LoginGate {
        let global = self.global.spent_in_window(now);
        let global_retry = self.global.retry_after(now);
        let (spent, client_retry) = match self.clients.get_mut(client_key) {
            Some(client) => {
                let spent = client.spent_in_window(now);
                (spent, client.retry_after(now))
            }
            None => (0, LOGIN_BUDGET_WINDOW_SECS),
        };
        if spent >= LOGIN_FAILURE_BUDGET {
            return LoginGate::Refused {
                retry_after: client_retry,
            };
        }
        if global >= LOGIN_GLOBAL_FAILURE_BUDGET {
            return LoginGate::Refused {
                retry_after: global_retry,
            };
        }
        self.charge_client(client_key, now);
        self.global.charge(now);
        LoginGate::Compare(spent.saturating_add(1).max(global.saturating_add(1)))
    }

    /// Charge an admitted attempt, keeping the client map bounded by evicting
    /// the least recently seen client.
    fn charge_client(&mut self, client_key: &str, now: u64) {
        if self.clients.len() >= MAX_TRACKED_CLIENTS && !self.clients.contains_key(client_key) {
            if let Some(oldest) = self
                .clients
                .iter()
                .min_by_key(|(_, budget)| budget.last_seen)
                .map(|(key, _)| key.clone())
            {
                self.clients.remove(&oldest);
            }
        }
        // A first attempt starts its window now, not at zero: an entry left
        // with a zeroed window start would look long expired and its next
        // attempt would be admitted as a free budget.
        self.clients
            .entry(client_key.to_owned())
            .or_insert(LoginBudget {
                window_start: now,
                ..LoginBudget::default()
            })
            .charge(now);
    }

    /// A successful login restores that client's budget. The global budget is
    /// deliberately left alone: a success must not be able to hand an attacker
    /// a fresh global allowance.
    fn record_success(&mut self, client_key: &str) {
        self.clients.remove(client_key);
    }
}

/// Refuse a login attempt without comparing it: 429 with `Retry-After` and an
/// empty body, so a refused attempt is indistinguishable from any other.
fn login_refused(retry_after: u64) -> Response {
    let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
        admission: Arc::new(tokio::sync::Semaphore::new(ADMISSION_LIMIT)),
        login_slots: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONCURRENCY_LIMIT)),
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
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let client_key = resolve_client_ip(
        client.map(|c| c.0),
        &headers,
        &state.config.trusted_proxies,
    );
    // Bound how many guesses are in flight before anything else: a parallel
    // wave of guesses is refused without the passcode ever being compared.
    let _slot = match state.login_slots.clone().try_acquire_owned() {
        Ok(slot) => slot,
        Err(_) => return login_refused(1),
    };
    // Then charge the attempt to the per-client and the global budget, still
    // before comparing, so an exhausted budget refuses without comparing and
    // response timing never reveals whether a guess was correct: every attempt
    // that is compared pays the same escalating delay.
    let delay_step = {
        let mut throttle = state.throttles.lock().unwrap();
        match throttle.admit(&client_key, now_seconds()) {
            LoginGate::Compare(step) => step,
            LoginGate::Refused { retry_after } => return login_refused(retry_after),
        }
    };
    let millis =
        (100u64.saturating_mul(1u64.checked_shl(delay_step.min(6)).unwrap_or(u64::MAX))).min(5000);
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
        throttles.record_success(&client_key);
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
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
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
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    folder_data_blocking(state, String::new()).await
}
async fn folder_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    folder_data_blocking(state, path).await
}
async fn collection_page(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.collection(&path)).await {
        Ok(Ok(c)) => Html(layout(&c.title, &collection_html(&c))).into_response(),
        Ok(Err(e)) => error_response(e),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn collection_json(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
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
    let permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
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
        open_hashed_image(&served)
    })
    .await;
    match prepared {
        Ok(Ok((file, metadata, etag, mime, mismatch))) => {
            stream_image(file, metadata, etag, mime, mismatch, &headers, permit).await
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

/// A streamed file that keeps its admission permit until the last byte.
struct AdmittedFile {
    inner: tokio::fs::File,
    _admission: tokio::sync::OwnedSemaphorePermit,
}

impl tokio::io::AsyncRead for AdmittedFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

async fn stream_image(
    file: fs::File,
    metadata: fs::Metadata,
    etag: String,
    mime: &'static str,
    mismatch: bool,
    request_headers: &HeaderMap,
    admission: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let file = tokio::fs::File::from_std(file);
    // The admission permit lives for the whole stream, so slow
    // readers count against the concurrency bound while streaming.
    let stream = tokio_util::io::ReaderStream::new(AdmittedFile {
        inner: file,
        _admission: admission,
    });
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
    // Rendition URLs carry no content hash, so they must not be
    // cached as immutable: the source file or a regenerated
    // preview can change under the same URL. The ETag is a
    // SHA-256 of the served bytes, so `no-cache` revalidation is
    // exact and costs one 304 per reuse.
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
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
    fn login_budget_bounds_comparisons_per_client_and_globally() {
        // One client cannot spend more than its per-window budget, however
        // many guesses it fires: only the first ones are ever compared.
        let mut throttle = ThrottleState::default();
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            // Two guesses per second: the whole wave is inside one window.
            if matches!(throttle.admit("one", attempt / 2), LoginGate::Compare(_)) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_FAILURE_BUDGET);
        // A refused attempt says when the budget comes back...
        assert!(matches!(
            throttle.admit("one", LOGIN_BUDGET_WINDOW_SECS / 2),
            LoginGate::Refused { retry_after } if retry_after > 0
        ));
        // ...and a window that has passed restores it: no permanent lockout.
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            let now = LOGIN_BUDGET_WINDOW_SECS + attempt / 2;
            if matches!(throttle.admit("one", now), LoginGate::Compare(_)) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_FAILURE_BUDGET);

        // Spreading the guesses over many clients cannot beat the global
        // budget either.
        let mut throttle = ThrottleState::default();
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            if matches!(
                throttle.admit(&format!("client-{attempt}"), 0),
                LoginGate::Compare(_)
            ) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_GLOBAL_FAILURE_BUDGET);
        assert!(matches!(
            throttle.admit("client-new", 0),
            LoginGate::Refused { .. }
        ));

        // A success restores that client's own budget and only that client's:
        // the global allowance is not handed back.
        let mut throttle = ThrottleState::default();
        for attempt in 0..LOGIN_FAILURE_BUDGET as u64 {
            throttle.admit("one", attempt);
        }
        throttle.record_success("one");
        assert!(matches!(
            throttle.admit("one", LOGIN_FAILURE_BUDGET as u64),
            LoginGate::Compare(_)
        ));
        assert_eq!(throttle.clients["one"].spent, 1);
        assert_eq!(
            throttle.global.spent,
            LOGIN_FAILURE_BUDGET + 1,
            "a success must not hand back the global allowance"
        );
    }

    #[test]
    fn login_budget_keeps_tracked_clients_bounded_and_saturates() {
        // One window per global budget's worth of guesses, so the map really
        // does overflow and the eviction path runs.
        let mut throttle = ThrottleState::default();
        let mut admitted = 0;
        for index in 0..2_000u64 {
            let now = index / LOGIN_GLOBAL_FAILURE_BUDGET as u64 * LOGIN_BUDGET_WINDOW_SECS;
            if matches!(
                throttle.admit(&format!("client-{index}"), now),
                LoginGate::Compare(_)
            ) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 2_000);
        assert!(throttle.clients.len() <= MAX_TRACKED_CLIENTS);
        // Counters saturate instead of wrapping, and a refused attempt is not
        // charged at all.
        let mut only = ThrottleState::default();
        only.global.spent = u32::MAX;
        only.admit("one", 0);
        assert_eq!(only.global.spent, u32::MAX);
        assert!(only.clients.is_empty());
    }

    #[test]
    fn configured_budget_pushes_a_four_digit_keyspace_past_an_hour() {
        // Projection, not a wall-clock measurement: at the configured budget
        // an attacker needs at least this long to try every four-digit
        // passcode, whichever bound binds first.
        const FOUR_DIGIT_KEYSPACE: u64 = 10_000;
        const ONE_HOUR: u64 = 60 * 60;
        let per_client_windows = FOUR_DIGIT_KEYSPACE.div_ceil(LOGIN_FAILURE_BUDGET as u64);
        let global_windows = FOUR_DIGIT_KEYSPACE.div_ceil(LOGIN_GLOBAL_FAILURE_BUDGET as u64);
        let per_client = per_client_windows * LOGIN_BUDGET_WINDOW_SECS;
        let global = global_windows * LOGIN_BUDGET_WINDOW_SECS;
        let worst_case = per_client.min(global);
        assert!(
            worst_case >= ONE_HOUR,
            "full keyspace reachable in {worst_case}s, under the {ONE_HOUR}s floor \
             (per-client {per_client}s, global {global}s)"
        );
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

    #[test]
    fn trusted_proxy_client_ip_resolution() {
        let trusted_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let untrusted_peer: std::net::SocketAddr = "198.51.100.7:40000".parse().unwrap();
        let trusted_peer: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();

        // Untrusted peer with X-Forwarded-For is ignored
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        assert_eq!(
            resolve_client_ip(Some(untrusted_peer), &headers, &[trusted_ip]),
            "198.51.100.7"
        );

        // Trusted proxy with single hop X-Forwarded-For
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &headers, &[trusted_ip]),
            "203.0.113.195"
        );

        // Trusted proxy with multi-hop X-Forwarded-For extracts LAST hop
        let mut multi_headers = HeaderMap::new();
        multi_headers.insert(
            "x-forwarded-for",
            "10.0.0.1, 192.168.1.1, 203.0.113.50".parse().unwrap(),
        );
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &multi_headers, &[trusted_ip]),
            "203.0.113.50"
        );

        // Trusted proxy with invalid last hop falls back to peer IP
        let mut invalid_headers = HeaderMap::new();
        invalid_headers.insert("x-forwarded-for", "invalid-ip-string".parse().unwrap());
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &invalid_headers, &[trusted_ip]),
            "127.0.0.1"
        );

        // Trusted proxy with missing header returns peer IP
        let empty_headers = HeaderMap::new();
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &empty_headers, &[trusted_ip]),
            "127.0.0.1"
        );

        // No peer addr returns "unknown"
        assert_eq!(
            resolve_client_ip(None, &headers, &[trusted_ip]),
            "unknown"
        );
    }
}
