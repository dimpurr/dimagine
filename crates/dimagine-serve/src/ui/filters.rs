//! The filter panel beside the grid (W47).
//!
//! K27 motion 3 settled the shape: the chips in the search capsule are the
//! visible state, the query string behind them is the truth, and the facet list
//! with counts lives in a sheet that opens on demand. This is that sheet. It
//! differs from the sidebar in one way that decides everything else: the sidebar
//! *navigates* (a row is a fresh view of one folder, one collection, one tag),
//! while the panel *narrows* (a row is the view already on screen, with one more
//! filter on it or one fewer). So every count here answers "how many images
//! would this show beside the filters you already have", which is the question a
//! person is actually asking when the grid is already down to 14 pictures.
//!
//! Three rules hold across all four groups:
//!
//! · Every row is a link to the same view with that one filter toggled, on or
//!   off. The panel therefore works with scripting off, and every row a reader
//!   can use is a real tab stop.
//! · A value whose count is zero is dimmed and is *not* a link. Its own number
//!   already says what clicking it would produce — the empty view — so offering
//!   it would only offer a dead end. This is the one convention, applied the same
//!   way in every group; a value that is currently chosen is never hidden by it,
//!   because the row that is on is how you turn it off.
//! · A long list is capped, and the cap lifts through a link (`more=<facet>`),
//!   not through a click handler.

use dimagine_index::FacetCounts;

use crate::index_sync::{recent_label, SidebarData};
use crate::ui::escape_html;
use crate::view_query::ViewParams;

/// How many rows one facet list shows before the rest of it becomes a link.
/// Eight is a column a person reads at a glance; the panel is beside a grid of
/// pictures, and K27's own warning was the two-row facet wall.
const FACET_ROWS: usize = 8;

/// How many rows a list the reader has opened still stops at. A library with
/// two thousand tags is not made useful by a longer column: the pages that list
/// a whole universe (`/search`, `/folders`, `/collections`) already exist, and
/// the link below the list goes there rather than pretending otherwise.
const FACET_ROWS_OPEN: usize = 100;

/// The facet list names this module renders, each paired with the page that
/// lists that universe whole. `dimagine_view_query::FACET_LISTS` is the same
/// list of names the query string accepts.
const TAGS: &str = "tags";
const FOLDERS: &str = "folders";
const COLLECTIONS: &str = "collections";

/// A funnel: the panel's own glyph, the same one a valueless filter chip draws
/// (DESIGN.md §4.3), because this is where those filters come from.
const FILTER_GLYPH: &str = r##"<svg class="filter-glyph" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M2.4 3.6h11.2l-4.3 4.8v4.2l-2.6-1.5V8.4z"/></svg>"##;

/// One panel row, in the state it is drawn in.
struct Row {
    /// What the row says. A folder says its last segment (a panel column cannot
    /// hold a path), with the whole value in the tooltip and the `title`.
    label: String,
    /// The whole value, for the tooltip a truncating label always needs
    /// (DESIGN.md §4.1: never truncate without a way to read the string whole).
    tooltip: String,
    /// The count beside it: what this value would show beside the filters on.
    count: u64,
    /// Whether this filter is on right now.
    active: bool,
    /// The same view with this one filter toggled. Empty for a row that cannot
    /// be clicked, which is only ever the dimmed zero-count row.
    href: String,
}

