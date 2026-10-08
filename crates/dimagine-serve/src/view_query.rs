//! Parse and serialise viewer query strings to and from `dimagine_index::ViewQuery`.
//!
//! Query parameters (Spec §2):
//! - `in`: library-relative folder path (default: all)
//! - `sub`: `0` / `1` include subfolders (default: `1`)
//! - `c`: collection note path
//! - `tag`: repeatable, AND
//! - `q`: full-text search string
//! - `sort`: `added` | `modified` | `name` | `size` | `rating` (default: `added`)
//! - `dir`: `asc` | `desc` (default: `desc` for added/modified/size/rating, `asc` for name)
//! - `size`: `s` | `m` | `l` (default: `m`)
//! - `p`: 1-based page number, 120 items per page (default: `1`)
//!
//! Invalid parameters are ignored with a notice, never producing a 500.
//! Defaults are omitted from generated URLs.

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

pub use dimagine_index::SortKey;

use crate::index_sync::RECENT_LIMIT;

/// Items per page in the viewer grid (Spec §2).
pub const PAGE_SIZE: u32 = 120;

/// Characters that must be percent-encoded in query values.
/// Preserves safe characters such as `/`, `-`, `_`, `.`, `~`.
pub const QUERY_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'&')
    .add(b'=')
    .add(b'+')
    .add(b'%')
    .add(b'?');

/// Thumbnail grid tile size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThumbnailSize {
    S,
    #[default]
    M,
    L,
}

impl ThumbnailSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::S => "s",
            Self::M => "m",
            Self::L => "l",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "s" => Some(Self::S),
            "m" => Some(Self::M),
            "l" => Some(Self::L),
            _ => None,
        }
    }
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Asc,
    Desc,
}

impl Direction {
    /// The spelling used in a `sort=<key>-<dir>` option value.
    pub fn suffix(&self) -> &'static str {
        self.as_str()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }
}

/// Default direction for a sort key (Spec §2: desc for added/modified/size/rating, asc for name).
pub fn default_direction(sort: SortKey) -> Direction {
    match sort {
        SortKey::Name => Direction::Asc,
        SortKey::Added | SortKey::Modified | SortKey::Size | SortKey::Rating => Direction::Desc,
    }
}

pub fn sort_key_as_str(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Added => "added",
        SortKey::Modified => "modified",
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Rating => "rating",
    }
}

pub fn parse_sort_key(s: &str) -> Option<SortKey> {
    match s.to_ascii_lowercase().as_str() {
        "added" => Some(SortKey::Added),
        "modified" => Some(SortKey::Modified),
        "name" => Some(SortKey::Name),
        "size" => Some(SortKey::Size),
        "rating" => Some(SortKey::Rating),
        _ => None,
    }
}

/// Parsed viewer query parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewParams {
    /// Library-relative folder path (`in`). None means all folders.
    pub folder: Option<String>,
    /// Whether to include subfolders (`sub`, default true).
    pub recursive: bool,
    /// Collection note path (`c`).
    pub collection: Option<String>,
    /// Tags filter (repeatable `tag`, AND logic).
    pub tags: Vec<String>,
    /// Full-text query (`q`).
    pub q: Option<String>,
    /// Sort key (`sort`, default Added).
    pub sort: SortKey,
    /// Sort direction (`dir`, default depends on sort).
    pub direction: Direction,
    /// Thumbnail size (`size`, default M).
    pub size: ThumbnailSize,
    /// 1-based page number (`p`, default 1).
    pub page: u32,
    /// The untagged lens: only images whose note carries no tag (`untagged=1`).
    pub untagged: bool,
    /// The recent lens: the most recently added [`RECENT_LIMIT`] images,
    /// however old they are (`recent=1`, spec §3 `VIEWS`). A count, not a
    /// window (W34 audit #10).
    pub recent: bool,
    /// The filter panel's facet lists the URL has asked to see whole
    /// (`more=<facet>`, W47). Every list is capped so the panel stays one
    /// glanceable column; the link that lifts a cap is a URL rather than a
    /// click handler, so the longer list arrives without a script.
    pub more: Vec<String>,
    /// Notices for invalid query parameters that were safely ignored.
    pub notices: Vec<String>,
}

/// The facet lists [`ViewParams::more`] may name. A value outside this list is
/// ignored with a notice, like every other parameter the viewer cannot use, and
/// a generated URL writes them in this order, so one view has one spelling.
pub const FACET_LISTS: [&str; 3] = ["tags", "folders", "collections"];

/// Percent-encode a value for use inside a query string, with the same rules
/// [`ViewParams::to_query_string`] uses, so a hand-built link and a generated
/// one agree.
pub fn query_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

/// Decode one query-string value.
///
/// A `+` is a space: the search field and every other form on the page submit
/// with `application/x-www-form-urlencoded`, where a space is a `+`. Generated
/// links never contain a literal `+` — [`query_value`] writes `%2B` — so this
/// costs nothing in round-trip fidelity and makes a typed query work.
fn decode_query_value(raw: &str) -> Result<std::borrow::Cow<'_, str>, ()> {
    if raw.contains('+') {
        let spaced = raw.replace('+', " ");
        return percent_decode_str(&spaced)
            .decode_utf8()
            .map(|decoded| std::borrow::Cow::Owned(decoded.into_owned()))
            .map_err(|_| ());
    }
    percent_decode_str(raw).decode_utf8().map_err(|_| ())
}

impl Default for ViewParams {
    fn default() -> Self {
        Self {
            folder: None,
            recursive: true,
            collection: None,
            tags: Vec::new(),
            q: None,
            sort: SortKey::Added,
            direction: Direction::Desc,
            size: ThumbnailSize::M,
            page: 1,
            untagged: false,
            recent: false,
            more: Vec::new(),
            notices: Vec::new(),
        }
    }
}

