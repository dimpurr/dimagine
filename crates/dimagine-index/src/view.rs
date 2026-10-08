//! Read-only view queries over the index (Viewer Phase 1 backend).
//!
//! These functions answer the questions a viewer UI asks: which images are in
//! a folder (recursively or not), which match some tags and text, how are they
//! ordered, which collections exist and what do they contain. They are pure
//! reads over the SQLite index and never touch the library files.
//!
//! Paths are library-relative and `/`-separated, the same spelling the scan
//! stored. `folder` uses `""` for the library root.

use crate::{ImageMeta, Index, IndexError};
use rusqlite::OptionalExtension;

/// The highest `rating` FORMAT §3.1 allows.
const MAX_RATING: u8 = 5;

/// What the index knows about image headers, counted ([`Index::image_meta_stats`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageMetaStats {
    /// Image rows the index holds.
    pub images: u64,
    /// Images whose pixel dimensions were read.
    pub with_dimensions: u64,
    /// Images whose EXIF taken time was read.
    pub with_taken: u64,
    /// Images whose header stayed unknown (dimensions `NULL`), counted on
    /// every scan, because the unknown is a recorded fact about the content
    /// and not a transient failure only the first scan sees.
    pub unreadable_headers: u64,
}

/// How to order a [`ViewPage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortKey {
    /// `added_ns` (note `added`, else `imported`, else `first_seen_ns`).
    Added,
    /// `mtime_ns`.
    Modified,
    /// File name, case-insensitive.
    Name,
    /// `size`.
    Size,
    /// Note `rating`; notes without a rating sort last in both directions.
    Rating,
}

/// One image in a folder, collection or search result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewItem {
    /// Library-relative image path.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// The derived "added" time in ns since the Unix epoch.
    pub added_ns: i64,
    /// Note `title`, when the image has a note with one.
    pub title: Option<String>,
    /// Note `rating` (0-5), when the image has a note with a valid one. A
    /// value outside 0-5, or one that is not an integer, is read as none.
    pub rating: Option<u8>,
    /// Library-relative path of the image's note, when it has one.
    pub note_path: Option<String>,
}

/// A page of [`ViewItem`] plus the total match count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewPage {
    /// Total images matching the query, before `offset`/`limit`.
    pub total: u64,
    pub items: Vec<ViewItem>,
}

/// A collection note and how many image members it has.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionInfo {
    /// Library-relative path of the collection note.
    pub note_path: String,
    /// Note `title`.
    pub title: String,
    /// Number of image members (embeds that resolve to images).
    pub member_count: u64,
    /// Whether the collection list carries this note (the sidebar, the
    /// `/collections` page and the `/api/sidebar` `collections` array). An
    /// unlisted collection is still a collection: [`ViewQuery::collection`]
    /// shows its members and [`Index::appears_in`] names it. See
    /// [`collection_is_listed`] for the rule.
    pub listed: bool,
}

/// What one note offers as evidence of being a collection (FORMAT §5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CollectionEvidence<'a> {
    /// The note's `kind` property.
    pub kind: Option<&'a str>,
    /// The note is the note of one image (`<image>.<ext>.md`, FORMAT §3): its
    /// self-embed is a preview, not a membership (FORMAT §3.2).
    pub image_note: bool,
    /// Image embeds that resolve to exactly one image and are not the image
    /// note's own self-embed (FORMAT §3.2).
    pub members: usize,
    /// Strong image embeds that resolved to no single image: a note that means
    /// to collect images is one even when an embed is broken.
    pub unresolved: usize,
}

/// FORMAT §5: a collection is a Markdown note that embeds images, and
/// `kind: collection` exists so tools list it as one. FORMAT §3.2: an image
/// note's self-embed is a preview, not a membership, so it never makes a
/// collection on its own.
///
/// This is the one rule both the index queries and `dimagine-serve` use, so a
/// viewer and the renderer never disagree about what a collection is.
pub fn note_is_collection(evidence: &CollectionEvidence<'_>) -> bool {
    evidence.members > 0 || evidence.unresolved > 0 || evidence.kind == Some("collection")
}

/// CTO decision for the collection *list*, on top of FORMAT §5. The list a
/// person browses — the sidebar section, the `/collections` page and the
/// `/api/sidebar` `collections` array — carries a note only when it means to
/// collect: it says `kind: collection` (FORMAT §5), or it is not the note of
/// an image (FORMAT §3.2) and embeds at least one image.
///
/// Everything else stays as FORMAT §5 defines it: an image note that embeds
/// other images — Eagle-style notes embed their sibling previews, and on a
/// real library that buried the deliberate ones among 1,175 collections,
/// 1,170 of them image notes — is still a collection. It is reachable as
/// `/?c=<note>`, its members keep their order, and "appears in" on the image
/// page still names it; the list simply does not carry it.
///
/// Like [`note_is_collection`], this is the one rule both the index and
/// `dimagine-serve` read, so the list never disagrees with the index API:
/// `Index::collections` reports every collection with a `listed` flag, and
/// the serve surfaces that show a list filter by it.
pub fn collection_is_listed(evidence: &CollectionEvidence<'_>) -> bool {
    note_is_collection(evidence) && (evidence.kind == Some("collection") || !evidence.image_note)
}

/// The parameters of a [`Index::view`] query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewQuery {
    /// Library-relative folder; `None` means every folder, `Some("")` means
    /// the library root.
    pub folder: Option<String>,
    /// When true (the default) a folder matches its subfolders too.
    pub recursive: bool,
    /// Restrict to members of this collection note, in collection-note order.
    /// When set, `sort` and `descending` are ignored: the collection's own
    /// embed order is the only meaningful order for its members.
    pub collection: Option<String>,
    /// The image's note must carry all of these tags (AND, case-insensitive).
    pub tags: Vec<String>,
    /// Full-text filter over the image's note, reusing [`Index::search_text`]
    /// semantics (trigram FTS for 3+ chars, substring for shorter).
    pub text: Option<String>,
    /// Only images whose note carries no tag at all. The viewer's "Untagged"
    /// lens; a tag filter cannot express it, because "no tags" is not a tag.
    pub untagged: bool,
    /// Only images added at or after this instant, in ns since the Unix
    /// epoch. A general time-window filter; the viewer's Recent lens counts
    /// images instead of days (RECENT_LIMIT in dimagine-serve).
    pub added_after_ns: Option<i64>,
    pub sort: SortKey,
    pub descending: bool,
    pub offset: u32,
    pub limit: u32,
}

