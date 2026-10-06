//! The image page: the picture, its note, and the facts that travel with it.
//!
//! Spec §2 adds three things to what was here before: "Back to view", which
//! returns to the grid the image was reached from; "Appears in", the collections
//! that embed it; and the path with a copy button.

use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use percent_encoding::percent_decode_str;
use serde_json::Value;
use std::collections::HashSet;

use crate::catalog::ImageDetail;
use crate::ui::shell::Frame;
use crate::ui::{encode_path, escape_html, markdown_html};
use crate::view_query::query_value;
use crate::{catalog_error_page, error_page, error_response, AppState};

/// The destination of "Back to view".
const DESTINATION: crate::ui::shell::Destination = crate::ui::shell::Destination::Library;

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
    let appears_in = state
        .index
        .appears_in(&detail.path)
        .unwrap_or_else(|_| Vec::new());
    let body = image_body(&detail, &appears_in, back_target(query.as_deref()));
    let frame = Frame {
        banner: state.banner(),
        ..Frame::new(&detail.path, DESTINATION, body)
    };
    Html(frame.render()).into_response()
}

/// Where "Back to view" points: the view the page arrived from.
///
/// `v` is the view's own query string, carried whole rather than re-parsed, so
/// a view this server has not seen before still comes back intact. A `v` that
/// is not a query string falls back to the plain library rather than becoming a
/// link to something unchecked.
fn back_target(query: Option<&str>) -> String {
    let Some(query) = query else {
        return "/".to_owned();
    };
    let Some(value) = query.split('&').find_map(|pair| pair.strip_prefix("v=")) else {
        return "/".to_owned();
    };
    match percent_decode_str(value).decode_utf8() {
        // The query string. A tile writes it without a leading `?`, and a
        // hand-written link may include one, so both spellings land in the same
        // place.
        Ok(view) => {
            let query = view.strip_prefix('?').unwrap_or(view.as_ref());
            if !query.is_empty() && query.chars().all(is_query_character) {
                format!("/?{}", escape_html(query))
            } else {
                "/".to_owned()
            }
        }
        Err(_) => "/".to_owned(),
    }
}

/// The characters a view query may be built from: what
/// [`ViewParams::to_query_string`] writes, percent-encoding included. Anything
/// else in `v` did not come from this server, so it is not used as a link.
fn is_query_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "%&=+_-.~/?:@!$'()*,;".contains(character)
}

