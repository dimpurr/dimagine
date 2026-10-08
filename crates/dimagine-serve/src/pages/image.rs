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
use dimagine_index::{AppearsIn, Neighbours};
use percent_encoding::{percent_decode_str, utf8_percent_encode};
use serde_json::Value;
use std::collections::HashSet;

use crate::catalog::ImageDetail;
use crate::index_sync::RECENT_LIMIT;
use crate::ui::shell::Frame;
use crate::ui::{encode_path, escape_html, markdown_html, tile_anchor};
use crate::view_query::{query_value, ViewParams};
use crate::{catalog_error_page, error_page, error_response, AppState};

/// The destination of "Back to view".
const DESTINATION: crate::ui::shell::Destination = crate::ui::shell::Destination::Library;

/// The aspect-ratio thresholds that crop a tile or a stage (DESIGN.md §5.4),
/// the same pair the stylesheet's `data-fit` rules use.
const TALL_BELOW: f64 = 0.4;
const WIDE_ABOVE: f64 = 2.5;

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
    let neighbours = state
        .index
        .view_neighbours(
            &context.params.to_index_query(),
            &detail.path,
            context.lens_limit(),
        )
        .unwrap_or(None);
    let appears_in = state
        .index
        .appears_in_titled(&detail.path)
        .unwrap_or_else(|_| Vec::new());
    let body = image_body(&detail, &appears_in, &context, neighbours.as_ref());
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
    fn back_href(&self, image: &str) -> String {
        let anchor = tile_anchor(image);
        if self.query.is_empty() {
            format!("/#{anchor}")
        } else {
            // The query is escaped whole: it arrived validated as a query
            // string, and its `&` characters stay readable links.
            format!("/?{}#{anchor}", escape_html(&self.query))
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
    appears_in: &[AppearsIn],
    context: &ViewContext,
    neighbours: Option<&Neighbours>,
) -> String {
    let path = encode_path(&detail.path);
    let title = title_of(detail);
    let mut out = String::from("<div class=\"image-page\">");
    // The header: the way back, the walk through the view, and the original
    // file — one line per region, in the order a person reads them.
    out.push_str("<header class=\"image-header\">");
    out.push_str(&format!(
        "<a class=\"back-link\" href=\"{}\">← Back to view</a>",
        context.back_href(&detail.path)
    ));
    out.push_str(&nav_html(context, neighbours));
    out.push_str(&format!(
        "<a class=\"original-link\" href=\"/raw/{path}\">View original</a>"
    ));
    out.push_str("</header>");

    // The stage takes the picture's shape (W34 #2): a tall screenshot fills the
    // column and scrolls inside the well instead of painting a sliver in a big
    // empty box. When the note records the dimensions the fit is known here and
    // the page is right on the first paint; otherwise the script measures the
    // loaded image, and a shape nobody knows keeps the whole picture.
    out.push_str(&format!(
        "<div class=\"image-detail-layout\"><figure class=\"image-stage\"{}>\
         <img src=\"/media/{path}\" alt=\"{}\"></figure>",
        fit_attribute(known_fit(&detail.properties)),
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
    out.push_str(&properties_section(detail));
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
    let ratio = width / height;
    Some(if ratio < TALL_BELOW {
        "tall"
    } else if ratio > WIDE_ABOVE {
        "wide"
    } else {
        "normal"
    })
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

/// The properties as a definition list: the known FORMAT
/// §3.1 properties first, in show order, then any property
/// an import added, by name. The whole front matter stays
/// reachable, collapsed, as the raw JSON.
fn properties_section(detail: &ImageDetail) -> String {
    if detail.properties.is_null() {
        return String::new();
    }
    let mut out = String::from("<section class=\"image-section\"><h2>Properties</h2>");
    let rows = property_rows(&detail.properties);
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
fn property_rows(properties: &Value) -> Vec<(String, String)> {
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
        .filter(|key| !shown.contains(key.as_str()))
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
fn appears_in_html(appears_in: &[AppearsIn]) -> String {
    let mut out = String::from("<section class=\"image-section\"><h2>Appears in</h2>");
    if appears_in.is_empty() {
        out.push_str("<p class=\"appears-in-empty\">Not in any collection.</p>");
    } else {
        out.push_str("<ul class=\"appears-in\">");
        for collection in appears_in {
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
        let html = image_body(&detail(), &[], &context, None);
        assert!(
            html.contains(&format!("href=\"/?tag=eagle#{}\"", tile_anchor(image))),
            "{html}"
        );
        // A hand-written link may include it.
        let context = view_context(Some("v=%3Ftag%3Deagle"));
        assert_eq!(
            context.back_href(image),
            format!("/?tag=eagle#{}", tile_anchor(image))
        );
        let plain = view_context(None);
        assert_eq!(plain.back_href(image), format!("/#{}", tile_anchor(image)));
        let html = image_body(&detail(), &[], &plain, None);
        assert!(
            html.contains(&format!("href=\"/#{}\">← Back to view", tile_anchor(image))),
            "{html}"
        );
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
            assert_eq!(view_context(query).back_href(image), plain, "{query:?}");
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
            let target = context.back_href(image);
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
        let html = image_body(&detail(), &[], &context, Some(&neighbours_at(3, 124)));
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
        let html = image_body(&detail(), &[], &plain, Some(&neighbours_at(1, 4)));
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
        let html = image_body(&detail(), &[], &context, Some(&neighbours));
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
        let html = image_body(&detail(), &[], &context, None);
        assert!(!html.contains("image-nav"), "{html}");
        assert!(html.contains("Back to view"), "{html}");
    }

    #[test]
    fn the_path_is_shown_and_copyable_and_the_note_is_sanitised() {
        let (context, _) = plain();
        let html = image_body(&detail(), &[], &context, None);
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
            &[
                AppearsIn {
                    note_path: "browse.md".into(),
                    title: "Browse".into(),
                },
                AppearsIn {
                    note_path: "sets/eagle.md".into(),
                    title: String::new(),
                },
            ],
            &context,
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

    /// A collection's title is library content: whatever it says, it reaches
    /// the page escaped, and the link stays a link to this origin.
    #[test]
    fn a_hostile_collection_title_is_named_escaped() {
        let (context, _) = plain();
        let appears_in = vec![AppearsIn {
            note_path: "sets/x.md".into(),
            title: "\"><img onerror=alert(1)><svg><script>".into(),
        }];
        let html = image_body(&detail(), &appears_in, &context, None);
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
        let html = image_body(&detail(), &[], &context, None);
        assert!(html.contains("<h2>Appears in</h2>"), "{html}");
        assert!(html.contains("Not in any collection."), "{html}");
    }

    #[test]
    fn the_sheet_handle_names_the_picture_and_controls_the_panel() {
        let (context, _) = plain();
        let html = image_body(&detail(), &[], &context, None);
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
        let html = image_body(&detail, &[], &context, None);
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
        let html = image_body(&detail, &[], &context, None);

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
        let html = image_body(&detail, &[], &context, None);
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
        let html = image_body(&detail, &[], &context, None);
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
        let html = image_body(&detail, &[], &context, None);
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
        let html = image_body(&detail, &[], &context, None);
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
            image_body(&detail, &[], &context, None)
                .contains("class=\"image-stage\" data-fit=\"tall\""),
            "a tall picture fills the column and scrolls"
        );
        detail.properties = json!({"width": 2400, "height": 800});
        assert!(image_body(&detail, &[], &context, None)
            .contains("class=\"image-stage\" data-fit=\"wide\""));
        // An ordinary picture, including one an importer wrote as text.
        detail.properties = json!({"width": "1200", "height": "800"});
        assert!(image_body(&detail, &[], &context, None)
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
        let html = image_body(&bare, &[], &context, None);
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
