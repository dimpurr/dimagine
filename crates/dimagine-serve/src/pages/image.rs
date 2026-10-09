//! The image page: the picture, its note, and the facts that travel with it.
//!
//! Phase 2a adds three things to what was here before (K27 motion 4): the
//! view the picture was reached from stays the context — "Back to view"
//! lands on the tile it left, and prev/next walk that view's own order,
//! computed in SQL against the index; the phone lays the picture full width
//! below a pull-up sheet that is a plain section without the script; and
//! "Appears in" names every collection that embeds the picture, with what
//! each collection calls itself, or states that none does.

use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use dimagine_index::{AppearsIn, ImageMeta, Neighbours, TakenReason};
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use serde_json::Value;
use std::collections::HashSet;

use crate::catalog::ImageDetail;
use crate::index_sync::RECENT_LIMIT;
use crate::ui::shell::Frame;
use crate::ui::{encode_path, escape_html, markdown_html, tile_anchor};
use crate::view_query::{query_value, ViewParams, PAGE_SIZE};
use crate::{catalog_error_page, error_page, error_response, AppState};

/// The destination of "Back to view".
const DESTINATION: crate::ui::shell::Destination = crate::ui::shell::Destination::Library;

/// Where a shape becomes extreme and gets cropped for it (DESIGN.md §5.4). The
/// stage's `data-fit` reads the same decision the tile's does, from the tile's
/// own module, so the two cannot drift apart.
use crate::ui::components::fit_of_ratio;

/// A left chevron, the glyph of the prev link. A 16-unit grid and currentColor
/// strokes, like every glyph in the shell.
const NAV_PREV_GLYPH: &str = r#"<svg class="nav-glyph" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M9.5 3.5 5 8l4.5 4.5"/></svg>"#;
/// A right chevron, the glyph of the next link.
const NAV_NEXT_GLYPH: &str = r#"<svg class="nav-glyph" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M6.5 3.5 11 8l-4.5 4.5"/></svg>"#;

/// `GET /image/<path>`.
pub(crate) async fn image_page(
    State(state): State<AppState>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let catalog = state.catalog.clone();
    let detail = match tokio::task::spawn_blocking(move || catalog.image_detail(&path)).await {
        Ok(Ok(detail)) => detail,
        // The image page is a page a browser renders, so its errors
        // are error pages: the frame and the banner every page
        // carries, and the status the JSON routes answer with.
        Ok(Err(error)) => return catalog_error_page(&state, error),
        Err(_) => {
            return error_page(
                &state,
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server failed while reading this image.",
            )
        }
    };
    let context = view_context(query.as_deref());
    // The neighbours and the back-links are index reads about one image; an
    // index that cannot answer does not take the page down — it leaves the
    // picture and its note standing (W34: a half-answered page beats an
    // error where the answer was ornamental).
    //
    // The walk follows the order the view it was reached from keeps: the
    // ordinary sort for everything, and the EXIF taken time for a "Taken"
    // view (`v=sort=taken`), so the arrows and the position ("3 / 124")
    // never contradict the grid behind the back link. The two views that
    // keep an order of their own — a collection's embeds, the Recent lens —
    // are never taken-ordered (`ViewParams::orders_by_taken`), so they land
    // on the ordinary walk here.
    let neighbours = if context.params.orders_by_taken() {
        state
            .index
            .view_neighbours_by_taken(
                &context.params.to_index_query(),
                &detail.path,
                context.lens_limit(),
            )
            .unwrap_or(None)
    } else {
        state
            .index
            .view_neighbours(
                &context.params.to_index_query(),
                &detail.path,
                context.lens_limit(),
            )
            .unwrap_or(None)
    };
    // `None` here is the index declining to answer, which is a different fact
    // from answering "no collection embeds this picture" — the section says
    // which of the two it is (invariant 4).
    let appears_in = state.index.appears_in_titled(&detail.path).ok();
    // What the refresh read from the image's own header: its pixel
    // dimensions and the EXIF taken time. An index that cannot answer leaves
    // them unknown and the page shows nothing about them, rather than
    // inventing a shape or a date (invariant 4).
    let meta = state.index.image_meta(&detail.path).ok().flatten();
    let body = image_body(
        &detail,
        appears_in.as_deref(),
        &context,
        neighbours.as_ref(),
        meta,
    );
    let frame = Frame {
        banner: state.banner(),
        ..Frame::new(&detail.path, DESTINATION, body)
    };
    Html(frame.render()).into_response()
}

/// The view an image page was reached from, carried whole in `?v=` (a tile
/// writes its view there) and parsed once into the parameters the neighbours
/// walk. An unreadable `v` is the plain library: "Back" points at `/` and
/// the neighbours walk All — the only view that could have been meant, and
/// the view `/` itself shows.
struct ViewContext {
    /// The view's own query string, without its leading `?`; the page does
    /// not re-serialise it, so a view this server has not seen before still
    /// round-trips through the arrows and the back link.
    query: String,
    /// The same string parsed ([`ViewParams::parse`]); the neighbours walk
    /// `params.to_index_query()`, which holds the lenses' ordering rules.
    params: ViewParams,
}

impl ViewContext {
    /// The lens limit the grid that sent this page was drawn under: Recent
    /// is "the last [`RECENT_LIMIT`] added", so its pages stay inside that
    /// window and so do the neighbours' offsets.
    fn lens_limit(&self) -> Option<u64> {
        self.params.recent.then_some(RECENT_LIMIT)
    }

    /// Where "Back to view" points: the view the page came from, anchored to
    /// the image's own tile, so the grid comes back where it was — a fragment
    /// jump the browser makes without any script.
    ///
    /// `position` is the image's 1-based place in the view, which the walk
    /// knows whenever it answered at all. The page that holds the tile comes
    /// from it, not from the `p` the `v` carried: the arrows walk the whole
    /// view while that `p` stays where the reader started, so a walk across a
    /// page boundary would otherwise anchor the link to a tile that is not in
    /// the page it opens (RW45 M-2). Without a position — a stale `v`, an index
    /// that did not answer — the carried view travels as it was written.
    ///
    /// A collection is the one view that has no pages to ask for: its page
    /// lists every member at once (`pages/collections.rs`), and a `p` beside
    /// `c` is reported to the reader as ignored (`view_query`). Writing one
    /// from the position would therefore hand the reader back the view they
    /// came from *with a status line about a page they never asked for*
    /// (RW51 M-1), so the carried view travels as written there too. The
    /// anchor still resolves, because every member is on the page. The narrow
    /// case this gives up is a `c=` whose note the index still holds members
    /// for but which cannot be read just now, and so renders as the paginated
    /// library: there the tile can sit past the first page, and the way back
    /// lands on the grid's top instead of on the tile — a walk to re-open,
    /// against a page that misstates what it is showing to every reader of a
    /// collection that is merely long.
    fn back_href(&self, image: &str, position: Option<u64>) -> String {
        let anchor = tile_anchor(image);
        let query = match position {
            Some(position) if self.params.collection.is_none() => {
                with_page(&self.query, page_of_position(position))
            }
            _ => self.query.clone(),
        };
        if query.is_empty() {
            format!("/#{anchor}")
        } else {
            // The query is escaped whole: it arrived validated as a query
            // string, and its `&` characters stay readable links.
            format!("/?{query}#{anchor}", query = escape_html(&query))
        }
    }

    /// The `?v=` a neighbour link carries: the same view, spelled the way the
    /// tiles spell it, so the arrows keep walking the view however it was
    /// written.
    fn view_suffix(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        format!(
            "?v={}",
            utf8_percent_encode(&self.query, percent_encoding::NON_ALPHANUMERIC)
        )
    }

    /// What the position's tooltip calls the view: its own query string, or
    /// "All" for the plain library — the name a person gave it, not one the
    /// page invents.
    fn view_name(&self) -> &str {
        if self.query.is_empty() {
            "All"
        } else {
            &self.query
        }
    }
}

