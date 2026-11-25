//! Public PWA resources. These routes serve only static app metadata and
//! artwork; library pages and data stay behind the normal auth layer.

use axum::{
    Router,
    body::Body,
    http::{HeaderValue, header},
    response::Response,
    routing::get,
};

const MANIFEST: &str = include_str!("assets/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("assets/sw.js");
const OFFLINE_PAGE: &str = include_str!("assets/offline.html");
const ICON_192: &[u8] = include_bytes!("assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("assets/icon-512.png");
const MASKABLE_512: &[u8] = include_bytes!("assets/maskable-512.png");

pub fn router() -> Router {
    Router::new()
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/offline.html", get(offline_page))
        .route("/icons/icon-192.png", get(icon_192))
        .route("/icons/icon-512.png", get(icon_512))
        .route("/icons/maskable-512.png", get(maskable_512))
        .route("/apple-touch-icon.png", get(icon_192))
}

async fn manifest() -> Response {
    text_asset(
        MANIFEST,
        "application/manifest+json; charset=utf-8",
        "no-cache",
    )
}

async fn service_worker() -> Response {
    text_asset(SERVICE_WORKER, "text/javascript; charset=utf-8", "no-cache")
}

async fn offline_page() -> Response {
    text_asset(
        OFFLINE_PAGE,
        "text/html; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn icon_192() -> Response {
    image_asset(ICON_192)
}

async fn icon_512() -> Response {
    image_asset(ICON_512)
}

async fn maskable_512() -> Response {
    image_asset(MASKABLE_512)
}

fn text_asset(
    body: &'static str,
    content_type: &'static str,
    cache_control: &'static str,
) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

fn image_asset(body: &'static [u8]) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}
