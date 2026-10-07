//! Read-only library catalog and preview provider contracts and implementations.

use dimagine_core::format::file_extension;
use dimagine_core::library::{FileClass, FileEntry, Library};
use dimagine_core::links::{extract_markdown_links, key, LinkSyntax, Outcome, Resolver};
use dimagine_index::{note_is_collection, CollectionEvidence};
use saphyr::{LoadableYamlNode, Yaml};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path as FsPath, PathBuf},
    sync::{Arc, Condvar, Mutex},
};
use unicode_normalization::UnicodeNormalization;

use crate::ui::{markdown_html, LinkOutcome, LinkResolver};

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp",
];

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

/// A collection as the collection page renders it (FORMAT §5): the same
/// members, order and diagnostics the JSON listing answers with, plus the
/// note's own text.
///
/// FORMAT §5 makes the note the collection: the embeds are the members, in
/// order, and "a line directly after an embed is that member's note" — the
/// caption belongs to the member, which the grid beside the text shows with
/// it. What belongs to the note itself is everything else: its prose, its
/// headings, and the embeds that resolved to nothing, which stay because
/// FORMAT §5.1 keeps the line and the diagnostics report it. This is exactly
/// that remainder — the member embeds' lines and the caption lines that
/// follow them are taken out of the body — rendered by the sanitiser the
/// image page already uses, its links resolved against the library, so a
/// wikilink resolves, hostile markup stays text, and an empty string means
/// the note has no text of its own.
#[derive(Clone, Debug)]
pub struct CollectionPage {
    /// The collection as [`Catalog::collection`] reports it.
    pub collection: Collection,
    /// The note's own text rendered to safe HTML, or an empty string when
    /// the note is only embeds and captions.
    pub own_text_html: String,
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
    /// Markdown note body, as written.
    pub body: String,
    /// The note body rendered to safe HTML, its wikilinks and
    /// embeds resolved against the library (FORMAT §5.1), when
    /// the image has a note. `None` is different from an empty
    /// note: it means there is no note to render.
    pub body_html: Option<String>,
    /// Adjacent raw metadata filenames.
    pub raw_files: Vec<String>,
    /// Library-relative path of the image's note, when it has one. `None` is
    /// different from an empty note: it means there is no note to open.
    pub note_path: Option<String>,
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
    /// Return a collection as the collection page renders it: the listing
    /// plus the note's own text as safe HTML (FORMAT §5).
    fn collection_page(&self, path: &str) -> Result<CollectionPage, CatalogError>;
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
            let (members, diagnostics, _) =
                collect_collection(&self.resolver, &self.library.files, entry, &parsed, &text);
            if !is_collection(
                entry.class == FileClass::ImageNote,
                &parsed,
                &members,
                &diagnostics,
            ) {
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

    /// Read one note and decide what it collects: the collection the
    /// listing reports, the 1-based file line of each member's embed, and
    /// the note's body as written. Everything that asks a note for its
    /// members shares this one read, so the listing and the collection page
    /// can never disagree about a note or about its order.
    fn collection_note(
        &self,
        path: &str,
    ) -> Result<(Collection, Vec<usize>, ParsedNote), CatalogError> {
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
        let (members, diagnostics, member_lines) =
            collect_collection(&self.resolver, &self.library.files, &note, &parsed, &text);
        if !is_collection(
            class == FileClass::ImageNote,
            &parsed,
            &members,
            &diagnostics,
        ) {
            return Err(CatalogError::NotFound);
        }
        Ok((
            Collection {
                path: path.to_owned(),
                title: yaml_string(&parsed.properties, "title")
                    .unwrap_or_else(|| name.trim_end_matches(".md").to_string()),
                members,
                diagnostics,
            },
            member_lines,
            parsed,
        ))
    }

    /// The note body rendered to safe HTML, its wikilinks and
    /// embeds resolved against the library (FORMAT §5.1).
    ///
    /// `note_path` grounds note-relative links and identifies
    /// the note's own image, whose embed (FORMAT §3.2) the
    /// image page already shows.
    fn render_note_body(&self, note_path: &str, body: &str) -> String {
        let note = self
            .library
            .files
            .iter()
            .find(|entry| entry.rel == note_path);
        let links = NoteLinks {
            files: &self.library.files,
            resolver: &self.resolver,
            note,
        };
        markdown_html(body, Some(&links))
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
        Ok(self.collection_note(path)?.0)
    }

    fn collection_page(&self, path: &str) -> Result<CollectionPage, CatalogError> {
        let (collection, member_lines, parsed) = self.collection_note(path)?;
        let own_text_markdown = own_text_markdown(
            &parsed.body,
            parsed.body_line,
            &collection.members,
            &member_lines,
        );
        Ok(CollectionPage {
            collection,
            own_text_html: self.render_note_body(path, &own_text_markdown),
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
        let (properties, body, front_matter_error, note_relative_path, body_html) =
            if let Some(note_meta) = note_meta {
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
                let text =
                    fs::read_to_string(&canonical_note).map_err(|_| CatalogError::Unreadable)?;
                let parsed = parse_note(&text);
                let relative = note_relative.to_string_lossy().replace('\\', "/");
                let body_html = self.render_note_body(&relative, &parsed.body);
                (
                    parsed.properties,
                    parsed.body,
                    parsed.error,
                    Some(relative),
                    Some(body_html),
                )
            } else {
                (serde_json::Value::Null, String::new(), None, None, None)
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
            body_html,
            front_matter_error,
            raw_files,
            note_path: note_relative_path,
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

pub(crate) fn ignored_name(s: &str) -> bool {
    s.starts_with('.')
        || s.starts_with("._")
        || s == "Thumbs.db"
        || s.eq_ignore_ascii_case("desktop.ini")
}

pub(crate) fn is_image(path: &FsPath) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| IMAGE_EXTENSIONS.contains(&x.to_ascii_lowercase().as_str()))
}

pub(crate) struct ParsedNote {
    pub(crate) properties: serde_json::Value,
    pub(crate) body: String,
    pub(crate) body_line: usize,
    pub(crate) error: Option<String>,
}

pub(crate) fn parse_note(text: &str) -> ParsedNote {
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
    let mut front_matter = String::new();
    let mut line_number = 1usize;
    for line in lines.by_ref() {
        line_number += 1;
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let body = lines.collect::<String>();
            let parsed = match Yaml::load_from_str(&front_matter) {
                Ok(docs) => docs
                    .into_iter()
                    .next()
                    .map(|doc| yaml_to_json(&doc))
                    .unwrap_or(serde_json::Value::Null),
                Err(error) => {
                    return ParsedNote {
                        properties: serde_json::Value::Null,
                        body,
                        body_line: line_number + 1,
                        error: Some(error.to_string()),
                    }
                }
            };
            return ParsedNote {
                properties: parsed,
                body,
                body_line: line_number + 1,
                error: None,
            };
        }
        front_matter.push_str(line);
    }
    ParsedNote {
        properties: serde_json::Value::Null,
        body: text.to_owned(),
        body_line: 2,
        error: Some("front matter has no closing delimiter".to_owned()),
    }
}

pub(crate) fn yaml_to_json(yaml: &Yaml<'_>) -> serde_json::Value {
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

/// The note's whole front matter as pretty JSON, for the properties panel.
pub(crate) fn yaml_json_string(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_owned())
}

pub(crate) fn yaml_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}

/// Whether the note is a collection, by the one shared rule
/// ([`note_is_collection`], FORMAT §5). `image_note` is a fact the caller has
/// from the file walk: the note is `<image>.<ext>.md` (FORMAT §3.2); it does
/// not change the verdict, only feeds the evidence.
fn is_collection(
    image_note: bool,
    parsed: &ParsedNote,
    members: &[CollectionMember],
    diagnostics: &[CollectionDiagnostic],
) -> bool {
    note_is_collection(&CollectionEvidence {
        kind: yaml_string(&parsed.properties, "kind").as_deref(),
        image_note,
        members: members.len(),
        unresolved: diagnostics.len(),
    })
}

/// The members a note collects, the embeds it could not resolve, and the
/// 1-based file line of each member's embed.
///
/// The lines are what the note's own text is not ([`own_text_markdown`]):
/// the page that lists the members beside the note's text takes both rows
/// out of it. Every member's line is returned in the same order as the
/// member, so `members[i]` embeds at `lines[i]`.
fn collect_collection(
    resolver: &Resolver,
    files: &[FileEntry],
    note: &FileEntry,
    parsed: &ParsedNote,
    text: &str,
) -> (Vec<CollectionMember>, Vec<CollectionDiagnostic>, Vec<usize>) {
    let links = extract_markdown_links(&parsed.body, parsed.body_line);
    let lines: Vec<&str> = text.lines().collect();
    let mut members = Vec::new();
    let mut diagnostics = Vec::new();
    let mut member_lines = Vec::new();
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
                member_lines.push(link.line);
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
    (members, diagnostics, member_lines)
}

fn is_self_embed(note: &FileEntry, image: &FileEntry) -> bool {
    note.class == FileClass::ImageNote
        && note
            .paired_image_path()
            .is_some_and(|paired| paired == image.path)
}

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

/// The lines a collection note keeps for itself, as Markdown (FORMAT §5).
///
/// [`collect_collection`] reports every embed that resolved to an image as a
/// member, with the row it occupies in the note: the embed's line, and the
/// caption line directly after it when it has one ("a line directly after an
/// embed is that member's note"). Those two rows belong to the members, and
/// the page lists the members with their captions — so the note's own text
/// is its body without them: its prose, its headings, the note links, and
/// the embeds that resolved to nothing, which stay in place because FORMAT
/// §5.1 keeps the line while its diagnostic reports the failure.
///
/// `body_line` is the 1-based file line the body starts at, the same offset
/// the link extractor was given, so a member's body-relative index is its
/// file line minus it. A note without any text of its own — only embeds
/// and captions — yields an empty string.
fn own_text_markdown(
    body: &str,
    body_line: usize,
    members: &[CollectionMember],
    member_lines: &[usize],
) -> String {
    let member_rows: HashSet<usize> = members
        .iter()
        .zip(member_lines)
        .flat_map(|(member, line)| {
            // 0-based within the body: the embed's own row, and the caption
            // row that `caption_after` took for it.
            let embed = line.saturating_sub(body_line);
            if member.caption.is_empty() {
                vec![embed]
            } else {
                vec![embed, embed + 1]
            }
        })
        .collect();
    body.lines()
        .enumerate()
        .filter(|(index, _)| !member_rows.contains(index))
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Link resolution for one note body (FORMAT §5.1).
///
/// The image resolver of the library walk answers path and
/// bare-name links; [`NoteLinks`] adds what a note renderer
/// needs on top: a link to a note (not only to an image),
/// and the note's own image, whose embed the image page
/// already shows (FORMAT §3.2).
struct NoteLinks<'a> {
    files: &'a [FileEntry],
    resolver: &'a Resolver,
    /// The note the links are written in, when the walk found it.
    note: Option<&'a FileEntry>,
}

impl LinkResolver for NoteLinks<'_> {
    fn resolve_link(&self, target: &str) -> LinkOutcome {
        let Some(note) = self.note else {
            return LinkOutcome::Unresolved;
        };
        match self.resolver.resolve(target, note, LinkSyntax::WikiLink) {
            Outcome::Resolved(idx) => self.classify(&self.files[idx], note),
            // An extensionless name may still name a note: the
            // image resolver leaves `[[browse]]` alone on
            // purpose, while a note renderer resolves it to
            // `browse.md`, the way Obsidian does.
            Outcome::NotImageTarget | Outcome::NotFound => {
                self.note_by_stem(target).unwrap_or(LinkOutcome::Unresolved)
            }
            // Several files match: the viewer must not guess
            // (FORMAT §5.1 rule 3).
            Outcome::Ambiguous(_) => LinkOutcome::Unresolved,
        }
    }
}

impl<'a> NoteLinks<'a> {
    /// What a resolved library file means for a link.
    fn classify(&self, entry: &FileEntry, note: &FileEntry) -> LinkOutcome {
        match entry.class {
            FileClass::Image => {
                if is_self_embed(note, entry) {
                    LinkOutcome::SelfImage(entry.rel.clone())
                } else {
                    LinkOutcome::Image(entry.rel.clone())
                }
            }
            FileClass::ImageNote | FileClass::Note => LinkOutcome::Note(entry.rel.clone()),
            // Raw metadata, boards and anything else have no
            // page of their own in this viewer.
            _ => LinkOutcome::Unresolved,
        }
    }

