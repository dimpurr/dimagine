//! The `/collections` list, the collection page the library view
//! `/?c=<note>` renders (spec §2), and `/collection/<path>` redirected to
//! it (spec §2).
//!
//! A collection is a note that embeds images (FORMAT §5), so the page is the
//! note's own shape: a header that shows the note's title, its own text and
//! the way to its source, and the members in the order the note embeds
//! them, each with the caption line its embed gave it (FORMAT §5: "a line
//! directly after an embed is that member's note"). The member list comes
//! from a live read of the note — the file is the source of truth (FORMAT
//! §1.2) — deduplicated so a repeated embed shows once, at its first
//! position; the HTML page and `/api/view` agree in order for every
//! collection, and per-link rows the index stores twice (the same image
//! embedded through two spellings) are the one place the page cites the
//! note before the index. The JSON listing agents use
//! (`/api/collection/<path>`) stays as it was, with the member captions
//! and the diagnostics for embeds that resolved to nothing.

use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use dimagine_index::{IndexError, ViewItem, ViewQuery};
use std::collections::{HashMap, HashSet};

use crate::catalog::DiagnosticKind;
use crate::index_sync::SidebarData;
use crate::ui::components::{self, CollectionTile};
use crate::ui::shell::{Destination, Frame};
use crate::view_query::{normalise_decoded_path, query_value, ViewParams};
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

/// What the collection page renders: the header, the diagnostics and the
/// members it lists.
pub(crate) struct CollectionPageParts {
    /// The `<section>` the page opens with: the view's heading (the same
    /// name the tab shows), the note's own text, the item count and the
    /// "Open note" link to the note's source.
    pub(crate) header: String,
    /// The embeds that resolved to nothing, in the section the narrowed
    /// library view already showed them in (FORMAT §5.1).
    pub(crate) diagnostics: String,
    /// The members the page lists, in note order, a duplicate embed shown
    /// once at its first position.
    pub(crate) items: Vec<CollectionTile>,
}

/// Everything the library page needs to render a collection.
///
/// Returns `None` when the note is not a collection: `/?c=<path>` then
/// falls back to the plain narrowed library view, whose empty state already
/// says the right thing, the same answer it gave before the collection page
/// existed.
///
/// The member list comes from one note read ([`Catalog::collection_page`]):
/// the note is the collection, so its order and its duplicates are decided
/// by the file, not by the index. Display data (a member's title) is joined
/// in from the index where it knows the member — one query over the whole
/// collection, because a note's member list has no pages — and when the
/// view is narrowed by more than the collection (`tag`, `q`, `in`,
/// `untagged`, `recent` beside `c`), the index answer also picks which
/// members the narrowing keeps, in the note's order.
pub(crate) async fn collection_page_parts(
    state: &AppState,
    params: &ViewParams,
    view_name: &str,
) -> Option<CollectionPageParts> {
    let path = params.collection.clone()?;
    let catalog = state.catalog.clone();
    // One blocking read of the note: members, captions, diagnostics and the
    // note's own text are all in that file, so they can never disagree.
    let page = tokio::task::spawn_blocking(move || catalog.collection_page(&path))
        .await
        .ok()?
        .ok()?;
    // The index joins in display data and answers the narrowing, whole
    // collection in one query: `p` pages nothing here, and the query string
    // the tiles carry keeps the collection for the way back.
    let mut query = params.to_index_query();
    query.offset = 0;
    query.limit = u32::MAX;
    let known: HashMap<String, ViewItem> = state
        .index
        .view(&query)
        .ok()?
        .items
        .into_iter()
        // A member the index lists twice (two spellings of one embed) keeps
        // one row, the first — the page keeps the first position.
        .fold(HashMap::new(), |mut known, item| {
            known.entry(item.path.clone()).or_insert(item);
            known
        });
    let narrowed = params.folder.is_some()
        || !params.tags.is_empty()
        || params.q.is_some()
        || params.untagged
        || params.recent;
    let items = member_tiles(&page.collection.members, &known, narrowed);
    let count = items.len();
    Some(CollectionPageParts {
        header: collection_header_html(
            view_name,
            count,
            &page.own_text_html,
            &page.collection.path,
        ),
        diagnostics: collection_diagnostics_html(&page.collection.diagnostics),
        items,
    })
}

/// The members the grid lists: the note's order, a duplicate once at its
/// first position (its first caption — that is the row the note gave it),
/// and beyond a narrowed view only the members the narrowing keeps.
///
/// A member the index knows nothing about still shows: the note embeds it
/// and the file is there, so the tile is built from what is known, named by
/// its file (invariant 4 — unknown is not empty).
fn member_tiles(
    members: &[crate::catalog::CollectionMember],
    known: &HashMap<String, ViewItem>,
    narrowed: bool,
) -> Vec<CollectionTile> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut tiles = Vec::new();
    for member in members {
        if !seen.insert(member.path.as_str()) {
            continue; // a duplicate embed shows once, at its first position
        }
        if narrowed && !known.contains_key(&member.path) {
            continue;
        }
        let item = match known.get(&member.path) {
            Some(item) => item.clone(),
            None => ViewItem {
                path: member.path.clone(),
                size: 0,
                mtime_ns: 0,
                added_ns: 0,
                title: None,
                rating: None,
                note_path: None,
            },
        };
        tiles.push(CollectionTile {
            item,
            caption: member.caption.clone(),
        });
    }
    tiles
}

