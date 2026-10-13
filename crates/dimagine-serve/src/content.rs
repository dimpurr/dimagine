//! Content-addressed preview routes (ADR-020): `GET /p/<sha256>/<variant>`,
//! the URL that *is* the content.
//!
//! A rendition URL used to be `/thumb/<path>` and `/media/<path>`: the same
//! address for whatever bytes happened to be behind it, so the response could
//! never be cached for longer than one revalidation. The cache the renderer
//! keeps is already keyed by the file's SHA-256 (FORMAT §8), and the index
//! stores that digest per file — so the URL can name the content instead of
//! the path. `/p/<sha256>/thumb.g2` means "the `thumb` rendition, in
//! generation `g2`, of the file whose bytes hash to `<sha256>`", and nothing
//! else can ever answer it: a different image is a different digest, and a
//! changed image is a different digest. That is what makes `immutable` true
//! rather than a hope, and it is why a rename or a move keeps the URL
//! working.
//!
//! Three rules come with it, all from the ADR:
//!
//! - the generation rides in the URL's variant (`thumb.g2`), because a
//!   content digest says nothing about which policy produced the rendition —
//!   upgrade the policy and the old URL answers with a redirect to the
//!   current one, so a year-old browser cache cannot serve previous-generation
//!   pixels under a URL that only promised the content;
//! - the digest is looked up in the index and **never** used to trigger
//!   generation when the index does not know it: an unknown digest is a 404,
//!   because a request that carries no path cannot be an authorised request
//!   to spend decode time;
//! - the redirect from a path route is `302` with `no-cache`, so long caching
//!   belongs only to the immutable content URLs.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::{atomic::Ordering, Arc};

use crate::catalog::{is_image, Catalog, PreviewKind};
use crate::media::{self, CachePolicy, HeavyRequest};
use crate::{admission_denied, AppState, LIGHT_RETRY_AFTER_SECS};

/// How a rendition under a content URL may be cached: the URL is the content,
/// so it can be cached for as long as it exists. `private` because the answer
/// is library data — a shared cache must never store it — and `immutable`
/// because nothing can change what this URL means.
pub(crate) const IMMUTABLE_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// The generation the routes serve under, as `RENDITION_GENERATION` spells it,
/// read from that one constant rather than written a second time: a policy bump
/// moves the URL, the redirect and the cache entry name together, because there
/// is only one thing for them to follow.
///
/// A spelling the URL grammar cannot carry is a programming error rather than a
/// request to answer, and the preview crate's own test rejects it at build time.
fn current_generation() -> u32 {
    dimagine_preview::generation_number().expect("RENDITION_GENERATION must be g<N>")
}

/// How many images one preview backfill pass works through between the
/// `spawn_blocking` calls that make it interruptible. The provider has its
/// own concurrency bound; this is the outer batch size, chosen so a pass over
/// a large library still hands control back to the runtime between chunks.
const BACKFILL_BATCH: usize = 64;

