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
use dimagine_index::{FacetCounts, TakenReason, ViewItem, ViewPage};
use serde::Serialize;
use serde_json::Value;

use crate::catalog::ImageDetail;
use crate::index_sync::{recent_label, recent_window, SidebarData, RECENT_LIMIT};
use crate::ui::components::{self, TileFact, TileMetas};
use crate::ui::filters;
use crate::ui::shell::{Destination, Frame};
use crate::ui::{encode_path, escape_html};
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
    /// Pixel width, when the index read the header. `null` is an unknown — a
    /// header that could not be read — never a zero and never a guess.
    pub width: Option<u32>,
    /// Pixel height, under the terms of [`ViewItemJson::width`].
    pub height: Option<u32>,
    /// The SHA-256 of the file's bytes, `null` when the index holds no digest
    /// for them. It is the key the image's preview URL is addressed by
    /// (ADR-020), so a reader can build `/p/<sha256>/thumb.g2` itself and know
    /// it is asking for the content rather than for a path.
    pub sha256: Option<String>,
    /// The EXIF taken time in ns since the Unix epoch, `null` when the image
    /// carries no readable one. The same fact the "Taken" sort orders by, so
    /// an agent can see why a row sits where it does — an image with no taken
    /// time is last in both directions, not the oldest.
    pub taken_ns: Option<i64>,
    /// Which kind of unknown a `null` [`ViewItemJson::taken_ns`] is, in the one
    /// word `dimagine scan` and its JSON use — [`TakenReason::as_str`] is the
    /// list of them. `null` when there is a taken time to report, and `null`
    /// when the index holds the instant's absence but no recorded reason for it
    /// (a row written before reasons were recorded, or edited since). The five
    /// kinds are the index's own distinction and the page keeps them (invariant
    /// 4, RW49 L-2); the image page says the same fact in words.
    pub taken_reason: Option<&'static str>,
}

