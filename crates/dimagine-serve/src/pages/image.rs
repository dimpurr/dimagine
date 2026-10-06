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
use percent_encoding::percent_decode_str;

use crate::catalog::ImageDetail;
use crate::ui::shell::Frame;
use crate::ui::{encode_path, escape_html, markdown_html};
use crate::view_query::query_value;
use crate::{error_response, AppState};

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
        Ok(Err(error)) => return error_response(error),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
        out.push_str(&format!(
            "<section class=\"image-section\"><h2>Note</h2><p class=\"note-path\">{}</p>\
             <article class=\"note-body\">{}</article></section>",
            escape_html(note),
            markdown_html(&detail.body)
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

/// The whole front matter, so every property an import recorded stays visible
/// even when no field above claims it.
fn properties_section(detail: &ImageDetail) -> String {
    if detail.properties.is_null() {
        return String::new();
    }
    format!(
        "<section class=\"image-section\"><h2>Properties</h2><pre class=\"properties\">{}</pre></section>",
        escape_html(&crate::catalog::yaml_json_string(&detail.properties))
    )
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
}
