//! Media streaming and caching handler with byte-level ETag and MIME detection.

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
    time::{SystemTime, UNIX_EPOCH},
};

use crate::catalog::{is_image, CatalogError, PreviewKind};
use crate::{acquire_admission, admission_denied, error_response, AppState};

pub(crate) async fn media(
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