/// The header: the view's name (the same words the tab shows, W34 audit #12),
/// the note's own text as the catalog rendered it, the number of items the
/// page lists, and the link to the note's source.
///
/// The own text arrives already sanitised by the renderer every rendered
/// note goes through, so it is inserted the way the image page inserts its
/// note body; every string composed here — the heading, the count, the link
/// — is escaped where it is written.
fn collection_header_html(
    view_name: &str,
    item_count: usize,
    own_text_html: &str,
    note_path: &str,
) -> String {
    let note = if own_text_html.is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"collection-note note-body\">{}</div>",
            // The note's own text, rendered and sanitised upstream.
            own_text_html
        )
    };
    let noun = if item_count == 1 { "item" } else { "items" };
    format!(
        "<section class=\"search-section collection-header\">\
         <h1>{}</h1>{note}\
         <p class=\"collection-meta\">{} {noun} · \
         <a class=\"open-note\" href=\"/raw/{}\">Open note</a></p></section>",
        crate::ui::escape_html(view_name),
        item_count,
        crate::ui::encode_path(note_path),
    )
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

    fn member(path: &str, caption: &str) -> CollectionMember {
        CollectionMember {
            path: path.into(),
            caption: caption.into(),
        }
    }

    /// W46 §1: the member list is the note's — embed order, a duplicate once
    /// at its first position with the row the note gave it there — and a
    /// narrowing keeps that order while dropping what it does not match.
    #[test]
    fn the_member_list_keeps_the_note_order_and_deduplicates_at_the_first_position() {
        let members = vec![
            member("c.png", "Third"),
            member("a.png", "First time"),
            member("b.png", ""),
            member("a.png", "Second time"),
        ];
        let mut known = HashMap::new();
        for path in ["a.png", "b.png", "c.png"] {
            known.insert(
                path.to_owned(),
                ViewItem {
                    path: path.to_owned(),
                    size: 1,
                    mtime_ns: 1,
                    added_ns: 1,
                    title: Some("Joined from the index".into()),
                    rating: None,
                    note_path: None,
                },
            );
        }
        let tiles = member_tiles(&members, &known, false);
        assert_eq!(
            tiles
                .iter()
                .map(|tile| tile.item.path.as_str())
                .collect::<Vec<_>>(),
            vec!["c.png", "a.png", "b.png"],
            "the note's order, the duplicate once — at its first position"
        );
        assert_eq!(
            tiles[1].caption, "First time",
            "the caption of the first row"
        );
        assert_eq!(
            tiles[1].item.title.as_deref(),
            Some("Joined from the index"),
            "display data joins in from the index"
        );

        // A narrowing only keeps what it matches, in the note's order.
        let mut nothing = HashMap::new();
        nothing.insert(
            "b.png".to_owned(),
            ViewItem {
                path: "b.png".into(),
                size: 1,
                mtime_ns: 1,
                added_ns: 1,
                title: None,
                rating: None,
                note_path: None,
            },
        );
        let narrowed = member_tiles(&members, &nothing, true);
        assert_eq!(
            narrowed
                .iter()
                .map(|tile| tile.item.path.as_str())
                .collect::<Vec<_>>(),
            vec!["b.png"],
            "the narrowing picked one member, the note ordered it"
        );
    }

    /// A member the index has never seen still shows: the note embeds it, so
    /// the tile carries what is known and is named by its file — unknown is
    /// not empty (invariant 4).
    #[test]
    fn a_member_the_index_does_not_know_still_lists() {
        let tiles = member_tiles(&[member("new/thing.png", "Fresh")], &HashMap::new(), false);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].item.path, "new/thing.png");
        assert_eq!(tiles[0].item.title, None, "nothing was invented");
        assert_eq!(tiles[0].item.note_path, None);
        assert_eq!(tiles[0].caption, "Fresh");
    }

    /// The header names the view, shows the note's own text only when the
    /// note has one, counts the items in the page's own words, and links to
    /// the note's source.
    #[test]
    fn the_header_escapes_the_name_and_omits_an_empty_note() {
        let with_note = collection_header_html("Collection: A <b> set", 1, "<p>Kept</p>", "set.md");
        assert!(
            with_note.contains("Collection: A &lt;b&gt; set"),
            "{with_note}"
        );
        assert!(
            with_note.contains("class=\"collection-note note-body\""),
            "{with_note}"
        );
        assert!(
            with_note.contains("1 item · <a class=\"open-note\" href=\"/raw/set.md\">"),
            "{with_note}"
        );

        let bare = collection_header_html("Collection: Bare", 0, "", "notes/bare.md");
        assert!(
            !bare.contains("collection-note"),
            "no text of its own, no empty text block: {bare}"
        );
        assert!(bare.contains("0 items"), "{bare}");
        assert!(
            bare.contains("href=\"/raw/notes/bare.md\""),
            "spaces in the note path stay encoded: {bare}"
        );
    }
}
