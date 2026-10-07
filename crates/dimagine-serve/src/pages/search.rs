//! `/search`: a real text field, the tags with their counts, and the recent
//! views the client script remembers (spec §2, Phase 1).
//!
//! Submitting the field goes to `/` with `q`, because the result of a search is
//! the library grid — the same grid, one query string different.

use axum::{
    extract::State,
    response::{Html, IntoResponse, Response},
};

use crate::index_sync::SidebarData;
use crate::ui::components;
use crate::ui::shell::{Destination, Frame};
use crate::{acquire_admission, admission_denied, index_failure, AppState};

/// How many tags the "Top" group shows, as the tile settles (§4.7).
const TOP_TAGS: usize = 30;

/// The count of the low-value tail: used once, and not a useful headline.
/// These tags never enter the "Top" group; they live in the A–Z list, where
/// a count of one sits where its name belongs instead of at the top.
const LOW_VALUE_COUNT: u64 = 1;

/// `GET /search` — the field, the tags, and the remembered views.
pub(crate) async fn search_page(State(state): State<AppState>) -> Response {
    let _permit = match acquire_admission(&state) {
        Some(permit) => permit,
        None => return admission_denied(),
    };
    let Ok(sidebar) = state.index.sidebar_data() else {
        return index_failure(&state, false);
    };
    let frame = Frame {
        banner: state.banner(),
        sidebar: Some(&sidebar),
        ..Frame::new("Search", Destination::Search, search_body(&sidebar))
    };
    Html(frame.render()).into_response()
}

fn search_body(sidebar: &SidebarData) -> String {
    let mut out = search_form();
    out.push_str(&tag_section(sidebar));
    out.push_str(
        "<section class=\"search-section\"><h2>Recent views</h2>\
         <ul class=\"recent-views-list\" data-recent-views></ul>\
         <p class=\"search-hint\">Searches and tag views you open are listed here, on this device only.</p>\
         </section>",
    );
    out
}

/// The field submits to the library, so a result is a bookmarkable URL.
fn search_form() -> String {
    format!(
        "<section class=\"search-section\"><form class=\"search-form\" method=\"get\" action=\"/\">\
         <label class=\"search-label\" for=\"q\">Search notes, titles and tags</label>\
         <div class=\"search-pill-container\">{glyph}<input id=\"q\" name=\"q\" type=\"search\" \
         autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\" \
         placeholder=\"Search this library\"><button class=\"search-submit\" type=\"submit\">Search</button>\
         </div><p class=\"search-hint\">Three letters or more searches note text; shorter words match names and tags.</p>\
         </form></section>",
        glyph = components::SEARCH_GLYPH,
    )
}