    /// The one note whose name without `.md` is `target`,
    /// when exactly one exists (FORMAT §5.1 rules 2 and 3).
    fn note_by_stem(&self, target: &str) -> Option<LinkOutcome> {
        // Only an extensionless target can name a note by
        // its stem; a target with an extension was already
        // matched by its full name.
        if file_extension(target).is_some() {
            return None;
        }
        let wanted = key(target);
        let mut hits = Vec::new();
        for entry in self.files {
            if !matches!(entry.class, FileClass::Note | FileClass::ImageNote) {
                continue;
            }
            // A bare name matches the file name; a path
            // matches the whole library-relative path.
            let full = if target.contains('/') {
                &entry.rel
            } else {
                &entry.name
            };
            let Some(stem) = without_md(full) else {
                continue;
            };
            if key(stem) == wanted {
                hits.push(entry);
            }
        }
        match hits.len() {
            1 => Some(LinkOutcome::Note(hits[0].rel.clone())),
            _ => None,
        }
    }
}

/// The file name or path without its `.md`, matched
/// case-insensitively the way the file classes are.
fn without_md(name: &str) -> Option<&str> {
    let start = name.len().checked_sub(3)?;
    let tail = name.get(start..)?;
    tail.eq_ignore_ascii_case(".md").then_some(&name[..start])
}