fn image_body(detail: &ImageDetail, appears_in: &[String], back: String) -> String {
    let path = encode_path(&detail.path);
    let mut out = String::from("<div class=\"image-page\">");
    out.push_str("<header class=\"image-header\"><div class=\"image-header-nav\">");
    out.push_str(&format!(
        "<a class=\"back-link\" href=\"{}\">← Back to view</a>",
        back
    ));
    out.push_str(&format!(
        "<a class=\"original-link\" href=\"/raw/{path}\">View original</a></div></header>"
    ));

    out.push_str(&format!(
        "<div class=\"image-detail-layout\"><figure class=\"image-stage\">\
         <img src=\"/media/{path}\" alt=\"{label}\"></figure>",
        label = escape_html(&title_of(detail))
    ));
    out.push_str("<div class=\"image-panel\">");
    out.push_str(&format!(
        "<h1 class=\"image-title\">{}</h1>",
        escape_html(&title_of(detail))
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

/// "Appears in": every collection note that embeds this image.
fn appears_in_html(appears_in: &[String]) -> String {
    if appears_in.is_empty() {
        return String::new();
    }
    let items: String = appears_in
        .iter()
        .map(|collection| {
            format!(
                "<li><a href=\"/?c={}\">{}</a></li>",
                escape_html(&query_value(collection)),
                escape_html(collection)
            )
        })
        .collect();
    format!(
        "<section class=\"image-section\"><h2>Appears in</h2><ul class=\"appears-in\">{items}</ul></section>"
    )
}

/// Note `title`, else the file name (spec §4: alt text).
fn title_of(detail: &ImageDetail) -> String {
    detail
        .properties
        .get("title")
        .and_then(|title| title.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            detail
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&detail.path)
                .to_owned()
        })
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

    #[test]
    fn the_page_offers_a_way_back_to_the_view_it_came_from() {
        // A tile carries the view query without its `?`.
        let html = image_body(&detail(), &[], back_target(Some("v=tag%3Deagle")));
        assert!(html.contains("href=\"/?tag=eagle\">← Back to view"));
        // A hand-written link may include it.
        let with_mark = back_target(Some("v=%3Ftag%3Deagle"));
        assert_eq!(with_mark, "/?tag=eagle");
        let plain = image_body(&detail(), &[], back_target(None));
        assert!(plain.contains("href=\"/\">← Back to view"));
    }

    #[test]
    fn a_v_that_is_not_a_query_string_falls_back_to_the_library() {
        assert_eq!(back_target(Some("v=")), "/");
        assert_eq!(back_target(Some("v=%3F")), "/");
        assert_eq!(back_target(Some("other=1")), "/");
        assert_eq!(back_target(Some("v=%FF")), "/", "not UTF-8");
        // A `v` carrying markup is not used as a link.
        assert_eq!(back_target(Some("v=%3Cscript%3E")), "/");
    }

    /// "Back to view" can only ever be a link into this viewer: whatever `v`
    /// holds, the result stays a path on this origin.
    #[test]
    fn back_to_view_never_leaves_the_origin() {
        for value in [
            "v=https%3A%2F%2Fevil.example%2F",
            "v=%2F%2Fevil.example",
            "v=in%3D..%2F..",
            "v=%22%3E%3Cscript%3E",
        ] {
            let target = back_target(Some(value));
            assert!(
                target.starts_with('/') && !target.starts_with("//"),
                "{value} produced {target}"
            );
            assert!(!target.contains("<"), "{value} produced {target}");
        }
    }

    #[test]
    fn the_path_is_shown_and_copyable_and_the_note_is_sanitised() {
        let html = image_body(&detail(), &[], "/".into());
        assert!(html.contains("data-copy-text=\"refs/猫.png\""));
        assert!(html.contains("A <strong>quiet</strong> cat"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn provenance_travels_with_the_picture() {
        let html = image_body(
            &detail(),
            &["browse.md".into(), "sets/eagle.md".into()],
            "/".into(),
        );
        assert!(html.contains("href=\"https://example.com/pic/1\""));
        assert!(html.contains("rel=\"noreferrer noopener\""));
        assert!(html.contains("href=\"/?tag=eagle\""));
        assert!(html.contains("href=\"/?c=browse.md\""));
        assert!(html.contains("href=\"/?c=sets/eagle.md\""));
        assert!(html.contains("猫.png.eagle.json"));
    }

    #[test]
    fn an_image_with_no_note_shows_no_tags_and_no_appears_in() {
        let bare = ImageDetail {
            path: "plain.png".into(),
            properties: json!({}),
            front_matter_error: None,
            body: String::new(),
            body_html: None,
            raw_files: Vec::new(),
            note_path: None,
        };
        let html = image_body(&bare, &[], "/".into());
        assert!(!html.contains("Appears in"));
        assert!(!html.contains("Tags"));
        assert!(html.contains("alt=\"plain.png\""));
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
        let html = image_body(&detail, &[], "/".into());
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
        let html = image_body(&detail, &[], "/".into());

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
        let html = image_body(&detail, &[], "/".into());
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
        let html = image_body(&detail, &[], "/".into());
        assert!(
            html.contains("<span title=\"sometime last year\">sometime last year</span>"),
            "{html}"
        );
    }

    #[test]
    fn the_raw_front_matter_stays_in_a_collapsed_details() {
        let mut detail = detail();
        detail.properties = json!({"title": "猫", "height": 3429});
        let html = image_body(&detail, &[], "/".into());
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
        let html = image_body(&detail, &[], "/".into());
        assert!(html.contains("<dt>Title</dt>"), "{html}");
        assert!(!html.contains("<dt>Tags</dt>"), "{html}");
        assert!(!html.contains("<dt>height</dt>"), "{html}");
    }
}
