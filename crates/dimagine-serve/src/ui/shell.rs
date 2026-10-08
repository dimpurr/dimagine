//! The page frame: sidebar, top bar, content column, inspector, navigation.
//!
//! One frame serves every width (spec §3): CSS decides what is visible, so the
//! phone shows the bottom tab bar, the tablet an icon rail, and the desktop a
//! pinned sidebar and an inspector. Nothing here knows about the request.

use crate::assets::{css_url, js_url};
use crate::index_sync::SidebarData;
use crate::ui::escape_html;
use crate::view_query::{Direction, ViewParams, ViewSort};

/// W43: the theme boot, inlined into `<head>` so a stored choice paints
/// before the first frame — `app.js` is `defer`red and can arrive after the
/// browser has already painted the system theme. It sets the same
/// `data-theme` attribute `app.js` sets, from the same `dimagine.theme` key,
/// so the two agree without sharing code.
const THEME_BOOT: &str = "<script>try{var t=localStorage.getItem('dimagine.theme');if(t==='light'||t==='dark')document.documentElement.setAttribute('data-theme',t)}catch(e){}</script>";

/// Where a page sits in the viewer's four destinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Library,
    Folders,
    Collections,
    Search,
}

impl Destination {
    /// The URL of this destination.
    pub fn path(self) -> &'static str {
        match self {
            Self::Library => "/",
            Self::Folders => "/folders",
            Self::Collections => "/collections",
            Self::Search => "/search",
        }
    }

    /// The word on the tab.
    pub fn label(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Folders => "Folders",
            Self::Collections => "Collections",
            Self::Search => "Search",
        }
    }

    /// The glyph beside the label.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Library => ICON_LIBRARY,
            Self::Folders => ICON_FOLDERS,
            Self::Collections => ICON_COLLECTIONS,
            Self::Search => ICON_SEARCH,
        }
    }
}

/// A 2x2 grid: the library is a grid of pictures, not a page of text.
const ICON_LIBRARY: &str = r##"<svg class="tab-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><rect x="1.5" y="1.5" width="5.5" height="5.5" rx="1"/><rect x="9" y="1.5" width="5.5" height="5.5" rx="1"/><rect x="1.5" y="9" width="5.5" height="5.5" rx="1"/><rect x="9" y="9" width="5.5" height="5.5" rx="1"/></svg>"##;
/// A folder tab over a folder body.
const ICON_FOLDERS: &str = r##"<svg class="tab-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="M1.5 3.5h4l1.5 2h7.5v7.5a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z"/></svg>"##;
/// Two stacked sheets: a collection is a note holding other images.
const ICON_COLLECTIONS: &str = r##"<svg class="tab-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true" focusable="false"><rect x="4" y="2.5" width="9.5" height="11" rx="1"/><path d="M4 2.5H3a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h8"/></svg>"##;
/// A magnifier over a note.
const ICON_SEARCH: &str = r##"<svg class="tab-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true" focusable="false"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14.5 14.5" stroke-linecap="round"/></svg>"##;

/// What a page puts inside the frame.
pub struct Frame<'a> {
    /// Browser title, without the product name.
    pub title: &'a str,
    /// Which navigation entry is active.
    pub active: Destination,
    /// Sidebar data; `None` on a page without one, such as a sign-in page.
    pub sidebar: Option<&'a SidebarData>,
    /// The active view, for the top bar's chips, sort and size controls.
    pub view: Option<&'a ViewParams>,
    /// How many items match and how many are on screen, for the result count.
    pub result: Option<(u64, u64)>,
    /// Notices about query parameters that were ignored.
    pub notices: &'a [String],
    /// The no-login banner text, when the server runs
    /// `--auth none`: every page says there is no login, so
    /// none can be mistaken for a protected one.
    pub banner: Option<&'a str>,
    /// The page itself.
    pub content: String,
    /// Render the inspector column; only the library grid fills it.
    pub inspector: bool,
}

impl<'a> Frame<'a> {
    /// A frame with no sidebar, view, notices or banner.
    pub fn new(title: &'a str, active: Destination, content: impl Into<String>) -> Self {
        Self {
            title,
            active,
            sidebar: None,
            view: None,
            result: None,
            notices: &[],
            banner: None,
            content: content.into(),
            inspector: false,
        }
    }