impl ViewParams {
    /// Parse from a raw URL query string (with or without leading `?`).
    pub fn parse(query: &str) -> Self {
        let query = query.strip_prefix('?').unwrap_or(query);
        let mut params = Self::default();
        if query.is_empty() {
            return params;
        }

        let mut explicit_dir = None;
        let mut raw_sort = None;

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, raw_val) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };

            match key {
                "in" => match normalise_path(raw_val) {
                    Ok(path) => params.folder = path,
                    Err(reason) => {
                        params
                            .notices
                            .push(format!("Ignored folder 'in': {reason}"));
                    }
                },
                "sub" => match raw_val {
                    "0" => params.recursive = false,
                    "1" => params.recursive = true,
                    other => {
                        params.notices.push(format!(
                            "Ignored invalid 'sub' parameter '{other}' (expected 0 or 1)"
                        ));
                    }
                },
                "c" => match normalise_path(raw_val) {
                    Ok(path) => params.collection = path,
                    Err(reason) => {
                        params
                            .notices
                            .push(format!("Ignored collection 'c': {reason}"));
                    }
                },
                "tag" => match decode_query_value(raw_val) {
                    Ok(decoded) => {
                        let trimmed: String = decoded.trim().nfc().collect();
                        if !trimmed.is_empty() && !params.tags.contains(&trimmed) {
                            params.tags.push(trimmed);
                        }
                    }
                    Err(_) => {
                        params
                            .notices
                            .push("Ignored 'tag': invalid UTF-8".to_string());
                    }
                },
                "q" => match decode_query_value(raw_val) {
                    Ok(decoded) => {
                        let trimmed: String = decoded.trim().nfc().collect();
                        params.q = if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        };
                    }
                    Err(_) => {
                        params
                            .notices
                            .push("Ignored 'q': invalid UTF-8".to_string());
                    }
                },
                // `sort=<key>` or `sort=<key>-<dir>`: the native `<select>` in
                // the top bar has to put both in one value, so both spellings
                // are accepted and mean the same thing.
                "sort" => match raw_val.rsplit_once('-') {
                    Some((key, dir)) if Direction::parse(dir).is_some() => {
                        raw_sort = Some(key.to_string());
                        explicit_dir = Some(dir.to_string());
                    }
                    _ => raw_sort = Some(raw_val.to_string()),
                },
                "dir" => {
                    explicit_dir = Some(raw_val.to_string());
                }
                "size" => match ThumbnailSize::parse(raw_val) {
                    Some(s) => params.size = s,
                    None => {
                        params.notices.push(format!(
                            "Ignored unknown thumbnail size '{raw_val}' (expected s, m, l)"
                        ));
                    }
                },
                "p" => match raw_val.parse::<u32>() {
                    Ok(n) if n >= 1 => params.page = n,
                    _ => {
                        params.notices.push(format!(
                            "Ignored invalid page number '{raw_val}' (expected >= 1)"
                        ));
                    }
                },
                // `untagged` and `recent` are lenses rather than filters, so
                // they take the same 0/1 spelling as `sub`.
                "untagged" | "recent" => match raw_val {
                    "0" => params.setting(key, false),
                    "1" => params.setting(key, true),
                    other => params.notices.push(format!(
                        "Ignored invalid '{key}' parameter '{other}' (expected 0 or 1)"
                    )),
                },
                // `more=<facet>` lifts the cap on one filter-panel list. It
                // names a facet, so a name of no facet is worth a notice: the
                // panel below the link would otherwise look unchanged.
                "more" => match decode_query_value(raw_val) {
                    Ok(decoded) => {
                        let facet = decoded.trim().to_lowercase();
                        if FACET_LISTS.contains(&facet.as_str()) {
                            if !params.more.contains(&facet) {
                                params.more.push(facet);
                            }
                        } else {
                            params.notices.push(format!(
                                "Ignored unknown facet list '{facet}' (expected tags, folders, collections)"
                            ));
                        }
                    }
                    Err(_) => {
                        params
                            .notices
                            .push("Ignored 'more': invalid UTF-8".to_string());
                    }
                },
                unknown => {
                    params
                        .notices
                        .push(format!("Ignored unknown query parameter '{unknown}'"));
                }
            }
        }

        // Whether the URL asked for an order at all. A lens that fixes the
        // order says so only when the reader asked for something else.
        let asked_for_an_order = raw_sort.is_some() || explicit_dir.is_some();

        if let Some(s) = raw_sort {
            match parse_sort_key(&s) {
                Some(key) => params.sort = key,
                None => {
                    params.notices.push(format!(
                        "Ignored unknown sort '{s}' (expected added, modified, name, size, rating)"
                    ));
                }
            }
        }

        params.direction = match explicit_dir {
            Some(d) => match Direction::parse(&d) {
                Some(dir) => dir,
                None => {
                    params.notices.push(format!(
                        "Ignored unknown direction '{d}' (expected asc or desc)"
                    ));
                    default_direction(params.sort)
                }
            },
            None => default_direction(params.sort),
        };

        // The Recent lens is newest-first by definition, so a sort asked for
        // beside it cannot decide the order (W37 review, Medium #1). Say so,
        // rather than leaving a parameter that looks like it did something.
        if params.recent
            && asked_for_an_order
            && (params.sort != SortKey::Added || params.direction != Direction::Desc)
        {
            params.notices.push(format!(
                "Ignored sort: Recent is always the last {RECENT_LIMIT} added, newest first"
            ));
        }

        // A collection's order is the order of the embeds in its note
        // (FORMAT §5): the view the index answers keeps that order whatever
        // the query says — like the Recent lens, a sort beside it cannot
        // decide it. Unlike Recent, no spelling of `sort` names the order
        // in force, so a sort the URL asked for is always worth reporting.
        if params.collection.is_some() && asked_for_an_order {
            params
                .notices
                .push("Ignored sort: a collection keeps the order of its note's embeds".to_owned());
        }

        // Neither does it have pages: the members are the whole of one note's
        // embed list, listed at once (the count in the header is the count on
        // the page), so `p` beside `c` asks for a second page of a list that
        // has no second page. Say so in the same voice as the sort above.
        if params.collection.is_some() && params.page != 1 {
            params.notices.push(
                "Ignored page number: a collection lists all of its members at once".to_owned(),
            );
        }

        params
    }

    /// Serialise parameters into a query string, omitting all defaults (Spec §2).
    pub fn to_query_string(&self) -> String {
        let mut pairs = Vec::new();

        if let Some(folder) = &self.folder {
            pairs.push(format!("in={}", utf8_percent_encode(folder, QUERY_VALUE)));
        }

        if !self.recursive {
            pairs.push("sub=0".to_string());
        }

        if let Some(coll) = &self.collection {
            pairs.push(format!("c={}", utf8_percent_encode(coll, QUERY_VALUE)));
        }

        for tag in &self.tags {
            pairs.push(format!("tag={}", utf8_percent_encode(tag, QUERY_VALUE)));
        }

        if let Some(q) = &self.q {
            pairs.push(format!("q={}", utf8_percent_encode(q, QUERY_VALUE)));
        }

        if self.sort != SortKey::Added {
            pairs.push(format!("sort={}", sort_key_as_str(self.sort)));
        }

        if self.direction != default_direction(self.sort) {
            pairs.push(format!("dir={}", self.direction.as_str()));
        }

        if self.size != ThumbnailSize::M {
            pairs.push(format!("size={}", self.size.as_str()));
        }

        if self.page > 1 {
            pairs.push(format!("p={}", self.page));
        }

        if self.untagged {
            pairs.push("untagged=1".to_string());
        }

        if self.recent {
            pairs.push("recent=1".to_string());
        }

        // The expanded facet lists, in the list's own order rather than the
        // order the URL happened to spell them in: one view, one URL.
        for facet in FACET_LISTS {
            if self.more.iter().any(|shown| shown == facet) {
                pairs.push(format!("more={facet}"));
            }
        }

        pairs.join("&")
    }

    /// Build a URL with the given base path and these parameters.
    pub fn to_url(&self, base_path: &str) -> String {
        let qs = self.to_query_string();
        if qs.is_empty() {
            base_path.to_string()
        } else {
            format!("{base_path}?{qs}")
        }
    }

    /// Convert to a `dimagine_index::ViewQuery`.
    pub fn to_index_query(&self) -> dimagine_index::ViewQuery {
        // The Recent lens is "the most recently added [`RECENT_LIMIT`]"
        // (W34 audit #10), so Added-newest-first is the lens's definition,
        // not a choice: like a collection's embed order, a sort picked beside
        // it cannot decide which images the lens holds.
        let (sort, descending) = if self.recent {
            (SortKey::Added, true)
        } else {
            (self.sort, self.direction == Direction::Desc)
        };
        let mut offset = u64::from(self.page.saturating_sub(1)) * u64::from(PAGE_SIZE);
        let mut limit = u64::from(PAGE_SIZE);
        if self.recent {
            // The pages stay inside the lens: page one starts at the newest
            // image, page two ends at the 200th, nothing follows it. The
            // index's own total counts every match, so `IndexHandle::view`
            // callers cap the reported total the same way.
            offset = offset.min(RECENT_LIMIT);
            limit = limit.min(RECENT_LIMIT - offset);
        }
        dimagine_index::ViewQuery {
            folder: self.folder.clone(),
            recursive: self.recursive,
            collection: self.collection.clone(),
            tags: self.tags.clone(),
            text: self.q.clone(),
            untagged: self.untagged,
            added_after_ns: None,
            sort,
            descending,
            // Saturating, so an absurd `p` reads as "far past the end" instead
            // of overflowing on its way to the index.
            offset: offset.min(u64::from(u32::MAX)) as u32,
            limit: limit.min(u64::from(u32::MAX)) as u32,
        }
    }

    fn setting(&mut self, key: &str, value: bool) {
        match key {
            "untagged" => self.untagged = value,
            "recent" => self.recent = value,
            _ => unreachable!("setting() is only called for the lens parameters"),
        }
    }

    /// New instance with updated page number.
    pub fn with_page(&self, page: u32) -> Self {
        let mut next = self.clone();
        next.page = page.max(1);
        next
    }

    /// New instance with updated sort key (and its default direction).
    pub fn with_sort(&self, sort: SortKey) -> Self {
        let mut next = self.clone();
        next.sort = sort;
        next.direction = default_direction(sort);
        next.page = 1;
        next
    }

    /// New instance with updated sort key and direction.
    pub fn with_sort_and_dir(&self, sort: SortKey, direction: Direction) -> Self {
        let mut next = self.clone();
        next.sort = sort;
        next.direction = direction;
        next.page = 1;
        next
    }

    /// New instance with updated thumbnail size.
    pub fn with_size(&self, size: ThumbnailSize) -> Self {
        let mut next = self.clone();
        next.size = size;
        next
    }

    /// New instance without the folder restriction (`in`).
    pub fn without_folder(&self) -> Self {
        let mut next = self.clone();
        next.folder = None;
        next.page = 1;
        next
    }

    /// New instance with folder set.
    pub fn with_folder(&self, folder: Option<String>) -> Self {
        let mut next = self.clone();
        next.folder = folder;
        next.page = 1;
        next
    }

    /// New instance without collection (`c`).
    pub fn without_collection(&self) -> Self {
        let mut next = self.clone();
        next.collection = None;
        next.page = 1;
        next
    }

    /// New instance without tag filter.
    pub fn without_tag(&self, tag: &str) -> Self {
        let mut next = self.clone();
        // The index stores and matches tags folded (`dimagine_index::searchable`),
        // so `?tag=Eagle` and `?tag=eagle` are the one filter: removing a tag
        // removes every spelling of it, or the ✕ would leave a chip behind that
        // reads as a filter still narrowing the grid.
        let folded = dimagine_index::searchable(tag);
        next.tags
            .retain(|active| dimagine_index::searchable(active) != folded);
        next.page = 1;
        next
    }

    /// New instance with added tag.
    pub fn with_tag(&self, tag: String) -> Self {
        let mut next = self.clone();
        if !next.tags.contains(&tag) {
            next.tags.push(tag);
        }
        next.page = 1;
        next
    }

    /// New instance with the collection set (`c`), or cleared by `None` — the
    /// one mutator a filter-panel row needs, because a collection row toggles
    /// between choosing its note and giving it back.
    pub fn with_collection(&self, collection: Option<String>) -> Self {
        let mut next = self.clone();
        next.collection = collection;
        next.page = 1;
        next
    }

    /// New instance with the untagged lens set or cleared, keeping everything
    /// else on screen. The sidebar's [`ViewParams::only_untagged`] is the other
    /// gesture: a row in a navigation list starts a fresh view, a row in the
    /// filter panel narrows the one being looked at.
    pub fn with_untagged(&self, untagged: bool) -> Self {
        let mut next = self.clone();
        next.untagged = untagged;
        next.page = 1;
        next
    }

    /// New instance with the recent lens set or cleared, keeping everything
    /// else on screen (see [`ViewParams::with_untagged`]).
    pub fn with_recent(&self, recent: bool) -> Self {
        let mut next = self.clone();
        next.recent = recent;
        next.page = 1;
        next
    }

    /// New instance with one facet list shown whole instead of capped
    /// (`more=<facet>`). A cap is a reading aid, not a position in the library,
    /// so the page the grid is on stays where it was.
    pub fn with_more_of(&self, facet: &str) -> Self {
        let mut next = self.clone();
        if !next.more.iter().any(|shown| shown == facet) {
            next.more.push(facet.to_owned());
        }
        next
    }

    /// New instance with one facet list capped again — the other half of the
    /// round trip [`ViewParams::with_more_of`] opens.
    pub fn without_more_of(&self, facet: &str) -> Self {
        let mut next = self.clone();
        next.more.retain(|shown| shown != facet);
        next
    }

    /// Whether one facet list is currently shown whole.
    pub fn shows_more_of(&self, facet: &str) -> bool {
        self.more.iter().any(|shown| shown == facet)
    }

    /// How many of the view's filters are on: what the panel's own heading
    /// counts, so a collapsed panel on a phone still says that something is
    /// narrowing the grid and what is on screen cannot be mistaken for all of
    /// it. The page number, the sort and the tile size are not filters, and
    /// neither is a facet list shown whole.
    pub fn active_filter_count(&self) -> usize {
        usize::from(self.folder.is_some())
            + usize::from(self.collection.is_some())
            + self.tags.len()
            + usize::from(self.q.is_some())
            + usize::from(self.untagged)
            + usize::from(self.recent)
    }

    /// New instance without search query (`q`).
    pub fn without_q(&self) -> Self {
        let mut next = self.clone();
        next.q = None;
        next.page = 1;
        next
    }

    /// New instance with every filter off — folder, collection, tags,
    /// search text and both lenses — and everything else the reader
    /// chose kept: the sort and its direction, the tile size, the page
    /// and the expanded facet lists. The panel's "Clear all filters"
    /// is this view, because the label promises filters and nothing
    /// else; `active_filter_count` is the list of what it clears.
    pub fn without_filters(&self) -> Self {
        let mut next = self.clone();
        next.folder = None;
        next.collection = None;
        next.tags.clear();
        next.q = None;
        next.untagged = false;
        next.recent = false;
        next
    }

    /// New instance that does or does not include subfolders (`sub`).
    pub fn with_recursive(&self, recursive: bool) -> Self {
        let mut next = self.clone();
        next.recursive = recursive;
        next.page = 1;
        next
    }

    /// The untagged lens on its own: `Untagged` is a view, not a filter
    /// added to whatever was on screen before.
    pub fn only_untagged(&self) -> Self {
        Self {
            untagged: true,
            size: self.size,
            ..Self::default()
        }
    }

    /// The recent lens on its own.
    pub fn only_recent(&self) -> Self {
        Self {
            recent: true,
            size: self.size,
            ..Self::default()
        }
    }
}