impl ViewItemJson {
    /// The item joined with the header facts the index holds for it.
    ///
    /// These are the viewer's own addition to the page shape: the index's
    /// [`ViewItem`] deliberately does not carry them (its `image_meta` is a
    /// companion read), so this is the one place they join the row an agent
    /// reads — the dimensions the grid laid out by, and the taken time the
    /// "Taken" sort ordered by together with the kind of unknown it can be.
    pub fn from_facts(item: &ViewItem, fact: Option<&TileFact>) -> Self {
        let meta = fact.and_then(|fact| fact.meta);
        let taken_ns = meta.and_then(|meta| meta.taken_ns);
        Self {
            path: item.path.clone(),
            sha256: fact.and_then(|fact| fact.sha256.clone()),
            size: item.size,
            mtime_ns: item.mtime_ns,
            added_ns: item.added_ns,
            title: item.title.clone(),
            rating: item.rating,
            note_path: item.note_path.clone(),
            width: meta.and_then(|meta| meta.width),
            height: meta.and_then(|meta| meta.height),
            taken_ns,
            // A reason explains a missing instant, so it travels only with a
            // missing instant. The index pairs the two (`taken_reason` is
            // `None` exactly when `taken_ns` is `Some`); a hand-edited row that
            // carries both reports the instant — the value the sort used — and
            // has nothing left to explain.
            taken_reason: taken_ns.map_or_else(
                || {
                    meta.and_then(|meta| meta.taken_reason)
                        .map(TakenReason::as_str)
                },
                |_| None,
            ),
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

impl ViewPageJson {
    /// The page the index answered, each row joined with its header facts.
    pub fn from_metas(page: &ViewPage, metas: &TileMetas) -> Self {
        let items = page
            .items
            .iter()
            .map(|item| {
                let fact = metas.get(item.path.as_str()).cloned();
                ViewItemJson::from_facts(item, fact.as_ref())
            })
            .collect();
        Self {
            total: page.total,
            page: 1,
            page_size: PAGE_SIZE,
            items,
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
        None => return admission_denied(1),
    };
    let params = ViewParams::parse(query.as_deref().unwrap_or(""));
    let Some(result) = read(&state, &params) else {
        return index_failure(&state, false);
    };
    // W34 audit #12: a narrowed view says its own name — in the tab and in a
    // heading above the grid — while the plain library stays "Library".
    let view_name = view_title(&params, &result.sidebar);
    // A collection is a page of its own (FORMAT §5): the note's header above
    // the members in note order, the count of what is listed. Every other
    // narrowing of the library keeps the plain grid.
    let (body, result_counts) = if let Some(_collection) = &params.collection {
        // `/?c=<note>` is the collection page, whoever narrowed it further,
        // so its header always names the note (view_title puts the collection
        // before the folder that only narrows its members).
        let header_name = view_name.as_deref().unwrap_or("Collection").to_owned();
        match crate::pages::collections::collection_page_parts(&state, &params, &header_name).await
        {
            Some(parts) => {
                // The count says what the page lists — the note's members,
                // once each — never the index's per-link rows, so the header,
                // the toolbar and the tiles can never disagree.
                let shown = parts.items.len() as u64;
                let grid = if parts.items.is_empty() {
                    components::empty_state(&params)
                } else {
                    components::collection_grid(&parts.items, &params)
                };
                (
                    format!("{}{}{}", parts.header, parts.diagnostics, grid),
                    (shown, shown),
                )
            }
            // The note is not a collection, or could not be read: the view
            // the library always gave for `/?c=<path>`, heading and grid and
            // nothing invented.
            None => (
                plain_body(&params, &result.page, &result.metas, view_name.as_deref()),
                (result.page.total, result.page.items.len() as u64),
            ),
        }
    } else {
        (
            plain_body(&params, &result.page, &result.metas, view_name.as_deref()),
            (result.page.total, result.page.items.len() as u64),
        )
    };
    // W76 §3: the info bar appears only when it has something to say — and the
    // one thing that can say it is a photo the URL picks out. A collection page
    // needs no column beside it: its note's header already carries the words the
    // mockup once repeated there (the 10-20 correction: "没有例外"). With nothing
    // selected there is no column at all — never an empty "Select an image" bar —
    // and the grid takes the room.
    let inspector_html = match &params.selection {
        Some(sel) => selection_inspector(&state, sel).await,
        None => None,
    };
    // W76 §1: one pass of the facet queries answers two readers at once. The
    // phone's drawer always shows these current-scope counts (that is its whole
    // job); the desktop sidebar adopts them only once a filter is on, so a facet
    // row's number says what stacking it would add. With no filter the sidebar
    // keeps whole-library counts. A count the index could not answer leaves the
    // drawer off and the sidebar on its whole-library numbers — a number about
    // the library is never invented (invariant 4).
    let facets = state
        .index
        .view_facet_counts(&params.to_index_query(), recent_window(params.recent))
        .ok();
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&result.sidebar),
        facets: if params.active_filter_count() > 0 {
            facets.as_ref()
        } else {
            None
        },
        view: Some(&params),
        result: Some(result_counts),
        notices: &params.notices,
        inspector_html,
        ..Frame::new(
            view_name.as_deref().unwrap_or("Library"),
            Destination::Library,
            wrap_with_panel(facets.as_ref(), &params, &result.sidebar, body),
        )
    };
    Html(frame.render()).into_response()
}

/// The inspector body for the one photo the URL picks out (`?sel=<path>`).
/// Reads the same detail the image page shows; `None` when the path is not a
/// readable image, so a stale or hand-typed selection leaves no column behind.
async fn selection_inspector(state: &AppState, path: &str) -> Option<String> {
    let catalog = state.catalog.clone();
    let path = path.to_owned();
    let detail = tokio::task::spawn_blocking(move || catalog.image_detail(&path))
        .await
        .ok()?
        .ok()?;
    Some(inspector_html_for_detail(&detail))
}

/// The populated inspector for one image: preview, title, path, source, tags and
/// note — the same fields the click-driven inspector fills from, drawn server-side
/// so the chosen photo reads without a script.
fn inspector_html_for_detail(detail: &ImageDetail) -> String {
    let encoded = encode_path(&detail.path);
    let title = detail
        .properties
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            detail
                .path
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(&detail.path)
                .to_owned()
        });
    let title_html = escape_html(&title);
    let mut parts = vec![format!(
        "<a href=\"/image/{encoded}\"><img class=\"inspector-preview-img\" \
         src=\"/thumb/{encoded}\" alt=\"{title_html}\"></a>"
    )];
    parts.push(format!("<h3 class=\"inspector-title\">{title_html}</h3>"));
    parts.push(format!(
        "<div class=\"inspector-path-row\"><code class=\"image-path\">{}</code></div>",
        escape_html(&detail.path)
    ));
    if let Some(source) = detail
        .properties
        .get("source")
        .and_then(Value::as_str)
        .filter(|source| source.starts_with("http"))
    {
        parts.push(format!(
            "<div class=\"inspector-section\"><h4>Source</h4>\
             <a class=\"source-link\" href=\"{}\" rel=\"noreferrer noopener\">{}</a></div>",
            escape_html(source),
            escape_html(source)
        ));
    }
    let tags: Vec<String> = detail
        .properties
        .get("tags")
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|tag| tag.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if !tags.is_empty() {
        let chips = tags
            .iter()
            .map(|tag| {
                format!(
                    "<a class=\"inspector-tag\" href=\"/?tag={}\">{}</a>",
                    crate::view_query::query_value(tag),
                    escape_html(tag)
                )
            })
            .collect::<String>();
        parts.push(format!(
            "<div class=\"inspector-section\"><h4>Tags</h4>\
             <div class=\"inspector-tags\">{chips}</div></div>"
        ));
    }
    let note = detail
        .body_html
        .clone()
        .or_else(|| (!detail.body.trim().is_empty()).then(|| escape_html(&detail.body)));
    if let Some(note) = note {
        parts.push(format!(
            "<div class=\"inspector-section\"><h4>Note</h4>\
             <div class=\"note-body\">{note}</div></div>"
        ));
    }
    parts.push(format!(
        "<p><a href=\"/image/{encoded}\">Open the image page</a></p>"
    ));
    parts.join("")
}

