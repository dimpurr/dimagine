//! UI components: tiles, chips, sort menu, breadcrumb, counts, sidebar.
//!
//! Everything here returns HTML fragments. No component reads the request or
//! the filesystem; the page handlers decide what data a component gets.

use dimagine_index::ViewItem;
use percent_encoding::utf8_percent_encode;

use crate::index_sync::{recent_label, SidebarData, RECENT_LIMIT};
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

/// === W43: search field, chips and states ===
///
/// The pill, the chips it carries and the empty / error / loading states
/// follow the refined style tile (`nm/specs/style-tile.html`, DESIGN.md
/// §4.3, §4.7, §4.8): the kind of a filter is a glyph, not an `in:` / `tag:`
/// prefix; the chip sits inside the search capsule on one scrolling line;
/// a state is a small monochrome glyph, a regular-weight title, one
/// sentence and one text-style action.
///
/// A 12px folder: the kind of a `in:` filter, drawn instead of written.
const CHIP_GLYPH_FOLDER: &str = r##"<svg class="chip-kind" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="M1.5 3.5h4l1.5 2h7.5v7.5a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z"/></svg>"##;
/// A 12px tag.
const CHIP_GLYPH_TAG: &str = r##"<svg class="chip-kind" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" aria-hidden="true" focusable="false"><path d="M2 2h5l7 7-5 5-7-7z"/><circle cx="5" cy="5" r="1.1" fill="currentColor" stroke="none"/></svg>"##;
/// Stacked frames: the kind of a collection filter.
const CHIP_GLYPH_COLLECTION: &str = r##"<svg class="chip-kind" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true" focusable="false"><rect x="4" y="2.5" width="9.5" height="11" rx="1"/><path d="M4 2.5H3a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h8"/></svg>"##;
/// Text lines: a bare search term.
const CHIP_GLYPH_TEXT: &str = r##"<svg class="chip-kind" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true" focusable="false"><path d="M3.2 4.6h9.6M3.2 8h9.6M3.2 11.4h5.6"/></svg>"##;
/// A funnel: a filter that carries no value (`untagged`, `recent`).
const CHIP_GLYPH_FILTER: &str = r##"<svg class="chip-kind" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M2.4 3.6h11.2l-4.3 4.8v4.2l-2.6-1.5V8.4z"/></svg>"##;
/// The remove mark inside a chip.
const CHIP_REMOVE_GLYPH: &str = r##"<svg viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true" focusable="false"><path d="M1.6 1.6 8.4 8.4M8.4 1.6 1.6 8.4"/></svg>"##;
/// A magnifier for the leading edge of a search capsule.
pub const SEARCH_GLYPH: &str = r##"<svg class="search-glyph" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true" focusable="false"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14.5 14.5" stroke-linecap="round"/></svg>"##;
/// The state glyph (DESIGN.md §4.8): 28px on screen, drawn on a 24-unit
/// grid with 1.5-unit strokes, monochrome, no container.
pub const STATE_GLYPH_PHOTO: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><rect x="3" y="4.5" width="18" height="15" rx="3"/><circle cx="8.6" cy="9.7" r="1.5"/><path d="M3.6 16.6l4.3-3.7 3.2 2.5 3.5-3.1 5.1 4.4"/></svg>"##;
/// The error variant of the state glyph; `--danger` lands on this glyph and
/// nowhere else (§4.8).
pub const STATE_GLYPH_WARN: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M12 3.7 21.1 19.8H2.9z"/><path d="M12 9.7v4.5"/><path d="M12 17.1h.01" stroke-width="2"/></svg>"##;

/// Which glyph a chip draws. The tooltip names the kind in words, so the
/// label itself carries only the value.
#[derive(Clone, Copy)]
enum ChipKind {
    Folder,
    Tag,
    Collection,
    Text,
    Filter,
}

impl ChipKind {
    fn glyph(self) -> &'static str {
        match self {
            Self::Folder => CHIP_GLYPH_FOLDER,
            Self::Tag => CHIP_GLYPH_TAG,
            Self::Collection => CHIP_GLYPH_COLLECTION,
            Self::Text => CHIP_GLYPH_TEXT,
            Self::Filter => CHIP_GLYPH_FILTER,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Folder => "Folder",
            Self::Tag => "Tag",
            Self::Collection => "Collection",
            Self::Text => "Search",
            Self::Filter => "Filter",
        }
    }
}

/// One removable scope chip, as the tile draws it: a quiet capsule whose
/// kind is a glyph, whose label truncates at 22ch with the whole value in
/// the native tooltip, and whose ✕ is a link to the same view without this
/// one filter — so removal works with scripting off and is a real tab stop.
/// The label and tooltip quote the user's own words, so they are escaped
/// here rather than trusting every caller.
fn chip(kind: ChipKind, label: &str, tooltip_value: &str, remove_url: &str) -> String {
    let tooltip = format!("{}: {}", kind.name(), tooltip_value);
    format!(
        "<span class=\"chip\" title=\"{tooltip}\">{glyph}\
         <span class=\"chip-label\">{label}</span>\
         <a class=\"chip-remove\" href=\"{url}\" aria-label=\"{remove}\">{X}</a></span>",
        tooltip = escape_html(&tooltip),
        glyph = kind.glyph(),
        label = escape_html(label),
        url = escape_html(remove_url),
        remove = escape_html(&format!("Remove filter {tooltip}")),
        X = CHIP_REMOVE_GLYPH,
    )
}

