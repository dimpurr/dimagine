//! Media streaming and caching handler with byte-level ETag and MIME
//! detection.
//!
//! Two families of route reach this module:
//!
//! - the **path routes** (`/thumb/<path>`, `/media/<path>`, `/raw/<path>`),
//!   which answer a rendition request with a redirect to the content URL
//!   (ADR-020 item 3) and stream an original or a note's bytes directly;
//! - the **content routes** (`/p/<sha256>/<variant>`, [`crate::content`]),
//!   which are the only answers that carry `immutable`.
//!
//! Admission is two classes (W70), decided before any permit is taken: a
//! cached preview or a note's source view is light — 64 permits held
//! around the file open; anything that may generate a preview, stream an
//! original, or answer `/raw` of an image is heavy — 8 permits, a
//! bounded queue, held until the last byte.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path as FsPath,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use unicode_normalization::UnicodeNormalization;

use crate::catalog::{is_image, CatalogError, PreviewKind, PreviewProvider};
use crate::{
    admission_denied, content, error_response, AppState, HEAVY_RETRY_AFTER_SECS,
    LIGHT_RETRY_AFTER_SECS,
};

/// The path routes' old answers: a rendition request is now a redirect to
/// the content URL, and `/raw` streams the bytes as written.
pub(crate) async fn media(
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
    // Class first, pay later: the decision is a resolve, a stat and an
    // index read — the digest the rendition URL is keyed by is the one the
    // index already stores for this path, size and mtime, so a grid's
    // revalidations can tell "already on disk" from "may generate"
    // without touching either class's permits and without reading a
    // single byte of the image (W70, M1 folded into W71/ADR-020). A
    // request that will only ever read a cached preview never waits
    // behind, and never holds up, a preview generator (W70).
    let catalog = state.catalog.clone();
    let previews = state.previews.clone();
    let index = state.index.clone();
    let classify_path = path.clone();
    let classified = tokio::task::spawn_blocking(move || {
        classify(&catalog, &previews, &index, &classify_path, kind)
    })
    .await;
    let class = match classified {
        Ok(Ok(class)) => class,
        Ok(Err(error)) => return error_response(error),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match class {
        MediaClass::Missing => StatusCode::NOT_FOUND.into_response(),
        MediaClass::NoteSource(original) => serve_note_source(&state, original, &headers).await,
        MediaClass::Redirect(target) => found_redirect(&target),
        MediaClass::ExistingPreview(preview) => {
            match serve_cached_preview(&state, preview, &headers).await {
                LightOutcome::Answered(response) => response,
                // The entry existed at classification and not at open.
                // Not answered: re-run the request in the heavy class, the
                // only class allowed to generate a rendition or stream an
                // original (W70's "no multi-MB thumb without the heavy
                // permit" runs both directions).
                LightOutcome::Vanished => {
                    serve_heavy(&state, HeavyRequest::Path { path, kind }, &headers).await
                }
            }
        }
        MediaClass::Heavy => serve_heavy(&state, HeavyRequest::Path { path, kind }, &headers).await,
    }
}

/// Which admission class a media request belongs to.
enum MediaClass {
    /// Nothing to serve — not an image, and not a note under `/raw`:
    /// answered 404 before any permit is taken.
    Missing,
    /// A note's own bytes as `text/plain` (the "Open note" source
    /// view): small and side-effect free — light.
    NoteSource(std::path::PathBuf),
    /// A rendition request under a path URL whose content the index knows:
    /// the content URL carries the immutable answer, so this one redirects
    /// there (ADR-020 item 3).
    Redirect(String),
    /// A rendition already on disk, for a request whose content the index
    /// does *not* know: the light class answers it itself rather than
    /// redirect to a content URL the index would answer with a 404.
    ExistingPreview(std::path::PathBuf),
    /// Anything that may generate a preview, fall back to streaming
    /// the original, or answer `/raw` of an image — heavy.
    Heavy,
}

/// The cheap half of W70: sort one request into an admission class.
/// No permit is held, and nothing is generated; the call is a resolve, an
/// extension/note check, a stat and — for a rendition — the digest the
/// rendition URL is keyed by, which the index already stores (M1's fix:
/// the classification no longer reads and hashes the source file on every
/// request).
fn classify(
    catalog: &Arc<dyn crate::catalog::Catalog>,
    previews: &Arc<dyn PreviewProvider>,
    index: &crate::IndexHandle,
    path: &str,
    kind: Option<PreviewKind>,
) -> Result<MediaClass, CatalogError> {
    let original = catalog.resolve_path(path)?;
    if !is_image(&original) {
        // `/raw` is also the source view of a file the page links to as
        // "Open note": the bytes as written. Served as `text/plain`, so a
        // browser shows the note's markup as text and never runs it as a
        // page of this origin — the media path's counterpart of the
        // sanitiser the rendered notes go through. Rendition routes
        // (`/media`, `/thumb`) stay for images only.
        //
        // The gate is the note test, not "everything that is not an image":
        // a library also holds raw source JSON, canvases and whatever else
        // an importer left, and a route whose comment calls itself the
        // source view of a note should not become a reader for all of them.
        // Same classes `collection_note` accepts for a collection
        // (`FileClass::Note | FileClass::ImageNote`, FORMAT §3). A note is
        // light: small, and its ETag comes from metadata, not a hash.
        if kind.is_none() && is_note(&original) {
            return Ok(MediaClass::NoteSource(original));
        }
        return Ok(MediaClass::Missing);
    }
    match kind {
        // `/raw` of an image streams the original itself, which is the
        // multi-MB object the classes exist to keep scarce: heavy, and
        // out of scope for the content URLs (ADR-020 "Out of scope").
        None => Ok(MediaClass::Heavy),
        Some(requested) => {
            let KeyedContent { hash, indexed } = keyed_content(index, path, &original)?;
            match hash {
                // The index holds a digest for these bytes, so the content
                // URL resolves: the answer belongs there, where it can be
                // cached for as long as it exists (ADR-020 item 3).
                Some(hash) if indexed => {
                    Ok(MediaClass::Redirect(content::preview_url(&hash, requested)))
                }
                // The digest came off the file rather than out of the index —
                // the index has no digest for these facts, which is what a file
                // added between two rescans, or a library whose scan has not
                // reached it, looks like. The content URL would then answer 404,
                // so the rendition is answered here: light when it is published,
                // heavy when it has to be generated.
                Some(hash) => match previews.existing_preview(&hash, requested) {
                    Some(preview) => Ok(MediaClass::ExistingPreview(preview)),
                    None => Ok(MediaClass::Heavy),
                },
                // No digest anywhere: the file cannot be read well enough to
                // hash. The request still waits in the heavy class, which
                // answers it the way it always did, because a request that
                // used to work must keep working.
                None => Ok(MediaClass::Heavy),
            }
        }
    }
}

/// A content digest, and whether the index was the one that knew it.
struct KeyedContent {
    /// The digest of the file's bytes, `None` when neither the index nor a
    /// read of the file could produce one.
    hash: Option<String>,
    /// Whether the digest came from the index — which is also what proves
    /// the index knows these bytes at all, so the content URL resolves them.
    indexed: bool,
}

/// The content digest of one image, read rather than computed wherever the
/// index can answer.
///
/// The rendition cache is keyed by the file's SHA-256 and the index already
/// stores that digest per file, so the digest is a *read* — a stat for the
/// facts and one indexed lookup — and the file's bytes are read only when the
/// index has no digest for those facts (a library the scan has not reached, or
/// an index that could not be opened). That is the whole of M1: before W71
/// every `/thumb` and `/media` request hashed the source from the disk, with
/// no admission control at all.
///
/// A file read that way is read once, not once per request: what its bytes
/// hashed to is remembered on the handle, so the second request for the same
/// path, size and mtime costs a stat again. The remembered digest is not
/// `indexed`, because the index is still the one that knows the bytes — which
/// is what keeps the path route answering directly rather than redirecting to
/// a content URL the index would call a 404 (RW71 review M-1).
fn keyed_content(
    index: &crate::IndexHandle,
    path: &str,
    original: &FsPath,
) -> Result<KeyedContent, CatalogError> {
    let size = file_size(original);
    let mtime_ns = file_mtime_ns(original);
    if let Some(hash) = index.content_hash(path, size, mtime_ns) {
        return Ok(KeyedContent {
            hash: Some(hash),
            indexed: true,
        });
    }
    // The index spells library-relative paths the way the walk found them; a
    // URL can arrive in the other unicode form, so the same row is asked for
    // in both before a file is read for one.
    let nfc: String = path.nfc().collect();
    let nfd: String = path.nfd().collect();
    for candidate in [nfc, nfd] {
        if candidate != path {
            if let Some(hash) = index.content_hash(&candidate, size, mtime_ns) {
                return Ok(KeyedContent {
                    hash: Some(hash),
                    indexed: true,
                });
            }
        }
    }
    // The index has nothing, so the handle's own memory of this file is asked
    // before its bytes are read.
    if let Some(hash) = index.remembered_digest(path, size, mtime_ns) {
        return Ok(KeyedContent {
            hash: Some(hash),
            indexed: false,
        });
    }
    let hash = dimagine_preview::sha256_file(original).ok();
    if let Some(hash) = &hash {
        index.remember_digest(path, size, mtime_ns, hash);
    }
    Ok(KeyedContent {
        hash,
        indexed: false,
    })
}

fn file_size(path: &FsPath) -> u64 {
    fs::metadata(path).map(|facts| facts.len()).unwrap_or(0)
}

fn file_mtime_ns(path: &FsPath) -> i64 {
    fs::metadata(path)
        .and_then(|facts| facts.modified())
        .ok()
        .and_then(|when| when.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos() as i64)
}

/// A light-class answer, or the one race a light request can lose.
enum LightOutcome {
    Answered(Response),
    /// The rendition vanished between the classification stat and the
    /// open. The caller re-runs the request in the heavy class.
    Vanished,
}

/// Serve a note's source view: light. The permit is held around the
/// open only; the stream flows from the file handle without it —
/// a note is a small text file, and the ETag comes from its
/// metadata, so there is no hashing under the permit either.
async fn serve_note_source(
    state: &AppState,
    original: std::path::PathBuf,
    request_headers: &HeaderMap,
) -> Response {
    let Some(permit) = state.admission.acquire_light().await else {
        return admission_denied(LIGHT_RETRY_AFTER_SECS);
    };
    let opened = tokio::task::spawn_blocking(move || open_source_view(&original)).await;
    match opened {
        Ok(Ok((file, metadata, etag, mime, mismatch))) => {
            // Light policy: the permit covered the open; the stream
            // flows from the file handle without it (W70).
            drop(permit);
            stream_image(
                file,
                metadata,
                mime,
                mismatch,
                CachePolicy::Revalidate(etag),
                request_headers,
                None,
            )
            .await
        }
        Ok(Err(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Serve a rendition that classification found on disk: light. The
/// permit is held around the open and released before the stream, which
/// keeps flowing from the open file handle — documented as the light
/// policy, because a preview can never become the multi-MB object
/// the heavy class exists for.
///
/// The ETag is the byte hash of what is being served, not the content:
/// a rendition found by a *path* route is not the URL's own content
/// answer, and a regenerated preview has to be caught under the same URL.
async fn serve_cached_preview(
    state: &AppState,
    preview: std::path::PathBuf,
    request_headers: &HeaderMap,
) -> LightOutcome {
    let Some(permit) = state.admission.acquire_light().await else {
        return LightOutcome::Answered(admission_denied(LIGHT_RETRY_AFTER_SECS));
    };
    let opened = tokio::task::spawn_blocking(move || open_byte_hash(&preview)).await;
    match opened {
        Ok(Ok((opened, etag))) => {
            // Light policy, stated where it is chosen: the permit
            // covered open-to-hash; the stream does not need it.
            drop(permit);
            LightOutcome::Answered(
                stream_image(
                    opened.file,
                    opened.metadata,
                    opened.mime,
                    opened.mismatch,
                    CachePolicy::Revalidate(etag),
                    request_headers,
                    None,
                )
                .await,
            )
        }
        // An I/O failure here is the one race a light request can lose.
        Ok(Err(ServeImageError::Io)) => LightOutcome::Vanished,
        // `open_byte_hash` reports only I/O; a catalogue error here
        // cannot occur (the class was decided by a resolve under no
        // permit), but an answer beats a panic if it ever does.
        Ok(Err(ServeImageError::Catalog(_))) => {
            LightOutcome::Answered(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => LightOutcome::Answered(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

/// What the heavy class is being asked for.
pub(crate) enum HeavyRequest {
    /// A path route: the rendition the provider gives for a path, or the
    /// original itself, byte-hashed and revalidated.
    Path {
        path: String,
        kind: Option<PreviewKind>,
    },
    /// A content route ([`content::content_preview`]): the paths that
    /// carry the named content, and the size class to serve. The
    /// rendition is generated when it is missing and served under the
    /// URL's own ETag; when no rendition can be made the original bytes
    /// are served instead, byte-hashed and never immutable.
    Content {
        hash: String,
        paths: Vec<String>,
        kind: PreviewKind,
    },
}

/// The heavy class: everything that may generate a preview, fall back to
/// the original, or answer `/raw` of an image. Queues bounded (256) and
/// waits (up to 10 s) for a permit before refusing; the permit is held
/// until the last streamed byte, as it always was — here the bodies can
/// be genuinely expensive, and slow readers count against the bound.
pub(crate) async fn serve_heavy(
    state: &AppState,
    request: HeavyRequest,
    request_headers: &HeaderMap,
) -> Response {
    let Some(permit) = state.admission.acquire_heavy().await else {
        return admission_denied(HEAVY_RETRY_AFTER_SECS);
    };
    let catalog = state.catalog.clone();
    let previews = state.previews.clone();
    let root = state.catalog.root().to_path_buf();
    let prepared =
        tokio::task::spawn_blocking(move || prepare_heavy(&catalog, &previews, &root, request))
            .await;
    match prepared {
        Ok(Ok((opened, cache))) => {
            stream_image(
                opened.file,
                opened.metadata,
                opened.mime,
                opened.mismatch,
                cache,
                request_headers,
                Some(permit),
            )
            .await
        }
        Ok(Err(ServeImageError::Catalog(error))) => error_response(error),
        Ok(Err(ServeImageError::Io)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// What the heavy class opens, and how the answer may be cached.
///
/// The two policies are decided here rather than at the response, because
/// they are a property of what was opened: a rendition the URL names its
/// content, and everything else carries bytes that can change under the
/// same URL.
pub(crate) enum CachePolicy {
    /// A rendition under a content URL: cacheable for a year, because
    /// the URL *is* the content — a different image is a different
    /// digest and a different URL (ADR-020 item 2).
    Content(String),
    /// Bytes that can change under the same URL: revalidate every time
    /// (a path rendition, an original served as a fallback, a note).
    Revalidate(String),
}

/// A file opened for streaming: the handle, the facts the response
/// states, and the MIME the bytes say.
pub(crate) struct OpenedFile {
    pub(crate) file: fs::File,
    pub(crate) metadata: fs::Metadata,
    pub(crate) mime: &'static str,
    pub(crate) mismatch: bool,
}

/// Open the heavy class's target and decide its cache.
fn prepare_heavy(
    catalog: &Arc<dyn crate::catalog::Catalog>,
    previews: &Arc<dyn PreviewProvider>,
    root: &FsPath,
    request: HeavyRequest,
) -> Result<(OpenedFile, CachePolicy), ServeImageError> {
    match request {
        HeavyRequest::Path { path, kind } => {
            let original = catalog
                .resolve_path(&path)
                .map_err(ServeImageError::Catalog)?;
            // Rendition routes are for images; a note reaches this class
            // only if it stopped being one between the classification and
            // the permit, so it is answered like any other missing image.
            if !is_image(&original) {
                return Err(ServeImageError::Catalog(CatalogError::NotFound));
            }
            let served = match kind {
                Some(requested) => previews
                    .preview_path(&path, requested)
                    .and_then(|p| fs::canonicalize(p).ok())
                    .filter(|p| p.starts_with(root) && p.is_file())
                    .unwrap_or_else(|| original.clone()),
                // `/raw` of an image: the bytes as written.
                None => original.clone(),
            };
            let (opened, etag) = open_byte_hash(&served)?;
            Ok((opened, CachePolicy::Revalidate(etag)))
        }
        HeavyRequest::Content { hash, paths, kind } => {
            // The rendition the URL names. `preview_path` generates what
            // the cache is missing, under the provider's own concurrency
            // bound, and `None` means no rendition can be made from this
            // content — which is answered by the original, outside the
            // URL's promise.
            let rendition = paths.iter().find_map(|path| {
                let found = previews
                    .preview_path(path, kind)
                    .and_then(|p| fs::canonicalize(p).ok())
                    .filter(|p| p.starts_with(root) && p.is_file())?;
                open_file(&found).ok()
            });
            let (opened, cache) = match rendition {
                Some(opened) => (opened, CachePolicy::Content(content::etag(&hash, kind))),
                None => {
                    // The fallback: the original bytes, byte-hashed, and
                    // never immutable — the URL promised this content's
                    // rendition, not its original.
                    let original = paths
                        .iter()
                        .find_map(|path| catalog.resolve_path(path).ok())
                        .ok_or(ServeImageError::Io)?;
                    let (opened, etag) = open_byte_hash(&original)?;
                    (opened, CachePolicy::Revalidate(etag))
                }
            };
            Ok((opened, cache))
        }
    }
}

pub(crate) enum ServeImageError {
    Catalog(CatalogError),
    Io,
}

/// Whether a path is a note — the only thing `/raw` serves as text. The
/// library walk's own classifier decides it (FORMAT §3), so the route and the
/// walk cannot disagree about what a note is; an image note (`a.png.md`) counts,
/// because it is read as a note just the same.
fn is_note(path: &FsPath) -> bool {
    matches!(
        dimagine_core::library::classify(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
        ),
        dimagine_core::library::FileClass::Note | dimagine_core::library::FileClass::ImageNote
    )
}

const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

/// The source view of a note: its bytes, `text/plain`, and an ETag built from
/// the file's own size and modification time rather than a SHA-256 of its
/// contents.
///
/// A note has no rendition behind its URL and the viewer never writes one, so
/// a whole-file read per "Open note" click buys only the second-nanosecond
/// certainty that the file's metadata does not already give: a revalidation
/// misses only an edit that leaves both the size and the timestamp exactly as
/// they were.
fn open_source_view(
    path: &FsPath,
) -> Result<(fs::File, fs::Metadata, String, &'static str, bool), ServeImageError> {
    let opened = open_file(path)?;
    let etag = view_etag(opened.metadata.len(), modified_nanos(&opened.metadata));
    Ok((
        opened.file,
        opened.metadata,
        etag,
        TEXT_PLAIN,
        opened.mismatch,
    ))
}

/// Open a file and read only the head it takes to name the MIME the bytes
/// claim. Nothing about the file's length is read here: a rendition under a
/// content URL is answered from the URL's own ETag, so no request ever has to
/// hash what it serves (ADR-020 item 2).
pub(crate) fn open_file(path: &FsPath) -> Result<OpenedFile, ServeImageError> {
    let mut file = fs::File::open(path).map_err(|_| ServeImageError::Io)?;
    let metadata = file.metadata().map_err(|_| ServeImageError::Io)?;
    let mut prefix = Vec::with_capacity(64);
    let mut buffer = [0u8; 64];
    loop {
        let read = file.read(&mut buffer).map_err(|_| ServeImageError::Io)?;
        if read == 0 {
            break;
        }
        if prefix.len() < 64 {
            prefix.extend_from_slice(&buffer[..read.min(64 - prefix.len())]);
        }
        if prefix.len() >= 64 {
            break;
        }
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ServeImageError::Io)?;
    // An image (or a rendition of one): the MIME the bytes say, detected from
    // the prefix, with a note when the extension disagrees.
    let mime = detected_image_mime(&prefix).unwrap_or("application/octet-stream");
    let extension_mime = mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream");
    Ok(OpenedFile {
        file,
        metadata,
        mime,
        mismatch: mime != extension_mime,
    })
}

/// A file and the SHA-256 of every byte in it: the ETag of a response whose
/// URL cannot promise its own content.
fn open_byte_hash(path: &FsPath) -> Result<(OpenedFile, String), ServeImageError> {
    let mut opened = open_file(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    {
        let mut file = &opened.file;
        loop {
            let read = file.read(&mut buffer).map_err(|_| ServeImageError::Io)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| ServeImageError::Io)?;
    let etag = format!("\"{:x}\"", hasher.finalize());
    Ok((opened, etag))
}

fn modified_nanos(metadata: &fs::Metadata) -> i64 {
    let Ok(stamp) = metadata.modified() else {
        return 0;
    };
    match stamp.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MIN),
    }
}

/// The ETag a note's source view and a conflict report both render: the
/// file's own size and modification time (never a hash of its bytes — a note
/// has no rendition and is small enough that its metadata is the version the
/// viewer revalidates against). A pre-epoch mtime is folded to zero, the
/// value the source view has always used, so the two callers cannot spell the
/// same file two ways (W67 D4).
pub(crate) fn view_etag(size: u64, mtime_ns: i64) -> String {
    let mtime = u64::try_from(mtime_ns).unwrap_or(0);
    format!("\"size-{size:x}-mtime-{mtime:x}\"")
}

/// A streamed file. The admission permit, when one is held (the heavy
/// class), lives until the last byte; the light class releases its
/// permit after the open and streams with `None` — both policies are
/// chosen in [`serve_cached_preview`] and [`serve_heavy`].
struct AdmittedFile {
    inner: tokio::fs::File,
    _admission: Option<tokio::sync::OwnedSemaphorePermit>,
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

/// The response for a rendition the light class already opened: the same
/// response the heavy class builds, minus the permit to hold.
pub(crate) async fn stream_served(
    opened: OpenedFile,
    cache: CachePolicy,
    request_headers: &HeaderMap,
    admission: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    stream_image(
        opened.file,
        opened.metadata,
        opened.mime,
        opened.mismatch,
        cache,
        request_headers,
        admission,
    )
    .await
}

async fn stream_image(
    file: fs::File,
    metadata: fs::Metadata,
    mime: &'static str,
    mismatch: bool,
    cache: CachePolicy,
    request_headers: &HeaderMap,
    admission: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    let file = tokio::fs::File::from_std(file);
    // The heavy class holds its permit for the whole stream, so slow
    // readers count against the concurrency bound while streaming; the
    // light class passed `None` and streams on the open file handle
    // alone, bounded by the preview's own small size.
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
    let (etag, cache_control) = match cache {
        CachePolicy::Content(etag) => (etag, content::IMMUTABLE_CACHE_CONTROL),
        CachePolicy::Revalidate(etag) => (etag, "private, no-cache"),
    };
    let mut response = Response::new(body);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    // ADR-018 §5: image responses tell the browser not to reinterpret the
    // bytes as something else. The MIME is the sniffed one, so a mismatched
    // extension cannot coax a renderer out of served bytes.
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
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

/// `302 Found` to a content URL, never cached: the redirect is a hop, and
/// caching it would pin a path to one digest for as long as the browser
/// kept it (ADR-020 item 3 — a 301 would do exactly that, and a path can be
/// reused for a different image or its content can change).
pub(crate) fn found_redirect(target: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    if let Ok(location) = HeaderValue::from_str(target) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
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

#[cfg(test)]
mod tests {
    use super::view_etag;

    #[test]
    fn a_pre_epoch_mtime_renders_the_same_etag_as_zero() {
        // The conflict report (write_api.rs) and the source view share this
        // helper, so a negative `Fingerprint::mtime_ns` and the source view's
        // folded zero spell one ETag (W67 D4).
        assert_eq!(view_etag(10, -1), view_etag(10, 0));
        assert_eq!(view_etag(255, 1), "\"size-ff-mtime-1\"");
    }
}