/// The grid, or the collection page, with the phone's filter drawer beside it
/// (the desktop column is gone — see the W76 stylesheet).
///
/// `counts` is the one facet read the page already did for the sidebar; the
/// drawer and the sidebar share it, so the two can never show different numbers
/// for the same value. `None` — the index could not answer — leaves the drawer
/// off the page (the sidebar still shows whole-library counts); a number about
/// the library is never invented (repository invariant 4).
fn wrap_with_panel(
    counts: Option<&FacetCounts>,
    params: &ViewParams,
    sidebar: &SidebarData,
    results: String,
) -> String {
    match counts {
        Some(counts) => {
            filters::with_panel(filters::filter_panel(sidebar, counts, params), results)
        }
        None => results,
    }
}

/// The body every non-collection view uses: the heading and the grid, or
/// what is missing.
fn plain_body(
    params: &ViewParams,
    page: &ViewPage,
    metas: &TileMetas,
    view_name: Option<&str>,
) -> String {
    format!(
        "{}{}",
        view_heading(view_name),
        grid_body(params, page, metas)
    )
}

/// `GET /api/view` — the same page of the same query, as JSON.
pub(crate) async fn view_json(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(1),
    };
    let params = ViewParams::parse(query.as_deref().unwrap_or(""));
    let Some(result) = read(&state, &params) else {
        return index_failure(&state, true);
    };
    let page = ViewPageJson {
        page: params.page,
        ..ViewPageJson::from_metas(&result.page, &result.metas)
    };
    Json(page).into_response()
}

