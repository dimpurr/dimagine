//! The library page: the grid of images, and the `/api/view` JSON beside it.
//!
//! Spec §2 makes `/` the library — every image, narrowed by the query string —
//! and spec §0 makes the same query string drive the HTML page and the JSON,
//! so an agent and a browser see the same list in the same order.

use axum::{
    extract::{RawQuery, State},
    response::{Html, IntoResponse, Response},
    Json,
};
use dimagine_index::{ViewItem, ViewPage};
use serde::Serialize;

use crate::index_sync::{recent_label, SidebarData, RECENT_LIMIT};
use crate::ui::components;
use crate::ui::escape_html;
use crate::ui::shell::{Destination, Frame};
use crate::view_query::{ViewParams, PAGE_SIZE};
use crate::{acquire_admission, admission_denied, index_failure, AppState};

/// One image as `/api/view` reports it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ViewItemJson {
    /// Library-relative image path.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub added_ns: i64,
    pub title: Option<String>,
    pub rating: Option<u8>,
    pub note_path: Option<String>,
}

impl From<&ViewItem> for ViewItemJson {
    fn from(item: &ViewItem) -> Self {
        Self {
            path: item.path.clone(),
            size: item.size,
            mtime_ns: item.mtime_ns,
            added_ns: item.added_ns,
            title: item.title.clone(),
            rating: item.rating,
            note_path: item.note_path.clone(),
        }
    }
}

/// A page of images as `/api/view` reports it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ViewPageJson {
    /// Images matching the query, before paging.
    pub total: u64,
    /// The page that was asked for, 1-based.
    pub page: u32,
    /// Images per page.
    pub page_size: u32,
    pub items: Vec<ViewItemJson>,
}

impl From<&ViewPage> for ViewPageJson {
    fn from(page: &ViewPage) -> Self {
        Self {
            total: page.total,
            page: 1,
            page_size: PAGE_SIZE,
            items: page.items.iter().map(ViewItemJson::from).collect(),
        }
    }
}

/// `GET /` — the library grid.
pub(crate) async fn library_page(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let params = ViewParams::parse(query.as_deref().unwrap_or(""));
    let Some(result) = read(&state, &params) else {
        return index_failure(&state, false);
    };
    let diagnostics = collection_diagnostics(&state, &params).await;
    // W34 audit #12: a narrowed view says its own name — in the tab and in a
    // heading above the grid — while the plain library stays "Library".
    let view_name = view_title(&params, &result.sidebar);
    let body = format!(
        "{}{diagnostics}{}",
        view_heading(view_name.as_deref()),
        grid_body(&params, &result.page)
    );
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&result.sidebar),
        view: Some(&params),
        result: Some((result.page.total, result.page.items.len() as u64)),
        notices: &params.notices,
        inspector: true,
        ..Frame::new(
            view_name.as_deref().unwrap_or("Library"),
            Destination::Library,
            body,
        )
    };
    Html(frame.render()).into_response()
}

/// A collection view also states which embeds it could not resolve.
///
/// FORMAT §5.1: an embed that matches no image, or matches several, is
/// reported rather than dropped. Reading the members from the index would lose
/// that, so the note is read for its diagnostics and the index answers the
/// listing.
async fn collection_diagnostics(state: &AppState, params: &ViewParams) -> String {
    let Some(collection) = &params.collection else {
        return String::new();
    };
    let path = collection.clone();
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.collection(&path)).await {
        Ok(Ok(collection)) => {
            crate::pages::collections::collection_diagnostics_html(&collection.diagnostics)
        }
        _ => String::new(),
    }
}

/// `GET /api/view` — the same page of the same query, as JSON.
pub(crate) async fn view_json(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let params = ViewParams::parse(query.as_deref().unwrap_or(""));
    let Some(result) = read(&state, &params) else {
        return index_failure(&state, true);
    };
    let page = ViewPageJson {
        page: params.page,
        ..ViewPageJson::from(&result.page)
    };
    Json(page).into_response()
}

/// `GET /api/sidebar` — folders, collections and tags with their counts.
pub(crate) async fn sidebar_json(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    match state.index.sidebar_data() {
        Ok(sidebar) => Json(sidebar.as_ref()).into_response(),
        Err(_) => index_failure(&state, true),
    }
}

/// What one request needs from the index: the page and the sidebar.
struct Page {
    page: ViewPage,
    sidebar: std::sync::Arc<SidebarData>,
}

/// Run one view query and read the sidebar the frame needs. `None` means the
/// index could not answer, and the caller turns that into a reported error
/// rather than an empty page.
fn read(state: &AppState, params: &ViewParams) -> Option<Page> {
    let mut page = state.index.view(&params.to_index_query()).ok()?;
    if params.recent {
        // Recent is "the last [`RECENT_LIMIT`] added" (W34 audit #10); the
        // index counts every match, so the lens states its own size here:
        // 200 or fewer, the same number the sidebar row shows.
        page.total = page.total.min(RECENT_LIMIT);
    }
    let sidebar = state.index.sidebar_data().ok()?;
    Some(Page { page, sidebar })
}

/// The result area: the grid, or what is missing, and the way on.
fn grid_body(params: &ViewParams, page: &ViewPage) -> String {
    if page.items.is_empty() {
        // An empty slice with a non-zero total is not an empty view: the page
        // is past the end, and the empty view's words ("The library is empty")
        // would contradict the count the toolbar is showing beside it (W37
        // review, Medium #2). `/?recent=1&p=3` on a 205-image library is how a
        // hand-typed URL or a stale bookmark gets there.
        if page.total > 0 {
            return components::past_end_state(params, page.total);
        }
        return components::empty_state(params);
    }
    format!(
        "{}{}",
        components::grid(&page.items, params),
        components::load_more(params, page.total)
    )
}