/// The page of the grid that holds the image at this 1-based position: the
/// grid is [`PAGE_SIZE`] tiles to a page, so the tile at position 121 is on
/// page 2. The Recent lens pages inside its own window the same way.
fn page_of_position(position: u64) -> u64 {
    (position.saturating_sub(1)) / u64::from(PAGE_SIZE) + 1
}

/// The view's query string with its page set to `page`. Every other pair
/// travels exactly as it was written; the page is the one thing the position
/// knows better than the link that arrived here. Page 1 needs no pair, so a
/// view that never carried one stays as bare as it was.
fn with_page(query: &str, page: u64) -> String {
    let mut pairs: Vec<String> = Vec::new();
    let mut carried_page = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        if pair.split('=').next() == Some("p") {
            carried_page = true;
            continue;
        }
        pairs.push(pair.to_owned());
    }
    if page > 1 || carried_page {
        pairs.push(format!("p={page}"));
    }
    pairs.join("&")
}

/// Read the `?v=` a tile or a hand wrote, and the view it names.
///
/// `v` is carried whole rather than re-parsed, so a view this server has not
/// seen before still comes back intact; the parse only decides which order
/// the neighbours walk. A `v` that is not a query string falls back to the
/// plain library rather than becoming a link to something unchecked.
fn view_context(request: Option<&str>) -> ViewContext {
    let raw = request.and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("v=").map(str::to_owned))
    });
    let view = match raw {
        Some(ref value) => match percent_decode_str(value).decode_utf8() {
            Ok(decoded) => {
                let view = decoded.strip_prefix('?').unwrap_or(decoded.as_ref());
                if !view.is_empty() && view.chars().all(is_query_character) {
                    view.to_owned()
                } else {
                    String::new()
                }
            }
            Err(_) => String::new(),
        },
        None => String::new(),
    };
    ViewContext {
        params: ViewParams::parse(&view),
        query: view,
    }
}

/// The characters a view query may be built from: what
/// [`ViewParams::to_query_string`] writes, percent-encoding included. Anything
/// else in `v` did not come from this server, so it is not used as a link.
fn is_query_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "%&=+_-.~/?:@!$'()*,;".contains(character)
}

fn image_body(
    detail: &ImageDetail,
    appears_in: Option<&[AppearsIn]>,
    context: &ViewContext,
    neighbours: Option<&Neighbours>,
    meta: Option<ImageMeta>,
) -> String {
    let path = encode_path(&detail.path);
    let title = title_of(detail);
    // The dimensions the Image section states from the index, read once: the
    // Properties list needs to know which numbers the page has already said.
    let indexed = indexed_dimensions(meta);
    let mut out = String::from("<div class=\"image-page\">");
    // The header: the way back, the walk through the view, and the original
    // file — one line per region, in the order a person reads them.
    out.push_str("<header class=\"image-header\">");
    out.push_str(&format!(
        "<a class=\"back-link\" href=\"{}\">← Back to view</a>",
        context.back_href(&detail.path, neighbours.map(|walk| walk.position))
    ));
    out.push_str(&nav_html(context, neighbours));
    out.push_str(&format!(
        "<a class=\"original-link\" href=\"/raw/{path}\">View original</a>"
    ));
    out.push_str("</header>");

    // The stage takes the picture's shape (W34 #2): a tall screenshot fills the
    // column and scrolls inside the well instead of painting a sliver in a big
    // empty box. The indexed dimensions settle it on the first paint, and a
    // note that records them too is the fallback for an image the index does
    // not know; otherwise the script measures the loaded image, and a shape
    // nobody knows keeps the whole picture.
    out.push_str(&format!(
        "<div class=\"image-detail-layout\"><figure class=\"image-stage\"{}>\
         <img src=\"/media/{path}\" alt=\"{}\"></figure>",
        fit_attribute(stage_fit(detail, meta)),
        escape_html(&title)
    ));
    out.push_str("<div class=\"image-panel\" id=\"image-panel\">");
    // The pull-up sheet's handle (W45): it is the label a person drags on a
    // phone, and it is nothing at all without the script — the panel is
    // below the picture exactly as before.
    out.push_str(&format!(
        "<button type=\"button\" class=\"sheet-handle\" aria-expanded=\"false\" \
         aria-controls=\"image-panel\" aria-label=\"Show details\">\
         <span class=\"sheet-grip\" aria-hidden=\"true\"></span>\
         <span class=\"sheet-label\">{}</span></button>",
        escape_html(&title)
    ));
    out.push_str(&format!(
        "<h1 class=\"image-title\">{}</h1>",
        escape_html(&title)
    ));
    out.push_str(&path_row(&detail.path));
    out.push_str(&image_facts_section(meta));
    if let Some(source) = source_link(detail) {
        out.push_str(&format!("<p class=\"image-source\">Source: {source}</p>"));
    }
    if let Some(tags) = tags_of(detail) {
        out.push_str(&format!(
            "<section class=\"image-section\"><h2>Tags</h2><div class=\"inspector-tags\">{tags}</div></section>"
        ));
    }
    out.push_str(&appears_in_html(appears_in));
    if let Some(note) = detail.note_path.as_deref() {
        // The catalog renders the body with its links
        // resolved (FORMAT §5.1); a detail without the
        // rendered HTML still shows the note, unlinked.
        let body_html = detail
            .body_html
            .clone()
            .unwrap_or_else(|| markdown_html(&detail.body, None));
        out.push_str(&format!(
            "<section class=\"image-section\"><h2>Note</h2><p class=\"note-path\">{}</p>\
             <article class=\"note-body\">{}</article></section>",
            escape_html(note),
            body_html
        ));
    }
    if let Some(error) = &detail.front_matter_error {
        out.push_str(&format!(
            "<p class=\"error\">Front matter error: {}</p>",
            escape_html(error)
        ));
    }
    out.push_str(&properties_section(detail, indexed));
    out.push_str(&format!(
        "<section class=\"image-section\"><h2>Raw source files</h2><ul>{}</ul></section>",
        detail
            .raw_files
            .iter()
            .map(|file| format!("<li>{}</li>", escape_html(file)))
            .collect::<String>()
    ));
    out.push_str("</div></div></div>");
    out
}

/// The stage's shape, from the indexed dimensions first and the note's own
/// recorded ones second.
///
/// The index reads the header the file actually has, after EXIF orientation,
/// so it is the better source; the note's `width`/`height` (the Eagle import
/// writes them) is the fallback for an image the index has not read yet. When
/// neither has both dimensions the fit is unknown and the stage says nothing:
/// it does not guess (invariant 4).
fn stage_fit(detail: &ImageDetail, meta: Option<ImageMeta>) -> Option<&'static str> {
    if let Some((width, height)) = indexed_dimensions(meta) {
        return Some(fit_of_ratio(f64::from(width) / f64::from(height)));
    }
    known_fit(&detail.properties)
}

/// The picture's own shape as the index recorded it: both dimensions, neither
/// zero. `None` is the honest unknown — no row, half a pair, or a pair of
/// zeroes — and nothing downstream guesses a shape from it (invariant 4).
fn indexed_dimensions(meta: Option<ImageMeta>) -> Option<(u32, u32)> {
    let ImageMeta {
        width: Some(width),
        height: Some(height),
        ..
    } = meta?
    else {
        return None;
    };
    (width > 0 && height > 0).then_some((width, height))
}

/// The arrows and the position: prev/next within the view the page was
/// reached from, links that work without the script (the keys and the swipe
/// in `app.js` only follow them), and "3 / 124" in the middle — the position
/// in that view, the tooltip naming the view itself.
///
/// A view this image is not part of answers [`None`], and the whole walk
/// stays out of the header: the back link alone is the context left, which is
/// exactly what a stale `v=` means.
fn nav_html(context: &ViewContext, neighbours: Option<&Neighbours>) -> String {
    let Some(neighbours) = neighbours else {
        return String::new();
    };
    let suffix = escape_html(&context.view_suffix());
    let mut nav = String::from("<nav class=\"image-nav\" aria-label=\"Images in this view\">");
    nav.push_str(&nav_arrow(
        "image-nav-prev",
        "Previous image",
        neighbours.previous.as_deref(),
        &suffix,
        NAV_PREV_GLYPH,
    ));
    nav.push_str(&format!(
        "<span class=\"image-nav-position\" title=\"{}\">{} / {}</span>",
        escape_html(context.view_name()),
        neighbours.position,
        neighbours.total
    ));
    nav.push_str(&nav_arrow(
        "image-nav-next",
        "Next image",
        neighbours.next.as_deref(),
        &suffix,
        NAV_NEXT_GLYPH,
    ));
    nav.push_str("</nav>");
    nav
}