/// Normalise a folder or collection path.
///
/// Rules:
/// - Percent-decode.
/// - Reject absolute paths (starting with `/` or `\`).
/// - Reject paths containing `..` segments.
/// - Normalize unicode to NFC.
/// - Collapse duplicate slashes.
/// - Strip leading/trailing slashes.
/// - Spaces, brackets, unicode characters within segment names are preserved.
/// - A `+` is a space, as in any query string.
pub fn normalise_path(raw: &str) -> Result<Option<String>, &'static str> {
    let spaced = plus_as_space(raw);
    let decoded = percent_decode_str(&spaced)
        .decode_utf8()
        .map_err(|_| "invalid UTF-8")?;
    normalise_decoded(&decoded)
}

/// A path that arrived already decoded, as a path segment does from the router.
///
/// Here a literal `+` is a literal `+`: `/folder/a%2Bb` is the folder `a+b`,
/// not `a b`.
pub fn normalise_decoded_path(decoded: &str) -> Result<Option<String>, &'static str> {
    normalise_decoded(decoded)
}

/// `+` means a space in a query string, because every form on the page submits
/// as `application/x-www-form-urlencoded`. Generated links never contain a
/// literal `+` — [`query_value`] writes `%2B` — so this makes a typed query work
/// without costing anything in round-trip fidelity.
fn plus_as_space(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.contains('+') {
        std::borrow::Cow::Owned(raw.replace('+', " "))
    } else {
        std::borrow::Cow::Borrowed(raw)
    }
}