/// What the browser tab and the heading call this view (W34 audit #12). The
/// order a person would say it in: which folder, which collection, which
/// words, which tags, then the two lenses. `None` is the whole library,
/// which names itself without narrowing.
fn view_title(params: &ViewParams, sidebar: &SidebarData) -> Option<String> {
    if let Some(folder) = &params.folder {
        return Some(format!("Folder: {folder}"));
    }
    if let Some(collection) = &params.collection {
        return Some(format!(
            "Collection: {}",
            collection_name(sidebar, collection)
        ));
    }
    if let Some(text) = &params.q {
        return Some(format!("Search: {text}"));
    }
    if !params.tags.is_empty() {
        return Some(format!("Tag: {}", params.tags.join(", ")));
    }
    if params.untagged {
        return Some("Untagged".to_owned());
    }
    if params.recent {
        return Some(recent_label());
    }
    None
}

/// What a collection's note calls it: its `title` when the sidebar carries
/// the collection, its file name otherwise — an unlisted collection
/// (FORMAT §3.2) is still a view, and still deserves a name.
fn collection_name(sidebar: &SidebarData, path: &str) -> String {
    sidebar
        .collections
        .iter()
        .find(|collection| collection.path == path)
        .map(|collection| collection.title.as_str())
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| leaf(path).to_owned())
}

/// The view's heading, above the grid: the same name the tab shows, where a
/// person reads it. The plain library gets none — its pictures are the
/// heading.
fn view_heading(view_name: Option<&str>) -> String {
    match view_name {
        Some(name) => format!(
            "<section class=\"search-section\"><h1>{}</h1></section>",
            escape_html(name)
        ),
        None => String::new(),
    }
}

fn leaf(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_sync::SidebarCollection;

    #[test]
    fn the_json_page_reports_the_page_it_was_asked_for() {
        let page = ViewPage {
            total: 300,
            items: Vec::new(),
        };
        let json = ViewPageJson::from(&page);
        assert_eq!(json.total, 300);
        assert_eq!(json.page_size, 120);
    }

    fn empty_sidebar() -> SidebarData {
        SidebarData {
            folders: Vec::new(),
            collections: Vec::new(),
            tags: Vec::new(),
            total: 0,
            untagged: 0,
            recent: 0,
        }
    }

    /// W34 audit #12: a narrowed view names the thing that narrows it, in the
    /// order a person would say it — folder, collection, words, tags, lenses.
    #[test]
    fn the_title_names_what_narrows_the_screen() {
        let sidebar = empty_sidebar();
        assert_eq!(view_title(&ViewParams::default(), &sidebar), None);
        assert_eq!(
            view_title(&ViewParams::parse("in=refs/ui"), &sidebar).as_deref(),
            Some("Folder: refs/ui")
        );
        assert_eq!(
            view_title(&ViewParams::parse("c=browse.md"), &sidebar).as_deref(),
            Some("Collection: browse.md"),
            "a collection the sidebar does not carry is named by its file"
        );
        assert_eq!(
            view_title(&ViewParams::parse("q=street+view"), &sidebar).as_deref(),
            Some("Search: street view")
        );
        assert_eq!(
            view_title(&ViewParams::parse("tag=eagle"), &sidebar).as_deref(),
            Some("Tag: eagle")
        );
        assert_eq!(
            view_title(&ViewParams::parse("tag=eagle&tag=urban"), &sidebar).as_deref(),
            Some("Tag: eagle, urban"),
            "every tag is named, in the order they narrow"
        );
        assert_eq!(
            view_title(&ViewParams::parse("untagged=1"), &sidebar).as_deref(),
            Some("Untagged")
        );
        assert_eq!(
            view_title(&ViewParams::parse("recent=1"), &sidebar).as_deref(),
            Some("Recent — last 200 added")
        );
    }

    /// A collection is named by what its note calls it when the sidebar knows
    /// the collection, not by the file name.
    #[test]
    fn a_collection_is_named_by_its_title_when_the_sidebar_knows_it() {
        let sidebar = SidebarData {
            collections: vec![SidebarCollection {
                path: "collections/favs.md".to_owned(),
                title: "Favourites".to_owned(),
                count: 3,
            }],
            ..empty_sidebar()
        };
        assert_eq!(
            view_title(&ViewParams::parse("c=collections/favs.md"), &sidebar).as_deref(),
            Some("Collection: Favourites")
        );
    }

    /// The heading says the same name as the tab, once, and never shows a
    /// name the caller did not escape for it.
    #[test]
    fn the_heading_names_the_narrowed_view_and_escapes_it() {
        assert_eq!(view_heading(None), "");
        let html = view_heading(Some("Tag: a<b & co"));
        assert!(
            html.contains("<section class=\"search-section\"><h1>Tag: a&lt;b &amp; co</h1>"),
            "{html}"
        );
    }

    /// W37 review, Medium #2: an empty slice with a non-zero total is a page
    /// past the end, and saying "the library is empty" beside a count of 200
    /// is a lie the count itself exposes.
    #[test]
    fn an_empty_page_past_the_end_is_not_an_empty_view() {
        let past = ViewPage {
            total: 200,
            items: Vec::new(),
        };
        let html = grid_body(&ViewParams::parse("recent=1&p=3"), &past);
        assert!(html.contains("Nothing on this page"), "{html}");
        assert!(!html.contains("The library is empty"), "{html}");
        assert!(!html.contains("Nothing in Recent"), "{html}");

        // A view that really is empty keeps the empty view's words.
        let empty = ViewPage {
            total: 0,
            items: Vec::new(),
        };
        let html = grid_body(&ViewParams::default(), &empty);
        assert!(html.contains("The library is empty"), "{html}");
    }
}