/// One arrow: a link to that image in the same view, or — at the end of the
/// view — a quiet stub that keeps the row's shape, carrying no link and
/// nothing for a screen reader to visit.
fn nav_arrow(kind: &str, label: &str, target: Option<&str>, suffix: &str, glyph: &str) -> String {
    match target {
        Some(path) => format!(
            "<a class=\"{kind}\" href=\"/image/{}{suffix}\" title=\"{}\" \
             aria-label=\"{label}\">{glyph}</a>",
            encode_path(path),
            escape_html(leaf_name(path))
        ),
        None => format!("<span class=\"{kind} nav-end\" aria-hidden=\"true\">{glyph}</span>"),
    }
}

/// The file name of a library-relative path.
fn leaf_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// The stage's `data-fit`, when the note's front matter records the picture's
/// shape. The Eagle import writes `width` and `height`, and a served rendition
/// keeps the source aspect ratio, so what the note records is what the stage
/// shows. A missing, non-numeric or zero dimension says nothing about the
/// shape, and the stage then says nothing — it does not guess (invariant 4).
fn known_fit(properties: &Value) -> Option<&'static str> {
    let width = numeric_property(properties, "width")?;
    let height = numeric_property(properties, "height")?;
    Some(fit_of_ratio(width / height))
}

/// A property as a usable number: `3429`, `"3429"` and `1.5` all count.
/// Anything else — absent, a list, a word, a zero — is not a dimension.
fn numeric_property(properties: &Value, key: &str) -> Option<f64> {
    let number = match properties.get(key)? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (number.is_finite() && number > 0.0).then_some(number)
}

/// The `data-fit="…"` attribute, or nothing when the shape is unknown.
fn fit_attribute(fit: Option<&str>) -> String {
    fit.map(|fit| format!(" data-fit=\"{fit}\""))
        .unwrap_or_default()
}

/// What the image's own header says about it: its pixel dimensions and the
/// EXIF "taken" time, as the refresh read them.
///
/// Two rows, in the Properties table's shape (DESIGN.md §4.6), placed beside
/// the picture they describe rather than among the note's own properties —
/// these are facts about the bytes, which the note does not own.
///
/// The section appears when the index holds a row for the image. A fact that is
/// `NULL` there is a recorded unknown — a header that could not be read, or a
/// picture with no date in it — and it is shown as the word "Unknown" rather
/// than left out or filled with a placeholder date (invariant 4). An image the
/// index holds no row for at all says nothing here: there is nothing recorded
/// to report.
fn image_facts_section(meta: Option<ImageMeta>) -> String {
    let Some(meta) = meta else {
        return String::new();
    };
    let dimensions = match indexed_dimensions(Some(meta)) {
        Some((width, height)) => format!("<dd>{width} × {height}</dd>"),
        // One dimension without the other is no shape at all, so the pair is
        // the unknown rather than half a number.
        None => "<dd>Unknown</dd>".to_owned(),
    };
    let taken = match meta.taken_ns.and_then(taken_value) {
        Some(taken) => taken,
        // An unknown instant is not one fact but five, and the index recorded
        // which one this is; the page says which (RW49 L-2, and what
        // `Index::image_meta` promises a "Taken" detail).
        None => format!("<dd>{}</dd>", taken_unknown(meta.taken_reason)),
    };
    format!(
        "<section class=\"image-section\"><h2>Image</h2><dl class=\"property-list\">\
         <div class=\"property\"><dt>Dimensions</dt>{dimensions}</div>\
         <div class=\"property\"><dt>Taken</dt>{taken}</div></dl></section>"
    )
}

/// The kind of unknown a missing taken time is, in the words a person reads.
///
/// The index distinguishes these five because they need different answers
/// ([`dimagine_index::TakenReason`]), and it keeps the distinction all the way
/// out of the database; the page is where the distinction is either kept or
/// dropped, and dropping it is what invariant 4 forbids. `None` is the sixth
/// state — the row records no reason at all, which is what a database written
/// before the reason existed, or hand-edited since, carries — and it says only
/// what it knows: that there is no taken time here.
fn taken_unknown(reason: Option<TakenReason>) -> &'static str {
    match reason {
        Some(TakenReason::NoExif) => "Unknown — the file carries no EXIF",
        Some(TakenReason::ExifWithoutDate) => "Unknown — its EXIF names no date",
        Some(TakenReason::UnreadableExif) => "Unknown — its EXIF could not be read",
        Some(TakenReason::UnreadableDate) => "Unknown — the date it carries is unreadable",
        Some(TakenReason::UnreadableFile) => "Unknown — the file is not a readable image",
        None => "Unknown",
    }
}

/// The EXIF taken time, as a machine-readable `<time>` beside the words a
/// person reads.
///
/// The instant is stored in UTC and the camera's own offset is not stored with
/// it, so the page says UTC rather than re-labelling the time as the reader's
/// zone — a shift nobody recorded is not applied to a recorded time. Seconds
/// appear only when the EXIF block refined below the minute, so an ordinary
/// photo is not padded with a `:00` that was never written.
fn taken_value(taken_ns: i64) -> Option<String> {
    let seconds = taken_ns.div_euclid(1_000_000_000);
    let nanos = taken_ns.rem_euclid(1_000_000_000) as u32;
    let moment = DateTime::from_timestamp(seconds, nanos)?;
    let clock = if nanos == 0 {
        moment.format("%d %b %Y, %H:%M")
    } else {
        moment.format("%d %b %Y, %H:%M:%S")
    };
    Some(format!(
        "<dd><time datetime=\"{}\">{} UTC</time></dd>",
        escape_html(&moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        escape_html(&clock.to_string())
    ))
}

/// The properties as a definition list: the known FORMAT
/// §3.1 properties first, in show order, then any property
/// an import added, by name. The whole front matter stays
/// reachable, collapsed, as the raw JSON.
///
/// `indexed` is the dimensions pair the Image section above printed, so a note
/// that carries the identical pair is not asked to say it again.
fn properties_section(detail: &ImageDetail, indexed: Option<(u32, u32)>) -> String {
    if detail.properties.is_null() {
        return String::new();
    }
    let mut out = String::from("<section class=\"image-section\"><h2>Properties</h2>");
    let rows = property_rows(&detail.properties, indexed);
    if !rows.is_empty() {
        out.push_str("<dl class=\"property-list\">");
        for (term, definition) in rows {
            out.push_str(&format!(
                "<div class=\"property\"><dt>{term}</dt><dd>{definition}</dd></div>"
            ));
        }
        out.push_str("</dl>");
    }
    out.push_str(&format!(
        "<details class=\"properties-raw\"><summary>Raw properties</summary>\
         <pre class=\"properties\">{}</pre></details></section>",
        escape_html(&crate::catalog::yaml_json_string(&detail.properties))
    ));
    out
}

/// The FORMAT §3.1 properties, in the order they are shown.
const PROPERTY_ORDER: &[&str] = &[
    "title",
    "tags",
    "rating",
    "source",
    "author",
    "license",
    "created",
    "imported",
    "added",
    "id",
    "copied_from",
    "sources",
];

/// The property rows: `(label, rendered value)` pairs,
/// known properties first, then the unknown ones by name.
fn property_rows(properties: &Value, indexed: Option<(u32, u32)>) -> Vec<(String, String)> {
    let Some(object) = properties.as_object() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let mut shown = HashSet::new();
    for key in PROPERTY_ORDER {
        let Some(value) = object.get(*key) else {
            continue;
        };
        if empty_property(value) {
            continue;
        }
        shown.insert(*key);
        rows.push((property_label(key), property_value(key, value)));
    }
    let mut unknown: Vec<&str> = object
        .keys()
        .filter(|key| {
            !shown.contains(key.as_str()) && !repeats_indexed_pair(properties, key, indexed)
        })
        .map(String::as_str)
        .collect();
    unknown.sort_unstable();
    for key in unknown {
        let Some(value) = object.get(key) else {
            continue;
        };
        if empty_property(value) {
            continue;
        }
        rows.push((escape_html(key), property_value(key, value)));
    }
    rows
}

/// Whether this note property repeats the number the Image section has just
/// printed from the index — decided on the pair, not on the key.
///
/// An import that writes `width`/`height` into the note records the picture's
/// shape a second time, and when the two agree they are one fact in two places
/// with nothing to tell the reader they are the same pair (RW49 L-5). The page
/// states it once, in the section that owns facts about the bytes; the raw
/// front matter below still carries the note's own copy verbatim.
///
/// A pair that *disagrees* with the index is two claims rather than one — a
/// note can carry the shape before EXIF orientation while the index read the
/// one after — so both rows stay, where the reader can see them disagree. That
/// holds even when only half the pair disagrees (RW53 review Low-1): a note
/// whose `width` matches and whose `height` does not keeps both rows, because
/// suppressing the agreeing half alone would leave a lone `height` that reads
/// as a note carrying a height and no width, beside the pair it disagrees with.
/// Nothing is dropped on the way: the note's numbers are only left out when
/// the page has already said the exact pair.
fn repeats_indexed_pair(properties: &Value, key: &str, indexed: Option<(u32, u32)>) -> bool {
    if !matches!(key, "width" | "height") {
        return false;
    }
    let Some((width, height)) = indexed else {
        return false;
    };
    numeric_property(properties, "width") == Some(f64::from(width))
        && numeric_property(properties, "height") == Some(f64::from(height))
}

/// Whether a property has nothing to show: a `null`, or
/// an empty list or object.
fn empty_property(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
        _ => false,
    }
}