/// `GET /p/<sha256>/<variant>` — the rendition of one content, by digest.
///
/// The digest is the only thing answered with an `immutable` response, and it
/// is answered exactly one way: whatever this server would serve for that
/// content now. The generation in the variant is part of that answer — a URL
/// naming a different generation redirects to the current one.
pub(crate) async fn content_preview(
    State(state): State<AppState>,
    Path((hash, variant)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(requested) = parse_variant(&variant) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Spelling first: a digest that is not 64 hexadecimal characters names no
    // content, so it is refused before the index is consulted.
    let Some(hash) = normalise_hash(&hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Then the content: the paths that carry it, in path order. A digest the
    // index does not know is a 404 — it is never a request to generate
    // anything, because a URL with no path in it is not an authorised request
    // to spend decode time (ADR-020, attack review).
    let paths = image_paths(&state.catalog, state.index.paths_for_content(&hash));
    if paths.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // A URL naming another generation of the same content: the current one is
    // the answer, and the redirect is a hop rather than the answer.
    if requested.generation != current_generation() {
        return media::found_redirect(&preview_url(&hash, requested.kind));
    }
    let kind = requested.kind;
    // Fast path: a published rendition is a light-class answer — one open, no
    // permit while it streams, and no hashing of anything. `None` is the heavy
    // class's turn: either nothing is published yet, or the entry the probe
    // found was not there for the open.
    let root = state.catalog.root().to_path_buf();
    let published = state
        .previews
        .existing_preview(&hash, kind)
        .and_then(|preview| std::fs::canonicalize(preview).ok())
        .filter(|preview| preview.starts_with(&root) && preview.is_file());
    let mut light = None;
    if let Some(preview) = published {
        let Some(permit) = state.admission.acquire_light().await else {
            return admission_denied(LIGHT_RETRY_AFTER_SECS);
        };
        let opened = tokio::task::spawn_blocking(move || media::open_file(&preview)).await;
        // Light policy (W70): the permit covered the open; the stream does not
        // need it.
        drop(permit);
        match opened {
            Ok(Ok(opened)) => {
                light = Some(
                    media::stream_served(
                        opened,
                        CachePolicy::Content(etag(&hash, kind)),
                        &headers,
                        None,
                    )
                    .await,
                );
            }
            // The probe saw the entry and the open did not: the one race a light
            // request can lose. The path route answers it by re-running the
            // request in the heavy class (`LightOutcome::Vanished`), which is
            // what a content URL does below as well — a 404 would be a dead end
            // for exactly the client that retries (RW71 review L-2).
            Ok(Err(_)) => {}
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
    // The heavy class generates what is missing — bounded by the same eight
    // permits and the provider's own decoders — and falls back to the original
    // when the content cannot be rendered, or cannot be read, at all.
    match light {
        Some(response) => response,
        None => {
            media::serve_heavy(
                &state,
                HeavyRequest::Content { hash, paths, kind },
                &headers,
            )
            .await
        }
    }
}

/// The paths of one digest a rendition route may answer.
///
/// The index resolves a digest by bytes alone — `by_sha256` matches any `files`
/// row, and a row can be a raw export or a note left by an importer rather than
/// a picture — so the gate a path route classifies with is the one that decides
/// what a rendition URL is allowed to serve. A digest that names no image is a
/// 404, not the byte dump of `/raw` under a URL that promised a rendition
/// (RW71 review L-1).
fn image_paths(catalog: &Arc<dyn Catalog>, paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|path| {
            catalog
                .resolve_path(path)
                .is_ok_and(|original| is_image(&original))
        })
        .collect()
}

/// The URL of one content's rendition, in the current generation: the address
/// every page, every JSON body and every redirect emits.
pub(crate) fn preview_url(hash: &str, kind: PreviewKind) -> String {
    format!(
        "/p/{hash}/{}.{}",
        kind_name(kind),
        dimagine_preview::RENDITION_GENERATION
    )
}

/// Where one image's rendition of one size class is read from, for a page that
/// has just the path.
///
/// The digest is the index's own, keyed by the file's own size and mtime — the
/// same reuse the media route makes (W71 M1) — so the answer is a stat and one
/// indexed lookup rather than a hash of the image. A page carrying a grid of
/// tiles wants the batched form instead ([`crate::pages::library::tile_metas`]);
/// this is the one-image form, for a stage, a cover or a single picture.
pub(crate) async fn image_src(state: &AppState, path: &str, kind: PreviewKind) -> String {
    let path = path.to_owned();
    let index = state.index.clone();
    let resolved = state.catalog.resolve_path(&path).ok();
    let keyed = path.clone();
    let digest = tokio::task::spawn_blocking(move || {
        let original = resolved?;
        let facts = std::fs::metadata(&original).ok()?;
        let modified = facts
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos() as i64;
        index.content_hash(&keyed, facts.len(), modified)
    })
    .await
    .ok()
    .flatten();
    match digest {
        Some(hash) => preview_url(&hash, kind),
        None => format!("/{}/{}", path_route(kind), crate::ui::encode_path(&path)),
    }
}

/// The path route one size class's path URL answers with before it redirects.
fn path_route(kind: PreviewKind) -> &'static str {
    match kind {
        PreviewKind::Thumb => "thumb",
        PreviewKind::View => "media",
    }
}

/// The `ETag` a content-URL answer carries: the digest, the size class and
/// the generation — the three things this URL promises.
pub(crate) fn etag(hash: &str, kind: PreviewKind) -> String {
    format!(
        "\"{hash}.{}.{}\"",
        kind_name(kind),
        dimagine_preview::RENDITION_GENERATION
    )
}

/// The generation the URLs are spelled with, as `RENDITION_GENERATION` spells
/// it. Every page carries it, and the script reads it back off the grid
/// section rather than keeping a copy (ADR-020 item 8).
pub(crate) fn preview_generation() -> &'static str {
    dimagine_preview::RENDITION_GENERATION
}

/// The spelling of one size class in a URL variant.
fn kind_name(kind: PreviewKind) -> &'static str {
    match kind {
        PreviewKind::Thumb => "thumb",
        PreviewKind::View => "view",
    }
}