/// The panel, in the column beside the grid (desktop) or above it (phone, where
/// it is folded behind its own heading until it is opened).
///
/// `counts` comes from one pass of the index's facet queries; `sidebar` supplies
/// the *values* — every tag, folder and collection the library has — so a value
/// the current filters leave nothing for is still on the list, dimmed, rather
/// than silently missing (which would read as the library having changed).
///
/// **The lists are rendered once and mounted twice**, in a `<details>` (the
/// phone's fold) and in a plain `<div>` (the desktop's column), and the
/// stylesheet shows exactly one of the two at a width. One mount cannot do both:
/// a phone needs the region closed until it is tapped, which is what a
/// `<details>` is for, while a desktop needs it open whatever the `open`
/// attribute says — and *hiding* the content of an open `<details>` is easy
/// (`display` reaches its children, which are rendered) while *showing* the
/// content of a closed one is not: a closed `<details>` does not project its
/// children at all, so author `display` on them has nothing to act on. Chrome
/// 155 measures it this way, and the property that could override it
/// (`::details-content`) is not in every browser this viewer is read in. The two
/// mounts are therefore the honest shape: each holds the same string, only one is
/// ever rendered, so the reader sees one panel, reaches one set of links, and
/// neither state needs a script.
pub fn filter_panel(sidebar: &SidebarData, counts: &FacetCounts, params: &ViewParams) -> String {
    let active = params.active_filter_count();
    let badge = if active == 0 {
        String::new()
    } else {
        // A collapsed panel on a phone still has to say that the grid is
        // narrowed, and how many times over.
        format!("<span class=\"filter-fold-count\">{active}</span>")
    };

    // The shared body: what the numbers mean, the four groups, and the way out.
    let mut body = String::from(
        "<p class=\"filter-hint\">Each number is what that value would show \
         beside the filters already on.</p>",
    );
    body.push_str(&group("Views", None, params, &lens_rows(counts, params)));
    body.push_str(&group(
        "Folders",
        Some(FOLDERS),
        params,
        &folder_rows(sidebar, counts, params),
    ));
    body.push_str(&group(
        "Collections",
        Some(COLLECTIONS),
        params,
        &collection_rows(sidebar, counts, params),
    ));
    body.push_str(&group(
        "Tags",
        Some(TAGS),
        params,
        &tag_rows(sidebar, counts, params),
    ));
    if active > 1 {
        // Only from the second filter on. With one filter the chip beside the
        // search field already is the way out, and W43 settled that a state does
        // not repeat a way out the chips offer (the empty-folder page is the case
        // that test guards). Clearing *all* of several filters is the one thing
        // no single chip can do, so that is the only thing this link promises —
        // and only that: the sort, the tile size and the page the reader chose
        // are not filters, so they stay.
        body.push_str(&format!(
            "<a class=\"filter-clear\" href=\"{}\">Clear all filters</a>",
            escape_html(&params.without_filters().to_url("/")),
        ));
    }

    // The heading belongs to the column and the button to the fold, and since
    // only one of the two mounts renders, the count of filters on is written in
    // whichever one the reader is looking at.
    format!(
        "<aside class=\"filter-aside\" aria-label=\"Filters\">\
         <details class=\"filter-fold\">\
         <summary class=\"filter-fold-summary\">{FILTER_GLYPH}\
         <span class=\"filter-fold-title\">Filters</span>{badge}</summary>\
         <div class=\"filter-fold-body\">{body}</div></details>\
         <div class=\"filter-static\"><h2 class=\"filter-title\">Filters{badge}</h2>{body}</div>\
         </aside>"
    )
}

/// The two lenses. They are the viewer's other two filters, and the reason they
/// belong in the panel rather than only in the sidebar is that they combine: the
/// newest 200 *of what is on screen* is a different, and more useful, question
/// than the newest 200 of everything.
fn lens_rows(counts: &FacetCounts, params: &ViewParams) -> Vec<Row> {
    vec![
        Row {
            label: "Recent".to_owned(),
            tooltip: recent_label(),
            count: counts.recent,
            active: params.recent,
            href: params.with_recent(!params.recent).to_url("/"),
        },
        Row {
            label: "Untagged".to_owned(),
            tooltip: "Untagged".to_owned(),
            count: counts.untagged,
            active: params.untagged,
            href: params.with_untagged(!params.untagged).to_url("/"),
        },
    ]
}

/// The folder rows: every folder the library has, each with the images the other
/// filters leave inside it and its subfolders — or inside it alone, when `sub=0`
/// is on, which is what the rows' own links show. Ranked by what would show, not
/// by the tree — the tree is what the sidebar is for, and a parent's count always
/// contains its children's, which a ranked list says more honestly here.
fn folder_rows(sidebar: &SidebarData, counts: &FacetCounts, params: &ViewParams) -> Vec<Row> {
    sidebar
        .folders
        .iter()
        .map(|folder| {
            let active = params.folder.as_deref() == Some(folder.path.as_str());
            Row {
                label: leaf_name(&folder.path),
                tooltip: folder.path.clone(),
                count: counts.folders.get(&folder.path).copied().unwrap_or(0),
                active,
                href: params
                    .with_folder(if active {
                        None
                    } else {
                        Some(folder.path.clone())
                    })
                    .to_url("/"),
            }
        })
        .collect()
}

