//! `/collections`: the collection list, and `/collection/<path>` redirected to
//! the library narrowed by `c` (spec §2).
//!
//! The JSON listing agents use (`/api/collection/<path>`) stays as it was, with
//! the member captions and the diagnostics for embeds that resolved to nothing.

use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use dimagine_index::ViewQuery;

use crate::catalog::DiagnosticKind;
use crate::index_sync::SidebarData;
use crate::ui::components;
use crate::ui::shell::{Destination, Frame};
use crate::view_query::{normalise_decoded_path, query_value};
use crate::{acquire_admission, admission_denied, index_failure, json_result, AppState};

/// `GET /collections` — title, member count and cover for each collection.
pub(crate) async fn collections_page(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let Ok(sidebar) = state.index.sidebar_data() else {
        return index_failure(&state, false);
    };
    let covers = covers(&state, &sidebar);
    let body = collection_body(&sidebar, &covers);
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&sidebar),
        ..Frame::new("Collections", Destination::Collections, body)
    };
    Html(frame.render()).into_response()
}

/// The first member of each collection, which is its cover (spec §2).
///
/// One query per collection, each capped at a single row: a cover is the first
/// embed, never the whole list.
fn covers(state: &AppState, sidebar: &SidebarData) -> Vec<Option<String>> {
    sidebar
        .collections
        .iter()
        .map(|collection| {
            state
                .index
                .view(&ViewQuery {
                    collection: Some(collection.path.clone()),
                    ..ViewQuery::default()
                })
                .ok()
                .and_then(|page| page.items.first().map(|item| item.path.clone()))
        })
        .collect()
}

fn collection_body(sidebar: &SidebarData, covers: &[Option<String>]) -> String {
    if sidebar.collections.is_empty() {
        return "<div class=\"empty-state\"><h3>No collections yet</h3>\
                <p>A collection is a note that embeds images.</p></div>"
            .to_owned();
    }
    let mut out = String::from("<div class=\"collection-grid\">");
    for (collection, cover) in sidebar.collections.iter().zip(covers) {
        out.push_str(&components::collection_card(
            &collection.path,
            &collection.title,
            collection.count,
            cover.as_deref(),
        ));
    }
    out.push_str("</div>");
    out
}

/// `GET /collection/<path>` — 301 to the library view of that collection.
pub(crate) async fn collection_redirect(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match collection_target(&state, &path, query.as_deref()) {
        Some(target) => Redirect::permanent(&target).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Where `/collection/<path>` points: `/?c=<path>`.
///
/// A collection that is not in the index is not redirected: an empty grid would
/// read as "this collection has no images", which is a different fact from "this
/// collection is not here".
fn collection_target(state: &AppState, path: &str, query: Option<&str>) -> Option<String> {
    // The router already decoded this segment, so a `+` in it is a `+`.
    let collection = normalise_decoded_path(path).ok().flatten()?;
    let known = state
        .index
        .sidebar_data()
        .ok()?
        .collections
        .iter()
        .any(|entry| entry.path == collection);
    known.then(|| format!("/?c={}{}", query_value(&collection), carry_query(query)))
}

fn carry_query(query: Option<&str>) -> String {
    match query.filter(|query| !query.is_empty()) {
        Some(query) => format!("&{query}"),
        None => String::new(),
    }
}

/// `GET /api/collection/<path>` — one collection with its members and
/// diagnostics, exactly as before.
pub(crate) async fn collection_json(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.collection(&path)).await {
        Ok(result) => json_result(result),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Why an embed is not a member, in the words the format uses (FORMAT §5.1).
///
/// A collection's members come from the index, which resolves each embed to an
/// image or to nothing; this is how the "nothing" stays visible. The library
/// page shows it above the grid, so a broken embed is reported rather than
/// dropped.
pub(crate) fn collection_diagnostics_html(
    diagnostics: &[crate::catalog::CollectionDiagnostic],
) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    format!(
        "<section class=\"collection-diagnostics\">{}</section>",
        diagnostics
            .iter()
            .map(|diagnostic| match diagnostic.kind {
                DiagnosticKind::Ambiguous => format!(
                    "<p class=\"error\">Line {}: <code>{}</code> matches several files, \
                     not guessing: {}</p>",
                    diagnostic.line,
                    crate::ui::escape_html(&diagnostic.target),
                    diagnostic
                        .candidates
                        .iter()
                        .map(|candidate| crate::ui::escape_html(candidate))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                DiagnosticKind::Missing => format!(
                    "<p class=\"error\">Line {}: <code>{}</code> matches no file</p>",
                    diagnostic.line,
                    crate::ui::escape_html(&diagnostic.target)
                ),
            })
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Collection, CollectionDiagnostic, CollectionMember};

    fn diagnostics_html(diagnostics: &[CollectionDiagnostic]) -> String {
        collection_diagnostics_html(diagnostics)
    }

    fn collection() -> Collection {
        Collection {
            path: "set.md".into(),
            title: "A set".into(),
            members: vec![CollectionMember {
                path: "refs/a.png".into(),
                caption: "First".into(),
            }],
            diagnostics: vec![
                CollectionDiagnostic {
                    target: "photo.png".into(),
                    line: 3,
                    kind: DiagnosticKind::Ambiguous,
                    candidates: vec!["a/photo.png".into(), "b/photo.png".into()],
                },
                CollectionDiagnostic {
                    target: "gone.png".into(),
                    line: 7,
                    kind: DiagnosticKind::Missing,
                    candidates: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn a_collection_states_both_ways_an_embed_can_fail() {
        let html = diagnostics_html(&collection().diagnostics);
        assert!(html.contains("matches several files"));
        assert!(html.contains("a/photo.png"));
        assert!(html.contains("b/photo.png"));
        assert!(html.contains("matches no file"));
    }
}
