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

    /// W71 / ADR-020, pinned where the crate has no JS harness: the script
    /// builds a content URL for every picture it renders itself — the tiles a
    /// "Load more" appends, the inspector's preview — spells it with the
    /// generation the page states rather than a copy of the policy, and asks
    /// without a generation when the page states none (the route answers that
    /// with a redirect to the current one).
    #[test]
    fn app_js_ships_the_content_addressed_preview_urls() {
        assert!(
            APP_JS.contains("function contentThumbSrc("),
            "the one place a tile's content URL is spelled"
        );
        assert!(
            APP_JS.contains("dataset.previewGeneration"),
            "the generation is read off the page, not carried here"
        );
        assert!(
            APP_JS.contains("function previewGenerationOf(element)"),
            "the reader of that attribute"
        );
        // No second copy of the generation policy: a bump in
        // `dimagine_preview::RENDITION_GENERATION` must not need an edit here,
        // so no generation may appear as a string literal (a comment naming
        // one is the one place worth spelling out).
        for stale in ["'.g2'", "\".g2\"", "'g2'", "thumb.' +"] {
            assert!(
                !APP_JS.contains(stale),
                "the script must not name a generation itself: {stale}"
            );
        }
        // A content route is a viewer media route, so the W70 retry policy
        // covers the URLs W71 introduced.
        assert!(
            APP_JS.contains("parsed.pathname.indexOf('/p/') === 0"),
            "a /p/ URL takes the same retry path as /thumb/ and /media/"
        );
    }

    /// W70 review L2, in the same string-pinning form: the 404 probe must not
    /// take a tile's broken form while a retry for that very picture is still
    /// in flight, and the probe's own abort timer is cleared whichever way the
    /// fetch settles.
    #[test]
    fn app_js_keeps_a_retry_in_flight_and_clears_the_probe_timer() {
        assert!(
            APP_JS.contains("if (!image.complete) return;"),
            "a load still in flight is not a failure that has happened yet"
        );
        assert!(
            APP_JS.contains("timer = window.setTimeout("),
            "the probe keeps its timer to clear"
        );
        assert_eq!(
            APP_JS
                .matches("if (timer) window.clearTimeout(timer);")
                .count(),
            2,
            "both ways the probe settles clear its own timeout"
        );
    }

    /// W69, pinned where the crate has no JS harness: the drag-select
    /// policy (a 500 ms press within a 10 px drift window opens
    /// selection mode on the pressed tile, whose own add-or-remove
    /// state is the decision the whole drag applies), and the gesture
    /// guards that keep tap-to-open and scroll intact (a drift past
    /// the window cancels the press before it fires; the captured
    /// pointer's tile is a hit test, not the event's target; the
    /// release's click is swallowed once selection mode is on; a
    /// touch that moves after selection mode is on is not a scroll).
    /// String-pinning is the honest limit of a test that cannot
    /// execute the file; a harness that can execute it replaces this.
    #[test]
    fn app_js_ships_the_w69_drag_select_policy() {
        assert!(
            APP_JS.contains("var SELECT_PRESS_MS = 500;"),
            "the long-press window"
        );
        assert!(
            APP_JS.contains("var SELECT_DRIFT_PX = 10;"),
            "the drift window that tells a press from a scroll"
        );
        for name in [
            "enterSelectionMode",
            "exitSelectionMode",
            "cancelSelectionGesture",
            "endSelectionGesture",
            "tileAtPoint",
            "setTileSelected",
            "updateSelectionBar",
            "selectionBar",
        ] {
            assert!(
                APP_JS.contains(&format!("function {name}(")),
                "the drag-select function {name}"
            );
        }
        assert!(
            APP_JS.contains("grid.setPointerCapture(gesture.id)"),
            "the captured pointer keeps the drag over the grid"
        );
        assert!(
            APP_JS.contains("document.elementFromPoint(x, y)"),
            "the tile under the finger is hit-tested, not the capture target"
        );
        assert!(
            APP_JS.contains("dx * dx + dy * dy > SELECT_DRIFT_PX * SELECT_DRIFT_PX"),
            "a drift past the window cancels the press"
        );
        assert!(
            APP_JS.contains("{ passive: false }"),
            "a selection drag is not a scroll"
        );
        // W69 follow-up M2: the touchmove block is scoped to the finger a
        // long press already claimed, so a fresh touch after release
        // scrolls the page again instead of leaving a dead zone where a
        // swipe neither selects nor scrolls.
        assert!(
            APP_JS.contains("if (selectionGesture && selectionGesture.mode) {"),
            "only the claiming gesture's touchmove is prevented"
        );
        // W69 follow-up M1: the double click that opens an image carries
        // the same `selecting` guard as the single click, so a double tap
        // in selection mode cannot navigate away and drop the selection.
        assert_eq!(
            APP_JS
                .matches("if (grid && grid.classList.contains('selecting')) {")
                .count(),
            4,
            "the click and dblclick of the page's own tiles and the appended ones all stand down while selecting"
        );
        // W75 Low-3: the tiles a "Load more" appends carry the same
        // double-click-to-open as the page's own tiles — the same
        // `selecting` guard and the same open — so an appended tile is
        // not single-click-only outside selection mode.
        assert_eq!(
            APP_JS
                .matches("tile.addEventListener('dblclick', function (event) {")
                .count(),
            2,
            "the page's own tiles and the appended ones both open on double click"
        );
        assert_eq!(
            APP_JS
                .matches("window.location.href = imageHref(tile.dataset.path);")
                .count(),
            2,
            "both tile classes open the tile's own page"
        );
        assert!(
            APP_JS.contains("if (grid && grid.classList.contains('selecting')) return;"),
            "the arrows and Enter wait while selection mode is open"
        );
        assert!(
            APP_JS.contains("event.stopPropagation();"),
            "the release's click is swallowed in the capture phase"
        );
        assert!(
            APP_JS.contains("var swallowClick = false;"),
            "the flag that holds the click a long press ends in"
        );
    }
}
