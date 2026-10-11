//! Static assets embedded via `include_str!` and served with content hashes.
//!
//! Spec §0: CSS and one small vanilla JS file ship inside the binary and are
//! served from `/assets/<name>-<hash>.{css,js}`. The hash is in the URL, so the
//! responses are immutable and can carry a long cache; a rebuild that changes
//! the file changes the URL.

use sha2::{Digest, Sha256};
use std::sync::LazyLock;

pub const APP_CSS: &str = include_str!("app.css");
pub const APP_JS: &str = include_str!("app.js");

/// Hash length in hex characters. Long enough that a collision is not a
/// practical concern, short enough to keep the URL readable.
const HASH_LEN: usize = 12;

static CSS_HASH: LazyLock<String> = LazyLock::new(|| hash(APP_CSS));
static JS_HASH: LazyLock<String> = LazyLock::new(|| hash(APP_JS));

fn hash(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())[..HASH_LEN].to_string()
}

/// Hashed URL of the stylesheet, for a `<link rel="stylesheet">`.
pub fn css_url() -> String {
    format!("/assets/app-{}.css", *CSS_HASH)
}

/// Hashed URL of the script, for a `<script src>`.
pub fn js_url() -> String {
    format!("/assets/app-{}.js", *JS_HASH)
}

/// The exact route the stylesheet is served on.
pub fn css_route() -> String {
    css_url()
}

/// The exact route the script is served on.
pub fn js_route() -> String {
    js_url()
}

/// The CSS body if `path` is the current stylesheet URL, else `None`.
pub fn css_at(path: &str) -> Option<&'static str> {
    (path == css_url()).then_some(APP_CSS)
}

/// The JS body if `path` is the current script URL, else `None`.
pub fn js_at(path: &str) -> Option<&'static str> {
    (path == js_url()).then_some(APP_JS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_urls_carry_a_content_hash_and_resolve_back() {
        assert!(css_url().starts_with("/assets/app-"));
        assert!(css_url().ends_with(".css"));
        assert!(js_url().ends_with(".js"));
        assert_ne!(css_url(), js_url());
        assert_eq!(css_at(&css_url()), Some(APP_CSS));
        assert_eq!(js_at(&js_url()), Some(APP_JS));
    }

    #[test]
    fn a_stale_or_foreign_asset_path_is_not_served() {
        assert_eq!(css_at("/assets/app-deadbeef1234.css"), None);
        assert_eq!(css_at("/assets/app.css"), None);
        assert_eq!(js_at(&css_url()), None);
        assert_eq!(css_at("/api/view"), None);
    }

    #[test]
    fn a_changed_body_changes_the_url() {
        // The hash is what makes the cache safe, so it must depend on the
        // bytes and nothing else.
        assert_eq!(hash("a"), &hash("a")[..]);
        assert_ne!(hash("a"), hash("b"));
        assert_eq!(hash("a").len(), HASH_LEN);
    }

    /// The W70 retry policy, pinned where the crate has no JS harness —
    /// this repo ships none (no package.json, no jest/vitest anywhere),
    /// so until one exists this test stands in for it: the schedule the
    /// fix specifies (3 attempts, 1/2/4 s steps, jitter capped at
    /// 500 ms), the DOM-free functions a future harness should exercise
    /// (`retryAttemptOf` / `retryDelayFor` / `retryUrlFor`, kept pure
    /// for exactly that day — the note their section carries), and the
    /// 404 probe that keeps a retry loop from running on a missing
    /// image. String-pinning is the honest limit of a test that cannot
    /// execute the file; a harness that can execute it replaces this.
    #[test]
    fn app_js_ships_the_w70_retry_policy() {
        assert!(
            APP_JS.contains("var RETRY_BACKOFF_MS = [1000, 2000, 4000];"),
            "the 1/2/4 second backoff table"
        );
        assert!(
            APP_JS.contains("var RETRY_JITTER_MS = 500;"),
            "jitter bounded to half a second"
        );
        for name in ["retryAttemptOf", "retryDelayFor", "retryUrlFor"] {
            assert!(
                APP_JS.contains(&format!("function {name}(")),
                "the pure policy function {name}"
            );
        }
        assert!(
            APP_JS.contains("method: 'HEAD'"),
            "the 404 probe before its schedule is spent"
        );
    }
}
