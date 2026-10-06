//! `/folders`: the folder tree with recursive counts.
//!
//! `/folder/<path>` no longer lists a folder of its own; it answers 301 to the
//! library narrowed by `in`, so a link shared from an older version still lands
//! on the images it meant (spec §2). The JSON listings agents use
//! (`/api/folder`, `/api/folder/<path>`) stay exactly as they were.

use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};

use crate::catalog::{CatalogError, Collection, ImageEntry};
use crate::index_sync::SidebarData;
use crate::ui::shell::{Destination, Frame};
use crate::view_query::{normalise_decoded_path, query_value};
use crate::{acquire_admission, admission_denied, index_failure, json_result, AppState};

/// What `/api/folder` reports, unchanged from before the viewer was rebuilt.
#[derive(serde::Serialize)]
pub(crate) struct FolderData {
    pub(crate) folder: String,
    pub(crate) breadcrumbs: Vec<(String, String)>,
    pub(crate) folders: Vec<String>,
    pub(crate) images: Vec<ImageEntry>,
    pub(crate) collections: Vec<Collection>,
}

/// `GET /folders` — every folder with its recursive count, as a tree.
pub(crate) async fn folders_page(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let Ok(sidebar) = state.index.sidebar_data() else {
        return index_failure(&state, false);
    };
    let body = folder_tree(&sidebar);
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&sidebar),
        ..Frame::new("Folders", Destination::Folders, body)
    };
    Html(frame.render()).into_response()
}

/// The folder tree, as a list of rows.
fn folder_tree(sidebar: &SidebarData) -> String {
    if sidebar.folders.is_empty() {
        return "<div class=\"empty-state\"><h3>No folders yet</h3>\
                <p>Images in subfolders appear here once the library has some.</p></div>"
            .to_owned();
    }
    format!(
        "<div class=\"folder-tree\">{}</div>",
        sidebar
            .folders
            .iter()
            .map(|folder| {
                let depth = folder.path.matches('/').count();
                format!(
                    "<a class=\"sidebar-row folder-row depth-{depth}\" href=\"/?in={path}\">\
                     <span class=\"sidebar-row-label\">{label}</span>\
                     <span class=\"sidebar-row-count\">{count}</span></a>",
                    path = crate::ui::escape_html(&query_value(&folder.path)),
                    label = crate::ui::escape_html(leaf(&folder.path)),
                    count = folder.count,
                )
            })
            .collect::<String>()
    )
}

/// `GET /folder/<path>` — 301 to the library view of that folder.
pub(crate) async fn folder_redirect(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match folder_target(&state, &path, query.as_deref()) {
        Some(target) => Redirect::permanent(&target).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Where `/folder/<path>` points: `/?in=<path>`.
///
/// A path the viewer will not show anyway never becomes a redirect. Sending it
/// on would render an empty library and read as "this folder is empty" when the
/// truth is that it is not there.
fn folder_target(state: &AppState, path: &str, query: Option<&str>) -> Option<String> {
    // The router already decoded this segment, so a `+` in it is a `+`.
    let folder = normalise_decoded_path(path).ok().flatten()?;
    let known = state
        .index
        .sidebar_data()
        .ok()?
        .folders
        .iter()
        .any(|entry| entry.path == folder);
    known.then(|| format!("/?in={}{}", query_value(&folder), carry_query(query)))
}

/// Keep the rest of a shared link's query string across a redirect.
fn carry_query(query: Option<&str>) -> String {
    match query.filter(|query| !query.is_empty()) {
        Some(query) => format!("&{query}"),
        None => String::new(),
    }
}

/// `GET /api/folder` — the root listing.
pub(crate) async fn folder_root_json(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    folder_data_blocking(state, String::new()).await
}

/// `GET /api/folder/<path>` — one folder's listing.
pub(crate) async fn folder_json(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    folder_data_blocking(state, path).await
}

async fn folder_data_blocking(state: AppState, folder: String) -> Response {
    match tokio::task::spawn_blocking(move || folder_data(&state, &folder)).await {
        Ok(result) => json_result(result),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn folder_data(state: &AppState, folder: &str) -> Result<FolderData, CatalogError> {
    let folders = state.catalog.list_subfolders(folder)?;
    let images_all = state.catalog.list_folder(folder)?;
    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{}/", folder.trim_end_matches('/'))
    };
    let images = images_all
        .into_iter()
        .filter(|image| {
            image
                .path
                .strip_prefix(&prefix)
                .is_some_and(|tail| !tail.contains('/'))
        })
        .collect();
    let collections = state.catalog.list_collections(folder)?;
    let breadcrumbs = crate::ui::breadcrumbs(folder);
    Ok(FolderData {
        folder: folder.to_owned(),
        breadcrumbs,
        folders,
        images,
        collections,
    })
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
    fn an_old_query_survives_a_redirect() {
        assert_eq!(carry_query(Some("tag=eagle")), "&tag=eagle");
        assert_eq!(carry_query(Some("")), "");
        assert_eq!(carry_query(None), "");
    }
}