/// `GET /api/sidebar` — folders, collections and tags with their counts.
pub(crate) async fn sidebar_json(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(1),
    };
    match state.index.sidebar_data() {
        Ok(sidebar) => Json(sidebar.as_ref()).into_response(),
        Err(_) => index_failure(&state, true),
    }
}

/// What one request needs from the index: the page, the header facts of the
/// images on it, and the sidebar.
struct Page {
    page: ViewPage,
    metas: TileMetas,
    sidebar: std::sync::Arc<SidebarData>,
}

/// Run one view query and read the sidebar the frame needs. `None` means the
/// index could not answer, and the caller turns that into a reported error
/// rather than an empty page.
///
/// The "Taken" sort is the one order the index answers as its own query
/// (`Index::view_by_taken`); every other view, including the two that keep an
/// order of their own (a collection's embeds, the Recent lens), goes through
/// `Index::view`. Which one is in force is [`ViewParams::orders_by_taken`].
fn read(state: &AppState, params: &ViewParams) -> Option<Page> {
    let query = params.to_index_query();
    let mut page = if params.orders_by_taken() {
        state.index.view_by_taken(&query).ok()?
    } else {
        state.index.view(&query).ok()?
    };
    if params.recent {
        // Recent is "the last [`RECENT_LIMIT`] added" (W34 audit #10); the
        // index counts every match, so the lens states its own size here:
        // 200 or fewer, the same number the sidebar row shows.
        page.total = page.total.min(RECENT_LIMIT);
    }
    // The header facts of the images on this page: one indexed row each, no
    // file opened. A path the index does not hold simply has none, and its
    // tile falls back to the square cell.
    let metas = tile_metas(state, page.items.iter());
    let sidebar = state.index.sidebar_data().ok()?;
    Some(Page {
        page,
        metas,
        sidebar,
    })
}

/// The header facts of the images on one page, keyed by path.
///
/// Each row is a lookup on the primary key of a table the refresh already
/// filled, so a page of a hundred and twenty costs a hundred and twenty
/// primary-key reads rather than a scan; a failure is an unknown, never a
/// guessed shape (invariant 4).
pub(crate) fn tile_metas<'i>(
    state: &AppState,
    items: impl Iterator<Item = &'i ViewItem>,
) -> TileMetas {
    let mut metas = TileMetas::new();
    for item in items {
        // Two reads: the header facts, and the content digest the row already
        // carries. Each answers with a row or with `None` for a path the index
        // never saw; only a failing index is skipped, and it is skipped as the
        // unknown it is rather than failing the page.
        let meta = state.index.image_meta(&item.path).ok().flatten();
        // The digest is the row's own, keyed by the size and mtime this very
        // page was read with — the same reuse the media route makes (W71 M1).
        // Reading it here is what lets every tile ask for its content instead
        // of for a path.
        let sha256 = state
            .index
            .content_hash(&item.path, item.size, item.mtime_ns);
        if meta.is_none() && sha256.is_none() {
            continue;
        }
        metas.insert(item.path.clone(), TileFact { meta, sha256 });
    }
    metas
}

/// The result area: the grid, or what is missing, and the way on.
fn grid_body(params: &ViewParams, page: &ViewPage, metas: &TileMetas) -> String {
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
        components::grid(&page.items, params, metas),
        components::load_more(params, page.total)
    )
}