fn collection_rows(sidebar: &SidebarData, counts: &FacetCounts, params: &ViewParams) -> Vec<Row> {
    sidebar
        .collections
        .iter()
        .map(|collection| {
            let active = params.collection.as_deref() == Some(collection.path.as_str());
            Row {
                label: if collection.title.is_empty() {
                    leaf_name(&collection.path)
                } else {
                    collection.title.clone()
                },
                tooltip: collection.path.clone(),
                count: counts
                    .collections
                    .get(&collection.path)
                    .copied()
                    .unwrap_or(0),
                active,
                href: params
                    .with_collection(if active {
                        None
                    } else {
                        Some(collection.path.clone())
                    })
                    .to_url("/"),
            }
        })
        .collect()
}

/// The tag rows. A tag is the one repeatable filter, so a row's count
/// keeps the tags already on: the count for `beta` beside `tag=alpha`
/// is what `tag=alpha&tag=beta` shows, which is the row's own link.
/// The chosen tag's own row counts the view as it stands — where the
/// reader is; its link is the way out, the view without that one tag.
fn tag_rows(sidebar: &SidebarData, counts: &FacetCounts, params: &ViewParams) -> Vec<Row> {
    sidebar
        .tags
        .iter()
        .map(|tag| {
            let active = tag_is_active(params, &tag.tag);
            Row {
                label: tag.tag.clone(),
                tooltip: tag.tag.clone(),
                count: counts.tags.get(&tag.tag).copied().unwrap_or(0),
                active,
                href: if active {
                    params.without_tag(&tag.tag)
                } else {
                    params.with_tag(tag.tag.clone())
                }
                .to_url("/"),
            }
        })
        .collect()
}

/// Whether a tag is one of the view's tags. The index folds a tag's case when it
/// stores and matches it (`dimagine_index::searchable`), so the row is chosen for
/// the tag the URL means, not for a byte-identical spelling of it.
fn tag_is_active(params: &ViewParams, tag: &str) -> bool {
    let folded = dimagine_index::searchable(tag);
    params
        .tags
        .iter()
        .any(|active| dimagine_index::searchable(active) == folded)
}

/// One facet group: its title, the rows worth showing, and the link that opens
/// the rest of the list.
///
/// Rows the reader has already chosen always survive the cap: a filter that is
/// on never disappears from the panel because a third one was added, or the only
/// way left to remove it is the chip.
fn group(title: &str, facet: Option<&str>, params: &ViewParams, rows: &[Row]) -> String {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .active
            .cmp(&left.active)
            .then(right.count.cmp(&left.count))
            .then_with(|| left.label.cmp(&right.label))
    });

    let opened = facet.is_some_and(|facet| params.shows_more_of(facet));
    let shown = if opened { FACET_ROWS_OPEN } else { FACET_ROWS };
    let visible = ordered.iter().take(shown).copied().collect::<Vec<_>>();
    let hidden = ordered.len().saturating_sub(visible.len());

    let mut out = format!(
        "<section class=\"filter-group\"><h3 class=\"filter-group-title\">{}</h3>",
        escape_html(title)
    );
    if visible.is_empty() {
        out.push_str("<p class=\"filter-group-empty\">None in this library.</p>");
    } else {
        out.push_str("<ul class=\"filter-rows\">");
        for row in visible {
            out.push_str(&row_html(row));
        }
        out.push_str("</ul>");
    }

    if hidden > 0 && !opened {
        // The cap lifts through the query string, so the longer list needs no
        // script and a reader can bookmark the opened list.
        let facet = facet.unwrap_or_default();
        out.push_str(&format!(
            "<a class=\"filter-more\" href=\"{}\">Show {hidden} more {}</a>",
            escape_html(&params.with_more_of(facet).to_url("/")),
            escape_html(title),
        ));
    } else if opened && hidden > 0 {
        // The way back to the short list is only on the page when the longer list
        // was actually longer; and if even that had to stop, the page holding the
        // whole universe is named rather than a longer column being promised.
        let facet = facet.unwrap_or_default();
        out.push_str(&format!(
            "<a class=\"filter-more\" href=\"{}\">Show fewer</a>",
            escape_html(&params.without_more_of(facet).to_url("/")),
        ));
        out.push_str(&format!(
            "<p class=\"filter-more-end\">{hidden} more — all of them are on \
             <a href=\"{}\">the {} page</a>.</p>",
            escape_html(&universe_url(facet)),
            escape_html(&title.to_lowercase()),
        ));
    }
    out.push_str("</section>");
    out
}

