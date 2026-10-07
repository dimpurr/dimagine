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
use dimagine_index::{IndexError, ViewQuery};

use crate::catalog::DiagnosticKind;
use crate::index_sync::SidebarData;
use crate::ui::components;
use crate::ui::shell::{Destination, Frame};
use crate::view_query::{normalise_decoded_path, query_value};
use crate::{
    acquire_admission, admission_denied, error_page, index_failure, json_result, AppState,
};

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
        Ok(Some(target)) => Redirect::permanent(&target).into_response(),
        Ok(None) => error_page(
            &state,
            StatusCode::NOT_FOUND,
            "This collection is not in this library.",
        ),
        // An index that cannot answer is not an answer of "no" (W39 review
        // L6): the note may well be a collection, so this is the 503 that says
        // why, the shape every other list page gives.
        Err(_) => index_failure(&state, false),
    }
}

/// Where `/collection/<path>` points: `/?c=<path>`.
///
/// A collection that is not in the index is not redirected: an empty grid would
/// read as "this collection has no images", which is a different fact from "this
/// collection is not here".
///
/// "In the index" is the question, not "in the list": the list leaves off the
/// image-note collections (`collection_is_listed`, FORMAT §3.2), but an unlisted
/// collection is still a collection with members, so this URL keeps resolving
/// for it (W27f review L-1: consulting the list here made an unlisted note a
/// 404 while `/?c=<note>` answered 200).
///
/// `Err` is the index declining to answer at all; the caller turns that into
/// the 503 that says so rather than a 404 that claims the note is not a
/// collection.
fn collection_target(
    state: &AppState,
    path: &str,
    query: Option<&str>,
) -> Result<Option<String>, IndexError> {
    // The router already decoded this segment, so a `+` in it is a `+`.
    let Some(collection) = normalise_decoded_path(path).ok().flatten() else {
        return Ok(None);
    };
    if !state.index.is_collection(&collection)? {
        return Ok(None);
    }
    Ok(Some(format!(
        "/?c={}{}",
        query_value(&collection),
        carry_query(query)
    )))
}

/// The query a legacy collection URL carries into its redirect.
///
/// Everything the URL carried travels, except its own `c`: the redirect's
/// target sets `c` to the collection the pretty URL named, and because the last
/// `c` wins a carried one would silently point the page at a different
/// collection (W39 review L4: `/collection/set-0001.md?c=set-0002.md` read
/// "Set 2"). The pretty URL is the one that names the collection, so it wins.
fn carry_query(query: Option<&str>) -> String {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return String::new();
    };
    let carried: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("c"))
        .collect();
    if carried.is_empty() {
        return String::new();
    }
    format!("&{}", carried.join("&"))
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
