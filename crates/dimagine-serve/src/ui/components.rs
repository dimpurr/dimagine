//! UI components: tiles, chips, sort menu, breadcrumb, counts, sidebar.
//!
//! Everything here returns HTML fragments. No component reads the request or
//! the filesystem; the page handlers decide what data a component gets.

use dimagine_index::ViewItem;
use percent_encoding::utf8_percent_encode;

use crate::index_sync::SidebarData;
use crate::ui::escape_html;
use crate::ui::shell::{Destination, Frame, SORT_CHOICES};
use crate::view_query::{
    sort_key_as_str, Direction, SortKey, ThumbnailSize, ViewParams, PAGE_SIZE,
};

/// How many tags the sidebar lists before offering the rest on `/search`.
const SIDEBAR_TAG_LIMIT: usize = 30;

/// Encode a value for use inside a query string.
fn query_value(value: &str) -> String {
    utf8_percent_encode(value, crate::view_query::QUERY_VALUE).to_string()
}

/// The link back to the library that carries `params`.
fn view_url(params: &ViewParams) -> String {
    params.to_url("/")
}

/// The four destinations as links, for the rail and the phone tab bar.
///
/// The active one carries both `active` (which the stylesheet colours) and
/// `aria-current` (which a screen reader announces); either alone would leave
/// one of the two wrong.
pub fn navigation(active: Destination, rail: bool) -> String {
    let class = if rail { "rail-item" } else { "tab-item" };
    DESTINATIONS
        .iter()
        .map(|destination| {
            let (state, current) = if *destination == active {
                (" active", " aria-current=\"page\"")
            } else {
                ("", "")
            };
            format!(
                "<a class=\"{class}{state}\" href=\"{}\"{current}>{}{}</a>",
                destination.path(),
                destination.icon(),
                destination.label()
            )
        })
        .collect()
}

const DESTINATIONS: [Destination; 4] = [
    Destination::Library,
    Destination::Folders,
    Destination::Collections,
    Destination::Search,
];

/// The search pill. On the library it shows the active scope; tapping it goes
/// to `/search`, where there is a real field (spec §3).
pub fn search_pill(view: Option<&ViewParams>) -> String {
    let label = match view {
        Some(params) if !params.to_query_string().is_empty() => "Refine",
        _ => "Search",
    };
    format!(
        "<a class=\"search-pill-container\" href=\"/search\"><span class=\"search-label\">{label}</span></a>"
    )
}

/// `N items`, and how many are on screen when paging.
pub fn result_count(total: u64) -> String {
    let noun = if total == 1 { "item" } else { "items" };
    format!("<span class=\"result-count\">{total} {noun}</span>")
}

/// The sort menu, as a native `<select>` in a GET form so it works without JS.
///
/// Only the parameters that are not the sort travel as hidden fields, so
/// changing the sort keeps the scope and returns to page one.
pub fn sort_control(params: &ViewParams) -> String {
    let mut out = String::from("<form class=\"sort-form\" method=\"get\" action=\"/\">");
    for (name, value) in [
        ("in", &params.folder),
        ("q", &params.q),
        ("size", &Some(params.size.as_str().to_owned())),
    ] {
        if let Some(value) = value {
            out.push_str(&format!(
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                escape_html(&query_value(value))
            ));
        }
    }
    for tag in &params.tags {
        out.push_str(&format!(
            "<input type=\"hidden\" name=\"tag\" value=\"{}\">",
            escape_html(&query_value(tag))
        ));
    }
    if params.untagged {
        out.push_str("<input type=\"hidden\" name=\"untagged\" value=\"1\">");
    }
    if params.recent {
        out.push_str("<input type=\"hidden\" name=\"recent\" value=\"1\">");
    }
    if let Some(collection) = &params.collection {
        out.push_str(&format!(
            "<input type=\"hidden\" name=\"c\" value=\"{}\">",
            escape_html(&query_value(collection))
        ));
    }
    if !params.recursive {
        out.push_str("<input type=\"hidden\" name=\"sub\" value=\"0\">");
    }
    out.push_str("<select class=\"sort-select\" name=\"sort\" aria-label=\"Sort order\">");
    for (key, direction, label) in SORT_CHOICES {
        let selected = if params.sort == key && params.direction == direction {
            " selected"
        } else {
            ""
        };
        out.push_str(&format!(
            "<option value=\"{}-{dir}\"{selected}>{label}</option>",
            sort_key_as_str(key),
            dir = direction.suffix()
        ));
    }
    // Without JavaScript the control needs a button to submit the form.
    out.push_str("</select><button class=\"sort-submit\" type=\"submit\">Go</button></form>");
    out
}

