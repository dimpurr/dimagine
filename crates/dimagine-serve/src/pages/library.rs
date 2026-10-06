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

use crate::ui::components;
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
    let body = format!("{diagnostics}{}", grid_body(&params, &result.page));
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&result.sidebar),
        view: Some(&params),
        result: Some((result.page.total, result.page.items.len() as u64)),
        notices: &params.notices,
        inspector: true,
        ..Frame::new(library_title(&params), Destination::Library, body)
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
    sidebar: std::sync::Arc<crate::index_sync::SidebarData>,
}

/// Run one view query and read the sidebar the frame needs. `None` means the
/// index could not answer, and the caller turns that into a reported error
/// rather than an empty page.
fn read(state: &AppState, params: &ViewParams) -> Option<Page> {
    let page = state.index.view(&params.to_index_query()).ok()?;
    let sidebar = state.index.sidebar_data().ok()?;
    Some(Page { page, sidebar })
}

/// The result area: the grid, or what is missing, and the way on.
fn grid_body(params: &ViewParams, page: &ViewPage) -> String {
    if page.items.is_empty() {
        return components::empty_state(params);
    }
    format!(
        "{}{}",
        components::grid(&page.items, params),
        components::load_more(params, page.total)
    )
}

/// The title says what is on screen.
fn library_title(params: &ViewParams) -> &str {
    if let Some(folder) = &params.folder {
        leaf(folder)
    } else if params.collection.is_some() {
        "Collection"
    } else if params.untagged {
        "Untagged"
    } else if params.recent {
        "Recent"
    } else if params.q.is_some() || !params.tags.is_empty() {
        "Search"
    } else {
        "Library"
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

    #[test]
    fn the_title_names_the_scope_on_screen() {
        assert_eq!(library_title(&ViewParams::default()), "Library");
        assert_eq!(library_title(&ViewParams::parse("in=refs/ui")), "ui");
        assert_eq!(
            library_title(&ViewParams::parse("c=browse.md")),
            "Collection"
        );
        assert_eq!(library_title(&ViewParams::parse("untagged=1")), "Untagged");
        assert_eq!(library_title(&ViewParams::parse("recent=1")), "Recent");
        assert_eq!(library_title(&ViewParams::parse("tag=eagle")), "Search");
    }
}