/// The active scope as chips, each with a link that removes it. The chips
/// live *inside* the search capsule (`search_pill`), on one line that
/// scrolls sideways rather than wrapping.
pub fn scope_chips(params: &ViewParams) -> String {
    let mut chips = Vec::new();
    if let Some(folder) = &params.folder {
        chips.push(chip(
            ChipKind::Folder,
            &leaf_name(folder),
            folder,
            &view_url(&params.without_folder()),
        ));
    }
    if let Some(collection) = &params.collection {
        chips.push(chip(
            ChipKind::Collection,
            &leaf_name(collection),
            collection,
            &view_url(&params.without_collection()),
        ));
    }
    for tag in &params.tags {
        chips.push(chip(
            ChipKind::Tag,
            tag,
            tag,
            &view_url(&params.without_tag(tag)),
        ));
    }
    if let Some(text) = &params.q {
        chips.push(chip(
            ChipKind::Text,
            text,
            text,
            &view_url(&params.without_q()),
        ));
    }
    if params.untagged {
        chips.push(chip(ChipKind::Filter, "untagged", "untagged", "/"));
    }
    if params.recent {
        chips.push(chip(ChipKind::Filter, "recent", "recent", "/"));
    }
    if chips.is_empty() {
        return String::new();
    }
    format!("<span class=\"scope-chips\">{}</span>", chips.join(""))
}

/// The search pill: the toolbar's capsule, and where the chips live (§4.3).
/// On the library it shows the active scope as chips on one line; tapping it
/// goes to `/search`, where there is a real field (spec §3). The capsule may
/// be a link rather than a field — there is simply no input. Once chips are
/// inside, the capsule cannot also be one link around them, so it becomes a
/// field-shaped container and the trailing label is the link to the page.
pub fn search_pill(view: Option<&ViewParams>) -> String {
    let label = match view {
        Some(params) if !params.to_query_string().is_empty() => "Refine",
        _ => "Search",
    };
    let chips = view.map(scope_chips).unwrap_or_default();
    if chips.is_empty() {
        return format!(
            "<a class=\"search-pill-container\" href=\"/search\">{glyph}<span class=\
             \"search-label\">{label}</span></a>",
            glyph = SEARCH_GLYPH,
        );
    }
    format!(
        "<div class=\"search-pill-container pill-scoped\">{glyph}{chips}\
         <a class=\"search-label\" href=\"/search\">{label}</a></div>",
        glyph = SEARCH_GLYPH,
    )
}

/// `N items`, and how many are on screen when paging.
pub fn result_count(total: u64) -> String {
    let noun = if total == 1 { "item" } else { "items" };
    format!("<span class=\"result-count\">{total} {noun}</span>")
}