impl Default for ViewQuery {
    fn default() -> Self {
        Self {
            folder: None,
            recursive: true,
            collection: None,
            tags: Vec::new(),
            text: None,
            untagged: false,
            added_after_ns: None,
            sort: SortKey::Added,
            descending: true,
            offset: 0,
            limit: 50,
        }
    }
}

/// The columns one view row is made of, read from the image row and its note
/// in a single pass: no row's properties are parsed in Rust any more.
const VIEW_COLUMNS: &str = "f.path, f.size, f.mtime_ns, f.added_ns, f.rating, n.path, \
     CASE WHEN json_type(n.props_json,'$.title')='text' \
       THEN json_extract(n.props_json,'$.title') END";

/// A view query under construction. `params` has to stay in the order the `?`
/// placeholders appear in `from` and then in `where_sql`; every value is bound,
/// never interpolated.
struct ViewSql {
    /// A leading `WITH ...` clause, empty unless a collection is queried: a
    /// common table expression belongs in front of the `SELECT`, not inside
    /// its `FROM`.
    with: String,
    from: String,
    /// The tables the count needs. No condition refers to `notes`, so counting
    /// never joins it.
    count_from: String,
    where_sql: String,
    params: Vec<Box<dyn rusqlite::ToSql>>,
    /// The `ORDER BY` of a non-collection query.
    order_by: String,
    /// A collection query orders by the note's own embed order instead.
    collection: bool,
}

impl ViewSql {
    fn new() -> Self {
        Self {
            with: String::new(),
            from: String::from("FROM files f LEFT JOIN notes n ON n.image_path=f.path"),
            count_from: String::from("FROM files f"),
            where_sql: String::from("f.kind='image'"),
            params: Vec::new(),
            order_by: String::new(),
            collection: false,
        }
    }

    fn bind(&mut self, value: impl rusqlite::ToSql + 'static) {
        self.params.push(Box::new(value));
    }

    fn and(&mut self, condition: &str) {
        self.where_sql.push_str(" AND ");
        self.where_sql.push_str(condition);
    }

    /// How many images match, before paging.
    fn count(&self) -> String {
        format!(
            "{with}SELECT count(*) {from} WHERE {where_sql}",
            with = self.with,
            from = self.count_from,
            where_sql = self.where_sql
        )
    }

    /// One page of images.
    ///
    /// The page is chosen from the image rows alone and the notes are joined
    /// afterwards, so a page of fifty never looks up twenty thousand notes;
    /// the outer `ORDER BY` restores the order the page was picked in. The two
    /// paging parameters are bound, so they are the last placeholders.
    fn rows(&self) -> String {
        let inner_order = self.order_by();
        let (outer_order, page_columns): (&str, &str) = if self.collection {
            (" ORDER BY page.ord", ", m.ord AS ord")
        } else {
            (inner_order.as_str(), "")
        };
        format!(
            "{with}SELECT {VIEW_COLUMNS} \
             FROM (SELECT f.path{page_columns} {from} WHERE {where_sql}{inner_order} \
                   LIMIT ? OFFSET ?) AS page \
             JOIN files f ON f.path=page.path \
             LEFT JOIN notes n ON n.image_path=f.path{outer_order}",
            with = self.with,
            from = self.from,
            where_sql = self.where_sql,
            inner_order = inner_order,
            outer_order = outer_order,
            page_columns = page_columns
        )
    }

    /// The SQL and the parameters of one page query: the filters first, then
    /// the paging pair, which is where the two page placeholders sit.
    fn into_page(mut self, limit: i64, offset: i64) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        self.params.push(Box::new(limit));
        self.params.push(Box::new(offset));
        (self.rows(), self.params)
    }

    fn order_by(&self) -> String {
        if self.collection {
            String::from(" ORDER BY m.ord")
        } else {
            self.order_by.clone()
        }
    }
}

/// Build the SQL for one [`Index::view`] query: filtering, sorting and paging
/// all happen in SQLite, so the sort columns and the tag table carry indexes
/// instead of every call reading the whole index into memory.
fn view_sql(q: &ViewQuery) -> ViewSql {
    view_sql_with_order(q, order_by(q))
}

/// Build the SQL for one page query under an explicit `ORDER BY` — the
/// subquery, connectors and filters of a view stay identical, only the chosen
/// order differs ([`view_sql`] wraps this with [`order_by`],
/// [`Index::view_by_taken`] with [`order_by_taken`]).
fn view_sql_with_order(q: &ViewQuery, order_by: String) -> ViewSql {
    let mut sql = ViewSql::new();
    if let Some(collection) = &q.collection {
        // The collection's own embed order is the only meaningful order for
        // its members, so the links table's insertion order is the sort key.
        // The self-embed is excluded here, once, for both members and count.
        sql.with = String::from(
            "WITH members(ord,path) AS ( \
               SELECT rowid,target FROM links WHERE src=? AND syntax<>'wiki_link' \
                 AND target IS NOT NULL \
                 AND target<>COALESCE((SELECT image_path FROM notes WHERE path=?),'') \
             ) ",
        );
        sql.from = String::from(
            "FROM files f JOIN members m ON m.path=f.path \
             LEFT JOIN notes n ON n.image_path=f.path",
        );
        sql.count_from = sql.from.clone();
        sql.bind(collection.clone());
        sql.bind(collection.clone());
        sql.collection = true;
    } else {
        sql.order_by = order_by;
    }
    if let Some(folder) = &q.folder {
        if !q.recursive {
            sql.and("f.folder=?");
            sql.bind(folder.clone());
        } else if !folder.is_empty() {
            // A range on the path, not a LIKE: every path inside the folder
            // starts with `folder/`, so the range is exactly the recursive
            // set, `refs` cannot reach `refs2` or `refs.jpg`, and
            // `LIKE ... ESCAPE` (which cannot use the index) is not needed.
            sql.and("(f.path>=? AND f.path<?)");
            sql.bind(format!("{folder}/"));
            sql.bind(format!("{folder}0"));
        }
        // An empty folder means the library root, which every path is in.
    }
    for tag in &q.tags {
        sql.and(
            "EXISTS(SELECT 1 FROM note_tags \
             WHERE note_tags.image_path=f.path AND note_tags.tag=?)",
        );
        sql.bind(crate::searchable(tag));
    }
    if q.untagged {
        // "No tags" is not a tag, so it cannot be a `note_tags` lookup: it is
        // the absence of a row, which is what NOT EXISTS states.
        sql.and("NOT EXISTS(SELECT 1 FROM note_tags WHERE note_tags.image_path=f.path)");
    }
    if let Some(after) = q.added_after_ns {
        sql.and("f.added_ns>=?");
        sql.bind(after);
    }
    if let Some(text) = &q.text {
        // The same two rules as `Index::search_text`: the trigram tokenizer
        // needs three characters, shorter terms match as substrings.
        let query = crate::searchable(text);
        if query.chars().count() < 3 {
            let escaped = query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            sql.and(
                "f.path IN (SELECT image_path FROM notes WHERE image_path IS NOT NULL \
                 AND (title LIKE ? ESCAPE '\\' OR tags LIKE ? ESCAPE '\\' \
                 OR body LIKE ? ESCAPE '\\'))",
            );
            sql.bind(pattern.clone());
            sql.bind(pattern.clone());
            sql.bind(pattern);
        } else {
            sql.and(
                "f.path IN (SELECT matched.image_path FROM notes matched \
                 JOIN notes_fts ON notes_fts.rowid=matched.note_id \
                 WHERE notes_fts MATCH ? AND matched.image_path IS NOT NULL)",
            );
            sql.bind(format!("\"{}\"", query.replace('"', "\"\"")));
        }
    }
    sql
}