/// The human label of a known property.
fn property_label(key: &str) -> String {
    let label = match key {
        "title" => "Title",
        "tags" => "Tags",
        "rating" => "Rating",
        "source" => "Source",
        "author" => "Author",
        "license" => "License",
        "created" => "Created",
        "imported" => "Imported",
        "added" => "Added",
        "id" => "ID",
        "copied_from" => "Copied from",
        "sources" => "Sources",
        _ => key,
    };
    escape_html(label)
}

/// One property value, rendered for the definition list.
fn property_value(key: &str, value: &Value) -> String {
    match key {
        "tags" => tags_value(value),
        "source" => source_value(value),
        "created" | "imported" | "added" => date_value(value),
        "id" | "copied_from" => format!("<code>{}</code>", scalar_value(value)),
        _ => scalar_value(value),
    }
}

/// A property value as text: scalars as themselves, lists
/// and maps as the JSON they are.
fn scalar_value(value: &Value) -> String {
    match value {
        Value::String(text) => escape_html(text),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => escape_html(&other.to_string()),
    }
}

/// `tags` as links to the tagged view.
fn tags_value(value: &Value) -> String {
    let Some(tags) = value.as_array() else {
        return scalar_value(value);
    };
    tags.iter()
        .filter_map(|tag| tag.as_str())
        .map(|tag| {
            format!(
                "<a class=\"inspector-tag\" href=\"/?tag={}\">{}</a>",
                escape_html(&query_value(tag)),
                escape_html(tag)
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

/// `source` as a link when it is a URL, as text otherwise.
fn source_value(value: &Value) -> String {
    let Some(source) = value.as_str() else {
        return scalar_value(value);
    };
    if source.starts_with("http://") || source.starts_with("https://") {
        format!(
            "<a class=\"source-link\" href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
            escape_html(source),
            escape_html(source)
        )
    } else {
        escape_html(source)
    }
}

/// A date or datetime property: human-readable, with the
/// original value in the `title` attribute.
fn date_value(value: &Value) -> String {
    let Some(text) = value.as_str() else {
        return scalar_value(value);
    };
    format!(
        "<span title=\"{}\">{}</span>",
        escape_html(text),
        escape_html(&human_date(text))
    )
}

/// An ISO 8601 date or datetime (FORMAT §3.1), in a form
/// a person reads at a glance. Anything else is shown as
/// it was written.
fn human_date(value: &str) -> String {
    if let Ok(datetime) = DateTime::parse_from_rfc3339(value) {
        return datetime.format("%d %b %Y, %H:%M (%:z)").to_string();
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date.format("%d %b %Y").to_string();
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(datetime) = NaiveDateTime::parse_from_str(value, format) {
            return datetime.format("%d %b %Y, %H:%M").to_string();
        }
    }
    value.to_owned()
}

/// The path, with a button that copies it. Without JavaScript the button does
/// nothing, so the path is shown as text either way.
fn path_row(path: &str) -> String {
    format!(
        "<div class=\"inspector-path-row\"><code class=\"image-path\">{}</code>\
         <button type=\"button\" class=\"copy-btn\" data-copy-text=\"{}\">Copy</button></div>",
        escape_html(path),
        escape_html(path)
    )
}

/// "Appears in": every collection note that embeds this image, named the way
/// the sidebar names a collection — its own `title`, falling back to the
/// file name — and linked to its view (K27 motion 6). None is a state a
/// person reads, not a section left off: this picture is in no collection.
///
/// An index that did not answer is a third state and says so: "not in any
/// collection" is a fact about the library, and a closed index does not know
/// it (invariant 4 — the same rule the walk follows when the index is silent,
/// which is why the arrows are simply absent rather than said to be nowhere).
fn appears_in_html(appears_in: Option<&[AppearsIn]>) -> String {
    let mut out = String::from("<section class=\"image-section\"><h2>Appears in</h2>");
    match appears_in {
        None => out.push_str(
            "<p class=\"appears-in-empty\">The collections could not be read right now.</p>",
        ),
        Some([]) => out.push_str("<p class=\"appears-in-empty\">Not in any collection.</p>"),
        Some(collections) => {
            out.push_str("<ul class=\"appears-in\">");
            for collection in collections {
                let label = if collection.title.is_empty() {
                    leaf_name(&collection.note_path).to_owned()
                } else {
                    collection.title.clone()
                };
                out.push_str(&format!(
                    "<li><a href=\"/?c={}\" title=\"{}\">{}</a></li>",
                    escape_html(&query_value(&collection.note_path)),
                    escape_html(&collection.note_path),
                    escape_html(&label)
                ));
            }
            out.push_str("</ul>");
        }
    }
    out.push_str("</section>");
    out
}

/// Note `title`, else the file name (spec §4: alt text).
fn title_of(detail: &ImageDetail) -> String {
    detail
        .properties
        .get("title")
        .and_then(|title| title.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| leaf_name(&detail.path).to_owned())
}

fn tags_of(detail: &ImageDetail) -> Option<String> {
    let tags = detail.properties.get("tags")?.as_array()?;
    let links: Vec<String> = tags
        .iter()
        .filter_map(|tag| tag.as_str())
        .map(|tag| {
            format!(
                "<a class=\"inspector-tag\" href=\"/?tag={}\">{}</a>",
                escape_html(&query_value(tag)),
                escape_html(tag)
            )
        })
        .collect();
    (!links.is_empty()).then(|| links.join(""))
}

/// `source` is a URL recorded by an import; it is shown when the note has one.
fn source_link(detail: &ImageDetail) -> Option<String> {
    let source = detail.properties.get("source")?.as_str()?;
    if !(source.starts_with("http://") || source.starts_with("https://")) {
        return None;
    }
    Some(format!(
        "<a class=\"source-link\" href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
        escape_html(source),
        escape_html(source)
    ))
}

/// `GET /api/image/<path>` — unchanged, for agents.
pub(crate) async fn image_json(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let catalog = state.catalog.clone();
    match tokio::task::spawn_blocking(move || catalog.image_detail(&path)).await {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail() -> ImageDetail {
        ImageDetail {
            path: "refs/猫.png".into(),
            properties: json!({
                "title": "猫",
                "tags": ["eagle", "cat"],
                "source": "https://example.com/pic/1",
            }),
            front_matter_error: None,
            body: "A **quiet** cat.\n<script>alert(1)</script>\n".into(),
            body_html: None,
            raw_files: vec!["猫.png.eagle.json".into()],
            note_path: Some("refs/猫.png.md".into()),
        }
    }

    /// The plain-library context: no `v` at all.
    fn plain() -> (ViewContext, &'static str) {
        (view_context(None), "refs/猫.png")
    }

    fn neighbours_at(position: u64, total: u64) -> Neighbours {
        Neighbours {
            position,
            total,
            previous: Some(format!("refs/prev{position}.png")),
            next: Some(format!("refs/next{position}.png")),
        }
    }

    #[test]
    fn the_page_offers_a_way_back_that_lands_on_the_tile_it_left() {
        let image = "refs/猫.png";
        // A tile carries the view query without its `?`.
        let context = view_context(Some("v=tag%3Deagle"));
        let html = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(
            html.contains(&format!("href=\"/?tag=eagle#{}\"", tile_anchor(image))),
            "{html}"
        );
        // A hand-written link may include it.
        let context = view_context(Some("v=%3Ftag%3Deagle"));
        assert_eq!(
            context.back_href(image, None),
            format!("/?tag=eagle#{}", tile_anchor(image))
        );
        let plain = view_context(None);
        assert_eq!(
            plain.back_href(image, None),
            format!("/#{}", tile_anchor(image))
        );
        let html = image_body(&detail(), Some(&[]), &plain, None, None);
        assert!(
            html.contains(&format!("href=\"/#{}\">← Back to view", tile_anchor(image))),
            "{html}"
        );
    }

    /// RW45 M-2: the arrows walk the whole view while the `p` the reader
    /// arrived with stays where they arrived, so the page the way back asks for
    /// is the one the position says the tile is on — an anchor into a page that
    /// is not there lands at the top of the grid, not on the tile.
    #[test]
    fn the_way_back_asks_for_the_page_the_tile_is_on() {
        let image = "refs/猫.png";
        let anchor = tile_anchor(image);
        // A tile from page 3 of a tag view, walked to a later page and back
        // out past the first one.
        let carried = view_context(Some("v=tag%3Deagle%26p%3D3"));
        assert_eq!(
            carried.back_href(image, Some(121)),
            format!("/?tag=eagle&amp;p=2#{anchor}"),
            "the carried filter travels, the page comes from the position"
        );
        assert_eq!(
            carried.back_href(image, Some(1)),
            format!("/?tag=eagle&amp;p=1#{anchor}"),
            "the page the walk carried there is replaced, not left behind"
        );
        // The plain library pages too, and its first page needs no pair.
        let plain = view_context(None);
        assert_eq!(plain.back_href(image, Some(120)), format!("/#{anchor}"));
        assert_eq!(
            plain.back_href(image, Some(121)),
            format!("/?p=2#{anchor}"),
            "the first tile of the second page"
        );
        assert_eq!(
            plain.back_href(image, Some(361)),
            format!("/?p=4#{anchor}"),
            "{PAGE_SIZE} tiles to a page"
        );
        // Nothing to place the image by — a stale `v`, an index that did not
        // answer — and the carried view travels exactly as it was written.
        assert_eq!(
            carried.back_href(image, None),
            format!("/?tag=eagle&amp;p=3#{anchor}")
        );
    }

    /// RW51 M-1: a collection page lists all of its members at once, so the
    /// position settles nothing about paging there — and a `p` written onto the
    /// way back hands the reader the view they came from *plus* a status line
    /// about a page they never asked for ("Ignored page number: a collection
    /// lists all of its members at once"). The view travels as it was written;
    /// the anchor still resolves, every member being on the page.
    #[test]
    fn the_way_back_from_a_collection_asks_for_no_page() {
        let image = "refs/猫.png";
        let anchor = tile_anchor(image);
        let collection = view_context(Some("v=c%3Dalbum.md"));
        for position in [1, 121, 205] {
            assert_eq!(
                collection.back_href(image, Some(position)),
                format!("/?c=album.md#{anchor}"),
                "position {position} of a view that has no pages"
            );
        }
        // A `p` the reader arrived with is their own, and the page answers it
        // rather than the back link quietly rewriting it.
        let carried = view_context(Some("v=c%3Dalbum.md%26p%3D3"));
        assert_eq!(
            carried.back_href(image, Some(151)),
            format!("/?c=album.md&amp;p=3#{anchor}")
        );
        // A folder view, which really does page, keeps the page the position
        // says the tile is on.
        let folder = view_context(Some("v=in%3Drefs"));
        assert_eq!(
            folder.back_href(image, Some(151)),
            format!("/?in=refs&amp;p=2#{anchor}")
        );
    }

    /// RW45 M-3: an index that did not answer is not an answer of "none". The
    /// section says which of the two it is (invariant 4), the way the walk
    /// leaves the arrows out rather than claiming the picture is nowhere.
    #[test]
    fn a_collection_answer_that_did_not_arrive_is_not_called_an_empty_one() {
        let (context, _) = plain();
        let silent = image_body(&detail(), None, &context, None, None);
        assert!(
            silent.contains(
                "<p class=\"appears-in-empty\">The collections could not be read right now.</p>"
            ),
            "{silent}"
        );
        assert!(
            !silent.contains("Not in any collection."),
            "an unknown is not an empty row: {silent}"
        );
        // The answered "none" keeps its own sentence, unchanged.
        let answered = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(answered.contains("Not in any collection."), "{answered}");
        assert!(!answered.contains("could not be read"), "{answered}");
    }

    #[test]
    fn a_v_that_is_not_a_query_string_falls_back_to_the_library() {
        let image = "refs/猫.png";
        let plain = format!("/#{}", tile_anchor(image));
        for query in [
            Some("v="),
            Some("v=%3F"),
            Some("other=1"),
            Some("v=%FF"),
            None,
        ] {
            assert_eq!(
                view_context(query).back_href(image, None),
                plain,
                "{query:?}"
            );
        }
    }

    /// Whatever the `v` holds, the result stays a path on this origin.
    #[test]
    fn back_to_view_never_leaves_the_origin() {
        let image = "refs/猫.png";
        for value in [
            "v=https%3A%2F%2Fevil.example%2F",
            "v=%2F%2Fevil.example",
            "v=in%3D..%2F..",
            "v=%22%3E%3Cscript%3E",
        ] {
            let context = view_context(Some(value));
            let target = context.back_href(image, None);
            assert!(
                target.starts_with('/') && !target.starts_with("//"),
                "{value} produced {target}"
            );
            assert!(!target.contains("<"), "{value} produced {target}");
        }
    }

    #[test]
    fn the_arrows_walk_the_view_and_the_position_names_it() {
        let context = view_context(Some("v=tag%3Deagle"));
        let html = image_body(
            &detail(),
            Some(&[]),
            &context,
            Some(&neighbours_at(3, 124)),
            None,
        );
        // Both links carry the same view the page arrived with, spelled as
        // the tiles spell it, and name their image for the tooltip.
        assert!(
            html.contains(
                "<a class=\"image-nav-prev\" href=\"/image/refs/prev3.png?v=tag%3Deagle\" \
                 title=\"prev3.png\" aria-label=\"Previous image\">"
            ),
            "{html}"
        );
        assert!(
            html.contains(
                "<a class=\"image-nav-next\" href=\"/image/refs/next3.png?v=tag%3Deagle\" \
                 title=\"next3.png\" aria-label=\"Next image\">"
            ),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"image-nav-position\" title=\"tag=eagle\">3 / 124</span>"),
            "{html}"
        );
        // The plain library needs no ?v= on its arrows ...
        let (plain, _) = plain();
        let html = image_body(
            &detail(),
            Some(&[]),
            &plain,
            Some(&neighbours_at(1, 4)),
            None,
        );
        assert!(html.contains("href=\"/image/refs/prev1.png\" "), "{html}");
        assert!(html.contains("title=\"All\">1 / 4</span>"), "{html}");
    }

    /// At the ends of a view the arrow stays as a stub: no link, no place for
    /// a screen reader to visit, and the row keeps its shape (§4.8's quiet
    /// holds here too — a missing neighbour is not an error to look at).
    #[test]
    fn the_ends_of_the_view_hold_their_shape_but_go_nowhere() {
        let (context, _) = plain();
        let mut neighbours = neighbours_at(1, 4);
        neighbours.previous = None;
        let html = image_body(&detail(), Some(&[]), &context, Some(&neighbours), None);
        assert!(
            html.contains("<span class=\"image-nav-prev nav-end\" aria-hidden=\"true\">"),
            "{html}"
        );
        assert!(
            !html.contains("class=\"image-nav-prev\" href="),
            "the first image offers no previous link: {html}"
        );
        assert!(
            html.contains("<a class=\"image-nav-next\" href=\"/image/refs/next1.png\""),
            "{html}"
        );
    }

    /// A view this image is not part of answers nothing: the header keeps
    /// only the back link, which is exactly what a stale `v=` means.
    #[test]
    fn an_image_the_view_does_not_show_walks_nowhere() {
        let context = view_context(Some("c=none.md"));
        let html = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(!html.contains("image-nav"), "{html}");
        assert!(html.contains("Back to view"), "{html}");
    }

    #[test]
    fn the_path_is_shown_and_copyable_and_the_note_is_sanitised() {
        let (context, _) = plain();
        let html = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(html.contains("data-copy-text=\"refs/猫.png\""));
        assert!(html.contains("A <strong>quiet</strong> cat"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn provenance_travels_with_the_picture() {
        let (context, _) = plain();
        let html = image_body(
            &detail(),
            Some(&[
                AppearsIn {
                    note_path: "browse.md".into(),
                    title: "Browse".into(),
                },
                AppearsIn {
                    note_path: "sets/eagle.md".into(),
                    title: String::new(),
                },
            ]),
            &context,
            None,
            None,
        );
        assert!(html.contains("href=\"https://example.com/pic/1\""));
        assert!(html.contains("rel=\"noreferrer noopener\""));
        assert!(html.contains("href=\"/?tag=eagle\""));
        // A collection is named by its title when it has one, by its file
        // name when it does not, and linked by the path either way.
        assert!(html.contains("href=\"/?c=browse.md\" title=\"browse.md\">Browse</a>"));
        assert!(
            html.contains("href=\"/?c=sets/eagle.md\" title=\"sets/eagle.md\">eagle.md</a>"),
            "{html}"
        );
        assert!(html.contains("猫.png.eagle.json"));
    }

    /// A picture with no note beside it has no tags and no note text to show,
    /// and the panel says nothing about either rather than opening an empty
    /// section; what it does keep is the picture itself and the collections
    /// that name it (K27 principle 4: never a picture without its facts).
    #[test]
    fn an_image_with_no_note_shows_no_tags_and_no_note() {
        let bare = ImageDetail {
            path: "plain.png".into(),
            properties: json!({}),
            front_matter_error: None,
            body: String::new(),
            body_html: None,
            raw_files: Vec::new(),
            note_path: None,
        };
        let (context, _) = plain();
        let html = image_body(&bare, Some(&[]), &context, None, None);
        assert!(!html.contains("<h2>Tags</h2>"), "{html}");
        assert!(!html.contains("<h2>Note</h2>"), "{html}");
        assert!(html.contains("alt=\"plain.png\""), "{html}");
        assert!(html.contains("Not in any collection."), "{html}");
    }

    /// A collection's title is library content: whatever it says, it reaches
    /// the page escaped, and the link stays a link to this origin.
    #[test]
    fn a_hostile_collection_title_is_named_escaped() {
        let (context, _) = plain();
        let appears_in = vec![AppearsIn {
            note_path: "sets/x.md".into(),
            title: "\"><img onerror=alert(1)><svg><script>".into(),
        }];
        let html = image_body(&detail(), Some(&appears_in), &context, None, None);
        assert!(
            html.contains("Appears in</h2><ul class=\"appears-in\">"),
            "{html}"
        );
        assert!(
            html.contains("&quot;&gt;&lt;img onerror=alert(1)&gt;&lt;svg&gt;&lt;script&gt;"),
            "{html}"
        );
        assert!(
            !html.contains("\"><img onerror"),
            "the raw title rendered as markup: {html}"
        );
        assert!(!html.contains("<script>"), "{html}");
        // The path stays query-encoded inside the href, never pasted raw —
        // a `/` is a safe query character, everything hostile is escaped.
        assert!(html.contains("href=\"/?c=sets/x.md\""), "{html}");
    }

    /// No collection is a state, not a silence: the section says so in one
    /// quiet line (K27 motion 6's back-link list, with §4.8's plain speech).
    #[test]
    fn an_image_in_no_collection_says_so() {
        let (context, _) = plain();
        let html = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(html.contains("<h2>Appears in</h2>"), "{html}");
        assert!(html.contains("Not in any collection."), "{html}");
    }

    #[test]
    fn the_sheet_handle_names_the_picture_and_controls_the_panel() {
        let (context, _) = plain();
        let html = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(
            html.contains(
                "class=\"sheet-handle\" aria-expanded=\"false\" aria-controls=\"image-panel\""
            ),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"sheet-label\">猫</span>"),
            "{html}"
        );
        // The state of a disclosure belongs to the button that changes it: the
        // panel is the region named by `aria-controls`, and an `aria-expanded`
        // on a plain <div> would be read as a second, meaningless one.
        assert!(
            html.contains("<div class=\"image-panel\" id=\"image-panel\">"),
            "{html}"
        );
    }

    #[test]
    fn a_source_that_is_not_a_url_is_not_linked() {
        let mut detail = detail();
        detail.properties = json!({"source": "javascript:alert(1)"});
        assert!(source_link(&detail).is_none());
        detail.properties = json!({"source": "someone, off-site"});
        assert!(source_link(&detail).is_none());
    }

    #[test]
    fn a_front_matter_error_is_reported_not_swallowed() {
        let mut detail = detail();
        detail.front_matter_error = Some("expected a list".into());
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);
        assert!(html.contains("Front matter error: expected a list"));
    }

    #[test]
    fn known_properties_render_as_a_definition_list_in_order() {
        let mut detail = detail();
        detail.properties = json!({
            "title": "A title",
            "tags": ["one", "two"],
            "rating": 4,
            "source": "https://example.com/pic/1",
            "author": "Example Artist",
            "license": "unknown",
            "created": "2021-10-15",
            "imported": "2026-10-04T14:30:12+01:00",
            "added": "2023-07-12T20:54:07+01:00",
            "id": "01JA8X3Q7K2M9V4T6R1B5N0C3Q",
            "height": 3429,
        });
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);

        // A definition list, not raw JSON.
        assert!(html.contains("<dl class=\"property-list\">"), "{html}");
        assert!(html.contains("<dt>Title</dt><dd>A title</dd>"), "{html}");
        assert!(html.contains("<dt>Rating</dt><dd>4</dd>"), "{html}");
        assert!(
            html.contains("<dt>Author</dt><dd>Example Artist</dd>"),
            "{html}"
        );
        assert!(html.contains("<dt>License</dt><dd>unknown</dd>"), "{html}");
        assert!(
            html.contains("<dt>ID</dt><dd><code>01JA8X3Q7K2M9V4T6R1B5N0C3Q</code></dd>"),
            "{html}"
        );
        // An unknown property follows the known ones.
        assert!(html.contains("<dt>height</dt><dd>3429</dd>"), "{html}");
        // Tags are links to the tagged view.
        assert!(
            html.contains("<dt>Tags</dt><dd><a class=\"inspector-tag\" href=\"/?tag=one\">one</a>"),
            "{html}"
        );
        // Source is a link.
        assert!(html.contains(
            "<dt>Source</dt><dd><a class=\"source-link\" href=\"https://example.com/pic/1\" rel=\"noreferrer noopener\">https://example.com/pic/1</a></dd>"
        ), "{html}");
        // The known properties come before the unknown one.
        let title_at = html.find("<dt>Title</dt>").unwrap();
        let height_at = html.find("<dt>height</dt>").unwrap();
        assert!(title_at < height_at, "{html}");
    }

    #[test]
    fn dates_are_human_readable_with_the_original_in_the_title() {
        let mut detail = detail();
        detail.properties = json!({
            "created": "2021-10-15",
            "imported": "2026-10-04T14:30:12+01:00",
        });
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);
        assert!(
            html.contains("<dt>Created</dt><dd><span title=\"2021-10-15\">15 Oct 2021</span></dd>"),
            "{html}"
        );
        assert!(
            html.contains("<dt>Imported</dt><dd><span title=\"2026-10-04T14:30:12+01:00\">04 Oct 2026, 14:30 (+01:00)</span></dd>"),
            "{html}"
        );
    }

    #[test]
    fn a_date_that_is_not_iso_8601_is_shown_as_written() {
        let mut detail = detail();
        detail.properties = json!({ "created": "sometime last year" });
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);
        assert!(
            html.contains("<span title=\"sometime last year\">sometime last year</span>"),
            "{html}"
        );
    }

    #[test]
    fn the_raw_front_matter_stays_in_a_collapsed_details() {
        let mut detail = detail();
        detail.properties = json!({"title": "猫", "height": 3429});
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);
        assert!(
            html.contains("<details class=\"properties-raw\"><summary>Raw properties</summary>"),
            "{html}"
        );
        assert!(
            html.contains("<pre class=\"properties\">{\n  &quot;height&quot;: 3429,\n  &quot;title&quot;: &quot;猫&quot;\n}</pre>"),
            "{html}"
        );
        // The raw JSON is escaped, never live markup.
        assert!(
            !html.contains("<pre class=\"properties\">{\n  \"height\""),
            "{html}"
        );
    }

    #[test]
    fn properties_without_a_value_are_not_listed() {
        let mut detail = detail();
        detail.properties = json!({
            "title": "Shown",
            "tags": [],
            "height": null,
        });
        let (context, _) = plain();
        let html = image_body(&detail, Some(&[]), &context, None, None);
        assert!(html.contains("<dt>Title</dt>"), "{html}");
        assert!(!html.contains("<dt>Tags</dt>"), "{html}");
        assert!(!html.contains("<dt>height</dt>"), "{html}");
    }

    /// The stage takes the shape the note records, so a page the index has no
    /// dimensions for is still right on its first paint (W34 #2).
    #[test]
    fn the_stage_takes_the_shape_the_note_records() {
        let mut detail = detail();
        let (context, _) = plain();
        // A phone screenshot, and a panorama.
        detail.properties = json!({"width": 780, "height": 48000});
        assert!(
            image_body(&detail, Some(&[]), &context, None, None)
                .contains("class=\"image-stage\" data-fit=\"tall\""),
            "a tall picture fills the column and scrolls"
        );
        detail.properties = json!({"width": 2400, "height": 800});
        assert!(image_body(&detail, Some(&[]), &context, None, None)
            .contains("class=\"image-stage\" data-fit=\"wide\""));
        // An ordinary picture, including one an importer wrote as text.
        detail.properties = json!({"width": "1200", "height": "800"});
        assert!(image_body(&detail, Some(&[]), &context, None, None)
            .contains("class=\"image-stage\" data-fit=\"normal\""));
    }

    /// A shape nobody recorded is not invented: the stage keeps the whole
    /// picture and the script measures it if it can (invariant 4).
    #[test]
    fn a_shape_the_note_does_not_record_is_not_guessed() {
        for properties in [
            json!({}),
            json!({"width": 1200}),
            json!({"height": 800}),
            json!({"width": 0, "height": 800}),
            json!({"width": null, "height": 800}),
            json!({"width": "wide", "height": 800}),
            json!({"width": [1, 2], "height": 800}),
        ] {
            assert_eq!(known_fit(&properties), None, "{properties}");
        }
        let mut bare = detail();
        bare.properties = json!({});
        let (context, _) = plain();
        let html = image_body(&bare, Some(&[]), &context, None, None);
        assert!(html.contains("<figure class=\"image-stage\">"), "{html}");
        assert!(!html.contains("data-fit"), "{html}");
    }

    /// The thresholds are DESIGN.md §5.4's, and both boundaries are exclusive:
    /// an aspect ratio of exactly 0.4 or exactly 2.5 is an ordinary picture.
    #[test]
    fn the_crop_thresholds_are_the_design_system_ones() {
        assert_eq!(
            known_fit(&json!({"width": 4, "height": 10})),
            Some("normal")
        );
        assert_eq!(
            known_fit(&json!({"width": 4, "height": 10.001})),
            Some("tall")
        );
        assert_eq!(
            known_fit(&json!({"width": 25, "height": 10})),
            Some("normal")
        );
        assert_eq!(
            known_fit(&json!({"width": 25.001, "height": 10})),
            Some("wide")
        );
    }

    /// W49: the page shows what the image's own header says — its dimensions
    /// and the EXIF taken time — and says "Unknown" where the index recorded
    /// none, rather than leaving a gap a reader would read as a value
    /// (DESIGN.md §4.6, invariant 4).
    #[test]
    fn the_page_shows_the_taken_time_and_the_dimensions_when_it_knows_them() {
        let (context, _) = plain();
        let html = image_body(
            &detail(),
            Some(&[]),
            &context,
            None,
            Some(ImageMeta {
                width: Some(3024),
                height: Some(4032),
                taken_ns: Some(1_689_191_647_123_000_000),
                taken_reason: None,
            }),
        );
        assert!(
            html.contains("<dt>Dimensions</dt><dd>3024 × 4032</dd>"),
            "{html}"
        );
        // The instant as stored (UTC, milliseconds) and the words beside it.
        assert!(
            html.contains(
                "<dt>Taken</dt><dd><time datetime=\"2023-07-12T19:54:07.123Z\">\
                 12 Jul 2023, 19:54:07 UTC</time></dd>"
            ),
            "{html}"
        );
        assert!(html.contains("<h2>Image</h2>"), "{html}");
    }

    /// An unknown is a recorded fact and stays one all the way to the page: a
    /// screenshot has no EXIF date, and the page says so rather than showing a
    /// date nobody recorded or leaving an empty cell.
    #[test]
    fn an_unknown_header_fact_reads_as_unknown_never_as_a_value() {
        let (context, _) = plain();
        let html = image_body(
            &detail(),
            Some(&[]),
            &context,
            None,
            Some(ImageMeta {
                width: Some(1200),
                height: None,
                taken_ns: None,
                taken_reason: None,
            }),
        );
        assert_eq!(
            html.matches("<dd>Unknown</dd>").count(),
            2,
            "half a shape is no shape, and no date is no date: {html}"
        );
        assert!(!html.contains("1200 ×"), "{html}");
        assert!(
            !html.contains("1970") && !html.contains("1 Jan"),
            "an epoch is not a taken time: {html}"
        );

        // A header the refresh could not read at all: both facts unknown.
        let unreadable = image_body(
            &detail(),
            Some(&[]),
            &context,
            None,
            Some(ImageMeta {
                width: None,
                height: None,
                taken_ns: None,
                taken_reason: None,
            }),
        );
        assert!(unreadable.contains("<h2>Image</h2>"), "{unreadable}");
        assert_eq!(unreadable.matches("<dd>Unknown</dd>").count(), 2);

        // An image the index holds no row for says nothing about its header.
        let unindexed = image_body(&detail(), Some(&[]), &context, None, None);
        assert!(!unindexed.contains("<h2>Image</h2>"), "{unindexed}");
        assert!(!unindexed.contains("<dt>Taken</dt>"), "{unindexed}");
    }

    /// RW49 L-2: the index records five kinds of "no taken time" and the page
    /// is where they are either kept or dropped. A screenshot with no EXIF and
    /// a file that is not an image at all are both a missing instant, but they
    /// are not the same fact, and one word for both is invariant 4 failing at
    /// the last step. A row that recorded no reason is not given one either.
    #[test]
    fn the_page_names_which_kind_of_missing_taken_time_it_shows() {
        let facts = |reason: Option<TakenReason>| {
            image_facts_section(Some(ImageMeta {
                width: Some(1),
                height: Some(1),
                taken_ns: None,
                taken_reason: reason,
            }))
        };
        for (reason, words) in [
            (Some(TakenReason::NoExif), "the file carries no EXIF"),
            (Some(TakenReason::ExifWithoutDate), "its EXIF names no date"),
            (
                Some(TakenReason::UnreadableExif),
                "its EXIF could not be read",
            ),
            (
                Some(TakenReason::UnreadableDate),
                "the date it carries is unreadable",
            ),
            (
                Some(TakenReason::UnreadableFile),
                "the file is not a readable image",
            ),
        ] {
            let html = facts(reason);
            assert!(
                html.contains(&format!("<dt>Taken</dt><dd>Unknown — {words}</dd>")),
                "{reason:?}: {html}"
            );
            assert!(
                !html.contains("<time"),
                "an unknown instant is never dressed as a date: {html}"
            );
        }

        // Nothing recorded about why: the bare unknown, with no cause invented.
        let unrecorded = facts(None);
        assert!(
            unrecorded.contains("<dt>Taken</dt><dd>Unknown</dd>"),
            "{unrecorded}"
        );

        // The dimensions row is unchanged by any of this.
        assert!(
            facts(Some(TakenReason::NoExif)).contains("<dt>Dimensions</dt><dd>1 × 1</dd>"),
            "the pair the index does know is still stated: {}",
            facts(Some(TakenReason::NoExif))
        );
    }

    /// RW49 L-5: an import that writes `width`/`height` into the note records
    /// the picture's shape a second time, and the page said the same two
    /// numbers twice with nothing to say they were the same pair. Now the
    /// Properties list keeps silent about a pair the Image section has just
    /// printed — and keeps both rows when the two disagree, because two
    /// readings of one picture are two facts (invariant 4). The note's own
    /// numbers are never hidden: the raw front matter still carries them.
    #[test]
    fn the_note_does_not_repeat_the_dimensions_the_image_section_just_stated() {
        let mut note = detail();
        note.properties = json!({"title": "Copy", "width": 3024, "height": 4032});
        let page = |meta: ImageMeta| image_body(&note, Some(&[]), &plain().0, None, Some(meta));
        let indexed = |width: u32, height: u32| ImageMeta {
            width: Some(width),
            height: Some(height),
            taken_ns: None,
            taken_reason: None,
        };

        let same = page(indexed(3024, 4032));
        assert!(
            same.contains("<dt>Dimensions</dt><dd>3024 × 4032</dd>"),
            "{same}"
        );
        assert!(
            !same.contains("<dt>width</dt>") && !same.contains("<dt>height</dt>"),
            "one fact, stated once: {same}"
        );
        assert!(
            same.contains("&quot;width&quot;: 3024"),
            "the note's own copy stays reachable in the raw front matter: {same}"
        );

        // The note carries the shape before EXIF orientation, the index the one
        // after: both readings are news, and both are shown.
        let rotated = page(indexed(4032, 3024));
        assert!(
            rotated.contains("<dt>Dimensions</dt><dd>4032 × 3024</dd>"),
            "{rotated}"
        );
        assert!(
            rotated.contains("<dt>width</dt>") && rotated.contains("<dt>height</dt>"),
            "a pair that disagrees with the index is a second fact: {rotated}"
        );

        // Half an agreement is decided with the pair (RW53 review Low-1): the
        // note's width matches the index and its height does not, and dropping
        // only the agreeing row would leave a lone `height` — read as a note
        // that carried a height and no width at all, beside the pair two lines
        // up that the disagreement is with.
        let mut uneven = detail();
        uneven.properties = json!({"title": "Copy", "width": 3024, "height": 4033});
        let uneven = image_body(
            &uneven,
            Some(&[]),
            &plain().0,
            None,
            Some(indexed(3024, 4032)),
        );
        assert!(
            uneven.contains("<dt>width</dt>") && uneven.contains("<dt>height</dt>"),
            "one number of a pair agreeing keeps the pair whole: {uneven}"
        );

        // No indexed pair means nothing was stated above, so the note's own
        // numbers are the only ones the reader gets.
        let no_pair = page(ImageMeta {
            width: None,
            height: Some(4032),
            taken_ns: None,
            taken_reason: None,
        });
        assert!(
            no_pair.contains("<dt>width</dt>") && no_pair.contains("<dt>height</dt>"),
            "the note is the only source here: {no_pair}"
        );
    }

    /// The stored instant is an instant, so it is shown in UTC — the camera's
    /// own offset is not stored beside it, and a shift nobody recorded is not
    /// applied to a recorded time. Seconds appear only when EXIF refined below
    /// the minute.
    #[test]
    fn a_taken_time_reads_in_utc_and_drops_a_second_nobody_wrote() {
        let whole = taken_value(1_689_191_647_000_000_000).unwrap();
        assert!(whole.contains("12 Jul 2023, 19:54 UTC"), "{whole}");
        assert!(
            whole.contains(">12 Jul 2023, 19:54 UTC</time>"),
            "the visible words carry no second EXIF never wrote: {whole}"
        );
        assert!(
            whole.contains("datetime=\"2023-07-12T19:54:07.000Z\""),
            "{whole}"
        );

        let refined = taken_value(1_689_191_647_123_000_000).unwrap();
        assert!(refined.contains("19:54:07 UTC"), "{refined}");

        // Before the epoch and far past it are still instants, not errors.
        assert!(taken_value(-1_000_000_000).unwrap().contains("1969"));
        assert!(taken_value(4_102_444_800_000_000_000)
            .unwrap()
            .contains("2100"));
    }

    /// The indexed dimensions are the better source for the stage's shape —
    /// they are the file's own, after EXIF orientation — and the note's
    /// recorded ones are the fallback.
    #[test]
    fn the_stage_takes_the_indexed_shape_first() {
        let mut detail = detail();
        let (context, _) = plain();
        // The note claims an ordinary shape; the file is a panorama.
        detail.properties = json!({"width": 1000, "height": 1000});
        let html = image_body(
            &detail,
            Some(&[]),
            &context,
            None,
            Some(ImageMeta {
                width: Some(1600),
                height: Some(400),
                taken_ns: None,
                taken_reason: None,
            }),
        );
        assert!(
            html.contains("class=\"image-stage\" data-fit=\"wide\""),
            "the file's own dimensions win: {html}"
        );

        // Half a shape from the index falls back to what the note records.
        let html = image_body(
            &detail,
            Some(&[]),
            &context,
            None,
            Some(ImageMeta {
                width: None,
                height: Some(400),
                taken_ns: None,
                taken_reason: None,
            }),
        );
        assert!(html.contains("data-fit=\"normal\""), "{html}");
    }

    /// An unreadable `v` falls back to the plain library for the walk too: the
    /// neighbours and the back link never disagree about which view they mean.
    #[test]
    fn an_unreadable_v_is_the_plain_library_for_the_walk_as_well() {
        let context = view_context(Some("v=%FF"));
        assert_eq!(context.query, "");
        assert_eq!(context.params, ViewParams::default());
        assert_eq!(context.lens_limit(), None);
        let recent = view_context(Some("v=recent%3D1"));
        assert!(recent.params.recent);
        assert_eq!(recent.lens_limit(), Some(RECENT_LIMIT));
    }
}