/// The tags as the tile draws them (§4.7): a filter box, the top 30 by count
/// with A–Z as tie-break, and the full A–Z list with counts underneath. The
/// filter box is a client-side narrowing of the two lists; with scripting off
/// both lists are already on the page.
fn tag_section(sidebar: &SidebarData) -> String {
    if sidebar.tags.is_empty() {
        return "<section class=\"search-section\"><h2>Tags</h2>\
                <p class=\"search-hint\">No tags yet.</p></section>"
            .to_owned();
    }
    let mut by_count: Vec<_> = sidebar.tags.iter().collect();
    by_count.sort_by(|left, right| right.count.cmp(&left.count).then(left.tag.cmp(&right.tag)));
    let mut by_name = by_count.clone();
    by_name.sort_by(|a, b| a.tag.cmp(&b.tag));
    let top: Vec<_> = by_count
        .into_iter()
        .filter(|tag| tag.count > LOW_VALUE_COUNT)
        .take(TOP_TAGS)
        .collect();

    let mut out = String::from("<section class=\"search-section\"><h2>Tags</h2>");
    out.push_str(&format!(
        "<div class=\"search-pill-container tag-filter-field\">{glyph}\
         <input id=\"tag-filter\" type=\"search\" placeholder=\"Filter tags\" \
         aria-label=\"Filter tags by name\" autocomplete=\"off\" spellcheck=\"false\"></div>",
        glyph = components::SEARCH_GLYPH,
    ));
    out.push_str("<div class=\"tag-groups\">");
    if !top.is_empty() {
        out.push_str("<div class=\"tag-group\" data-tag-group=\"top\">");
        out.push_str("<h3 class=\"tag-group-title\">Top tags</h3><div class=\"tag-list\">");
        for tag in &top {
            out.push_str(&components::tag_row(&tag.tag, tag.count));
        }
        out.push_str("</div></div>");
    }
    out.push_str("<div class=\"tag-group\" data-tag-group=\"az\">");
    out.push_str("<h3 class=\"tag-group-title\">All tags A–Z</h3><div class=\"tag-list\">");
    for tag in &by_name {
        out.push_str(&components::tag_row(&tag.tag, tag.count));
    }
    out.push_str("</div></div></div>");
    out.push_str(&components::inline_state(
        "No tags match.",
        "Nothing in the library carries that spelling.",
        "Clear filter",
        "data-clear-tag-filter",
    ));
    out.push_str("</section>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_sync::SidebarTag;

    fn sidebar(tag_count: usize) -> SidebarData {
        SidebarData {
            folders: Vec::new(),
            collections: Vec::new(),
            tags: (0..tag_count)
                .map(|index| SidebarTag {
                    tag: format!("tag-{index:03}"),
                    count: 10 - index.min(9) as u64,
                })
                .collect(),
            total: 0,
            untagged: 0,
            recent: 0,
        }
    }

    #[test]
    fn the_field_searches_the_library_and_is_labelled() {
        let body = search_body(&sidebar(3));
        assert!(body.contains("action=\"/\""));
        assert!(body.contains("name=\"q\""));
        assert!(body.contains("for=\"q\""));
        assert!(body.contains("<label"));
    }

    #[test]
    fn tags_are_listed_by_count_and_link_to_the_library() {
        let body = search_body(&sidebar(3));
        assert!(body.contains("href=\"/?tag=tag-000\""));
        assert!(body.contains("sidebar-row-count\">10"));
        assert!(
            body.contains("data-recent-views"),
            "the script fills this in"
        );
    }

    #[test]
    fn a_library_without_tags_says_so() {
        assert!(search_body(&sidebar(0)).contains("No tags yet."));
    }

    /// W34 audit #7: the flat 200-row dump is gone; the page is a filter box,
    /// a Top group and a full A–Z list, and the count-1 tail never headlines.
    #[test]
    fn the_low_value_tail_is_not_pinned_to_the_top() {
        let body = search_body(&sidebar(40));
        let top_at = body.find("data-tag-group=\"top\"").expect("a top group");
        let az_at = body.find("data-tag-group=\"az\"").expect("an a-z group");
        let top_end = body.find("data-tag-group=\"az\"").expect("a-z group");
        let top_html = &body[top_at..top_end];
        // The 31 single-use tags of this library are absent from the Top
        // group and present in the A–Z list.
        assert!(!top_html.contains("tag-009"), "{top_html}");
        assert!(body[az_at..].contains("tag-009"));
        assert!(top_html.contains("tag-000"), "the 10-count tag headlines");
    }

    #[test]
    fn the_top_group_shows_at_most_thirty() {
        let data = SidebarData {
            folders: Vec::new(),
            collections: Vec::new(),
            tags: (0..100)
                .map(|index| SidebarTag {
                    tag: format!("tag-{index:03}"),
                    count: 200 - index as u64,
                })
                .collect(),
            total: 0,
            untagged: 0,
            recent: 0,
        };
        let body = search_body(&data);
        let top_end = body
            .find("data-tag-group=\"az\"")
            .expect("the a-z group follows the top group");
        let top_html = &body[..top_end];
        assert_eq!(top_html.matches("tag-row").count(), TOP_TAGS);
        assert!(!top_html.contains("tag-030"), "the 31st is not in Top");
        // The A–Z list is complete.
        assert_eq!(body[top_end..].matches("tag-row").count(), 100);
    }

    #[test]
    fn the_a_z_list_is_ordered_by_name_with_counts() {
        let body = search_body(&sidebar(40));
        let az_at = body.find("data-tag-group=\"az\"").expect("an a-z group");
        let az = &body[az_at..];
        let positions: Vec<usize> = (0..40)
            .map(|index| {
                az.find(&format!(">tag-{index:03}</span>"))
                    .unwrap_or_else(|| panic!("tag-{index:03} is in the A–Z list"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "ascending: {positions:?}"
        );
    }

    #[test]
    fn the_filter_box_and_its_no_match_state_are_on_the_page() {
        let body = search_body(&sidebar(3));
        assert!(body.contains("id=\"tag-filter\""));
        assert!(body.contains("aria-label=\"Filter tags by name\""));
        let state_at = body.find("inline-state").expect("an inline state");
        assert!(body[state_at..].contains("hidden"), "it starts hidden");
        assert!(body[state_at..].contains("No tags match."));
        assert!(body[state_at..].contains("data-clear-tag-filter"));
    }
}