/// One requested rendition: the size class, and the generation the URL named.
struct RequestedVariant {
    kind: PreviewKind,
    generation: u32,
}

/// The URL variant, parsed. `None` for anything that is not
/// `<thumb|view>[.g<N>]` — an unknown size class, a generation that is not a
/// number, a generation too large to spell.
///
/// A variant with no generation at all is an older spelling of the same
/// request and is answered with a redirect to the current one; the strictness
/// exists to keep everything that is not a rendition request out of the route
/// at all, not to turn a stale bookmarked URL into a 404.
fn parse_variant(segment: &str) -> Option<RequestedVariant> {
    let (name, generation) = match segment.split_once('.') {
        Some((name, generation)) => (name, Some(generation)),
        None => (segment, None),
    };
    let kind = match name {
        "thumb" => PreviewKind::Thumb,
        "view" => PreviewKind::View,
        _ => return None,
    };
    let generation = match generation {
        Some(tag) => tag.strip_prefix('g').and_then(|digits| {
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| digits.parse::<u32>().ok())
                .flatten()
        })?,
        // No generation named: the spelling before the generation rode in the
        // URL. Redirected to the current one by the route.
        None => 0,
    };
    Some(RequestedVariant { kind, generation })
}

/// A digest as the cache keys and the URLs spell it: exactly 64 hexadecimal
/// characters, lower-cased. `None` for anything else, before it can reach
/// the index or a cache path.
pub(crate) fn normalise_hash(raw: &str) -> Option<String> {
    (raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

/// Start the background pass that fills in the renditions the index says a
/// library is missing (ADR-020 item 6): no request should wait for a
/// rendition the library has simply not built yet.
///
/// One pass, started once, and interruptible: the work is chunked through
/// `spawn_blocking`, and each chunk hands control back to the runtime, so
/// dropping this task stops it between chunks rather than mid-decode — a
/// dropped future still lets the in-flight chunk of [`BACKFILL_BATCH`] images
/// finish. It
/// takes no admission permit — the provider's own concurrency bound is what
/// keeps it from crowding out a request, and a permit held across a whole
/// pass is exactly the starvation the light class exists to avoid. Images
/// that appear after the snapshot the pass took are built on demand by the
/// routes above.
pub(crate) fn ensure_preview_backfill(state: &Arc<AppState>) {
    if state
        .backfill_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // The router is being built on a plain thread (a test, a tool): no
        // background task, and the routes still answer on demand.
        return;
    };
    let state = Arc::clone(state);
    runtime.spawn(async move {
        backfill_once(&state).await;
    });
}

async fn backfill_once(state: &Arc<AppState>) {
    // A snapshot of the library as it stands, not a live cursor: a pass that
    // re-read its plan between images could chase a library that keeps
    // changing under it.
    let rows = state.index.image_rows();
    if rows.is_empty() {
        return;
    }
    let mut built = 0usize;
    for chunk in rows.chunks(BACKFILL_BATCH) {
        // Each chunk runs on a blocking worker, so a decode never parks the
        // async executor, and the hand-back between chunks is where dropping
        // this task stops the pass.
        let state = Arc::clone(state);
        let chunk = chunk.to_vec();
        built += tokio::task::spawn_blocking(move || gap_count(&state, &chunk))
            .await
            .unwrap_or(0);
    }
    if built > 0 {
        eprintln!("dimagine: built {built} missing preview renditions in the background");
    }
}

/// How many renditions one batch of images was missing, and built. Each image's
/// digest comes from the index when the index holds it — a read, no file opened
/// — and the provider does the rest under its own concurrency bound. What the
/// provider could not make is not counted, because the line this feeds says
/// what was built (RW71 review L-4).
fn gap_count(state: &AppState, rows: &[(String, u64, i64)]) -> usize {
    let mut built = 0usize;
    for (path, size, mtime_ns) in rows {
        let hash = state.index.content_hash(path, *size, *mtime_ns);
        for kind in [PreviewKind::Thumb, PreviewKind::View] {
            let published = hash
                .as_ref()
                .and_then(|hash| state.previews.existing_preview(hash, kind))
                .is_some();
            if published {
                continue;
            }
            // `preview_path` is the provider's own `ensure`: it renders what the
            // cache is missing and answers `None` when nothing can be made of
            // this content — one fewer rendition built, not one more.
            if state.previews.preview_path(path, kind).is_some() {
                built += 1;
            }
        }
    }
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    /// There is one generation, and the number the route compares is the one
    /// the URL spells and the cache entry name carries — read off
    /// `RENDITION_GENERATION`, with no second copy to keep honest. Written as a
    /// test rather than a const assertion, because the value comes from a
    /// function that is not const.
    #[test]
    fn the_url_generation_is_the_one_the_cache_names_carry() {
        let url = preview_url(&"a".repeat(64), PreviewKind::Thumb);
        assert_eq!(
            url.rsplit('.').next().expect("a URL names its generation"),
            format!("g{}", current_generation()),
            "the number the route compares is the one the URL spells"
        );
        assert_eq!(
            current_generation(),
            dimagine_preview::generation_number().expect("RENDITION_GENERATION must be g<N>")
        );
    }

    #[test]
    fn a_variant_is_the_size_class_and_the_generation_it_names() {
        assert!(parse_variant("thumb").is_some());
        assert!(parse_variant("view").is_some());
        let current =
            parse_variant(&format!("thumb.{}", dimagine_preview::RENDITION_GENERATION)).unwrap();
        assert_eq!(current.generation, current_generation());
        assert_eq!(kind_name(current.kind), "thumb");
        // An older generation of the same size class parses, and the route
        // answers it with a redirect rather than serving it.
        assert_eq!(parse_variant("view.g1").unwrap().generation, 1);
        assert_eq!(parse_variant("thumb.g0").unwrap().generation, 0);
    }

    #[test]
    fn anything_that_is_not_a_rendition_variant_is_refused() {
        // The current spelling parses — stated first, so the list below cannot
        // quietly grow to include a spelling the route serves.
        assert!(
            parse_variant(&format!("view.{}", dimagine_preview::RENDITION_GENERATION)).is_some()
        );
        for spelling in [
            "",
            "thumbs",
            "raw",
            "thumb.jpg",
            "thumb.g",
            "thumb.gx",
            "thumb.g2x",
            "thumb.g-1",
            "thumb.g 2",
            "Thumb.g2",
            "thumb.g99999999999999999999",
            "..",
            ".",
            "thumb/",
        ] {
            assert!(
                parse_variant(spelling).is_none(),
                "{spelling:?} must not parse as a variant"
            );
        }
    }

    #[test]
    fn a_digest_is_exactly_sixty_four_hexadecimal_characters() {
        let digest = "a".repeat(64);
        assert_eq!(normalise_hash(&digest).as_deref(), Some(digest.as_str()));
        // Case-insensitive in, lower-case out: the same bytes are the same key.
        assert_eq!(
            normalise_hash(&"A".repeat(64)).as_deref(),
            Some(digest.as_str())
        );
        for wrong in [
            "",
            "a",
            "z".repeat(64).as_str(),
            "a".repeat(63).as_str(),
            "a".repeat(65).as_str(),
            "..",
            "..",
            "/",
        ] {
            assert!(normalise_hash(wrong).is_none(), "{wrong:?} is not a digest");
        }
    }

    /// The URL and the ETag are the three facts the address promises: the
    /// content, the size class and the generation.
    #[test]
    fn the_url_and_the_etag_carry_the_content_the_class_and_the_generation() {
        let hash = "b".repeat(64);
        assert_eq!(
            preview_url(&hash, PreviewKind::Thumb),
            format!("/p/{hash}/thumb.{}", dimagine_preview::RENDITION_GENERATION)
        );
        assert_eq!(
            etag(&hash, PreviewKind::View),
            format!("\"{hash}.view.{}\"", dimagine_preview::RENDITION_GENERATION)
        );
    }
}