/// The page that lists one facet's whole universe.
fn universe_url(facet: &str) -> String {
    match facet {
        FOLDERS => "/folders".to_owned(),
        COLLECTIONS => "/collections".to_owned(),
        _ => "/search".to_owned(),
    }
}

/// One row, in whichever of its two states it is in.
fn row_html(row: &Row) -> String {
    let (label, tooltip) = (escape_html(&row.label), escape_html(&row.tooltip));
    if !row.active && row.count == 0 {
        // Dimmed, and no link: the number already answered the question.
        return format!(
            "<li><span class=\"filter-row is-empty\" title=\"{tooltip}\">\
             <span class=\"filter-row-label\">{label}</span>\
             <span class=\"filter-row-count\">{}</span></span></li>",
            row.count
        );
    }
    format!(
        "<li><a class=\"filter-row{}\" href=\"{}\"{} title=\"{tooltip}\">\
         <span class=\"filter-row-label\">{label}</span>\
         <span class=\"filter-row-count\">{}</span></a></li>",
        if row.active { " is-active" } else { "" },
        escape_html(&row.href),
        // A link that toggles a filter has no "current" within the list the way
        // a navigation link does; `aria-current` is still the closest honest
        // answer for assistive tech, and the chip row says it in words.
        if row.active {
            " aria-current=\"true\""
        } else {
            ""
        },
        row.count,
    )
}

