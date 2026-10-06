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

/// How many tags `/search` lists before it defers to the sidebar.
const TAG_LIMIT: usize = 200;

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
    "<section class=\"search-section\"><form class=\"search-form\" method=\"get\" action=\"/\">\
     <label class=\"search-label\" for=\"q\">Search notes, titles and tags</label>\
     <div class=\"search-pill-container\"><input id=\"q\" name=\"q\" type=\"search\" \
     autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\" \
     placeholder=\"Search this library\"><button class=\"search-submit\" type=\"submit\">Search</button>\
     </div><p class=\"search-hint\">Three letters or more searches note text; shorter words match names and tags.</p>\
     </form></section>"
        .to_owned()
}

/// Every tag with its count, most used first, each linking to the library view.
fn tag_section(sidebar: &SidebarData) -> String {
    if sidebar.tags.is_empty() {
        return "<section class=\"search-section\"><h2>Tags</h2>\
                <p class=\"search-hint\">No tags yet.</p></section>"
            .to_owned();
    }
    let mut tags: Vec<_> = sidebar.tags.iter().collect();
    tags.sort_by(|left, right| right.count.cmp(&left.count).then(left.tag.cmp(&right.tag)));
    let listed = tags.len().min(TAG_LIMIT);
    let mut out =
        String::from("<section class=\"search-section\"><h2>Tags</h2><div class=\"tag-list\">");
    for tag in tags.iter().take(TAG_LIMIT) {
        out.push_str(&components::tag_row(&tag.tag, tag.count));
    }
    out.push_str("</div>");
    if tags.len() > TAG_LIMIT {
        out.push_str(&format!(
            "<p class=\"search-hint\">{} more tags are not listed here.</p>",
            tags.len() - listed
        ));
    }
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
}