/// The `ORDER BY` for one sort: the sort column, ties always broken by path
/// ascending so the order is deterministic. A missing rating sorts last in both
/// directions, which needs the flag before the rating and never flipped.
fn order_by(q: &ViewQuery) -> String {
    let direction = if q.descending { "DESC" } else { "ASC" };
    let key = match q.sort {
        SortKey::Added => "f.added_ns",
        SortKey::Modified => "f.mtime_ns",
        SortKey::Name => "f.name_key",
        SortKey::Size => "f.size",
        SortKey::Rating => {
            return format!(" ORDER BY (f.rating IS NULL), f.rating {direction}, f.path ASC")
        }
    };
    format!(" ORDER BY {key} {direction}, f.path ASC")
}

/// The `ORDER BY` of [`Index::view_by_taken`]: the same shape a [`SortKey`]
/// sort uses, over the EXIF taken time instead. Taken times order
/// chronologically in the asked direction, ties break by path ascending, and
/// an image with no taken time — an unknown instant, not an old or a young
/// one — sorts last in both directions, the flag before the value, never
/// flipped with the direction. A collection query keeps its own embed order,
/// which always comes first (the rule [`ViewQuery::sort`] follows).
fn order_by_taken(q: &ViewQuery) -> String {
    let direction = if q.descending { "DESC" } else { "ASC" };
    format!(" ORDER BY (f.taken_ns IS NULL), f.taken_ns {direction}, f.path ASC")
}

/// The columns that decide whether a note is a collection (FORMAT §5): its
/// path and title source, whether it is the note of an image (FORMAT §3.2,
/// which the list rule reads), the number of image members (self-embed
/// excluded, FORMAT §3.2) and whether a strong image embed failed to resolve.
const CANDIDATE_COLUMNS: &str = "notes.path, notes.props_json, \
     (SELECT count(*) FROM links WHERE links.src=notes.path AND links.syntax<>'wiki_link' \
        AND links.target IS NOT NULL \
        AND links.target IN (SELECT path FROM files WHERE kind='image') \
        AND links.target<>COALESCE(notes.image_path,'')) AS members, \
     EXISTS(SELECT 1 FROM links WHERE links.src=notes.path AND links.syntax<>'wiki_link' \
        AND links.target IS NULL) AS unresolved, \
     (notes.image_path IS NOT NULL) AS image_note";

/// A cheap superset of [`note_is_collection`], parenthesised because callers
/// append their own `AND` conditions: the note has a strong image embed that
/// is not its own self-embed (FORMAT §3.2), or an embed that did not resolve,
/// or it says it is a collection. SQL narrows the candidates, the shared rule
/// decides, so the two never drift apart.
const CANDIDATE_WHERE: &str =
    "(EXISTS(SELECT 1 FROM links WHERE links.src=notes.path AND links.syntax<>'wiki_link' \
        AND (links.target IS NULL OR links.target<>COALESCE(notes.image_path,''))) \
     OR json_extract(notes.props_json,'$.kind')='collection')";

/// One note the queries still have to judge with [`note_is_collection`].
struct CollectionCandidate {
    path: String,
    props: serde_json::Value,
    /// The note is the note of an image (FORMAT §3.2), which the list rule
    /// reads.
    image_note: bool,
    members: usize,
    unresolved: usize,
}

impl CollectionCandidate {
    fn evidence(&self) -> CollectionEvidence<'_> {
        CollectionEvidence {
            kind: self.props.get("kind").and_then(serde_json::Value::as_str),
            image_note: self.image_note,
            members: self.members,
            unresolved: self.unresolved,
        }
    }
}

impl Index {
    /// Query images by folder, collection, tags and text, sorted and paged.
    ///
    /// Only images (`kind = 'image'`) are returned. When `collection` is set
    /// the members are returned in collection-note order (the order their
    /// embeds appear in the note) and `sort`/`descending` are ignored.
    pub fn view(&self, q: &ViewQuery) -> Result<ViewPage, IndexError> {
        self.view_page(|| view_sql(q), q.limit, q.offset)
    }

    /// Query images exactly like [`Index::view`], but ordered by the EXIF
    /// taken time ([`order_by_taken`]) instead of [`ViewQuery::sort`]: the
    /// read API a viewer's "Taken" sort is built on. Only images are
    /// returned; every filter and the paging behave as in [`Index::view`],
    /// and a `collection` query still answers in the collection's own embed
    /// order, which always comes first.
    pub fn view_by_taken(&self, q: &ViewQuery) -> Result<ViewPage, IndexError> {
        self.view_page(
            || view_sql_with_order(q, order_by_taken(q)),
            q.limit,
            q.offset,
        )
    }