fn leaf_name(path: &str) -> String {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// The grid and the panel, in the one layout that makes the panel a side column
/// on a desktop and a fold above the grid everywhere else.
pub fn with_panel(panel: String, results: String) -> String {
    format!(
        "<div class=\"refine-layout\">{panel}<div class=\"refine-results\">{results}</div></div>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_sync::{SidebarCollection, SidebarFolder, SidebarTag};
    use std::collections::BTreeMap;

    /// A library of twelve images: two folders (one inside the other), two
    /// collections, and a tag the filters on leave nothing for.
    fn sidebar() -> SidebarData {
        SidebarData {
            folders: vec![
                SidebarFolder {
                    path: "refs".to_owned(),
                    count: 7,
                },
                SidebarFolder {
                    path: "refs/ui".to_owned(),
                    count: 3,
                },
                SidebarFolder {
                    path: "shots".to_owned(),
                    count: 2,
                },
            ],
            collections: vec![
                SidebarCollection {
                    path: "guide.md".to_owned(),
                    title: "Guide".to_owned(),
                    count: 4,
                },
                SidebarCollection {
                    path: "odd.md".to_owned(),
                    title: String::new(),
                    count: 1,
                },
            ],
            tags: ["nature", "ui", "wide", "gone"]
                .iter()
                .map(|tag| SidebarTag {
                    tag: (*tag).to_owned(),
                    count: 0,
                })
                .collect(),
            total: 12,
            untagged: 2,
            recent: 12,
        }
    }

    /// The counts for the view on screen. A value the filters leave nothing for
    /// is **absent** from these maps, which is how the index reports a zero
    /// ([`FacetCounts`]) — the panel has to render that absence as a dimmed row.
    fn counts() -> FacetCounts {
        let map = |pairs: &[(&str, u64)]| {
            pairs
                .iter()
                .map(|(value, count)| ((*value).to_owned(), *count))
                .collect::<BTreeMap<String, u64>>()
        };
        FacetCounts {
            tags: map(&[("nature", 5), ("ui", 3), ("wide", 1)]),
            folders: map(&[("refs", 7), ("refs/ui", 3), ("shots", 2)]),
            collections: map(&[("guide.md", 4)]),
            untagged: 2,
            recent: 12,
        }
    }

    fn panel_of(params: &ViewParams) -> String {
        filter_panel(&sidebar(), &counts(), params)
    }

    /// The HTML of one row, given the class+href string that opens its element.
    fn row_slice(panel: &str, needle: &str) -> String {
        let start = panel
            .find(needle)
            .unwrap_or_else(|| panic!("no {needle} in\n{panel}"));
        panel[start..]
            .split_once("</li>")
            .map(|(head, _)| head.to_owned())
            .unwrap_or_else(|| panel[start..].to_owned())
    }

    #[test]
    fn every_row_carries_the_number_that_would_show_if_it_were_clicked() {
        let panel = panel_of(&ViewParams::default());
        assert!(
            panel.contains(
                "Each number is what that value would show beside the filters already on."
            ),
            "the panel has to say what its numbers mean: {panel}"
        );
        // nature 5 · ui 3 · wide 1, and the folder tree's 7.
        for (value, count) in [("nature", 5), ("ui", 3), ("wide", 1)] {
            assert!(
                panel.contains(&format!(
                    "<span class=\"filter-row-label\">{value}</span>\
                     <span class=\"filter-row-count\">{count}</span>"
                )),
                "{value} should be counted {count} in {panel}"
            );
        }
        assert!(panel.contains(
            "<span class=\"filter-row-label\">refs</span><span class=\"filter-row-count\">7</span>"
        ));
        assert!(panel.contains(">Recent</span><span class=\"filter-row-count\">12</span>"));
        assert!(panel.contains(">Guide</span><span class=\"filter-row-count\">4</span>"));
    }

    #[test]
    fn a_value_the_filters_leave_nothing_for_is_dimmed_and_offers_no_link() {
        let panel = panel_of(&ViewParams::default());
        let row = row_slice(&panel, "<span class=\"filter-row is-empty\" title=\"gone\"");
        assert!(row.contains(">gone</span>"), "{row}");
        assert!(
            row.contains("<span class=\"filter-row-count\">0</span>"),
            "{row}"
        );
        assert!(!row.contains("<a"), "a zero row must not be a link: {row}");
        assert!(!panel.contains("href=\"/?tag=gone\""), "{panel}");
    }

    /// The labels of the dimmed rows, in the page's order.
    fn dimmed_labels(panel: &str) -> Vec<String> {
        panel
            .split("<span class=\"filter-row is-empty\"")
            .skip(1)
            .map(|cell| {
                let cell = &cell[..cell.find("</li>").unwrap_or(cell.len())];
                let (_, label) = cell
                    .split_once("<span class=\"filter-row-label\">")
                    .expect("a dimmed row carries its label");
                label[..label.find('<').expect("a closed label span")].to_owned()
            })
            .collect()
    }

    /// The rule a dimmed row is supposed to express: it is dimmed because the
    /// view that value would open has nothing in it — and not for any other
    /// reason. Beside the grid the integration walk can only see a number
    /// printed as `0`, so a value wrongly dimmed while the map still holds its
    /// count (RW47 High-1's shape, `refs` dimmed at 0 with ten images behind
    /// its link) passes there; here the map is in the same room as the row.
    #[test]
    fn a_row_is_dimmed_because_the_counts_have_nothing_for_it() {
        let sidebar = sidebar();
        let counts = counts();
        let mut expected = sidebar
            .tags
            .iter()
            .filter(|tag| !counts.tags.contains_key(&tag.tag))
            .map(|tag| tag.tag.clone())
            .chain(
                sidebar
                    .folders
                    .iter()
                    .filter(|folder| !counts.folders.contains_key(&folder.path))
                    .map(|folder| leaf_name(&folder.path)),
            )
            .chain(
                sidebar
                    .collections
                    .iter()
                    .filter(|collection| !counts.collections.contains_key(&collection.path))
                    .map(|collection| {
                        if collection.title.is_empty() {
                            leaf_name(&collection.path)
                        } else {
                            collection.title.clone()
                        }
                    }),
            )
            .collect::<Vec<_>>();
        if counts.recent == 0 {
            expected.push("Recent".to_owned());
        }
        if counts.untagged == 0 {
            expected.push("Untagged".to_owned());
        }
        let mut dimmed = dimmed_labels(&panel_of(&ViewParams::default()));
        // The panel body is written twice (the phone's fold and the desktop
        // column, `filter_panel`), so each label arrives twice.
        expected.sort();
        expected.dedup();
        dimmed.sort();
        dimmed.dedup();
        assert_eq!(dimmed, expected, "the panel dimmed the wrong rows");

        // The same rule on a view with nothing left for anything: the filter
        // that is on is the way out and is never dimmed (W47), so it is the
        // only row still a link — and every other row, in every group, has to
        // say so rather than keep its old number or its old link.
        let empty = filter_panel(
            &sidebar,
            &FacetCounts::default(),
            &ViewParams::default().with_tag("gone".to_owned()),
        );
        let mut everything = sidebar
            .tags
            .iter()
            .filter(|tag| tag.tag != "gone")
            .map(|tag| tag.tag.clone())
            .chain(sidebar.folders.iter().map(|folder| leaf_name(&folder.path)))
            .chain(sidebar.collections.iter().map(|collection| {
                if collection.title.is_empty() {
                    leaf_name(&collection.path)
                } else {
                    collection.title.clone()
                }
            }))
            .collect::<Vec<_>>();
        everything.extend(["Recent".to_owned(), "Untagged".to_owned()]);
        let mut dimmed = dimmed_labels(&empty);
        everything.sort();
        everything.dedup();
        dimmed.sort();
        dimmed.dedup();
        assert_eq!(
            dimmed, everything,
            "a view with nothing left dims all of it"
        );
        assert!(
            empty.contains("class=\"filter-row is-active\""),
            "the filter that is on keeps its link: {empty}"
        );
    }

    #[test]
    fn a_filter_that_is_on_is_never_dimmed_even_when_it_reaches_zero() {
        // `tag=gone` leaves no images, so no folder has anything left either —
        // yet the folder row is the only in-panel way to let go of `refs`.
        let params = ViewParams::default()
            .with_folder(Some("refs".to_owned()))
            .with_tag("gone".to_owned());
        let panel = filter_panel(&sidebar(), &FacetCounts::default(), &params);
        let row = row_slice(&panel, "class=\"filter-row is-active\" href=\"/?tag=gone\"");
        assert!(row.contains(">refs</span>"), "{row}");
        assert!(
            row.contains("<span class=\"filter-row-count\">0</span>"),
            "{row}"
        );
        assert!(row.contains("aria-current=\"true\""), "{row}");
    }

    #[test]
    fn every_row_links_to_the_same_view_with_one_filter_changed() {
        let params = ViewParams::default()
            .with_folder(Some("refs".to_owned()))
            .with_tag("nature".to_owned());
        let panel = panel_of(&params);
        // Adding a tag keeps the folder and the tag already on.
        assert!(
            panel.contains("href=\"/?in=refs&amp;tag=nature&amp;tag=ui\""),
            "{panel}"
        );
        // The tag that is on links to the view without it, folder still on.
        assert!(
            panel.contains("class=\"filter-row is-active\" href=\"/?in=refs\""),
            "{panel}"
        );
        // And the folder that is on lets go of itself, tag still on.
        assert!(
            panel.contains("class=\"filter-row is-active\" href=\"/?tag=nature\""),
            "{panel}"
        );
        // Lenses round-trip through their own parameters.
        assert!(
            panel.contains("href=\"/?in=refs&amp;tag=nature&amp;untagged=1\""),
            "{panel}"
        );
        assert!(
            panel.contains("href=\"/?in=refs&amp;tag=nature&amp;recent=1\""),
            "{panel}"
        );
        // A collection row names the note, percent-encoded.
        assert!(
            panel.contains("href=\"/?in=refs&amp;c=guide.md&amp;tag=nature\""),
            "{panel}"
        );
    }

    #[test]
    fn the_panel_acts_through_links_alone() {
        let panel = panel_of(&ViewParams::default());
        assert!(!panel.contains("<script"), "{panel}");
        assert!(!panel.contains("onclick"), "{panel}");
        assert!(!panel.contains("javascript:"), "{panel}");
        // The fold is a real `<details>`, so opening it needs no script either.
        assert!(panel.contains("<details class=\"filter-fold\">"), "{panel}");
        assert!(
            panel.contains("<summary class=\"filter-fold-summary\">"),
            "{panel}"
        );
    }

    #[test]
    fn a_long_list_opens_through_the_url_and_a_chosen_value_never_falls_off_it() {
        let mut sidebar = sidebar();
        sidebar.tags = (0..20)
            .map(|index| SidebarTag {
                tag: format!("tag-{index:02}"),
                count: 0,
            })
            .collect();
        sidebar.tags.push(SidebarTag {
            tag: "picked".to_owned(),
            count: 0,
        });
        let params = ViewParams::default().with_tag("picked".to_owned());
        let panel = filter_panel(&sidebar, &FacetCounts::default(), &params);

        // 21 values, 8 shown, the chosen one among them even though it counts 0.
        assert!(
            panel.contains("class=\"filter-row is-active\" href=\"/\""),
            "{panel}"
        );
        assert!(
            panel.contains(">Show 13 more Tags</a>"),
            "the cap lifts through a link: {panel}"
        );
        assert!(panel.contains("more=tags"), "{panel}");

        // Opened, nothing is left behind, and the link that would fold it back
        // away is not offered: it would change nothing on the page.
        let opened = filter_panel(
            &sidebar,
            &FacetCounts::default(),
            &params.with_more_of(TAGS),
        );
        assert!(!opened.contains("Show 13 more"), "still-capped: {opened}");
        assert!(!opened.contains("Show fewer"), "nothing hidden: {opened}");
        assert!(opened.contains(">tag-19</span>"), "row lost: {opened}");
        // The chosen row is still there, and letting go of the tag keeps the
        // reader in the list they opened rather than dropping them back to 8.
        assert!(
            opened.contains("class=\"filter-row is-active\" href=\"/?more=tags\""),
            "chosen row gone: {opened}"
        );
        assert!(!opened.contains("the tags page"), "over-promised: {opened}");
    }

    #[test]
    fn a_list_too_long_even_opened_names_the_page_that_holds_it_all() {
        let mut sidebar = sidebar();
        sidebar.tags = (0..250)
            .map(|index| SidebarTag {
                tag: format!("tag-{index:04}"),
                count: 0,
            })
            .collect();
        let params = ViewParams::default().with_more_of(TAGS);
        let panel = filter_panel(&sidebar, &FacetCounts::default(), &params);
        assert!(panel.contains("150 more — all of them are on"), "{panel}");
        assert!(
            panel.contains("<a href=\"/search\">the tags page</a>"),
            "{panel}"
        );
        assert!(
            !panel.contains("tag-0100</span>"),
            "the opened cap still holds"
        );
    }

    #[test]
    fn a_folder_shows_its_leaf_and_keeps_the_whole_path_one_hover_away() {
        let panel = panel_of(&ViewParams::default());
        assert!(
            panel.contains("title=\"refs/ui\"><span class=\"filter-row-label\">ui</span>"),
            "{panel}"
        );
    }

    #[test]
    fn a_collection_without_a_title_says_what_its_note_is_called() {
        let panel = panel_of(&ViewParams::default());
        assert!(panel.contains(">odd.md</span>"), "{panel}");
        assert!(panel.contains("title=\"odd.md\""), "{panel}");
    }

    #[test]
    fn facet_values_are_escaped_before_they_reach_the_markup() {
        let mut sidebar = sidebar();
        sidebar.tags.push(SidebarTag {
            tag: "\"><b>evil</b>".to_owned(),
            count: 0,
        });
        let mut counts = counts();
        counts.tags.insert("\"><b>evil</b>".to_owned(), 4);
        let panel = filter_panel(&sidebar, &counts, &ViewParams::default());
        assert!(!panel.contains("\"><b>evil"), "{panel}");
        assert!(panel.contains("&lt;b&gt;evil&lt;/b&gt;"), "{panel}");
        // And the href carries it percent-encoded, not raw.
        assert!(
            panel.contains("href=\"/?tag=%22%3E%3Cb%3Eevil%3C/b%3E\""),
            "{panel}"
        );
    }

    #[test]
    fn the_number_of_filters_on_is_where_either_size_would_look_for_it() {
        let plain = panel_of(&ViewParams::default());
        assert!(!plain.contains("filter-fold-count"), "{plain}");
        assert!(!plain.contains("Clear all filters"), "{plain}");

        let narrowed = panel_of(
            &ViewParams::default()
                .with_folder(Some("refs".to_owned()))
                .with_untagged(true),
        );
        // Once in the desktop heading, once in the phone's fold, which is the
        // only part of the panel a collapsed phone can see.
        assert_eq!(
            narrowed.matches("filter-fold-count").count(),
            2,
            "{narrowed}"
        );
        assert!(
            narrowed.contains(
                "<h2 class=\"filter-title\">Filters<span class=\"filter-fold-count\">2</span></h2>"
            ),
            "{narrowed}"
        );
        assert!(
            narrowed.contains("<a class=\"filter-clear\" href=\"/\">Clear all filters</a>"),
            "{narrowed}"
        );
    }

    #[test]
    fn the_lists_are_written_once_and_mounted_twice() {
        // The fold and the column hold the same string, so they can never disagree.
        let panel = panel_of(&ViewParams::default());
        let fold = between(&panel, "<div class=\"filter-fold-body\">");
        let column = between(&panel, "<div class=\"filter-static\">");
        let column = column[column.find("<p class=\"filter-hint\">").unwrap()..].to_owned();
        assert!(!fold.is_empty());
        assert_eq!(fold, column, "the two mounts hold different lists");
        // And exactly one of them is on screen at a width, which is the
        // stylesheet's job and this the markup's.
        assert!(
            panel.starts_with("<aside class=\"filter-aside\""),
            "{panel}"
        );
        assert!(panel.contains("<details class=\"filter-fold\">"), "{panel}");
        assert!(panel.contains("<div class=\"filter-static\">"), "{panel}");
    }

    /// The body of one element, up to its matching close of the same tag — enough
    /// for these two mounts, whose bodies hold no nested `<div>`.
    fn between(haystack: &str, open: &str) -> String {
        let after = &haystack[haystack.find(open).unwrap() + open.len()..];
        after[..after.find("</div>").unwrap()].to_owned()
    }

    #[test]
    fn the_groups_follow_the_sidebar_in_the_sidebars_own_order() {
        let panel = panel_of(&ViewParams::default());
        let titles: Vec<&str> = panel
            .split("<h3 class=\"filter-group-title\">")
            .skip(1)
            .map(|tail| tail.split_once('<').unwrap().0)
            .take(4)
            .collect();
        assert_eq!(titles, vec!["Views", "Folders", "Collections", "Tags"]);
        // Both mounts carry the same four groups, in the same order.
        assert_eq!(
            panel.matches("<h3 class=\"filter-group-title\">").count(),
            8
        );
        // Views never has a list to open, so no cap link belongs to it.
        assert!(!panel.contains("more=views"), "{panel}");
    }

    #[test]
    fn an_empty_library_says_so_per_group_instead_of_listing_nothing_quietly() {
        let empty = SidebarData {
            folders: Vec::new(),
            collections: Vec::new(),
            tags: Vec::new(),
            total: 0,
            untagged: 0,
            recent: 0,
        };
        let panel = filter_panel(&empty, &FacetCounts::default(), &ViewParams::default());
        // Three groups say it, twice over: the fold and the column.
        assert_eq!(
            panel.matches("None in this library.").count(),
            6,
            "folders, collections and tags, in each mount: {panel}"
        );
        assert!(
            panel.contains(">Recent</span><span class=\"filter-row-count\">0</span>"),
            "{panel}"
        );
    }

    #[test]
    fn the_panel_and_the_grid_share_one_layout_element() {
        let wrapped = with_panel("<i>p</i>".to_owned(), "<i>g</i>".to_owned());
        assert_eq!(
            wrapped,
            "<div class=\"refine-layout\"><i>p</i><div class=\"refine-results\"><i>g</i>\
             </div></div>"
        );
    }
}