/// The three-step thumbnail size control. `size` is a URL parameter as well as
/// a remembered preference, so each step is a real link.
pub fn size_toggles(params: &ViewParams) -> String {
    let mut out =
        String::from("<div class=\"size-toggles\" role=\"group\" aria-label=\"Thumbnail size\">");
    for size in [ThumbnailSize::S, ThumbnailSize::M, ThumbnailSize::L] {
        let active = if params.size == size { " active" } else { "" };
        let current = if params.size == size {
            " aria-current=\"true\""
        } else {
            ""
        };
        out.push_str(&format!(
            "<a class=\"size-toggle-btn{active}\" data-size=\"{}\"{current} href=\"{}\" \
             aria-label=\"Thumbnail size {}\">{}</a>",
            size.as_str(),
            escape_html(&view_url(&params.with_size(size))),
            size.as_str(),
            size.as_str()
        ));
    }
    out.push_str("</div>");
    out
}

/// One removable scope chip.
fn chip(label: &str, remove_url: &str) -> String {
    format!(
        "<span class=\"chip\">{label}<a class=\"chip-remove\" href=\"{url}\" \
         aria-label=\"{remove}\">✕</a></span>",
        url = escape_html(remove_url),
        remove = escape_html(&format!("Remove filter {label}")),
    )
}

/// The active scope as chips, each with a link that removes it.
pub fn scope_chips(params: &ViewParams) -> String {
    let mut chips = Vec::new();
    if let Some(folder) = &params.folder {
        chips.push(chip(
            &format!("in: {}", leaf_name(folder)),
            &view_url(&params.without_folder()),
        ));
    }
    if let Some(collection) = &params.collection {
        chips.push(chip(
            &format!("collection: {}", leaf_name(collection)),
            &view_url(&params.without_collection()),
        ));
    }
    for tag in &params.tags {
        chips.push(chip(
            &format!("tag: {tag}"),
            &view_url(&params.without_tag(tag)),
        ));
    }
    if let Some(text) = &params.q {
        chips.push(chip(&format!("q: {text}"), &view_url(&params.without_q())));
    }
    if params.untagged {
        chips.push(chip("untagged", "/"));
    }
    if params.recent {
        chips.push(chip("recent", "/"));
    }
    if chips.is_empty() {
        return String::new();
    }
    format!("<div class=\"scope-chips\">{}</div>", chips.join(""))
}

/// The breadcrumb trail when `in` is set; each segment narrows to its prefix.
pub fn breadcrumbs(params: &ViewParams) -> String {
    let Some(folder) = &params.folder else {
        return String::new();
    };
    let mut trail = Vec::new();
    let mut walked = String::new();
    for segment in folder.split('/').filter(|segment| !segment.is_empty()) {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(segment);
        trail.push(format!(
            "<a href=\"{}\">{}</a>",
            escape_html(&view_url(&params.with_folder(Some(walked.clone())))),
            escape_html(segment)
        ));
    }
    format!(
        "<nav class=\"breadcrumbs\" aria-label=\"Folder\"><a href=\"{}\">Library</a>{}</nav>",
        escape_html(&view_url(&params.without_folder())),
        trail
            .iter()
            .map(|segment| format!("<span class=\"crumb-sep\">/</span>{segment}"))
            .collect::<String>()
    )
}

/// Notices about ignored query parameters.
pub fn notices(notices: &[String]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    format!(
        "<div class=\"notice-box\" role=\"status\"><ul>{}</ul></div>",
        notices
            .iter()
            .map(|notice| format!("<li>{}</li>", escape_html(notice)))
            .collect::<String>()
    )
}

/// One image tile. The tile is a plain link, so the grid works without JS; the
/// `data-path` attribute is what the inspector fills from.
pub fn tile(item: &ViewItem, params: &ViewParams) -> String {
    let label = item
        .title
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| leaf_name(&item.path));
    format!(
        "<a class=\"tile\" data-path=\"{path}\" href=\"/image/{path_encoded}{view}\">\
         <img loading=\"lazy\" src=\"/thumb/{path_encoded}\" alt=\"{label}\">\
         <span class=\"tile-caption\">{label}</span></a>",
        path = escape_html(&item.path),
        path_encoded = escape_html(&crate::ui::encode_path(&item.path)),
        view = image_view_suffix(params),
        label = escape_html(&label),
    )
}