    /// Run one page query, fresh-built twice because the count and the page
    /// bind temporary `ViewSql` state (the paging pair).
    fn view_page(
        &self,
        build: impl Fn() -> ViewSql,
        limit: u32,
        offset: u32,
    ) -> Result<ViewPage, IndexError> {
        let sql = build();
        let total: i64 = self
            .conn()?
            .query_row(&sql.count(), bound(&sql.params), |row| row.get(0))?;
        let (page_sql, page_params) = build().into_page(i64::from(limit), i64::from(offset));
        let mut stmt = self.conn()?.prepare(&page_sql)?;
        let rows = stmt.query_map(bound(&page_params), |row| {
            let size: i64 = row.get(1)?;
            let rating: Option<i64> = row.get(4)?;
            Ok(ViewItem {
                path: row.get(0)?,
                size: size.max(0) as u64,
                mtime_ns: row.get(2)?,
                added_ns: row.get(3)?,
                title: row.get(6)?,
                rating: rating.and_then(|value| u8::try_from(value).ok()),
                note_path: row.get(5)?,
            })
        })?;
        let items = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(ViewPage {
            total: total.max(0) as u64,
            items,
        })
    }

    /// How SQLite plans one [`Index::view`] query, one line per step. A
    /// diagnostic for a slow viewer query: it shows whether the indexes earn
    /// their keep or the planner fell back to scanning the index.
    pub fn view_query_plan(&self, q: &ViewQuery) -> Result<Vec<String>, IndexError> {
        self.plan_of(|| view_sql(q), q.limit, q.offset)
    }

    /// The same diagnostic for one [`Index::view_by_taken`] query.
    pub fn taken_query_plan(&self, q: &ViewQuery) -> Result<Vec<String>, IndexError> {
        self.plan_of(
            || view_sql_with_order(q, order_by_taken(q)),
            q.limit,
            q.offset,
        )
    }