fn normalise_decoded(decoded: &str) -> Result<Option<String>, &'static str> {
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err("absolute paths are not allowed");
    }

    let mut segments = Vec::new();
    for seg in trimmed.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err("parent segment '..' is not allowed");
        }
        let nfc_seg: String = seg.nfc().collect();
        segments.push(nfc_seg);
    }

    if segments.is_empty() {
        Ok(None)
    } else {
        Ok(Some(segments.join("/")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_params_serialise_to_empty_string() {
        let params = ViewParams::default();
        assert_eq!(params.to_query_string(), "");
        assert_eq!(params.to_url("/"), "/");
    }

    #[test]
    fn parse_empty_query_yields_defaults() {
        let parsed = ViewParams::parse("");
        assert_eq!(parsed, ViewParams::default());
        assert!(parsed.notices.is_empty());

        let parsed_q = ViewParams::parse("?");
        assert_eq!(parsed_q, ViewParams::default());
    }

    #[test]
    fn parse_and_serialise_round_trip_full() {
        let qs = "in=refs/ui%20design&sub=0&c=collections/favs.md&tag=art%26design&tag=urban&q=street%20view&sort=size&dir=asc&size=l&p=3";
        let parsed = ViewParams::parse(qs);

        assert_eq!(parsed.folder.as_deref(), Some("refs/ui design"));
        assert!(!parsed.recursive);
        assert_eq!(parsed.collection.as_deref(), Some("collections/favs.md"));
        assert_eq!(parsed.tags, vec!["art&design", "urban"]);
        assert_eq!(parsed.q.as_deref(), Some("street view"));
        assert_eq!(parsed.sort, SortKey::Size);
        assert_eq!(parsed.direction, Direction::Asc);
        assert_eq!(parsed.size, ThumbnailSize::L);
        assert_eq!(parsed.page, 3);
        // The query asks for a sort and a page number beside a collection:
        // the collection keeps the order of its note's embeds and lists all of
        // its members at once (FORMAT §5), so neither is something it can take,
        // and the reader is told both — the URL cannot look like it decided the
        // order or picked a page of a list with no pages.
        assert_eq!(
            parsed.notices,
            vec![
                "Ignored sort: a collection keeps the order of its note's embeds".to_owned(),
                "Ignored page number: a collection lists all of its members at once".to_owned(),
            ],
            "{:?}",
            parsed.notices
        );

        let reserialised = parsed.to_query_string();
        let parsed2 = ViewParams::parse(&reserialised);
        assert_eq!(parsed2.folder, parsed.folder);
        assert_eq!(parsed2.recursive, parsed.recursive);
        assert_eq!(parsed2.collection, parsed.collection);
        assert_eq!(parsed2.tags, parsed.tags);
        assert_eq!(parsed2.q, parsed.q);
        assert_eq!(parsed2.sort, parsed.sort);
        assert_eq!(parsed2.direction, parsed.direction);
        assert_eq!(parsed2.size, parsed.size);
        assert_eq!(parsed2.page, parsed.page);
    }

    /// A collection view says which order it keeps, and only when the URL
    /// asked for another — a lens on its own has nothing to ignore, the same
    /// rule the Recent lens follows (W37 review, Medium #1).
    #[test]
    fn a_collection_reports_a_sort_it_cannot_take() {
        assert_eq!(
            ViewParams::parse("c=favs.md&sort=name").notices,
            vec!["Ignored sort: a collection keeps the order of its note's embeds".to_owned()]
        );
        assert_eq!(
            ViewParams::parse("c=favs.md&dir=asc").notices,
            vec!["Ignored sort: a collection keeps the order of its note's embeds".to_owned()]
        );
        // A collection on its own, or beside filters that are not a sort,
        // says nothing: there was no order to ignore.
        assert!(ViewParams::parse("c=favs.md").notices.is_empty());
        assert!(ViewParams::parse("c=favs.md&tag=x&untagged=0")
            .notices
            .is_empty());
    }

    /// The same lens answers for the page number as for the sort: a
    /// collection has no second page, so a `p` asked beside `c` is ignored,
    /// and saying so is the only thing left to do with it.
    #[test]
    fn a_collection_reports_a_page_number_it_cannot_take() {
        assert_eq!(
            ViewParams::parse("c=favs.md&p=3").notices,
            vec!["Ignored page number: a collection lists all of its members at once".to_owned()]
        );
        // The first page is what the page shows anyway, so it is not a
        // request being refused, and a page number no page could be already
        // has its own, more specific notice.
        assert!(ViewParams::parse("c=favs.md&p=1").notices.is_empty());
        assert_eq!(
            ViewParams::parse("c=favs.md&p=many").notices,
            vec!["Ignored invalid page number 'many' (expected >= 1)".to_owned()]
        );
        // A page number on an ordinary view is honoured, and says nothing.
        let paged = ViewParams::parse("p=3");
        assert_eq!(paged.page, 3);
        assert!(paged.notices.is_empty());
    }

    #[test]
    fn path_normalisation_supports_spaces_brackets_and_unicode() {
        let query = "in=%5B2024%5D%20Photos/caf%C3%A9%20shots";
        let parsed = ViewParams::parse(query);
        assert_eq!(parsed.folder.as_deref(), Some("[2024] Photos/café shots"));
        assert!(parsed.notices.is_empty());
    }

    #[test]
    fn path_normalisation_rejects_parent_traversal() {
        let query = "in=refs/../secret";
        let parsed = ViewParams::parse(query);
        assert_eq!(parsed.folder, None);
        assert!(!parsed.notices.is_empty());
        assert!(parsed.notices[0].contains("parent segment '..'"));
    }

    #[test]
    fn path_normalisation_rejects_absolute_paths() {
        let query = "in=/etc/passwd";
        let parsed = ViewParams::parse(query);
        assert_eq!(parsed.folder, None);
        assert!(!parsed.notices.is_empty());
        assert!(parsed.notices[0].contains("absolute"));
    }

    #[test]
    fn path_normalisation_collapses_slashes() {
        let query = "in=refs///nested//folder/";
        let parsed = ViewParams::parse(query);
        assert_eq!(parsed.folder.as_deref(), Some("refs/nested/folder"));
    }

    #[test]
    fn invalid_parameters_are_ignored_with_notice() {
        let query = "sort=nonexistent&dir=sideways&size=huge&p=-1&sub=maybe";
        let parsed = ViewParams::parse(query);
        assert_eq!(parsed.sort, SortKey::Added);
        assert_eq!(parsed.direction, Direction::Desc);
        assert_eq!(parsed.size, ThumbnailSize::M);
        assert_eq!(parsed.page, 1);
        assert!(parsed.recursive);
        assert_eq!(parsed.notices.len(), 5);
    }

    #[test]
    fn sort_defaults_and_direction() {
        // Default sort added -> default dir desc -> omitted from URL
        let p1 = ViewParams::parse("sort=added");
        assert_eq!(p1.sort, SortKey::Added);
        assert_eq!(p1.direction, Direction::Desc);
        assert_eq!(p1.to_query_string(), "");

        // Name sort -> default dir asc -> dir omitted
        let p2 = ViewParams::parse("sort=name");
        assert_eq!(p2.sort, SortKey::Name);
        assert_eq!(p2.direction, Direction::Asc);
        assert_eq!(p2.to_query_string(), "sort=name");

        // Name sort with explicit desc -> dir included
        let p3 = ViewParams::parse("sort=name&dir=desc");
        assert_eq!(p3.sort, SortKey::Name);
        assert_eq!(p3.direction, Direction::Desc);
        assert_eq!(p3.to_query_string(), "sort=name&dir=desc");

        // Added sort with explicit asc -> dir included
        let p4 = ViewParams::parse("sort=added&dir=asc");
        assert_eq!(p4.sort, SortKey::Added);
        assert_eq!(p4.direction, Direction::Asc);
        assert_eq!(p4.to_query_string(), "dir=asc");
    }

    #[test]
    fn to_index_query_converts_correctly() {
        let params = ViewParams {
            folder: Some("refs".to_string()),
            recursive: true,
            collection: None,
            tags: vec!["tag1".to_string(), "tag2".to_string()],
            q: Some("query".to_string()),
            sort: SortKey::Rating,
            direction: Direction::Desc,
            size: ThumbnailSize::S,
            page: 2,
            untagged: false,
            recent: false,
            ..ViewParams::default()
        };

        let iq = params.to_index_query();
        assert_eq!(iq.folder.as_deref(), Some("refs"));
        assert!(iq.recursive);
        assert_eq!(iq.collection, None);
        assert_eq!(iq.tags, vec!["tag1", "tag2"]);
        assert_eq!(iq.text.as_deref(), Some("query"));
        assert_eq!(iq.sort, SortKey::Rating);
        assert!(iq.descending);
        assert_eq!(iq.offset, 120);
        assert_eq!(iq.limit, 120);
        assert!(!iq.untagged);
        assert_eq!(iq.added_after_ns, None);
    }

    #[test]
    fn the_lenses_round_trip_and_map_onto_the_index_query() {
        let untagged = ViewParams::parse("untagged=1").only_untagged();
        assert!(untagged.untagged);
        assert_eq!(untagged.to_query_string(), "untagged=1");
        assert!(untagged.to_index_query().untagged);

        let recent = ViewParams::parse("recent=1").only_recent();
        assert!(recent.recent);
        assert_eq!(recent.to_query_string(), "recent=1");
        let query = recent.to_index_query();
        assert_eq!(query.added_after_ns, None, "Recent counts images, not days");
        assert_eq!(query.sort, SortKey::Added);
        assert!(query.descending);
        assert_eq!(query.offset, 0);
        assert_eq!(query.limit, PAGE_SIZE);

        // `0` is the default, so it is left out of a generated URL.
        assert_eq!(
            ViewParams::parse("untagged=0&recent=0").to_query_string(),
            ""
        );
    }

    /// W34 audit #10: the lens is the [`RECENT_LIMIT`] most recently added
    /// images, so its pages stay inside that window and past it there is
    /// nothing left to page through.
    #[test]
    fn the_recent_lens_pages_inside_its_two_hundred() {
        let first = ViewParams::parse("recent=1").to_index_query();
        assert_eq!(first.offset, 0);
        assert_eq!(first.limit, PAGE_SIZE);

        let second = ViewParams::parse("recent=1&p=2").to_index_query();
        assert_eq!(second.offset, PAGE_SIZE);
        assert_eq!(
            second.limit,
            RECENT_LIMIT as u32 - PAGE_SIZE,
            "page two carries the images up to the 200th, not past it"
        );

        let third = ViewParams::parse("recent=1&p=3").to_index_query();
        assert_eq!(third.offset, RECENT_LIMIT as u32);
        assert_eq!(third.limit, 0, "nothing follows the 200th image");

        let absurd = ViewParams::parse("recent=1&p=999999").to_index_query();
        assert_eq!(absurd.offset, RECENT_LIMIT as u32);
        assert_eq!(absurd.limit, 0);
    }

    /// The order is part of the lens's definition: like a collection's embed
    /// order, a sort picked beside Recent cannot decide which images the lens
    /// holds, so it cannot decide their order either.
    #[test]
    fn the_recent_lens_keeps_the_added_order_a_sort_cannot_change_its_mind() {
        let params = ViewParams::parse("recent=1&sort=name&dir=asc");
        let sorted = params.to_index_query();
        assert_eq!(sorted.sort, SortKey::Added);
        assert!(sorted.descending);
        // W37 review, Medium #1: and the reader is told, so the sort control
        // is not the only place the truth appears.
        assert_eq!(params.notices.len(), 1);
        assert!(
            params.notices[0] == "Ignored sort: Recent is always the last 200 added, newest first",
            "{:?}",
            params.notices
        );

        // A lens on its own asked for no order, so there is nothing to ignore.
        assert!(ViewParams::parse("recent=1").notices.is_empty());
        assert!(ViewParams::parse("recent=1&dir=desc").notices.is_empty());

        // Without the lens, the sort belongs to the reader.
        let plain = ViewParams::parse("sort=name&dir=asc").to_index_query();
        assert_eq!(plain.sort, SortKey::Name);
        assert!(!plain.descending);
    }

    /// An absurd page number is far past the end of any library, so it reads
    /// as an empty page instead of overflowing the offset on its way to
    /// SQLite.
    #[test]
    fn an_absurd_page_number_saturates_rather_than_overflows() {
        let paged = ViewParams::parse("p=4294967295").to_index_query();
        assert_eq!(paged.offset, u32::MAX);
        assert_eq!(paged.limit, PAGE_SIZE);
    }

    #[test]
    fn a_lens_keeps_the_thumbnail_size_and_drops_the_rest() {
        let params = ViewParams::parse("in=refs&tag=eagle&size=l&recent=1").only_recent();
        assert_eq!(params.size, ThumbnailSize::L);
        assert_eq!(params.folder, None);
        assert!(params.tags.is_empty());
        assert!(params.q.is_none());
        assert_eq!(params.to_query_string(), "size=l&recent=1");
    }

    #[test]
    fn an_invalid_lens_is_ignored_with_a_notice() {
        let params = ViewParams::parse("untagged=yes&recent=maybe");
        assert!(!params.untagged);
        assert!(!params.recent);
        assert_eq!(params.notices.len(), 2);
        assert!(params
            .notices
            .iter()
            .all(|notice| notice.contains("expected 0 or 1")));
    }

    /// A form submits a space as `+`, so a typed query has to read back the
    /// word that was typed.
    #[test]
    fn a_plus_in_a_query_is_a_space() {
        assert_eq!(
            ViewParams::parse("q=two+words").q.as_deref(),
            Some("two words")
        );
        assert_eq!(
            ViewParams::parse("tag=one+tag").tags,
            vec!["one tag".to_owned()]
        );
        assert_eq!(
            ViewParams::parse("in=refs%2Fmy+folder").folder.as_deref(),
            Some("refs/my folder")
        );
    }

    /// A literal `+` in a name survives, because it is written `%2B`.
    #[test]
    fn a_percent_encoded_plus_stays_a_plus() {
        assert_eq!(ViewParams::parse("in=a%2Bb").folder.as_deref(), Some("a+b"));
        assert_eq!(ViewParams::parse("in=a+b").folder.as_deref(), Some("a b"));
    }

    /// The sort menu has to put the key and the direction in one value, so
    /// `sort=name-asc` and `sort=name&dir=asc` mean the same thing.
    #[test]
    fn a_sort_value_may_carry_its_direction() {
        let joined = ViewParams::parse("sort=name-desc");
        assert_eq!(joined.sort, SortKey::Name);
        assert_eq!(joined.direction, Direction::Desc);
        assert_eq!(joined.to_query_string(), "sort=name&dir=desc");

        let split = ViewParams::parse("sort=name&dir=desc");
        assert_eq!(joined, split);

        // A sort key that merely contains a dash is not a direction.
        let unknown = ViewParams::parse("sort=nonsense-sideways");
        assert_eq!(unknown.sort, SortKey::Added);
        assert!(!unknown.notices.is_empty());
    }

    #[test]
    fn helper_mutators_reset_page() {
        let params = ViewParams::default().with_page(5);
        assert_eq!(params.page, 5);

        let p2 = params.with_folder(Some("test".to_string()));
        assert_eq!(p2.page, 1);
        assert_eq!(p2.folder.as_deref(), Some("test"));

        let p3 = p2.with_page(3).without_folder();
        assert_eq!(p3.page, 1);
        assert_eq!(p3.folder, None);

        let p4 = p3.with_page(4).with_tag("newtag".to_string());
        assert_eq!(p4.page, 1);
        assert_eq!(p4.tags, vec!["newtag"]);

        let p5 = p4.with_page(2).without_tag("newtag");
        assert_eq!(p5.page, 1);
        assert!(p5.tags.is_empty());
    }

    /// W47: a filter-panel row toggles. The URL it links to is the whole
    /// mechanism, so every row's two states have to round-trip through the
    /// query string — on, off, and back on — with the rest of the view intact.
    #[test]
    fn every_panel_toggle_round_trips_and_keeps_the_rest_of_the_view() {
        let on = ViewParams::parse("in=refs&sub=0&tag=eagle&q=street&size=l&p=2");

        // Folder row: choosing it keeps the tags, the words and the size.
        let folder_on = on.without_folder().with_folder(Some("refs/ui".to_owned()));
        assert_eq!(folder_on.folder.as_deref(), Some("refs/ui"));
        assert_eq!(folder_on.tags, on.tags);
        assert_eq!(folder_on.q, on.q);
        assert_eq!(folder_on.size, ThumbnailSize::L);
        assert!(!folder_on.recursive);
        assert_eq!(folder_on.page, 1, "a new narrowing starts at page one");
        assert_eq!(
            folder_on
                .without_folder()
                .without_folder()
                .to_query_string(),
            ViewParams::parse("sub=0&tag=eagle&q=street&size=l").to_query_string(),
            "giving the folder back lands on the view before it"
        );

        // Collection row: one mutator, both directions.
        let collection_on = on.with_collection(Some("set.md".to_owned()));
        assert_eq!(
            collection_on.to_query_string(),
            "in=refs&sub=0&c=set.md&tag=eagle&q=street&size=l",
            "page one is the default, so it never rides in a URL"
        );
        let collection_off = collection_on.with_collection(None);
        assert_eq!(collection_off.collection, None);
        assert_eq!(
            collection_off.to_query_string(),
            on.with_page(1).to_query_string(),
            "the round trip is the same URL, not merely the same filters"
        );

        // Tag row, twice over: the second tag is the one already chosen.
        let second = on.with_tag("urban".to_owned());
        assert_eq!(second.tags, vec!["eagle", "urban"]);
        assert_eq!(second.with_tag("urban".to_owned()).tags, second.tags);
        assert_eq!(second.without_tag("urban").tags, vec!["eagle"]);

        // The lenses toggle beside everything else, which is what makes their
        // counts meaningful (an untagged image inside a folder is a question
        // the panel asks in one click).
        let lenses = on.with_untagged(true).with_recent(true);
        assert!(lenses.untagged && lenses.recent);
        assert_eq!(
            lenses
                .with_untagged(false)
                .with_recent(false)
                .to_query_string(),
            on.with_page(1).to_query_string()
        );
    }

    /// The index folds a tag's case (`dimagine_index::searchable`), so one tag
    /// spelled two ways is one filter. A ✕ that matched bytes exactly would
    /// leave the other spelling on the screen, still narrowing the grid.
    #[test]
    fn removing_a_tag_removes_every_spelling_of_it() {
        let params = ViewParams::parse("tag=Eagle&tag=URBAN");
        assert_eq!(params.without_tag("eagle").tags, vec!["URBAN"]);
        assert_eq!(
            params.without_tag("eagle").without_tag("urban").tags,
            Vec::<String>::new()
        );
        assert_eq!(params.without_tag("nope").tags, vec!["Eagle", "URBAN"]);
    }

    /// "Clear all filters" clears filters: the folder, the collection, the
    /// tags, the words and the two lenses. Everything else the reader chose —
    /// the order, the tile size, the page they are standing on, the facet
    /// lists they asked to see all of — travels to the destination with them,
    /// because the label promised filters and nothing else (W47b Low-5).
    /// `sub` goes too: it is the folder rows' own setting, not a filter.
    ///
    /// This is the unit beside the helper, which the integration test cannot
    /// replace: on a URL whose only parameters are filters the link with the
    /// bug and the link without it are the same string (`href="/"`).
    #[test]
    fn clearing_filters_keeps_everything_that_is_not_a_filter() {
        let view = ViewParams::parse(
            "in=refs&sub=0&c=favs.md&tag=art&q=street&sort=name&dir=desc\
             &size=l&p=2&untagged=1&recent=1&more=tags",
        );
        let cleared = view.without_filters();
        assert_eq!(cleared.folder, None);
        assert_eq!(cleared.collection, None);
        assert!(cleared.tags.is_empty());
        assert_eq!(cleared.q, None);
        assert!(!cleared.untagged);
        assert!(!cleared.recent);
        assert_eq!(cleared.sort, SortKey::Name);
        assert_eq!(cleared.direction, Direction::Desc);
        assert_eq!(cleared.size, ThumbnailSize::L);
        assert_eq!(cleared.page, 2);
        assert_eq!(cleared.more, vec!["tags".to_owned()]);
        assert!(!cleared.recursive);
        // The link the panel prints, in the order the serialiser writes it.
        assert_eq!(
            cleared.to_query_string(),
            "sub=0&sort=name&dir=desc&size=l&p=2&more=tags"
        );
    }

    /// W47: the panel caps each facet list and the *Show more* link is a URL,
    /// so the longer list has to survive a round trip and a *Show fewer* has to
    /// give the cap back exactly.
    #[test]
    fn a_shown_facet_list_round_trips_through_the_url() {
        let expanded = ViewParams::parse("in=refs").with_more_of("tags");
        assert!(expanded.shows_more_of("tags"));
        assert!(!expanded.shows_more_of("folders"));
        assert_eq!(expanded.to_query_string(), "in=refs&more=tags");
        assert_eq!(
            ViewParams::parse(&expanded.to_query_string()),
            expanded,
            "the link a reader bookmarks is the page they get"
        );

        // Spelled in any order, deduped, and written in one order: one view,
        // one URL.
        let both = ViewParams::parse("more=collections&more=tags&more=tags");
        assert_eq!(both.to_query_string(), "more=tags&more=collections");
        assert_eq!(
            both.without_more_of("tags")
                .without_more_of("collections")
                .to_query_string(),
            ""
        );

        // A name of no facet list is ignored, and says so: a link that did
        // nothing would look like a broken control.
        let junk = ViewParams::parse("more=ratings");
        assert!(junk.more.is_empty());
        assert_eq!(
            junk.notices,
            vec![
                "Ignored unknown facet list 'ratings' (expected tags, folders, collections)"
                    .to_owned()
            ]
        );
        // And it never reaches the index query, which knows nothing of caps.
        assert!(ViewParams::parse("more=tags&q=street")
            .to_index_query()
            .text
            .is_some());
    }

    /// The panel's heading counts the filters on, so a phone reading a
    /// collapsed panel still knows the grid is narrowed — and the page number,
    /// the sort and the tile size are not filters.
    #[test]
    fn the_panel_counts_the_filters_and_nothing_else() {
        assert_eq!(ViewParams::default().active_filter_count(), 0);
        assert_eq!(
            ViewParams::parse("size=l&sort=name&p=4").active_filter_count(),
            0
        );
        assert_eq!(ViewParams::parse("in=refs").active_filter_count(), 1);
        assert_eq!(
            ViewParams::parse("in=refs&tag=a&tag=b&q=x&untagged=1&recent=1&c=s.md")
                .active_filter_count(),
            7,
            "folder, collection, two tags, words, and the two lenses"
        );
        // A facet list shown whole is a longer list, not a narrower view.
        assert_eq!(ViewParams::parse("more=tags").active_filter_count(), 0);
    }
}