/// The `?v=` suffix that lets the image page offer "Back to view".
fn image_view_suffix(params: &ViewParams) -> String {
    let query = params.to_query_string();
    if query.is_empty() {
        return String::new();
    }
    format!(
        "?v={}",
        utf8_percent_encode(&query, percent_encoding::NON_ALPHANUMERIC)
    )
}

/// The grid of tiles for one page of results.
pub fn grid(items: &[ViewItem], params: &ViewParams) -> String {
    format!(
        "<section class=\"grid size-{}\" data-total=\"{}\">{}</section>",
        params.size.as_str(),
        items.len(),
        items
            .iter()
            .map(|item| tile(item, params))
            .collect::<String>()
    )
}

/// "Load more": a link to the next page, which JS turns into a fetch-and-append.
pub fn load_more(params: &ViewParams, total: u64) -> String {
    let next = u64::from(params.page) * u64::from(PAGE_SIZE);
    if next >= total {
        return String::new();
    }
    format!(
        "<div class=\"pagination\"><a class=\"load-more-btn\" data-next-page=\"{}\" \
         href=\"{}\">Load more</a></div>",
        params.page + 1,
        escape_html(&view_url(&params.with_page(params.page + 1)))
    )
}

/// What an empty view says, and the way out of it.
pub fn empty_state(params: &ViewParams) -> String {
    let mut out = String::from("<div class=\"empty-state\">");
    if let Some(folder) = &params.folder {
        out.push_str(&format!(
            "<h3>No images in {}</h3>",
            escape_html(&leaf_name(folder))
        ));
        if !params.recursive {
            out.push_str(&format!(
                "<p>This folder has no images of its own. <a href=\"{}\">Show subfolders?</a></p>",
                escape_html(&view_url(&params.with_recursive(true)))
            ));
        } else {
            out.push_str("<p>This folder and its subfolders have no images.</p>");
        }
    } else if let Some(text) = &params.q {
        out.push_str(&format!(
            "<h3>No images matching “{}”</h3><p>Try a different word, or clear the filters.</p>",
            escape_html(text)
        ));
    } else if let Some(collection) = &params.collection {
        out.push_str(&format!(
            "<h3>{} has no images</h3><p>Its embeds resolve to no image that is still here.</p>",
            escape_html(&leaf_name(collection))
        ));
    } else if params.untagged {
        out.push_str("<h3>Every image has a tag</h3><p>Nothing is untagged right now.</p>");
    } else if params.recent {
        out.push_str(
            "<h3>Nothing added recently</h3><p>No image was added in the last 30 days.</p>",
        );
    } else {
        out.push_str("<h3>The library is empty</h3><p>Add images and run a scan to see them.</p>");
    }
    if !params.to_query_string().is_empty() {
        out.push_str("<p><a class=\"back-link\" href=\"/\">Clear all filters</a></p>");
    }
    out.push_str("</div>");
    out
}

/// A folder card, for `/folders`.
pub fn folder_card(path: &str, count: u64) -> String {
    format!(
        "<a class=\"tile folder-card\" href=\"/?in={}\"><span class=\"card-icon\">{FOLDER_ICON}</span>\
         <span class=\"card-title\">{}</span><span class=\"card-count\">{count} items</span></a>",
        escape_html(&query_value(path)),
        escape_html(&leaf_name(path))
    )
}

/// A collection card, for `/collections`.
pub fn collection_card(path: &str, title: &str, count: u64, cover: Option<&str>) -> String {
    let cover_html = match cover {
        Some(image) => format!(
            "<img class=\"card-cover\" loading=\"lazy\" src=\"/thumb/{}\" alt=\"\">",
            escape_html(&crate::ui::encode_path(image))
        ),
        // A collection with no members has no cover, only the glyph.
        None => format!("<span class=\"card-icon\">{COLLECTION_ICON}</span>"),
    };
    let name = if title.is_empty() {
        leaf_name(path)
    } else {
        title.to_owned()
    };
    format!(
        "<a class=\"tile collection-card\" href=\"/?c={}\">{cover_html}\
         <span class=\"card-title\">{}</span><span class=\"card-count\">{count} items</span></a>",
        escape_html(&query_value(path)),
        escape_html(&name),
    )
}