    fn plan_of(
        &self,
        build: impl Fn() -> ViewSql,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<String>, IndexError> {
        let (count_sql, count_params) = {
            let sql = build();
            (sql.count(), sql.params)
        };
        let (page_sql, page_params) = build().into_page(i64::from(limit), i64::from(offset));
        let mut lines = Vec::new();
        for (label, statement, params) in [
            ("count", &count_sql, &count_params),
            ("page", &page_sql, &page_params),
        ] {
            let mut stmt = self
                .conn()?
                .prepare(&format!("EXPLAIN QUERY PLAN {statement}"))?;
            let rows = stmt.query_map(bound(params), |row| {
                Ok(format!(
                    "{label}: {} {}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(3)?
                ))
            })?;
            lines.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(lines)
    }

    /// The per-image header metadata of one path, as the refresh read it:
    /// pixel dimensions after EXIF orientation, and the EXIF "taken" time.
    /// `None` fields are honest unknowns — the header (or the date) could not
    /// be read — and `None` of the whole answer means the index holds no such
    /// image. A viewer's justified row asks its `width` here; a "Taken"
    /// detail asks its `taken_ns` here.
    ///
    /// This is a companion to [`Index::view`], not part of its page shape:
    /// the viewer's own page JSON is the index API's hand-mirrored mirror,
    /// and it grows when the viewer does, not when the index does.
    pub fn image_meta(&self, path: &str) -> Result<Option<ImageMeta>, IndexError> {
        let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = self
            .conn()?
            .query_row(
                "SELECT width,height,taken_ns FROM files WHERE path=?1 AND kind='image'",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(width, height, taken_ns)| ImageMeta {
            width: width.and_then(|value| u32::try_from(value).ok()),
            height: height.and_then(|value| u32::try_from(value).ok()),
            taken_ns,
        }))
    }

    /// What the index knows about image headers, counted: how many images
    /// there are, how many have dimensions, a taken time, or a header that
    /// could not be read (dims unknown). The counted warning of a refresh
    /// — and of every scan that follows, because the unknowns are recorded
    /// facts about content, not transient failures.
    pub fn image_meta_stats(&self) -> Result<ImageMetaStats, IndexError> {
        let row: (i64, i64, i64, i64) = self.conn()?.query_row(
            // `coalesce`, because `sum` over no image rows is NULL and an
            // empty library is zero unknowns, not an error.
            "SELECT count(*), count(width), count(taken_ns), \
                    coalesce(sum(CASE WHEN width IS NULL THEN 1 ELSE 0 END), 0) \
             FROM files WHERE kind='image'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let max_zero = |value: i64| value.max(0) as u64;
        Ok(ImageMetaStats {
            images: max_zero(row.0),
            with_dimensions: max_zero(row.1),
            with_taken: max_zero(row.2),
            unreadable_headers: max_zero(row.3),
        })
    }

    /// Image count per folder, including subfolders. Every folder that holds
    /// an image directly or in a subfolder appears, `""` is the library root.
    /// The folder tree is a recursive query over the folder column and each
    /// count is one range scan, so nothing walks the whole library.
    pub fn folder_counts(&self) -> Result<Vec<(String, u64)>, IndexError> {
        // Every folder that holds an image directly or in a subfolder is a
        // prefix of some image path, so the folders are built by peeling one
        // path segment off each folder that holds an image, and each count is
        // one range scan over the (kind, path) index. Nothing walks the
        // library.
        let mut stmt = self.conn()?.prepare(
            "WITH RECURSIVE prefixes(folder,rest) AS ( \
               SELECT '', folder || '/' FROM ( \
                 SELECT DISTINCT folder FROM files WHERE kind='image') WHERE folder<>'' \
               UNION ALL \
               SELECT prefixes.folder || CASE WHEN prefixes.folder='' THEN '' ELSE '/' END \
                    || substr(prefixes.rest,1,instr(prefixes.rest,'/')-1), \
                 substr(prefixes.rest, instr(prefixes.rest,'/')+1) \
               FROM prefixes WHERE instr(prefixes.rest,'/')>0 \
             ), tree(folder) AS ( \
               SELECT DISTINCT folder FROM prefixes WHERE folder<>'' UNION SELECT '' \
             ) SELECT tree.folder, CASE WHEN tree.folder='' \
                 THEN (SELECT count(*) FROM files WHERE kind='image') ELSE ( \
                 SELECT count(*) FROM files f WHERE f.kind='image' \
                   AND f.path>=tree.folder||'/' AND f.path<tree.folder||'0') END \
             FROM tree ORDER BY tree.folder",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?.max(0) as u64,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Image count per tag, over the tags of image notes that still have an
    /// image. A note whose image is gone is not counted: it cannot appear in
    /// the list either, and a sidebar that disagrees with the list is worse
    /// than useless. Case-insensitive.
    pub fn tag_counts(&self) -> Result<Vec<(String, u64)>, IndexError> {
        let mut stmt = self.conn()?.prepare(
            "SELECT note_tags.tag, count(*) FROM note_tags \
             JOIN files ON files.path=note_tags.image_path AND files.kind='image' \
             GROUP BY note_tags.tag ORDER BY note_tags.tag",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?.max(0) as u64,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Notes that are collections (FORMAT §5), with their image member count
    /// and whether the collection list carries each one
    /// ([`collection_is_listed`]). Every collection is here, listed or not,
    /// so the API keeps naming what `/?c=` accepts.
    pub fn collections(&self) -> Result<Vec<CollectionInfo>, IndexError> {
        Ok(self
            .collection_candidates(
                &format!(
                    "SELECT {CANDIDATE_COLUMNS} FROM notes \
                     WHERE {CANDIDATE_WHERE} ORDER BY notes.path"
                ),
                [],
            )?
            .into_iter()
            .filter(|candidate| note_is_collection(&candidate.evidence()))
            .map(|candidate| {
                let listed = collection_is_listed(&candidate.evidence());
                CollectionInfo {
                    title: candidate
                        .props
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    note_path: candidate.path,
                    member_count: candidate.members as u64,
                    listed,
                }
            })
            .collect())
    }

    /// Whether one note is a collection (FORMAT §5), answered from its own row.
    ///
    /// The legacy `/collection/<path>` route asks this question once per
    /// request, so it must not walk the whole list: `WHERE notes.path=?1` is a
    /// point lookup on the note's unique path, and the candidate predicate is
    /// the one [`Index::collections`] uses, so the two can never disagree about
    /// what a collection is or which of them the list carries. A path that is
    /// not a note at all is simply not a collection, never an error.
    pub fn is_collection(&self, path: &str) -> Result<bool, IndexError> {
        Ok(self
            .collection_candidates(
                &format!(
                    "SELECT {CANDIDATE_COLUMNS} FROM notes \
                     WHERE notes.path=?1 AND {CANDIDATE_WHERE}"
                ),
                [path],
            )?
            .into_iter()
            .any(|candidate| note_is_collection(&candidate.evidence())))
    }

    /// Collection note paths that embed `image_path`. An image note's own
    /// self-embed never counts (FORMAT §3.2).
    pub fn appears_in(&self, image_path: &str) -> Result<Vec<String>, IndexError> {
        Ok(self
            .collection_candidates(
                &format!(
                    "SELECT {CANDIDATE_COLUMNS} FROM notes WHERE {CANDIDATE_WHERE} \
                     AND EXISTS(SELECT 1 FROM links WHERE links.src=notes.path \
                         AND links.target=?1 AND links.syntax<>'wiki_link' \
                         AND links.target<>COALESCE(notes.image_path,'')) \
                     ORDER BY notes.path"
                ),
                [image_path],
            )?
            .into_iter()
            .filter(|candidate| note_is_collection(&candidate.evidence()))
            .map(|candidate| candidate.path)
            .collect())
    }

    /// Run one `SELECT {CANDIDATE_COLUMNS} FROM notes WHERE ...` query.
    fn collection_candidates<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<CollectionCandidate>, IndexError> {
        let mut stmt = self.conn()?.prepare(sql)?;
        let rows = stmt.query_map(params, |row| {
            Ok(CollectionCandidate {
                path: row.get(0)?,
                props: serde_json::from_str(&row.get::<_, String>(1)?).unwrap_or_default(),
                members: row.get::<_, i64>(2)?.max(0) as usize,
                unresolved: row.get::<_, i64>(3)?.max(0) as usize,
                image_note: row.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `EXPLAIN QUERY PLAN` for a statement, so a test can check that the
    /// planner uses the indexes rather than rebuilding one on every call.
    #[cfg(test)]
    fn explain(&self, sql: &str, params: &[Box<dyn rusqlite::ToSql>]) -> Vec<String> {
        let mut stmt = self
            .conn()
            .unwrap()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = stmt
            .query_map(bound(params), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(3)?))
            })
            .unwrap();
        rows.map(|row| row.unwrap())
            .map(|(id, detail)| format!("{id}: {detail}"))
            .collect()
    }
}

/// Bind the parameters of a query whose values were collected in order.
fn bound(params: &[Box<dyn rusqlite::ToSql>]) -> impl rusqlite::Params + '_ {
    rusqlite::params_from_iter(params.iter().map(|value| value.as_ref()))
}

/// FORMAT §3.1 defines `rating` as an integer 0-5. A value outside that range,
/// and any value that is not an integer, is not a rating: it reads as "no
/// rating", so it sorts last and never above a real five-star rating.
///
/// The scan stores the result on the image row, and the queries read it from
/// there; this is the one place the rule is written down.
pub fn note_rating(props: &serde_json::Value) -> Option<u8> {
    let rating = props.get("rating")?.as_u64()?;
    if rating > u64::from(MAX_RATING) {
        return None;
    }
    u8::try_from(rating).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileRecord, NoteRecord};

    fn props(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn rating_is_an_integer_between_zero_and_five() {
        assert_eq!(note_rating(&props(r#"{"rating":5}"#)), Some(5));
        assert_eq!(note_rating(&props(r#"{"rating":0}"#)), Some(0));
        assert_eq!(note_rating(&props(r#"{"rating":7}"#)), None);
        assert_eq!(note_rating(&props(r#"{"rating":300}"#)), None);
        assert_eq!(note_rating(&props(r#"{"rating":"4"}"#)), None);
        assert_eq!(note_rating(&props(r#"{"rating":4.5}"#)), None);
        assert_eq!(note_rating(&props(r#"{"rating":-1}"#)), None);
        assert_eq!(note_rating(&props(r#"{}"#)), None);
    }

    #[test]
    fn a_collection_is_an_embed_or_an_explicit_kind() {
        let collection = |kind, image_note, members, unresolved| {
            note_is_collection(&CollectionEvidence {
                kind,
                image_note,
                members,
                unresolved,
            })
        };
        assert!(collection(Some("collection"), true, 0, 0));
        assert!(collection(None, true, 1, 0), "an image embed is enough");
        assert!(
            collection(None, false, 0, 1),
            "a broken image embed is enough"
        );
        assert!(!collection(None, false, 0, 0));
        assert!(!collection(Some("note"), false, 0, 0));
    }

    /// W27f: the list rule on top of FORMAT §5. `kind: collection` always
    /// lists; every other collection lists when it is not the note of an image
    /// (FORMAT §3.2) — so an image note that embeds its siblings is a
    /// collection that the list leaves off.
    #[test]
    fn the_list_carries_kind_collections_and_plain_notes_not_image_notes() {
        let listed = |kind, image_note, members, unresolved| {
            collection_is_listed(&CollectionEvidence {
                kind,
                image_note,
                members,
                unresolved,
            })
        };
        assert!(listed(Some("collection"), true, 0, 0), "kind always lists");
        assert!(
            listed(Some("collection"), false, 0, 0),
            "with or without an image note"
        );
        assert!(
            listed(None, false, 1, 0),
            "a plain note with an embed lists"
        );
        assert!(
            listed(None, false, 0, 1),
            "a broken embed still means to collect"
        );
        assert!(
            !listed(None, true, 1, 0),
            "an image note embedding its sibling stays off the list"
        );
        assert!(
            !listed(None, false, 0, 0),
            "a note with nothing to embed is not listed"
        );
    }

    /// W27f end to end in the index API: an image note that embeds its
    /// siblings (and its own preview, FORMAT §3.2) is a collection the list
    /// leaves off, while membership, order and "appears in" still treat it
    /// as one (FORMAT §5).
    #[test]
    fn image_note_collections_are_off_the_list_but_answer_every_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str| FileRecord {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: None,
            rating: None,
            width: None,
            height: None,
            taken_ns: None,
        };
        let note = |path: &str, image_path: Option<&str>, props: &str| NoteRecord {
            path: path.to_owned(),
            image_path: image_path.map(str::to_owned),
            id: None,
            title: String::new(),
            tags: Vec::new(),
            props_json: props.to_owned(),
        };
        let embed = |src: &str, target: &str| crate::LinkRecord {
            src: src.to_owned(),
            raw: format!("![[{target}]]"),
            target: Some(target.to_owned()),
            state: crate::LinkState::Resolved,
            syntax: crate::LinkSyntax::WikiEmbed,
        };
        index.begin_scan().unwrap();
        index.upsert_file(&image("refs/page-01.png")).unwrap();
        index.upsert_file(&image("refs/page-02.png")).unwrap();
        // The image note: a sibling embed and its own preview (§3.2).
        index
            .upsert_note(&note(
                "refs/page-01.png.md",
                Some("refs/page-01.png"),
                r#"{"title":"Page one"}"#,
            ))
            .unwrap();
        index
            .replace_links(
                "refs/page-01.png.md",
                &[
                    embed("refs/page-01.png.md", "refs/page-02.png"),
                    embed("refs/page-01.png.md", "refs/page-01.png"),
                ],
            )
            .unwrap();
        // A plain note that embeds an image: a collection, and listed.
        index
            .upsert_note(&note("roundup.md", None, r#"{"title":"Roundup"}"#))
            .unwrap();
        index
            .replace_links("roundup.md", &[embed("roundup.md", "refs/page-01.png")])
            .unwrap();
        // `kind: collection` with no embeds at all: listed, always.
        index
            .upsert_note(&note(
                "guide.md",
                None,
                r#"{"kind":"collection","title":"Guide"}"#,
            ))
            .unwrap();
        index
            .finish_scan(&[
                "refs/page-01.png".into(),
                "refs/page-02.png".into(),
                "refs/page-01.png.md".into(),
                "roundup.md".into(),
                "guide.md".into(),
            ])
            .unwrap();

        // Every collection is named, with its place in the list recorded.
        let seen: Vec<(String, bool)> = index
            .collections()
            .unwrap()
            .into_iter()
            .map(|info| (info.note_path, info.listed))
            .collect();
        assert_eq!(
            seen,
            [
                ("guide.md".to_owned(), true),
                ("refs/page-01.png.md".to_owned(), false),
                ("roundup.md".to_owned(), true),
            ],
            "the image note is a collection the list leaves off"
        );

        // "Appears in" still names the image note (FORMAT §5).
        assert_eq!(
            index.appears_in("refs/page-02.png").unwrap(),
            vec!["refs/page-01.png.md".to_owned()]
        );

        // So does the collection view, in embed order and without the image
        // note's own image (FORMAT §3.2).
        let members = index
            .view(&ViewQuery {
                collection: Some("refs/page-01.png.md".into()),
                ..ViewQuery::default()
            })
            .unwrap();
        assert_eq!(members.total, 1);
        assert_eq!(members.items[0].path, "refs/page-02.png");
    }

    /// W39 review M2: the legacy `/collection/<path>` route asks about one
    /// note, so the index answers it from that note's own row. The rule is the
    /// list's (`note_is_collection`), and the lookup is an indexed search on
    /// the note's unique path rather than a walk over every collection.
    #[test]
    fn one_note_is_a_collection_without_listing_them_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str| FileRecord {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: None,
            rating: None,
            width: None,
            height: None,
            taken_ns: None,
        };
        let note = |path: &str, image_path: Option<&str>, props: &str| NoteRecord {
            path: path.to_owned(),
            image_path: image_path.map(str::to_owned),
            id: None,
            title: String::new(),
            tags: Vec::new(),
            props_json: props.to_owned(),
        };
        let embed = |src: &str, target: &str| crate::LinkRecord {
            src: src.to_owned(),
            raw: format!("![[{target}]]"),
            target: Some(target.to_owned()),
            state: crate::LinkState::Resolved,
            syntax: crate::LinkSyntax::WikiEmbed,
        };
        index.begin_scan().unwrap();
        index.upsert_file(&image("refs/page-01.png")).unwrap();
        index.upsert_file(&image("refs/page-02.png")).unwrap();
        // A listed collection: a plain note with an embed.
        index
            .upsert_note(&note("roundup.md", None, r#"{"title":"Roundup"}"#))
            .unwrap();
        index
            .replace_links("roundup.md", &[embed("roundup.md", "refs/page-01.png")])
            .unwrap();
        // An unlisted one: an image note embedding a sibling (FORMAT §3.2).
        index
            .upsert_note(&note(
                "refs/page-01.png.md",
                Some("refs/page-01.png"),
                r#"{"title":"Page one"}"#,
            ))
            .unwrap();
        index
            .replace_links(
                "refs/page-01.png.md",
                &[embed("refs/page-01.png.md", "refs/page-02.png")],
            )
            .unwrap();
        // A note that collects nothing at all.
        index.upsert_note(&note("plain.md", None, r#"{}"#)).unwrap();
        index
            .finish_scan(&[
                "refs/page-01.png".into(),
                "refs/page-02.png".into(),
                "refs/page-01.png.md".into(),
                "roundup.md".into(),
                "plain.md".into(),
            ])
            .unwrap();

        assert!(
            index.is_collection("roundup.md").unwrap(),
            "a listed collection"
        );
        assert!(
            index.is_collection("refs/page-01.png.md").unwrap(),
            "an unlisted collection is still a collection"
        );
        assert!(
            !index.is_collection("plain.md").unwrap(),
            "a note with no embed"
        );
        assert!(
            !index.is_collection("refs/page-01.png").unwrap(),
            "an image, not a note"
        );
        assert!(
            !index.is_collection("nope/missing.md").unwrap(),
            "a path that is not there is not a collection, and not an error"
        );

        // The question costs one indexed row, not a walk over the collections.
        let params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new("roundup.md")];
        let plan = index
            .explain(
                &format!(
                    "SELECT {CANDIDATE_COLUMNS} FROM notes \
                     WHERE notes.path=?1 AND {CANDIDATE_WHERE}"
                ),
                &params,
            )
            .join("; ");
        assert!(plan.contains("SEARCH"), "no point lookup: {plan}");
        assert!(!plan.contains("SCAN notes"), "a full scan: {plan}");
    }

    /// RW26 M-5: every view query has to be answered from the indexes. The
    /// plan must never fall back to a scan of `files` or `notes`, and never
    /// rebuild an automatic index on the way, which is what the whole fetch
    /// into Rust used to do on every call.
    #[test]
    fn view_queries_are_answered_from_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        for folder in ["", "refs", "refs/deep", "refs2"] {
            for index_in_folder in 0..50i64 {
                let path = if folder.is_empty() {
                    format!("root-{index_in_folder:02}.jpg")
                } else {
                    format!("{folder}/img-{index_in_folder:02}.jpg")
                };
                index
                    .upsert_file(&FileRecord {
                        path: path.clone(),
                        size: (1024 + index_in_folder) as u64,
                        mtime_ns: 1_000_000 + index_in_folder,
                        sha256: None,
                        kind: crate::FileKind::Image,
                        note_added_ns: Some(1_700_000_000_000_000_000 + index_in_folder),
                        rating: Some((index_in_folder % 6) as u8),
                        width: Some((index_in_folder % 9 + 1) as u32),
                        height: Some((index_in_folder % 4 + 1) as u32),
                        taken_ns: (index_in_folder % 3 != 0)
                            .then_some(1_600_000_000_000_000_000 + index_in_folder),
                    })
                    .unwrap();
                let note_path = format!("{path}.md");
                let mut note = NoteRecord {
                    path: note_path.clone(),
                    image_path: Some(path.clone()),
                    id: None,
                    title: format!("Synthetic title {index_in_folder}"),
                    tags: vec![
                        "nature".to_owned(),
                        if index_in_folder % 2 == 0 {
                            "green".to_owned()
                        } else {
                            "blue".to_owned()
                        },
                    ],
                    props_json: format!(r#"{{"rating":{}}}"#, index_in_folder % 6),
                };
                note.image_path = Some(path.clone());
                index.upsert_note(&note).unwrap();
                index
                    .set_note_body(&note_path, &format!("Synthetic body {index_in_folder}"))
                    .unwrap();
                index
                    .replace_links(
                        &note_path,
                        &[crate::LinkRecord {
                            src: note_path.clone(),
                            raw: format!("![[{index_in_folder}]]"),
                            target: Some(path.clone()),
                            state: crate::LinkState::Resolved,
                            syntax: crate::LinkSyntax::WikiEmbed,
                        }],
                    )
                    .unwrap();
            }
        }
        index.finish_scan(&[]).unwrap();

        let queries: Vec<(&str, ViewQuery)> = vec![
            (
                "added descending",
                ViewQuery {
                    sort: SortKey::Added,
                    descending: true,
                    ..ViewQuery::default()
                },
            ),
            (
                "name ascending",
                ViewQuery {
                    sort: SortKey::Name,
                    descending: false,
                    ..ViewQuery::default()
                },
            ),
            (
                "rating descending",
                ViewQuery {
                    sort: SortKey::Rating,
                    descending: true,
                    ..ViewQuery::default()
                },
            ),
            (
                "taken descending",
                // The taken order is its own query (Index::view_by_taken),
                // planned here so it answers from an index the same way.
                ViewQuery {
                    descending: true,
                    ..ViewQuery::default()
                },
            ),
            (
                "size descending",
                ViewQuery {
                    sort: SortKey::Size,
                    descending: true,
                    ..ViewQuery::default()
                },
            ),
            (
                "recursive folder",
                ViewQuery {
                    folder: Some("refs".into()),
                    ..ViewQuery::default()
                },
            ),
            (
                "non-recursive folder",
                ViewQuery {
                    folder: Some("refs".into()),
                    recursive: false,
                    ..ViewQuery::default()
                },
            ),
            (
                "tags",
                ViewQuery {
                    tags: vec!["nature".into()],
                    ..ViewQuery::default()
                },
            ),
            (
                "text",
                ViewQuery {
                    text: Some("synthetic".into()),
                    ..ViewQuery::default()
                },
            ),
            (
                "untagged",
                ViewQuery {
                    untagged: true,
                    ..ViewQuery::default()
                },
            ),
            (
                "added after an instant",
                ViewQuery {
                    added_after_ns: Some(1_700_000_000_000_000_000),
                    ..ViewQuery::default()
                },
            ),
        ];
        for (name, query) in queries {
            let rendered = if name == "taken descending" {
                index.taken_query_plan(&query).unwrap()
            } else {
                index.view_query_plan(&query).unwrap()
            };
            assert!(
                !rendered.iter().any(|line| line.contains("AUTOMATIC")),
                "{name} rebuilds an automatic index:\n{}",
                rendered.join("\n")
            );
            assert!(
                rendered.iter().any(|line| line.contains("USING INDEX")
                    || line.contains("USING COVERING INDEX")
                    || line.contains("SEARCH")),
                "{name} does not use an index:\n{}",
                rendered.join("\n")
            );
        }

        // The counts are grouped in SQL too.
        let folders = index
            .explain(
                "SELECT folder, count(*) FROM files WHERE kind='image' GROUP BY folder",
                &[],
            )
            .join("; ");
        assert!(!folders.contains("AUTOMATIC"), "{folders}");
        let tags = index
            .explain("SELECT tag, count(*) FROM note_tags GROUP BY tag", &[])
            .join("; ");
        assert!(tags.contains("note_tags"), "{tags}");
    }

    /// W48: a taken sort is a real sort over values with an honest unknown.
    /// Images without a taken time — an unreadable or absent EXIF date — sort
    /// last in both directions (an unknown instant is neither oldest nor
    /// newest), values order ascending or descending as asked, and equal
    /// values keep the stable path tiebreak the other sorts use.
    #[test]
    fn taken_sort_is_nulls_last_in_both_directions_with_a_path_tiebreak() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str, taken_ns: Option<i64>| FileRecord {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: None,
            rating: None,
            width: Some(4),
            height: Some(3),
            taken_ns,
        };
        index.begin_scan().unwrap();
        // Taken on three intertwined days; two images tie at the same second
        // and two have no taken time at all.
        index
            .upsert_file(&image("newer.jpg", Some(1_700_000_000_000_000_001)))
            .unwrap();
        index
            .upsert_file(&image("older.jpg", Some(1_600_000_000_000_000_000)))
            .unwrap();
        index
            .upsert_file(&image("b-tie.jpg", Some(1_650_000_000_000_000_000)))
            .unwrap();
        index
            .upsert_file(&image("a-tie.jpg", Some(1_650_000_000_000_000_000)))
            .unwrap();
        index.upsert_file(&image("unknown-b.jpg", None)).unwrap();
        index.upsert_file(&image("unknown-a.jpg", None)).unwrap();
        index
            .finish_scan(&[
                "newer.jpg".into(),
                "older.jpg".into(),
                "b-tie.jpg".into(),
                "a-tie.jpg".into(),
                "unknown-b.jpg".into(),
                "unknown-a.jpg".into(),
            ])
            .unwrap();

        let paths = |descending: bool| -> Vec<String> {
            index
                .view_by_taken(&ViewQuery {
                    limit: 100,
                    descending,
                    ..ViewQuery::default()
                })
                .unwrap()
                .items
                .into_iter()
                .map(|item| item.path)
                .collect()
        };
        assert_eq!(
            paths(false),
            [
                "older.jpg".to_owned(),
                "a-tie.jpg".to_owned(),
                "b-tie.jpg".to_owned(),
                "newer.jpg".to_owned(),
                "unknown-a.jpg".to_owned(),
                "unknown-b.jpg".to_owned(),
            ],
            "ascending: oldest first, ties by path, unknowns last"
        );
        assert_eq!(
            paths(true),
            [
                "newer.jpg".to_owned(),
                "a-tie.jpg".to_owned(),
                "b-tie.jpg".to_owned(),
                "older.jpg".to_owned(),
                "unknown-a.jpg".to_owned(),
                "unknown-b.jpg".to_owned(),
            ],
            "descending: newest first, and the unknowns are still last"
        );
        // The same query page-by-page: a page boundary keeps the order,
        // because ORDER BY sits inside the paged subquery.
        let page = index
            .view_by_taken(&ViewQuery {
                limit: 2,
                offset: 2,
                descending: true,
                ..ViewQuery::default()
            })
            .unwrap();
        let paged: Vec<String> = page.items.iter().map(|item| item.path.clone()).collect();
        assert_eq!(paged, ["b-tie.jpg", "older.jpg"], "page two in order");

        // The per-image metadata the viewer's justified row and Taken detail
        // read: unknowns are distinct from values, per axis.
        assert_eq!(
            index.image_meta("b-tie.jpg").unwrap(),
            Some(crate::ImageMeta {
                width: Some(4),
                height: Some(3),
                taken_ns: Some(1_650_000_000_000_000_000),
            })
        );
        assert_eq!(
            index.image_meta("unknown-a.jpg").unwrap(),
            Some(crate::ImageMeta {
                width: Some(4),
                height: Some(3),
                taken_ns: None,
            }),
            "an unknown taken time is not an unknown dimension"
        );
        assert_eq!(
            index.image_meta("refs/absent.jpg").unwrap(),
            None,
            "a path the index holds no image at is no metadata at all"
        );
        assert_eq!(
            index.image_meta_stats().unwrap(),
            ImageMetaStats {
                images: 6,
                with_dimensions: 6,
                with_taken: 4,
                unreadable_headers: 0
            }
        );
    }

    /// W48: the taken-order stats count what they should, including the
    /// headers no reader could parse.
    #[test]
    fn image_meta_stats_count_the_unknown_headers() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str, width: Option<u32>, taken_ns: Option<i64>| FileRecord {
            path: path.to_owned(),
            size: 1,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: None,
            rating: None,
            width,
            height: width.map(|height| height + 1),
            taken_ns,
        };
        index.begin_scan().unwrap();
        index
            .upsert_file(&image("healthy.jpg", Some(640), Some(1)))
            .unwrap();
        index
            .upsert_file(&image("bare-dims.jpg", Some(12), None))
            .unwrap();
        index.upsert_file(&image("broken.jpg", None, None)).unwrap();
        index
            .upsert_file(&FileRecord {
                path: "note.md".into(),
                size: 1,
                mtime_ns: 1,
                sha256: None,
                kind: crate::FileKind::Note,
                note_added_ns: None,
                rating: None,
                width: None,
                height: None,
                taken_ns: None,
            })
            .unwrap();
        index
            .finish_scan(&[
                "healthy.jpg".into(),
                "bare-dims.jpg".into(),
                "broken.jpg".into(),
                "note.md".into(),
            ])
            .unwrap();
        assert_eq!(
            index.image_meta_stats().unwrap(),
            ImageMetaStats {
                images: 3,
                with_dimensions: 2,
                with_taken: 1,
                unreadable_headers: 1
            }
        );
        // A non-image row never reports metadata, even though it is indexed.
        let note_row: i64 = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM files WHERE path='note.md' AND kind='note'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(note_row, 1, "the note row is in the index");
        assert_eq!(index.image_meta("note.md").unwrap(), None);
    }

    /// An index with no images at all is all zeros, not an error: an empty
    /// library has no unknown headers to count.
    #[test]
    fn image_meta_stats_on_an_empty_index_are_zero() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        assert_eq!(
            index.image_meta_stats().unwrap(),
            ImageMetaStats {
                images: 0,
                with_dimensions: 0,
                with_taken: 0,
                unreadable_headers: 0
            }
        );
    }
}