/// What the browser tab and the heading call this view (W34 audit #12). The
/// order a person would say it in: which collection, which folder, which
/// words, which tags, then the two lenses — a collection is the page its
/// note gives (`/?c=` renders the note's header and its members), so `in`
/// beside it only narrows which members show, and the folder chip and the
/// breadcrumb already say which folder. `None` is the whole library,
/// which names itself without narrowing.
fn view_title(params: &ViewParams, sidebar: &SidebarData) -> Option<String> {
    if let Some(collection) = &params.collection {
        return Some(format!(
            "Collection: {}",
            collection_name(sidebar, collection)
        ));
    }
    if let Some(folder) = &params.folder {
        return Some(format!("Folder: {folder}"));
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
    use dimagine_index::ImageMeta;

    fn item(path: &str) -> ViewItem {
        ViewItem {
            path: path.to_owned(),
            size: 10,
            mtime_ns: 1,
            added_ns: 1,
            title: None,
            rating: None,
            note_path: None,
        }
    }

    fn meta(taken_ns: Option<i64>, taken_reason: Option<TakenReason>) -> ImageMeta {
        ImageMeta {
            width: Some(4),
            height: Some(3),
            taken_ns,
            taken_reason,
        }
    }

    /// The index facts one JSON row carries, from a header alone: these tests
    /// read the header facts a row states, and a row whose digest the index
    /// does not hold carries no content URL (ADR-020).
    fn facts(meta: Option<ImageMeta>) -> TileFact {
        TileFact { meta, sha256: None }
    }

    /// RW49 L-2: the page JSON is what an agent reads, so the five kinds of
    /// "no taken time" reach it as the index recorded them rather than
    /// collapsing into one null — and the reason travels only with the absence
    /// it explains.
    #[test]
    fn the_json_reports_which_kind_of_missing_taken_time_a_row_is() {
        let instant = 1_689_191_647_000_000_000;
        let reason_of = |reason: Option<TakenReason>| {
            ViewItemJson::from_facts(&item("a.jpg"), Some(&facts(Some(meta(None, reason)))))
                .taken_reason
        };
        assert_eq!(reason_of(Some(TakenReason::NoExif)), Some("no-exif"));
        assert_eq!(
            reason_of(Some(TakenReason::ExifWithoutDate)),
            Some("exif-without-date")
        );
        assert_eq!(
            reason_of(Some(TakenReason::UnreadableExif)),
            Some("exif-unreadable")
        );
        assert_eq!(
            reason_of(Some(TakenReason::UnreadableDate)),
            Some("date-unreadable")
        );
        assert_eq!(
            reason_of(Some(TakenReason::UnreadableFile)),
            Some("file-unreadable")
        );
        assert_eq!(reason_of(None), None, "an unrecorded reason says nothing");
        assert_eq!(
            ViewItemJson::from_facts(&item("a.jpg"), None).taken_reason,
            None,
            "a row the index does not hold has no reason to name"
        );

        // A reason explains a missing instant and never rides with one, not even
        // on a row a hand edited into that shape.
        let both = ViewItemJson::from_facts(
            &item("a.jpg"),
            Some(&facts(Some(meta(Some(instant), Some(TakenReason::NoExif))))),
        );
        assert_eq!(both.taken_ns, Some(instant));
        assert_eq!(both.taken_reason, None);
        let valued = ViewItemJson::from_facts(
            &item("a.jpg"),
            Some(&facts(Some(meta(Some(instant), None)))),
        );
        assert_eq!(valued.taken_ns, Some(instant));
        assert_eq!(valued.taken_reason, None);
    }

    #[test]
    fn the_json_page_reports_the_page_it_was_asked_for() {
        let page = ViewPage {
            total: 300,
            items: Vec::new(),
        };
        let json = ViewPageJson::from_metas(&page, &TileMetas::new());
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
        let html = grid_body(&ViewParams::parse("recent=1&p=3"), &past, &TileMetas::new());
        assert!(html.contains("Nothing on this page"), "{html}");
        assert!(!html.contains("The library is empty"), "{html}");
        assert!(!html.contains("Nothing in Recent"), "{html}");

        // A view that really is empty keeps the empty view's words.
        let empty = ViewPage {
            total: 0,
            items: Vec::new(),
        };
        let html = grid_body(&ViewParams::default(), &empty, &TileMetas::new());
        assert!(html.contains("The library is empty"), "{html}");
    }
}