/// A tag row, for `/search`.
pub fn tag_row(tag: &str, count: u64) -> String {
    format!(
        "<a class=\"tag-row\" href=\"/?tag={}\"><span class=\"tag-name\">{}</span>\
         <span class=\"sidebar-row-count\">{count}</span></a>",
        escape_html(&query_value(tag)),
        escape_html(tag)
    )
}

/// The sidebar: `VIEWS`, `FOLDERS`, `COLLECTIONS`, `TAGS`, each row with its
/// count (spec §3, principle 3).
pub fn sidebar_sections(
    data: &SidebarData,
    params: &ViewParams,
    active_folder: Option<&str>,
) -> String {
    let mut out = String::new();

    out.push_str(
        "<section class=\"sidebar-section\"><h2 class=\"sidebar-section-title\">Views</h2>",
    );
    out.push_str(&sidebar_row("/", "All", data.total, is_all(params)));
    out.push_str(&sidebar_row(
        "/?recent=1",
        "Recent",
        data.recent,
        params.recent,
    ));
    out.push_str(&sidebar_row(
        "/?untagged=1",
        "Untagged",
        data.untagged,
        params.untagged,
    ));
    out.push_str("</section>");

    out.push_str(
        "<section class=\"sidebar-section\"><h2 class=\"sidebar-section-title\">Folders</h2>",
    );
    if data.folders.is_empty() {
        out.push_str("<p class=\"sidebar-empty\">No folders yet.</p>");
    } else {
        out.push_str(&folder_tree(
            &data.folders,
            params,
            active_folder.unwrap_or(""),
        ));
    }
    out.push_str("</section>");

    out.push_str(
        "<section class=\"sidebar-section\"><h2 class=\"sidebar-section-title\">Collections</h2>",
    );
    if data.collections.is_empty() {
        out.push_str("<p class=\"sidebar-empty\">No collections yet.</p>");
    } else {
        for collection in &data.collections {
            let label = if collection.title.is_empty() {
                leaf_name(&collection.path)
            } else {
                collection.title.clone()
            };
            out.push_str(&sidebar_row(
                &format!("/?c={}", query_value(&collection.path)),
                &label,
                collection.count,
                params.collection.as_deref() == Some(collection.path.as_str()),
            ));
        }
    }
    out.push_str("</section>");

    out.push_str(
        "<section class=\"sidebar-section\"><h2 class=\"sidebar-section-title\">Tags</h2>",
    );
    let mut tags: Vec<_> = data.tags.iter().collect();
    tags.sort_by(|left, right| right.count.cmp(&left.count).then(left.tag.cmp(&right.tag)));
    if tags.is_empty() {
        out.push_str("<p class=\"sidebar-empty\">No tags yet.</p>");
    } else {
        for tag in tags.iter().take(SIDEBAR_TAG_LIMIT) {
            out.push_str(&sidebar_row(
                &format!("/?tag={}", query_value(&tag.tag)),
                &tag.tag,
                tag.count,
                params.tags.iter().any(|active| active == &tag.tag),
            ));
        }
        if tags.len() > SIDEBAR_TAG_LIMIT {
            out.push_str(&sidebar_row(
                "/search",
                "All tags",
                tags.len() as u64,
                false,
            ));
        }
    }
    out.push_str("</section>");
    out
}

fn is_all(params: &ViewParams) -> bool {
    params.to_query_string().is_empty()
        || (params.sort == SortKey::Added
            && params.direction == Direction::Desc
            && !params.untagged
            && !params.recent)
}

/// The folder rows, nested by path depth so the tree reads as a tree.
fn folder_tree(
    folders: &[crate::index_sync::SidebarFolder],
    params: &ViewParams,
    active: &str,
) -> String {
    let mut rows = Vec::new();
    for folder in folders {
        let depth = folder.path.matches('/').count();
        let current = folder.path == active;
        rows.push(format!(
            "<a class=\"sidebar-row folder-row depth-{depth}{active}\" href=\"/?in={path}{carry}\">\
             <span class=\"sidebar-row-label\">{label}</span><span class=\"sidebar-row-count\">{count}</span></a>",
            active = if current { " active" } else { "" },
            path = escape_html(&query_value(&folder.path)),
            carry = carry_params(params),
            label = escape_html(&leaf_name(&folder.path)),
            count = folder.count,
        ));
    }
    rows.join("")
}