    /// The whole document, ready to serve.
    pub fn render(&self) -> String {
        let banner = self
            .banner
            .map(|text| format!("<p class=\"banner\">{}</p>", escape_html(text)))
            .unwrap_or_default();
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>{} · dimagine</title><link rel=\"stylesheet\" href=\"{}\">{}</head><body>\
             {}\
             <div class=\"app-shell\">{}{}<main class=\"main-column\">{}{}{}</main>{}{}</div>\
             <script src=\"{}\" defer></script></body></html>",
            escape_html(self.title),
            escape_html(&css_url()),
            THEME_BOOT,
            banner,
            self.rail(),
            self.sidebar(),
            self.top_bar(),
            self.notices(),
            self.content,
            self.inspector(),
            self.tab_bar(),
            escape_html(&js_url()),
        )
    }

    fn sidebar(&self) -> String {
        let Some(data) = self.sidebar else {
            return String::new();
        };
        let active = self.view.and_then(|view| view.folder.as_deref());
        let params = self.view.cloned().unwrap_or_default();
        format!(
            "<aside class=\"desktop-sidebar\"><div class=\"sidebar-brand\">dimagine</div>{}</aside>",
            crate::ui::components::sidebar_sections(data, &params, active)
        )
    }

    fn rail(&self) -> String {
        format!(
            "<nav class=\"icon-rail\" aria-label=\"Sections\">{}</nav>",
            crate::ui::components::navigation(self.active, true)
        )
    }

    fn tab_bar(&self) -> String {
        format!(
            "<nav class=\"bottom-tab-bar\" aria-label=\"Sections\">{}</nav>",
            crate::ui::components::navigation(self.active, false)
        )
    }

    fn top_bar(&self) -> String {
        let mut bar = String::from("<header class=\"top-bar\"><div class=\"top-bar-row\">");
        // On `/search` the page carries the real field, so a pill that would
        // only link back to it is left out.
        if self.active != Destination::Search {
            bar.push_str(&crate::ui::components::search_pill(self.view));
        }
        if let (Some(view), Some((total, _))) = (self.view, self.result) {
            bar.push_str("<div class=\"toolbar-controls\">");
            bar.push_str(&crate::ui::components::result_count(total));
            bar.push_str(&crate::ui::components::sort_control(view));
            bar.push_str(&crate::ui::components::size_toggles(view));
            bar.push_str("</div>");
        }
        bar.push_str("</div>");
        // W43: the chips live inside the search capsule (`search_pill`), on
        // one line — a second row of chips under the bar is no longer drawn.
        bar.push_str("</header>");
        if let Some(view) = self.view {
            bar.push_str(&crate::ui::components::breadcrumbs(view));
        }
        bar
    }

    fn notices(&self) -> String {
        crate::ui::components::notices(self.notices)
    }

    fn inspector(&self) -> String {
        if !self.inspector {
            return String::new();
        }
        "<aside class=\"right-inspector\" aria-label=\"Details\"><p class=\"inspector-empty\">\
         Select an image to see its details.</p></aside>"
            .to_owned()
    }
}