/// The sort menu, as a native `<select>` in a GET form so it works without JS.
///
/// Two views have no order to choose, so they state the order in force
/// instead of a menu whose picks the grid would ignore: a collection
/// (note order, [`note_order_control`]) and the Recent lens (added order,
/// [`recent_sort_control`], W37 review, Medium #1).
///
/// Only the parameters that are not the sort travel as hidden fields, so
/// changing the sort keeps the scope and returns to page one.
pub fn sort_control(params: &ViewParams) -> String {
    if params.collection.is_some() {
        return note_order_control();
    }
    if params.recent {
        return recent_sort_control();
    }
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
    // `c` needs no hidden field: a view over a collection never reaches this
    // menu — it states the note's own order instead (`note_order_control`).
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

/// The sort control over a collection, where there is no order to choose.
///
/// "Order of embeds is the order of the collection" (FORMAT §5), so every
/// menu choice would be a promise the grid does not keep — the lie the
/// Recent lens's menu told (W37 review, Medium #1). Like Recent, the control
/// states the order in force: a focusable label (W39 review L1 — never a
/// disabled control, which cannot be focused or announced), the whole
/// sentence in `title` and `aria-label` (hover, focus, screen readers), and
/// only the short words visible, keeping the toolbar's one row within a
/// 390 px phone beside the count, the size segments and the theme switch
/// (W39 review M1). "Note order" is the shortest honest name for the
/// order of the embeds in the note.
fn note_order_control() -> String {
    let note = "A collection is ordered by the embeds in its note.";
    format!(
        "<span class=\"sort-form\">         <span class=\"search-hint note-order\" role=\"note\" \
         tabindex=\"0\" aria-label=\"Sort order — {note}\" title=\"{note}\">Note order</span></span>",
        note = escape_html(note),
    )
}

/// The sort control on the Recent lens, where there is nothing to choose.
///
/// Recent is "the last [`RECENT_LIMIT`] added, newest first" by definition
/// (W34 audit #10), so a sort beside it cannot decide the order. Offering the
/// menu anyway made the control lie: it showed the picked option while the grid
/// stayed Added-descending, and picking another one changed the URL and nothing
/// else (W37 review, Medium #1). So it shows the order in force.
///
/// It is a focusable label, not a disabled `<select>`: a disabled control is
/// unfocusable and its `title` never renders, so a keyboard-only reader had no
/// way to reach the sentence (W39 review L1), the UA dimming was not the
/// design's disabled state (L2), and the visible hint that carried it was 107px
/// too wide for the 390px toolbar (M1). A label is in the tab order, draws the
/// focus ring, and carries the whole sentence in `title` (hover and focus) and
/// `aria-label` (screen readers).
///
/// It is deliberately as narrow as a label can be: one descending arrow beside
/// the count, in the count's own muted text scale. The bar is one row
/// (DESIGN.md §4.4) — count, this label, the size segments, the theme switch —
/// and at 390px the five controls only fit when this one is an arrow: anything
/// wordier squeezes the size segments until `L` is clipped, which is what the
/// worded label and the hint it carried did (W39 review M1). The words are not
/// gone, they are one hover or one Tab away.
///
/// The one view that reaches it with a collection set narrows its members and
/// keeps their note order, so the collection's own label ([`note_order_control`])
/// states that order instead — `sort_control` visits it first.
fn recent_sort_control() -> String {
    let note = format!("Recent is always the last {RECENT_LIMIT} added, newest first.");
    format!(
        "<span class=\"sort-form\"><span class=\"search-hint recent-order\" role=\"note\" \
         tabindex=\"0\" aria-label=\"Sort order — {note}\" title=\"{note}\">↓</span></span>",
        note = escape_html(&note),
    )
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
///
/// The tile is square and the picture is whole (`object-fit: contain`). Only an
/// extreme aspect ratio is cropped, and the aspect ratio cannot be emitted here:
/// the index stores no dimensions (DESIGN.md §4.5 — "computed from the loaded
/// image's natural size" until it does). The script therefore measures the
/// thumbnail once it loads and writes `data-fit` (`tall` / `wide` / `normal`)
/// plus the measured `data-w` / `data-h` on the tile, which is what the
/// stylesheet crops by. A size that never arrives leaves the tile in its
/// default `contain`, so an unknown is never turned into a guess (invariant 4).
pub fn tile(item: &ViewItem, params: &ViewParams) -> String {
    let label = item
        .title
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| leaf_name(&item.path));
    format!(
        "<a class=\"tile\" data-path=\"{path}\" href=\"/image/{path_encoded}{view}\" title=\"{label}\">\
         <img loading=\"lazy\" decoding=\"async\" src=\"/thumb/{path_encoded}\" alt=\"{label}\">\
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

/// One collection member as the collection page lists it: the image as the
/// library grid shows it, plus the caption line its note gives it.
pub struct CollectionTile {
    /// Display data for the tile, joined from the index where it knows the
    /// image; a member the index has never seen still shows, named by its
    /// file.
    pub item: ViewItem,
    /// The line directly after the embed (FORMAT §5), as written; empty is
    /// no caption, a distinct fact that is not invented here.
    pub caption: String,
}

/// The collection page's grid: the library's tiles, each with the caption
/// its note gives it shown under the picture — the same place the inspector
/// renders a note's caption (DESIGN.md §4.6), not the hover overlay the
/// tile's own name uses. A member without a caption line shows no caption.
///
/// The tile inside each item is exactly the library tile ([`tile`]): its
/// prefix is the contract `tests/viewer.rs` parses, the inspector fills
/// from its `data-path`, and the aspect script finds it.
pub fn collection_grid(items: &[CollectionTile], params: &ViewParams) -> String {
    format!(
        "<section class=\"grid grid-collection size-{}\" data-total=\"{}\">{}</section>",
        params.size.as_str(),
        items.len(),
        items
            .iter()
            .map(|member| {
                let caption = if member.caption.is_empty() {
                    String::new()
                } else {
                    // The caption is the note's own words: escaped for the
                    // element and the tooltip, and truncating only after
                    // the whole line stays readable one hover away
                    // (DESIGN.md §4.1).
                    format!(
                        "<span class=\"member-caption\" title=\"{}\">{}</span>",
                        escape_html(&member.caption),
                        escape_html(&member.caption)
                    )
                };
                format!(
                    "<div class=\"grid-item\">{}{caption}</div>",
                    tile(&member.item, params)
                )
            })
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

/// What an empty view says, and the way out of it — the §4.8 anatomy: a
/// small monochrome glyph, a title one step above the sentence, one
/// sentence, and at most one text-style action. Where there is already a
/// way out there is no second one; "Clear all filters" appears only when
/// nothing else is.
pub fn empty_state(params: &ViewParams) -> String {
    let (title, sentence, action): (String, String, Option<(String, String)>) =
        if let Some(collection) = &params.collection {
            // A collection is the page (FORMAT §5); a filter beside it
            // (folder, tags, words, a lens) only narrows which members show,
            // so the empty sentence says which of the two found nothing —
            // the same order `view_title` names the view in. The chip row
            // and the action below already carry the way out.
            let narrowed = params.folder.is_some()
                || !params.tags.is_empty()
                || params.q.is_some()
                || params.untagged
                || params.recent;
            (
                format!("{} has no images", leaf_name(collection)),
                if narrowed {
                    "No member of this collection matches the filters beside it.".to_owned()
                } else {
                    "Its embeds resolve to no image that is still here.".to_owned()
                },
                None,
            )
        } else if let Some(folder) = &params.folder {
            if params.recursive {
                (
                    format!("No images in {}", leaf_name(folder)),
                    "This folder and its subfolders have no images.".to_owned(),
                    None,
                )
            } else {
                (
                    format!("No images in {}", leaf_name(folder)),
                    "This folder has no images of its own.".to_owned(),
                    Some((
                        "Show subfolders".to_owned(),
                        view_url(&params.with_recursive(true)),
                    )),
                )
            }
        } else if let Some(text) = &params.q {
            (
                format!("No images matching “{text}”"),
                "Try a different word, or clear the filters.".to_owned(),
                None,
            )
        } else if params.untagged {
            (
                "Every image has a tag".to_owned(),
                "Nothing is untagged right now.".to_owned(),
                None,
            )
        } else if params.recent {
            // W34 audit #10: Recent is "the last 200 added", so an empty lens
            // means an empty library or filters that match none of it — never
            // a claim about days.
            (
                "Nothing in Recent".to_owned(),
                "The library is empty, or the filters beside Recent match no image.".to_owned(),
                None,
            )
        } else {
            (
                "The library is empty".to_owned(),
                "Add images and run a scan to see them.".to_owned(),
                None,
            )
        };

    let mut out = format!(
        "<div class=\"empty-state\"><span class=\"state-glyph\" aria-hidden=\"true\">\
         {STATE_GLYPH_PHOTO}</span><h3>{}</h3><p>{}</p>",
        escape_html(&title),
        escape_html(&sentence),
    );
    let action = match action {
        Some((label, url)) => Some((label, url)),
        None if !params.to_query_string().is_empty() => {
            Some(("Clear all filters".to_owned(), "/".to_owned()))
        }
        None => None,
    };
    if let Some((label, url)) = action {
        out.push_str(&format!(
            "<a class=\"state-action\" href=\"{}\">{}</a>",
            escape_html(&url),
            escape_html(&label),
        ));
    }
    out.push_str("</div>");
    out
}

/// A page past the end of a view (W37 review, Medium #2).
///
/// The view is not empty — the count beside it says how many images it holds —
/// so the empty view's words would contradict it ("the library is empty" on a
/// library of 205). This says where the page sits and offers the one way back
/// to the images.
pub fn past_end_state(params: &ViewParams, total: u64) -> String {
    let noun = if total == 1 { "item" } else { "items" };
    format!(
        "<div class=\"empty-state\"><span class=\"state-glyph\" aria-hidden=\"true\">\
         {STATE_GLYPH_PHOTO}</span><h3>Nothing on this page</h3>\
         <p>This view has {total} {noun}; this page is past the end of it.</p>\
         <a class=\"state-action\" href=\"{}\">Back to the first page</a></div>",
        escape_html(&view_url(&params.with_page(1))),
    )
}

/// The error form of the §4.8 state: a **neutral** surface. `--danger`
/// lands on the status glyph and nowhere else — not the title, not a fill,
/// not a border. The sentence names the precise cause; this is never a big
/// red block and never a raw exception.
pub fn error_state(title: &str, sentence: &str) -> String {
    format!(
        "<div class=\"empty-state state-error\"><span class=\"state-glyph\" aria-hidden=\"true\">\
         {STATE_GLYPH_WARN}</span><h3>{}</h3><p>{}</p></div>",
        escape_html(title),
        escape_html(sentence),
    )
}

/// The inline form of §4.8 — the same anatomy one size down, for inside a
/// panel or a list. Its action is a `<button>` carrying `action_attribute`,
/// because an inline state such as "No tags match" is client-side and has
/// no URL to point at; the stylesheet keeps it off screen until the script
/// reveals it.
pub fn inline_state(
    title: &str,
    sentence: &str,
    action_label: &str,
    action_attribute: &str,
) -> String {
    format!(
        "<div class=\"inline-state\" hidden><span class=\"state-glyph\" aria-hidden=\"true\">\
         {STATE_GLYPH_PHOTO}</span><p class=\"state-title\">{}</p><p class=\"state-sentence\">\
         {}</p><button class=\"state-action\" type=\"button\" {action_attribute}>{}</button></div>",
        escape_html(title),
        escape_html(sentence),
        escape_html(action_label),
    )
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
        // The label states the definition — "the last 200 added" — so the
        // row can no longer read as a smaller "All" (W34 audit #10).
        &recent_label(),
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
///
/// Every row carries its own path and its parent's, which is all the client
/// script needs to collapse and expand nodes; the disclosure triangle is a
/// real `<button>` outside the row link, so both the row and the toggle are
/// keyboard stops. Without the script the whole tree is simply shown: the
/// stylesheet hides the triangles until `html.js` is present.
///
/// The full path rides in `title` and in a styled tooltip span, because a
/// truncating label must always have a way to be read whole (W34 audit #6,
/// DESIGN.md §4.1).
fn folder_tree(
    folders: &[crate::index_sync::SidebarFolder],
    params: &ViewParams,
    active: &str,
) -> String {
    let parents: std::collections::HashSet<&str> = folders
        .iter()
        .filter_map(|folder| folder.path.rsplit_once('/').map(|(parent, _)| parent))
        .collect();
    let mut out = String::from("<div class=\"sidebar-tree\">");
    for folder in folders {
        let depth = folder.path.matches('/').count();
        let parent = folder.path.rsplit_once('/').map_or("", |(head, _)| head);
        let label = leaf_name(&folder.path);
        let current = folder.path == active;
        let has_children = parents.contains(folder.path.as_str());
        let open_by_default = depth == 0;
        out.push_str(&format!(
            "<div class=\"folder-node depth-{depth}\">{triangle}\
             <a class=\"sidebar-row folder-row{active}\" \
             data-folder=\"{raw}\" data-parent=\"{raw_parent}\" href=\"/?in={path}{carry}\" \
             title=\"{full}\"><span class=\"sidebar-row-label\">{label}</span>\
             <span class=\"sidebar-row-count\">{count}</span>\
             <span class=\"sidebar-tip\" aria-hidden=\"true\">{full}</span></a></div>",
            triangle = if has_children {
                format!(
                    "<button type=\"button\" class=\"tree-triangle\" data-folder=\"{raw}\" \
                     aria-expanded=\"{expanded}\" aria-label=\"{verb} {label}\">\
                     <span class=\"tree-tri\"></span></button>",
                    raw = escape_html(&folder.path),
                    expanded = open_by_default,
                    verb = if open_by_default {
                        "Collapse"
                    } else {
                        "Expand"
                    },
                    label = escape_html(&label),
                )
            } else {
                String::new()
            },
            active = if current { " active" } else { "" },
            raw = escape_html(&folder.path),
            raw_parent = escape_html(parent),
            path = escape_html(&query_value(&folder.path)),
            carry = carry_params(params),
            full = escape_html(&folder.path),
            label = escape_html(&label),
            count = folder.count,
        ));
    }
    out.push_str("</div>");
    out
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
        "<a class=\"sidebar-row{active}\" href=\"{href}\" title=\"{label}\">\
         <span class=\"sidebar-row-label\">{label}</span>\
         <span class=\"sidebar-row-count\">{count}</span>\
         <span class=\"sidebar-tip\" aria-hidden=\"true\">{label}</span></a>",
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
        // Not `recent` and not a collection: those two views state the order
        // in force instead of offering choices the grid would ignore (W37
        // review, Medium #1; FORMAT §5 note order) — see
        // `the_recent_order_is_a_focusable_arrow_not_a_wide_hint` and
        // `a_collection_states_the_note_order_not_a_menu`.
        let params = ViewParams::parse("in=refs&sub=0&untagged=1");
        let html = sort_control(&params);
        assert!(html.contains("name=\"sub\" value=\"0\""));
        assert!(html.contains("name=\"untagged\" value=\"1\""));
    }

    #[test]
    fn chips_name_the_scope_and_offer_a_way_to_drop_it() {
        let params = ViewParams::parse("in=refs/ui&tag=eagle&tag=nature&q=street&c=browse.md");
        let html = scope_chips(&params);
        // The kind is a glyph, not a prefix: no `in:` / `tag:` / `q:` text,
        // just the value — the whole "kind: value" survives in the tooltip.
        assert!(html.contains("title=\"Folder: refs/ui\""));
        assert!(html.contains("title=\"Tag: eagle\""));
        assert!(html.contains("title=\"Tag: nature\""));
        assert!(html.contains("title=\"Search: street\""));
        assert!(html.contains("title=\"Collection: browse.md\""));
        assert!(!html.contains("in:"), "{html}");
        assert!(!html.contains("tag:"), "{html}");
        assert!(!html.contains("q:"), "{html}");
        assert_eq!(html.matches("chip-remove").count(), 5);
        // Dropping a tag keeps the rest of the scope, in the canonical order.
        assert!(html.contains("href=\"/?in=refs/ui&amp;c=browse.md&amp;tag=nature&amp;q=street\""));
    }

    #[test]
    fn there_are_no_chips_for_a_plain_library_view() {
        assert!(scope_chips(&ViewParams::default()).is_empty());
    }

    #[test]
    fn a_chip_escapes_a_search_word_carrying_markup() {
        let html = scope_chips(&ViewParams::parse(
            "q=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E",
        ));
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "the escaped form is present: {html}"
        );
        assert!(
            !html.contains("<img src=x"),
            "the raw form rendered: {html}"
        );
    }

    #[test]
    fn a_chip_escapes_a_tag_name_carrying_script_and_quotes() {
        let html = scope_chips(&ViewParams::parse("tag=%3Cscript%3E%22x%22"));
        assert!(html.contains("&lt;script&gt;&quot;x&quot;"), "{html}");
        assert!(!html.contains("<script>"), "the raw form rendered: {html}");
        assert!(!html.contains("\"x\""), "a raw quote pair rendered: {html}");
    }

    #[test]
    fn a_chip_and_a_crumb_escape_a_folder_name_carrying_markup() {
        let params = ViewParams::parse("in=%22%3E%3Cimg%20onerror%3E");
        let chips = scope_chips(&params);
        assert!(
            chips.contains("Folder: &quot;&gt;&lt;img onerror&gt;"),
            "{chips}"
        );
        assert!(
            !chips.contains("\"><img onerror"),
            "the raw form rendered: {chips}"
        );
        let trail = breadcrumbs(&params);
        assert!(trail.contains("&quot;&gt;&lt;img onerror&gt;"), "{trail}");
        assert!(
            !trail.contains("\"><img onerror"),
            "the raw form rendered: {trail}"
        );
    }

    #[test]
    fn a_chip_escapes_a_collection_name_carrying_markup() {
        let html = scope_chips(&ViewParams::parse("c=%22%3E%3Csvg%3E.md"));
        assert!(
            html.contains("Collection: &quot;&gt;&lt;svg&gt;.md"),
            "{html}"
        );
        // The chip's own glyph is an `<svg`; the injected payload is the
        // whole `"><svg>` pair closing a quoted attribute, which never
        // appears raw.
        assert!(!html.contains("\"><svg>"), "the raw form rendered: {html}");
    }

    #[test]
    fn a_chip_remove_link_carries_the_rest_of_the_scope_url_encoded() {
        let html = scope_chips(&ViewParams::parse(
            "tag=eagle&q=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E",
        ));
        assert!(
            html.contains("href=\"/?q=%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E\""),
            "dropping the tag keeps q percent-encoded: {html}"
        );
        assert!(
            html.contains("href=\"/?tag=eagle\""),
            "dropping q keeps tag: {html}"
        );
    }

    #[test]
    fn a_collection_title_carrying_markup_is_escaped_on_its_card() {
        let card = collection_card("set.md", "Set <img onerror=x>", 3, None);
        assert!(card.contains("Set &lt;img onerror=x&gt;"), "{card}");
        assert!(
            !card.contains("<img onerror"),
            "the raw form rendered: {card}"
        );
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
        assert!(html.contains("Recent — last 200 added</span><span class=\"sidebar-row-count\">40"));
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
        assert!(html.contains("class=\"sidebar-row folder-row active\""));
        assert!(html.contains("folder-node depth-0"));
        assert!(html.contains("folder-node depth-1"));
    }

    /// W34 audit #4: the tree must be collapsible, so every row carries its
    /// place in the tree and every branch carries a keyboard-reachable toggle.
    #[test]
    fn folder_rows_name_their_node_and_parent_and_branches_get_a_triangle() {
        let html = sidebar_sections(&sidebar(), &ViewParams::default(), None);
        assert!(
            html.contains("data-folder=\"refs\" data-parent=\"\""),
            "{html}"
        );
        assert!(
            html.contains("data-folder=\"refs/ui\" data-parent=\"refs\""),
            "{html}"
        );
        // "refs" has a child: it gets the disclosure button; "refs/ui" does not.
        assert_eq!(html.matches("class=\"tree-triangle\"").count(), 1, "{html}");
        assert!(
            html.contains("aria-expanded=\"true\" aria-label=\"Collapse refs\""),
            "the top level opens by default: {html}"
        );
    }

    #[test]
    fn a_deeper_branch_starts_collapsed() {
        let mut data = sidebar();
        data.folders.push(crate::index_sync::SidebarFolder {
            path: "refs/ui/icons".into(),
            count: 4,
        });
        let html = sidebar_sections(&data, &ViewParams::default(), None);
        assert!(
            html.contains("aria-expanded=\"false\" aria-label=\"Expand ui\""),
            "{html}"
        );
    }

    /// W34 audit #6: a label that can truncate always has its full string in
    /// `title` and in the styled tooltip the script reveals.
    #[test]
    fn sidebar_rows_carry_their_full_name_for_the_tooltip() {
        let html = sidebar_sections(&sidebar(), &ViewParams::default(), None);
        assert!(html.contains("title=\"refs/ui\""), "{html}");
        assert!(
            html.contains("<span class=\"sidebar-tip\" aria-hidden=\"true\">Recent"),
            "{html}"
        );
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

    /// The tile's own prefix is a contract: `tests/viewer.rs` reads a page's
    /// image paths back out of it.
    #[test]
    fn a_tile_opens_with_the_attributes_the_pages_parse() {
        let item = ViewItem {
            path: "refs/a.png".into(),
            size: 1,
            mtime_ns: 0,
            added_ns: 0,
            title: None,
            rating: None,
            note_path: None,
        };
        assert!(tile(&item, &ViewParams::default())
            .starts_with("<a class=\"tile\" data-path=\"refs/a.png\""));
    }

    /// W34 audit #3 / DESIGN.md §4.5: the whole name stays reachable while the
    /// caption is hidden, and the aspect hooks belong to the script — the tile
    /// never states a crop it has not measured.
    #[test]
    fn a_tile_carries_the_whole_name_and_no_guessed_aspect() {
        let item = ViewItem {
            path: "refs/猫 & co.png".into(),
            size: 1,
            mtime_ns: 0,
            added_ns: 0,
            title: Some("猫 & co".into()),
            rating: None,
            note_path: None,
        };
        let html = tile(&item, &ViewParams::default());
        assert!(html.contains("title=\"猫 &amp; co\""), "{html}");
        assert!(html.contains("alt=\"猫 &amp; co\""), "{html}");
        for hook in ["data-fit", "data-w", "data-h"] {
            assert!(
                !html.contains(hook),
                "the aspect is measured in the browser, not assumed: {html}"
            );
        }
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
        assert!(shallow.contains("Show subfolders"));
        assert!(shallow.contains("href=\"/?in=refs\""));

        let deep = empty_state(&ViewParams::parse("in=refs"));
        assert!(deep.contains("no images"));
        assert!(!deep.contains("Show subfolders"));

        let search = empty_state(&ViewParams::parse("q=nothing"));
        assert!(search.contains("Clear all filters"));
    }

    /// §4.8: the empty state is a glyph, a title, one sentence and at most
    /// one action — and the two-action case keeps only the way out.
    #[test]
    fn an_empty_state_carries_one_glyph_and_at_most_one_action() {
        let html = empty_state(&ViewParams::parse("in=refs&sub=0&tag=eagle"));
        assert_eq!(html.matches("state-glyph").count(), 1, "{html}");
        assert_eq!(html.matches("state-action").count(), 1, "{html}");
        assert!(!html.contains("Clear all filters"), "{html}");
        let plain = empty_state(&ViewParams::default());
        assert!(plain.contains("state-glyph"));
        assert!(!plain.contains("state-action"), "nothing to do, no action");
    }

    #[test]
    fn an_error_state_is_neutral_text_with_a_red_glyph_only() {
        let html = error_state("Not found", "No image at <that> path.");
        assert!(html.contains("state-error"), "{html}");
        assert!(html.contains("No image at &lt;that&gt; path."), "{html}");
        assert!(!html.contains("<that>"), "the raw form rendered: {html}");
        // The sentence and title are plain text on a neutral surface; the
        // red belongs to the glyph alone, and the stylesheet owns that.
        assert!(!html.contains("danger"), "{html}");
    }

    #[test]
    fn an_empty_library_says_what_to_do_about_it() {
        let html = empty_state(&ViewParams::default());
        assert!(html.contains("The library is empty"));
        assert!(!html.contains("Clear all filters"));
    }

    /// W34 audit #10: the lens is a count, so an empty Recent never claims
    /// anything about days.
    #[test]
    fn an_empty_recent_view_claims_no_window() {
        let html = empty_state(&ViewParams::parse("recent=1"));
        assert!(html.contains("Nothing in Recent"));
        assert!(!html.contains("30 days"), "{html}");
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
        // With chips inside, the capsule cannot be one link around them, so
        // the label is the link and the chips keep their own remove links.
        let scoped = search_pill(Some(&ViewParams::parse("in=refs")));
        assert!(scoped.contains(">Refine</a>"), "{scoped}");
        assert!(scoped.contains("class=\"search-pill-container pill-scoped\""));
        assert!(scoped.contains("chip-remove"));
    }

    /// The chips live inside the capsule, on the line the capsule already
    /// owns — never in a second row of their own (W34 #7, DESIGN.md §4.3).
    #[test]
    fn the_chips_live_inside_the_pill_not_beside_it() {
        let pill = search_pill(Some(&ViewParams::parse("tag=eagle&q=street")));
        let chips_at = pill.find("scope-chips").expect("a chip row");
        let capsule_at = pill.find("search-pill-container").expect("the capsule");
        assert!(
            capsule_at < chips_at,
            "the chip row is inside the capsule: {pill}"
        );
    }

    #[test]
    fn the_result_count_is_singular_for_one_item() {
        assert!(result_count(1).contains("1 item<"));
        assert!(result_count(0).contains("0 items<"));
        assert!(result_count(2).contains("2 items<"));
    }

    /// W37 review, Medium #1: the Recent lens is newest-first by definition, so
    /// its sort menu offered choices the grid ignored. W39 review M1/L1/L2: the
    /// replacement is a focusable label, not a disabled control — a disabled
    /// control cannot be focused, so its `title` is unreachable, and the 107px
    /// visible hint it carried overflowed the 390px phone toolbar. The visible
    /// label is one arrow because the toolbar's five controls only fit at 390px
    /// when it is; the sentence rides in `title` and `aria-label`.
    #[test]
    fn the_recent_order_is_a_focusable_arrow_not_a_wide_hint() {
        let html = sort_control(&ViewParams::parse("recent=1"));
        assert!(
            html.contains("role=\"note\" tabindex=\"0\""),
            "the order is a focusable label, not a disabled control: {html}"
        );
        assert!(
            html.contains(">↓</span>"),
            "the order in force is the one shown: {html}"
        );
        assert!(
            html.contains("title=\"Recent is always the last 200 added, newest first.\""),
            "the label carries the whole sentence: {html}"
        );
        assert!(
            html.contains(
                "aria-label=\"Sort order — Recent is always the last 200 added, newest first.\""
            ),
            "and a screen reader hears it: {html}"
        );
        // M1: the toolbar must fit a 390px phone, which holds only while the
        // sentence stays an attribute and never becomes visible text.
        assert!(
            !html.contains(">Recent is always"),
            "the sentence is not a visible hint: {html}"
        );
        assert!(
            !html.contains(">Added ↓</span>"),
            "and the label is not the wide control it replaced: {html}"
        );
        // L1/L2: nothing disabled, so nothing unfocusable or UA-dimmed.
        assert!(!html.contains("disabled"), "{html}");
        // Nothing else is offered, so nothing else can be promised.
        assert!(!html.contains("Name A→Z"), "{html}");
        assert!(!html.contains("type=\"submit\""), "{html}");
        assert!(
            !html.contains("type=\"hidden\""),
            "and nothing to submit: {html}"
        );
    }

    /// W39 review L3: over a collection the grid keeps the collection's own
    /// member order — the order of the embeds in its note (FORMAT §5) — so
    /// "newest first" would be false there, and the earlier silence is
    /// replaced by the honest name of the order in force: the W46
    /// counterpart of the Recent arrow.
    #[test]
    fn the_order_label_over_a_collection_is_the_note_order_not_the_recent_one() {
        let html = sort_control(&ViewParams::parse(
            "c=collections/featured-picks.md&recent=1",
        ));
        assert!(
            html.contains(">Note order</span>"),
            "the grid is in note order whatever the URL says: {html}"
        );
        assert!(
            !html.contains("newest first"),
            "Recent's order would be false over a collection: {html}"
        );
    }

    /// The sort menu's choices could not say the truth on a collection: the
    /// grid keeps the note's embed order whatever `sort` the URL carries, so
    /// the control states the order in force the way the Recent lens's does
    /// (W37 review, Medium #1; W39 review M1/L1/L2): a focusable label, its
    /// whole sentence in `title` and `aria-label`, only the two short words
    /// visible, never a disabled control and never a menu.
    #[test]
    fn a_collection_states_the_note_order_not_a_menu() {
        for query in [
            "c=browse.md",
            "c=browse.md&sort=name-asc",
            "c=browse.md&dir=asc",
        ] {
            let html = sort_control(&ViewParams::parse(query));
            assert!(
                html.contains("role=\"note\" tabindex=\"0\""),
                "'{query}': a focusable label, not a disabled control: {html}"
            );
            assert!(
                html.contains(">Note order</span>"),
                "'{query}': the order in force is the one shown: {html}"
            );
            assert!(
                html.contains(
                    "aria-label=\"Sort order — A collection is ordered by the embeds in its note.\""
                ),
                "'{query}': a screen reader hears the whole sentence: {html}"
            );
            assert!(
                !html.contains("<option"),
                "'{query}': a menu would promise what the grid does not keep: {html}"
            );
            assert!(!html.contains("disabled"), "'{query}': {html}");
        }
    }

    /// W37 review, Medium #1 (W39 review M1/L1/L2): the Recent lens is
    /// newest-first by definition, so its sort menu offered choices the grid
    /// ignored. W39 M1/L1/L2 considerations made the replacement a focusable
    /// label; see `the_recent_order_is_a_focusable_arrow_not_a_wide_hint`.
    #[test]
    fn the_recent_sort_control_shows_the_forced_order_not_the_url() {
        let html = sort_control(&ViewParams::parse("recent=1&sort=name-asc"));
        assert!(
            html.contains(">↓</span>"),
            "the grid is Added-descending whatever the URL says: {html}"
        );
        assert!(!html.contains(">Name A→Z<"), "{html}");
    }

    #[test]
    fn every_other_view_keeps_the_real_sort_menu() {
        for query in ["", "tag=nature", "in=refs", "untagged=1"] {
            let html = sort_control(&ViewParams::parse(query));
            assert!(
                html.contains(
                    "<select class=\"sort-select\" name=\"sort\" aria-label=\"Sort order\">"
                ),
                "'{query}' keeps the menu: {html}"
            );
            assert!(!html.contains("disabled"), "'{query}': {html}");
            assert!(html.contains("Name A→Z"), "'{query}': {html}");
        }
    }

    /// W37 review, Medium #2: a page past the end is not an empty view, so it
    /// must not borrow the empty view's words.
    #[test]
    fn a_page_past_the_end_states_the_size_and_the_way_back() {
        let html = past_end_state(&ViewParams::parse("recent=1&p=3"), 200);
        assert!(html.contains("<h3>Nothing on this page</h3>"), "{html}");
        assert!(
            html.contains("This view has 200 items; this page is past the end of it."),
            "{html}"
        );
        assert!(
            html.contains("href=\"/?recent=1\">Back to the first page</a>"),
            "the way back to the images: {html}"
        );
        // Never the empty library's words, and never a bare page.
        assert!(!html.contains("The library is empty"), "{html}");
    }

    #[test]
    fn a_page_past_the_end_of_a_one_item_view_says_item() {
        assert!(past_end_state(&ViewParams::parse("p=2"), 1).contains("This view has 1 item;"));
    }
}