/// The parameters a sidebar link carries along: everything except the lens the
/// link is setting.
fn carry_params(params: &ViewParams) -> String {
    let mut carry = params.clone();
    carry.folder = None;
    carry.collection = None;
    carry.tags.clear();
    carry.q = None;
    carry.untagged = false;
    carry.recent = false;
    carry.page = 1;
    carry
        .to_query_string()
        .split('&')
        .map(str::to_owned)
        .filter(|pair| !pair.is_empty())
        .map(|pair| format!("&amp;{pair}"))
        .collect::<String>()
}

fn sidebar_row(href: &str, label: &str, count: u64, active: bool) -> String {
    format!(
        "<a class=\"sidebar-row{active}\" href=\"{href}\"><span class=\"sidebar-row-label\">{label}</span>\
         <span class=\"sidebar-row-count\">{count}</span></a>",
        active = if active { " active" } else { "" },
        href = escape_html(href),
        label = escape_html(label),
    )
}

fn leaf_name(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

const FOLDER_ICON: &str = r##"<svg class="card-glyph" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="M1.5 3.5h4l1.5 2h7.5v7.5a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z"/></svg>"##;
const COLLECTION_ICON: &str = r##"<svg class="card-glyph" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true" focusable="false"><rect x="4" y="2.5" width="9.5" height="11" rx="1"/><path d="M4 2.5H3a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h8"/></svg>"##;

/// Render a whole page. Every page handler ends here, so the frame is the same
/// wherever the user is.
pub fn page(frame: Frame<'_>) -> String {
    frame.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_sync::{SidebarCollection, SidebarFolder, SidebarTag};

    fn sidebar() -> SidebarData {
        SidebarData {
            folders: vec![
                SidebarFolder {
                    path: "refs".into(),
                    count: 124,
                },
                SidebarFolder {
                    path: "refs/ui".into(),
                    count: 40,
                },
            ],
            collections: vec![SidebarCollection {
                path: "browse.md".into(),
                title: "browse".into(),
                count: 16,
            }],
            tags: (0..40)
                .map(|index| SidebarTag {
                    tag: format!("tag-{index:02}"),
                    count: 100 - index,
                })
                .collect(),
            total: 612,
            untagged: 9,
            recent: 40,
        }
    }

    #[test]
    fn the_sort_menu_keeps_the_scope_and_drops_the_old_sort() {
        let params =
            ViewParams::parse("in=refs/ui&tag=eagle&q=street&sort=size&dir=asc&p=4&size=l");
        let html = sort_control(&params);
        assert!(html.contains("name=\"in\" value=\"refs/ui\""));
        assert!(html.contains("name=\"tag\" value=\"eagle\""));
        assert!(html.contains("name=\"q\" value=\"street\""));
        assert!(html.contains("name=\"size\" value=\"l\""));
        assert!(
            !html.contains("name=\"p\""),
            "changing the sort returns to page one"
        );
        assert!(
            !html.contains("name=\"dir\""),
            "the direction rides in the sort value"
        );
        assert!(html.contains("<option value=\"size-asc\" selected>"));
    }

    #[test]
    fn the_sort_menu_carries_the_lenses_and_a_non_recursive_folder() {
        let params = ViewParams::parse("in=refs&sub=0&untagged=1&recent=1&c=set.md");
        let html = sort_control(&params);
        assert!(html.contains("name=\"sub\" value=\"0\""));
        assert!(html.contains("name=\"untagged\" value=\"1\""));
        assert!(html.contains("name=\"recent\" value=\"1\""));
        assert!(html.contains("name=\"c\" value=\"set.md\""));
    }

    #[test]
    fn chips_name_the_scope_and_offer_a_way_to_drop_it() {
        let params = ViewParams::parse("in=refs/ui&tag=eagle&tag=nature&q=street&c=browse.md");
        let html = scope_chips(&params);
        assert!(html.contains("in: ui"));
        assert!(html.contains("tag: eagle"));
        assert!(html.contains("tag: nature"));
        assert!(html.contains("q: street"));
        assert!(html.contains("collection: browse.md"));
        assert_eq!(html.matches("chip-remove").count(), 5);
        // Dropping a tag keeps the rest of the scope, in the canonical order.
        assert!(html.contains("href=\"/?in=refs/ui&amp;c=browse.md&amp;tag=nature&amp;q=street\""));
    }

    #[test]
    fn there_are_no_chips_for_a_plain_library_view() {
        assert!(scope_chips(&ViewParams::default()).is_empty());
    }

    #[test]
    fn the_breadcrumb_narrows_to_each_prefix() {
        let html = breadcrumbs(&ViewParams::parse("in=refs/ui/icons&tag=eagle"));
        assert!(html.contains("href=\"/?tag=eagle\">Library</a>"));
        assert!(html.contains("href=\"/?in=refs&amp;tag=eagle\">refs</a>"));
        assert!(html.contains("href=\"/?in=refs/ui&amp;tag=eagle\">ui</a>"));
        assert!(html.contains("href=\"/?in=refs/ui/icons&amp;tag=eagle\">icons</a>"));
    }

    #[test]
    fn there_is_no_breadcrumb_without_a_folder() {
        assert!(breadcrumbs(&ViewParams::default()).is_empty());
    }

    #[test]
    fn the_sidebar_lists_every_section_with_counts() {
        let params = ViewParams::parse("recent=1");
        let html = sidebar_sections(&sidebar(), &params, None);
        for section in ["Views", "Folders", "Collections", "Tags"] {
            assert!(html.contains(section), "missing the {section} section");
        }
        assert!(html.contains("All</span><span class=\"sidebar-row-count\">612"));
        assert!(html.contains("Untagged</span><span class=\"sidebar-row-count\">9"));
        assert!(html.contains("Recent</span><span class=\"sidebar-row-count\">40"));
        assert!(html.contains("browse</span><span class=\"sidebar-row-count\">16"));
        // The active lens is marked, and only that one.
        assert!(html.contains("class=\"sidebar-row active\" href=\"/?recent=1\""));
        assert_eq!(html.matches("sidebar-row active").count(), 1);
    }

    #[test]
    fn the_sidebar_caps_the_tags_and_offers_the_rest() {
        let html = sidebar_sections(&sidebar(), &ViewParams::default(), None);
        assert!(html.contains("tag-29"));
        assert!(!html.contains("tag-30"), "only 30 tags are listed");
        assert!(html.contains("All tags"));
        assert!(html.contains("All tags</span><span class=\"sidebar-row-count\">40"));
    }

    #[test]
    fn the_sidebar_marks_the_folder_it_is_inside() {
        let html = sidebar_sections(&sidebar(), &ViewParams::default(), Some("refs/ui"));
        assert!(html.contains("class=\"sidebar-row folder-row depth-1 active\""));
        assert!(html.contains("depth-0"));
    }

    #[test]
    fn a_sidebar_row_carries_the_size_choice_but_sets_the_focus() {
        let params = ViewParams::parse("in=refs&tag=eagle&size=l&untagged=1");
        let html = sidebar_sections(&sidebar(), &params, None);
        let folder_row = html
            .split("sidebar-row folder-row")
            .nth(1)
            .expect("a folder row");
        assert!(folder_row.contains("size=l"), "keeps the size");
        assert!(folder_row.contains("in=refs"), "sets the folder");
        assert!(
            !folder_row.contains("tag=eagle") && !folder_row.contains("untagged"),
            "the row is the scope, so the previous scope is dropped: {folder_row}"
        );
    }

    #[test]
    fn an_empty_library_says_so_in_the_sidebar() {
        let empty = SidebarData {
            folders: Vec::new(),
            collections: Vec::new(),
            tags: Vec::new(),
            total: 0,
            untagged: 0,
            recent: 0,
        };
        let html = sidebar_sections(&empty, &ViewParams::default(), None);
        assert_eq!(html.matches("sidebar-empty").count(), 3);
    }

    #[test]
    fn the_size_control_offers_three_links_and_marks_the_current_one() {
        let html = size_toggles(&ViewParams::parse("in=refs&size=l"));
        assert_eq!(html.matches("<a class=\"size-toggle-btn").count(), 3);
        assert!(html.contains("data-size=\"l\" aria-current=\"true\""));
        assert!(
            html.contains("href=\"/?in=refs\""),
            "`m` is the default, so choosing it adds no parameter"
        );
    }

    #[test]
    fn a_tile_is_a_link_that_remembers_the_view() {
        let item = ViewItem {
            path: "refs/ui/猫 & co.png".into(),
            size: 1,
            mtime_ns: 0,
            added_ns: 0,
            title: Some("猫 & co".into()),
            rating: Some(5),
            note_path: None,
        };
        let html = tile(&item, &ViewParams::parse("in=refs&sort=name"));
        assert!(html.contains(
            "href=\"/image/refs/ui/%E7%8C%AB%20%26%20co.png?v=in%3Drefs%26sort%3Dname\""
        ));
        assert!(html.contains("src=\"/thumb/refs/ui/%E7%8C%AB%20%26%20co.png\""));
        assert!(html.contains("alt=\"猫 &amp; co\""));
        assert!(html.contains("data-path=\"refs/ui/猫 &amp; co.png\""));
    }

    #[test]
    fn a_tile_without_a_title_is_named_after_its_file() {
        let item = ViewItem {
            path: "refs/plain.png".into(),
            size: 1,
            mtime_ns: 0,
            added_ns: 0,
            title: None,
            rating: None,
            note_path: None,
        };
        let html = tile(&item, &ViewParams::default());
        assert!(html.contains("alt=\"plain.png\""));
        assert!(
            !html.contains("?v="),
            "the whole library needs no back link"
        );
    }

    #[test]
    fn load_more_appears_only_while_there_is_more_to_show() {
        let params = ViewParams::default();
        assert!(
            load_more(&params, 10).is_empty(),
            "10 items fit on one page"
        );
        let html = load_more(&params, 300);
        assert!(html.contains("data-next-page=\"2\""));
        assert!(html.contains("href=\"/?p=2\""));
        let second = ViewParams::parse("p=2");
        let html = load_more(&second, 300);
        assert!(
            html.contains("data-next-page=\"3\""),
            "300 items fill 3 pages"
        );
        let third = ViewParams::parse("p=3");
        assert!(
            load_more(&third, 300).is_empty(),
            "the last page offers nothing"
        );
    }

    #[test]
    fn an_empty_folder_offers_subfolders_and_an_empty_search_offers_a_reset() {
        let shallow = empty_state(&ViewParams::parse("in=refs&sub=0"));
        assert!(shallow.contains("Show subfolders?"));
        assert!(shallow.contains("href=\"/?in=refs\""));

        let deep = empty_state(&ViewParams::parse("in=refs"));
        assert!(deep.contains("no images"));
        assert!(!deep.contains("Show subfolders?"));

        let search = empty_state(&ViewParams::parse("q=nothing"));
        assert!(search.contains("Clear all filters"));
    }

    #[test]
    fn an_empty_library_says_what_to_do_about_it() {
        let html = empty_state(&ViewParams::default());
        assert!(html.contains("The library is empty"));
        assert!(!html.contains("Clear all filters"));
    }

    #[test]
    fn folder_and_collection_cards_link_to_the_view_they_represent() {
        assert!(folder_card("refs/ui", 124).contains("href=\"/?in=refs/ui\""));
        let card = collection_card("browse.md", "browse", 16, Some("refs/a.png"));
        assert!(card.contains("href=\"/?c=browse.md\""));
        assert!(card.contains("src=\"/thumb/refs/a.png\""));
        assert!(collection_card("empty.md", "", 0, None).contains("card-glyph"));
    }

    #[test]
    fn notices_are_listed_not_dumped() {
        assert!(notices(&[]).is_empty());
        let html = notices(&["Ignored sort 'nope'".to_owned()]);
        assert!(html.contains("role=\"status\""));
        assert!(html.contains("<li>Ignored sort &#39;nope&#39;</li>"));
    }

    #[test]
    fn the_search_pill_is_a_link_to_the_search_page() {
        assert!(search_pill(None).contains("href=\"/search\""));
        assert!(search_pill(Some(&ViewParams::default())).contains(">Search</span>"));
        assert!(search_pill(Some(&ViewParams::parse("in=refs"))).contains(">Refine</span>"));
    }

    #[test]
    fn the_result_count_is_singular_for_one_item() {
        assert!(result_count(1).contains("1 item<"));
        assert!(result_count(0).contains("0 items<"));
        assert!(result_count(2).contains("2 items<"));
    }
}
#[cfg(test)]
mod debug_probe {
    #[test]
    fn probe() {
        println!(
            "SIZE: {}",
            super::size_toggles(&crate::view_query::ViewParams::parse("in=refs&size=l"))
        );
        println!(
            "CHIP: {}",
            super::scope_chips(&crate::view_query::ViewParams::parse(
                "in=refs/ui&tag=eagle&tag=nature&q=street&c=browse.md"
            ))
        );
    }
}