/// The sort choices offered by the top-bar menu, in menu order.
///
/// The two times a person sorts by sit together at the top: "Added" is when
/// the file came in, "Taken" is when the camera took the picture. The arrows
/// mean the same in both pairs (↓ is the newer end first). Only "Taken" can be
/// unknown — a screenshot, a scan, a JPEG without a date — and the menu does
/// not pretend otherwise: the grid marks the tiles with no taken time while
/// this order is in force, so an unknown is never read as a date (invariant 4).
pub const SORT_CHOICES: [(ViewSort, Direction, &str); 11] = [
    (ViewSort::Added, Direction::Desc, "Added ↓"),
    (ViewSort::Added, Direction::Asc, "Added ↑"),
    (ViewSort::Taken, Direction::Desc, "Taken ↓"),
    (ViewSort::Taken, Direction::Asc, "Taken ↑"),
    (ViewSort::Modified, Direction::Desc, "Modified ↓"),
    (ViewSort::Modified, Direction::Asc, "Modified ↑"),
    (ViewSort::Name, Direction::Asc, "Name A→Z"),
    (ViewSort::Name, Direction::Desc, "Name Z→A"),
    (ViewSort::Size, Direction::Desc, "Largest first"),
    (ViewSort::Size, Direction::Asc, "Smallest first"),
    (ViewSort::Rating, Direction::Desc, "Rating ★"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_destinations_are_the_spec_four() {
        let paths: Vec<&str> = [
            Destination::Library,
            Destination::Folders,
            Destination::Collections,
            Destination::Search,
        ]
        .iter()
        .map(|destination| destination.path())
        .collect();
        assert_eq!(paths, vec!["/", "/folders", "/collections", "/search"]);
    }

    #[test]
    fn a_frame_carries_the_stylesheet_the_script_and_no_sidebar_by_default() {
        let html = Frame::new("Sign in", Destination::Library, "<p>hi</p>").render();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains(&format!("href=\"{}\"", css_url())));
        assert!(html.contains(&format!("src=\"{}\"", js_url())));
        assert!(!html.contains("desktop-sidebar"));
        assert!(html.contains("<p>hi</p>"));
    }

    /// W43: a stored theme choice must be applied before the first paint, so
    /// the boot script sits in `<head>` — before `<body>`, before anything a
    /// browser could paint — and reads the same key `app.js` remembers by.
    #[test]
    fn the_theme_boot_script_precedes_the_body() {
        let html = Frame::new("Sign in", Destination::Library, "<p>hi</p>").render();
        let boot_at = html
            .find("localStorage.getItem('dimagine.theme')")
            .expect("a theme boot");
        let body_at = html.find("<body>").expect("a body");
        assert!(boot_at < body_at, "the boot closes the head: {html}");
        assert!(
            html[boot_at..].contains("data-theme"),
            "it sets the same attribute the stylesheet reads"
        );
    }

    /// The search page has the real field already; a pill beside it would only
    /// be a link back to the page you are on.
    #[test]
    fn the_search_page_carries_one_field_not_a_pill_and_a_field() {
        let on_search = Frame::new("Search", Destination::Search, "<form></form>").render();
        assert!(!on_search.contains("search-pill-container"));
        let elsewhere = Frame::new("Library", Destination::Library, "<p></p>").render();
        assert!(elsewhere.contains("search-pill-container"));
    }

    /// The no-login banner is a full-width bar above the whole shell. If it
    /// were a child of `.app-shell` it would become a flex column beside the
    /// rail and sidebar once the shell turns into a row at tablet width.
    #[test]
    fn the_no_login_banner_sits_above_the_shell_not_inside_it() {
        let mut frame = Frame::new("Library", Destination::Library, "<p>x</p>");
        frame.banner = Some(crate::NO_LOGIN_BANNER);
        let html = frame.render();
        let banner_at = html.find("class=\"banner\"").expect("a banner");
        let shell_at = html.find("class=\"app-shell\"").expect("a shell");
        assert!(
            banner_at < shell_at,
            "the banner must precede the shell: {html}"
        );
        assert!(
            !html.contains("<div class=\"app-shell\"><p class=\"banner\">"),
            "the banner must not be the shell's first child: {html}"
        );
    }

    #[test]
    fn the_navigation_marks_exactly_one_entry_active() {
        for rail in [false, true] {
            let html = crate::ui::components::navigation(Destination::Folders, rail);
            // Exactly one entry is current, marked for the stylesheet and for a
            // screen reader.
            assert_eq!(html.matches("aria-current=\"page\"").count(), 1, "{html}");
            assert_eq!(html.matches(" active").count(), 1, "{html}");
            assert!(
                html.contains("href=\"/folders\" aria-current=\"page\""),
                "{html}"
            );
            for destination in [
                Destination::Library,
                Destination::Folders,
                Destination::Collections,
                Destination::Search,
            ] {
                assert!(html.contains(&format!("href=\"{}\"", destination.path())));
            }
        }
    }
}
