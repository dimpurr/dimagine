//! Read-only view queries over the index (Viewer Phase 1 backend).
//!
//! These functions answer the questions a viewer UI asks: which images are in
//! a folder (recursively or not), which match some tags and text, how are they
//! ordered, which collections exist and what do they contain. They are pure
//! reads over the SQLite index and never touch the library files.
//!
//! Paths are library-relative and `/`-separated, the same spelling the scan
//! stored. `folder` uses `""` for the library root.

use crate::{reason_from_code, ImageMeta, Index, IndexError, MetaRow, TakenReason};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

/// The highest `rating` FORMAT §3.1 allows.
const MAX_RATING: u8 = 5;

/// What the index knows about image headers, counted ([`Index::image_meta_stats`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageMetaStats {
    /// Image rows the index holds.
    pub images: u64,
    /// Images whose pixel dimensions were read.
    pub with_dimensions: u64,
    /// Images whose taken time was read.
    pub with_taken: u64,
    /// Images whose dimensions stayed unknown (`width IS NULL`), counted on
    /// every scan, because the unknown is a recorded fact about the content
    /// and not a transient failure only the first scan sees.
    ///
    /// This is every image with a header that could not be read *and* every
    /// image in a format this build has no header reader for (AVIF, HEIF); the
    /// row does not record which, so the count says only that the dimensions
    /// are unknown. It is not a corruption count — a library of healthy AVIFs
    /// reports all of them here (RW48 Low-3).
    pub unknown_dimensions: u64,
    /// Images with no taken time, counted by the reason their own header gave
    /// ([`TakenReason`]) and ordered by it. A reason with no images is left
    /// out, so what is here is what an operator can act on; added to
    /// [`ImageMetaStats::with_taken`] it accounts for every image the refresh
    /// read, and the difference is the rows it never reached.
    pub taken_missing: Vec<(TakenReason, u64)>,
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

/// One collection on an image's "Appears in" list: the note that embeds the
/// image, and what the note calls itself. The title is library content
/// (FORMAT §3.1), so whoever renders it still has to escape it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppearsIn {
    /// Library-relative path of the collection note.
    pub note_path: String,
    /// Note `title`, empty when the note has none — the renderer then names
    /// the collection by its file name, the way the list does.
    pub title: String,
}

/// Where an image sits in a view, and what sits beside it (K27 motion 4:
/// prev/next walk the view the picture was reached from, not the whole
/// library).
///
/// `previous` and `next` are the images the grid showed immediately before
/// and after this one, in the same order the grid showed them; `None` at
/// either end of the view. `position` is 1-based inside the view and `total`
/// is how many images the view holds, after the lens limit (the Recent lens
/// reaches only [`RECENT_LIMIT`]-many rows; the caller states the cap because
/// the limit belongs to the viewer, not the index).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Neighbours {
    pub position: u64,
    pub total: u64,
    pub previous: Option<String>,
    pub next: Option<String>,
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
        // One spelling for the whole crate ([`members_cte`]), because the
        // neighbour reads ask `MIN`/`MAX(m.ord)` of this same CTE: two
        // spellings would be two rules for what counts as a member, and a drift
        // between them would mis-place a neighbour in silence.
        sql.with = members_cte().to_owned();
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

/// One image's own sort key, read from its `files` row for the neighbour
/// queries. The numbers are what `ORDER BY` compares, so the counts below
/// never re-derive them from another column.
#[derive(Clone, Debug)]
struct SortKeyValues {
    added_ns: i64,
    mtime_ns: i64,
    size: i64,
    name_key: String,
    rating: Option<i64>,
}

/// Read `image`'s `files` row. `None` when the index holds no such row — an
/// image the scan has not seen yet has no position anywhere.
fn read_sort_key(index: &Index, image: &str) -> Result<Option<SortKeyValues>, IndexError> {
    Ok(index
        .conn()?
        .query_row(
            "SELECT added_ns, mtime_ns, size, name_key, rating FROM files WHERE path=?1",
            [image],
            |row| {
                Ok(SortKeyValues {
                    added_ns: row.get(0)?,
                    mtime_ns: row.get(1)?,
                    size: row.get(2)?,
                    name_key: row.get(3)?,
                    rating: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Where one image sits in a view, and the 0-based offsets its neighbours sit
/// at in the same ordering: `(position, previous, next)`, with `position`
/// 1-based, `previous` `None` at the start of the view, and `next` the offset
/// of the row right after the image (whose existence the caller checks
/// against the total, or the lens limit, before fetching).
type NeighbourOffsets = (u64, Option<u64>, u64);

/// The offsets of a sorted view: the position is the count of rows the view's
/// own order puts before `image` plus one, and the neighbours are the rows at
/// the adjacent offsets — which the page statement answers, so the ordering
/// that decides them is the grid's ordering by construction.
fn sort_offsets(
    index: &Index,
    sql: &ViewSql,
    query: &ViewQuery,
    image: &str,
) -> Result<Option<NeighbourOffsets>, IndexError> {
    let Some(key) = read_sort_key(index, image)? else {
        return Ok(None);
    };
    // The rows the view orders strictly before `image`. Every sort ties on
    // `f.path ASC` ([`order_by`]), so the comparison is over the pair, and
    // `NULL` ratings — which sort last in both directions ([`SortKey::Rating`])
    // — are compared apart from the rating itself, never as a SQL `NULL` an
    // `OR` would quietly swallow.
    let (before, binds): (String, Vec<Box<dyn rusqlite::ToSql>>) = match query.sort {
        SortKey::Added => key_before("f.added_ns", query.descending, key.added_ns, image),
        SortKey::Modified => key_before("f.mtime_ns", query.descending, key.mtime_ns, image),
        SortKey::Size => key_before("f.size", query.descending, key.size, image),
        SortKey::Name => key_before("f.name_key", query.descending, key.name_key, image),
        SortKey::Rating => rating_before(query.descending, key.rating, image),
    };
    let extra: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|value| value.as_ref()).collect();
    let rows_before: i64 = index.conn()?.query_row(
        &format!(
            "{with}SELECT count(*) {from} WHERE {where_sql} AND ({before})",
            with = sql.with,
            from = sql.count_from,
            where_sql = sql.where_sql,
            before = before
        ),
        bound_with(sql, &extra),
        |row| row.get(0),
    )?;
    let position = rows_before.max(0) as u64 + 1;
    Ok(Some((position, position.checked_sub(2), position)))
}

/// `(condition, binds)` for the rows a sort column orders strictly before
/// `current`, with the path breaking ties ascending. `current` is duplicated
/// because the two `?` are two comparisons, and the path is `image` itself,
/// the one row being placed.
fn key_before<T: rusqlite::ToSql + Clone + 'static>(
    column: &str,
    descending: bool,
    current: T,
    image: &str,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let comparison = if descending { ">" } else { "<" };
    (
        format!("({column} {comparison} ? OR ({column} = ? AND f.path < ?))"),
        vec![
            Box::new(current.clone()) as Box<dyn rusqlite::ToSql>,
            Box::new(current),
            Box::new(image.to_owned()),
        ],
    )
}

/// The rating sort's own comparison ([`order_by`]): `(f.rating IS NULL)` first,
/// then `f.rating` in the view's direction, then the path. A `NULL` rating is
/// a state, not a value ([`note_rating`](fn@note_rating)), so the two cases are
/// written apart — one comparison for a rated current row, another for an
/// unrated one — instead of one expression a SQL `NULL` would make leaky.
fn rating_before(
    descending: bool,
    current: Option<i64>,
    image: &str,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    match current {
        Some(rating) => {
            let comparison = if descending { ">" } else { "<" };
            (
                format!(
                    "((f.rating IS NOT NULL AND f.rating {comparison} ?) \
                     OR (f.rating = ? AND f.path < ?))"
                ),
                vec![
                    Box::new(rating) as Box<dyn rusqlite::ToSql>,
                    Box::new(rating),
                    Box::new(image.to_owned()),
                ],
            )
        }
        None => (
            // Every rated row comes before every unrated one, in both
            // directions; unrated rows order by path among themselves.
            "(f.rating IS NOT NULL OR (f.rating IS NULL AND f.path < ?))".to_owned(),
            vec![Box::new(image.to_owned()) as Box<dyn rusqlite::ToSql>],
        ),
    }
}

/// The same offsets for a collection view: the members CTE orders rows by the
/// note's embed order (`m.ord`), so the position is the count of rows embedded
/// before this image plus one. An image embedded more than once — two
/// spellings of one embed, FORMAT §5 — holds the range between its first and
/// last occurrence: the neighbours stand at the offsets outside that range,
/// where a different picture is.
fn collection_offsets(
    index: &Index,
    sql: &ViewSql,
    query: &ViewQuery,
    image: &str,
) -> Result<Option<NeighbourOffsets>, IndexError> {
    let Some(collection) = query.collection.clone() else {
        return Ok(None);
    };
    // `MIN`/`MAX` over no row are SQL `NULL`, not an error value: the member
    // check above said this image is in the collection and this one now says it
    // is not, which only a rescan finishing underneath a page render can do.
    // That is an answer — there is no place to walk from — and it is returned as
    // one, not as a type error the caller has to guess at.
    let (first_ord, last_ord): (Option<i64>, Option<i64>) = index.conn()?.query_row(
        &format!(
            "{with}SELECT MIN(m.ord), MAX(m.ord) FROM members m WHERE m.path=?",
            with = members_cte()
        ),
        rusqlite::params![collection, collection, image],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (Some(first_ord), Some(last_ord)) = (first_ord, last_ord) else {
        return Ok(None);
    };
    let count_before = |ord: i64| -> Result<u64, IndexError> {
        let extras: Vec<&dyn rusqlite::ToSql> = vec![&ord];
        let counted: i64 = index.conn()?.query_row(
            &format!(
                "{with}SELECT count(*) {from} WHERE {where_sql} AND (m.ord < ?)",
                with = sql.with,
                from = sql.count_from,
                where_sql = sql.where_sql
            ),
            bound_with(sql, &extras),
            |row| row.get(0),
        )?;
        Ok(counted.max(0) as u64)
    };
    let before_first = count_before(first_ord)?;
    let before_last = count_before(last_ord)?;
    Ok(Some((
        // `before_first` is the 0-based index of the first occurrence, so the
        // position is one past it and the neighbour before the image stands
        // at the index before that; `before_last` is the index of the last
        // occurrence, so the neighbour after the image follows it.
        before_first + 1,
        before_first.checked_sub(1),
        before_last + 1,
    )))
}

/// The `WITH members ...` clause a collection view orders by — the one spelling
/// of it, used both by [`view_sql_with_order`] (which stores it as `sql.with`)
/// and on its own for the key reads, which do not need the rest of the view
/// statement. What counts as a member is spelled once in this crate because
/// both halves have to mean the same rows.
fn members_cte() -> &'static str {
    "WITH members(ord,path) AS ( \
       SELECT rowid,target FROM links WHERE src=? AND syntax<>'wiki_link' \
         AND target IS NOT NULL \
         AND target<>COALESCE((SELECT image_path FROM notes WHERE path=?),'') \
     ) "
}

/// The image at one 0-based offset in the view's own order, or `None` when the
/// offset lies past the end. The statement is [`Index::view`]'s own page
/// query, one row long, so what comes back is exactly what the grid showed at
/// that place.
fn row_at(index: &Index, query: &ViewQuery, offset: u64) -> Result<Option<String>, IndexError> {
    // The offset fits an i64 the way any count does; and a rank that
    // overflowed u32 could not have survived the count queries.
    let (statement, params) = view_sql(query).into_page(1, offset.min(i64::MAX as u64) as i64);
    Ok(index
        .conn()?
        .query_row(&statement, bound(&params), |row| row.get(0))
        .optional()?)
}

/// The `ORDER BY` of [`Index::view_by_taken`]: the same shape a [`SortKey`]
/// sort uses, over the taken time instead. Taken times order
/// chronologically in the asked direction, ties break by path ascending, and
/// an image with no taken time — an unknown instant, not an old or a young
/// one — sorts last in both directions, the flag before the value, never
/// flipped with the direction. A collection query keeps its own embed order,
/// which always comes first (the rule [`ViewQuery::sort`] follows).
fn order_by_taken(q: &ViewQuery) -> String {
    let direction = if q.descending { "DESC" } else { "ASC" };
    format!(" ORDER BY (f.taken_ns IS NULL), f.taken_ns {direction}, f.path ASC")
}

// === W47 · the filter panel's facet counts ==================================
//
// The panel beside the grid asks one question per row: *how many images would
// this value show, given the filters that are already on?* A row that
// replaces its filter — a folder, a collection, a lens — counts beside every
// other filter with its own stepped out; a tag row adds its tag to the tags
// already on (the one repeatable filter), so with `tag=eagle&tag=urban` on,
// the row for `portrait` counts what adding `portrait` to eagle and urban
// would show, not the nothing that all three together would. Every value of
// a facet comes back from one grouped query: a facet with sixty tags costs
// one query, not sixty.

/// Which facet of the filter panel a count answers for. The Recent lens is not
/// among them: its window is not another value beside the chosen one, it *is*
/// the value the row names ([`RecentWindow`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Facet {
    Tags,
    Folders,
    Collections,
    Untagged,
}

impl Facet {
    /// The view this facet's counts run over: the view on screen with the
    /// filter this facet answers for handled the way the panel's rows toggle
    /// it. A count is not a page, so the page and the order a page needs come
    /// off too.
    ///
    /// A folder, collection or lens row *replaces* its filter — the one
    /// folder, the one collection, the one lens — so those counts leave the
    /// filter out entirely: the count for `refs` beside `tag=nature` is what
    /// `in=refs&tag=nature` shows. A tag row *adds* its tag to the tags
    /// already on, so the tag counts keep them: the count for `beta` beside
    /// `tag=alpha` is what `tag=alpha&tag=beta` shows, which is the row's
    /// own link. The chosen tag's row then counts the view as it stands —
    /// where the reader is, the same as every other group's chosen row —
    /// rather than the view its way-out link opens.
    fn view_for_counts(self, view: &ViewQuery) -> ViewQuery {
        let mut narrowed = view.clone();
        match self {
            Self::Tags => {}
            Self::Folders => narrowed.folder = None,
            Self::Collections => narrowed.collection = None,
            Self::Untagged => narrowed.untagged = false,
        }
        narrowed.offset = 0;
        narrowed.limit = 0;
        narrowed
    }
}

/// The Recent lens, as a caller that counts facets has to state it: the lens is
/// the `N` newest added images and `N` is the viewer's number, not the index's
/// (the same way [`Index::view_neighbours`] takes its cap from the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentWindow {
    /// The lens is off, so only the Recent row looks inside the window: it says
    /// what clicking it would narrow the view to.
    Off { limit: u64 },
    /// The lens is on, so every facet's count is capped at the window:
    /// the grid the lens shows stops at the newest `limit` images of
    /// the set on screen, and a count beside it never promises more
    /// than the grid can show.
    On { limit: u64 },
}

impl RecentWindow {
    /// How far the lens reaches.
    fn limit(self) -> u64 {
        match self {
            Self::Off { limit } | Self::On { limit } => limit,
        }
    }

    /// Whether the lens caps the counts of the other facets.
    fn active(self) -> bool {
        matches!(self, Self::On { .. })
    }
}

/// Every facet value the view's filters leave, counted: the number the filter
/// panel puts beside each row. A value the map does not carry has no match
/// among those images — that is zero, a fact, and not an unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FacetCounts {
    /// Tag, in the index's folded spelling, to images carrying it.
    pub tags: BTreeMap<String, u64>,
    /// Folder, library-relative, to images inside it and its subfolders —
    /// what a recursive view counts, and the same thing a folder row
    /// means in the sidebar; a `sub=0` view counts a folder's direct
    /// members only, which is what its own rows link to. The library
    /// root is not a row of its own, so it never appears here.
    pub folders: BTreeMap<String, u64>,
    /// Collection note path to the members of it that remain.
    pub collections: BTreeMap<String, u64>,
    /// Images that carry no tag.
    pub untagged: u64,
    /// Images the Recent window reaches.
    pub recent: u64,
}

impl Index {
    /// The counts one filter panel needs, in five grouped queries.
    ///
    /// `view` is the view on screen: every filter in it narrows every
    /// facet except the one that answers for it ([`FacetCounts`]). Its
    /// `sort`, `offset` and `limit` are ignored, as they are for any
    /// count. A count never pages, so no page is read.
    ///
    /// The Recent lens cannot ride in a [`ViewQuery`] — a window is not
    /// a filter — so `recent` states it. The lens caps what the grid
    /// shows: its pages stop at the newest `limit` images of the set
    /// on screen, never at the library's newest `limit`, so every count
    /// beside the lens is capped at `limit` too. Capping — not cutting
    /// the counts to the library-wide window — is what makes a count
    /// say what its own row's link shows on any library the filters
    /// narrow: a filter whose images are all older than the window
    /// counts what it leaves, not a phantom zero.
    pub fn view_facet_counts(
        &self,
        view: &ViewQuery,
        recent: RecentWindow,
    ) -> Result<FacetCounts, IndexError> {
        let conn = self.conn()?;
        let window = recent.limit();
        let lens_on = recent.active();

        let mut tags = facet_sql(view, Facet::Tags);
        tags.and("note_tags.image_path=f.path");
        let mut tags = facet_values(conn, &tags, "note_tags.tag", ", note_tags")?;

        let folders = facet_sql(view, Facet::Folders);
        let direct = facet_values(conn, &folders, "f.folder", "")?;
        // A `sub=0` view shows a folder's direct members only, and a
        // folder row's own link keeps `sub=0`, so the counts keep the
        // view's own reach: the rollup to every parent is what a
        // recursive view means, and only that view rolls up.
        let mut folders = if view.recursive {
            folder_tree(direct)
        } else {
            direct
        };

        // Every collection at once, so the one collection the view may be
        // narrowed to leaves the statement entirely — its `WITH members` CTE
        // would ask the question of a single note. The member rule is the view's
        // own (`view_sql_with_order`): a strong embed (`syntax<>'wiki_link'`)
        // that resolves to an image and is not the note's own image
        // (FORMAT §3.2), and `count(DISTINCT f.path)` is the collection page's
        // rule that an image embedded twice shows once. A facet row and
        // `/?c=<that note>` can therefore never disagree about membership.
        let mut collections = facet_sql(view, Facet::Collections);
        collections.and(
            "links.syntax<>'wiki_link' AND links.target IS NOT NULL \
             AND links.target<>COALESCE(own.image_path,'')",
        );
        let mut collections = facet_values(
            conn,
            &collections,
            "links.src",
            " JOIN links ON links.target=f.path LEFT JOIN notes own ON own.path=links.src",
        )?;

        // "No tags" is not a tag, so the untagged row is the one facet whose
        // condition is written rather than grouped over a column — and the one
        // the view's own untagged lens has to step out of first.
        let mut untagged = facet_sql(view, Facet::Untagged);
        untagged.and("NOT EXISTS(SELECT 1 FROM note_tags WHERE note_tags.image_path=f.path)");
        let mut untagged = facet_count(conn, &untagged)?;

        // The Recent row counts what the lens would leave of the other
        // filters, capped at the window whether the lens is on or off:
        // the window is what that row names, and the lens can never
        // show more than it.
        let recent_sql = view_sql(&{
            let mut whole = view.clone();
            whole.offset = 0;
            whole.limit = 0;
            whole
        });
        let recent = facet_count(conn, &recent_sql)?.min(window);

        if lens_on {
            // The lens is on, so the grid stops at the newest `window`
            // images of the set on screen: every count beside it stops
            // there too, or it would promise images the grid cannot show.
            cap_counts(&mut tags, window);
            cap_counts(&mut folders, window);
            cap_counts(&mut collections, window);
            untagged = untagged.min(window);
        }

        Ok(FacetCounts {
            tags,
            folders,
            collections,
            untagged,
            recent,
        })
    }
}

/// The statement one facet's counts run over: the view the facet
/// answers beside ([`Facet::view_for_counts`]), in the shape
/// [`Index::view`] builds its own query in — so a count and the grid
/// beside it can never disagree about what a filter means, because
/// they are the same builder.
fn facet_sql(view: &ViewQuery, facet: Facet) -> ViewSql {
    view_sql(&facet.view_for_counts(view))
}

/// Cap every value's count at the Recent lens's reach: the lens's pages
/// stop at the newest `window` images of the set on screen, so a count
/// beside the lens never promises more than the lens can show.
fn cap_counts(counts: &mut BTreeMap<String, u64>, window: u64) {
    for count in counts.values_mut() {
        *count = (*count).min(window);
    }
}

/// How many rows a facet's statement leaves.
fn facet_count(conn: &rusqlite::Connection, sql: &ViewSql) -> Result<u64, IndexError> {
    let counted: i64 = conn.query_row(&sql.count(), bound(&sql.params), |row| row.get(0))?;
    Ok(counted.max(0) as u64)
}

/// Every value of one facet with how many rows carry it, in one grouped query:
/// `value` names the column the value lives in, `join` brings in the table that
/// holds it (empty when it is a column of the image row itself).
///
/// `count(DISTINCT f.path)` counts images, not rows: one image embedded twice
/// is one member of a collection, and a value that never arrives as an empty
/// string is no value at all, so that row is dropped.
fn facet_values(
    conn: &rusqlite::Connection,
    sql: &ViewSql,
    value: &str,
    join: &str,
) -> Result<BTreeMap<String, u64>, IndexError> {
    let statement = format!(
        "{with}SELECT {value}, count(DISTINCT f.path) {from}{join} \
         WHERE {where_sql} GROUP BY {value}",
        with = sql.with,
        value = value,
        from = sql.count_from,
        join = join,
        where_sql = sql.where_sql,
    );
    let mut stmt = conn.prepare(&statement)?;
    let rows = stmt.query_map(bound(&sql.params), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?.max(0) as u64,
        ))
    })?;
    Ok(rows
        .collect::<Result<BTreeMap<_, _>, _>>()?
        .into_iter()
        .filter(|(value, _)| !value.is_empty())
        .collect())
}

/// Turn "images in this folder" into "images in this folder and everything
/// below it", which is what a folder row means in the panel and the sidebar
/// alike (and the same reach [`Index::folder_counts`] counts with): each image
/// is added to its own folder and to every parent of that folder, so a folder
/// with nothing of its own but a full subfolder counts the subfolder and shows.
fn folder_tree(direct: BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    let mut tree: BTreeMap<&str, u64> = BTreeMap::new();
    for (folder, count) in &direct {
        let mut prefix = folder.as_str();
        loop {
            *tree.entry(prefix).or_insert(0) += count;
            match prefix.rfind('/') {
                Some(at) => prefix = &prefix[..at],
                None => break,
            }
        }
    }
    tree.into_iter()
        .map(|(folder, count)| (folder.to_owned(), count))
        .collect()
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

    /// The paths, the position and the size of the view: one image's place
    /// among its neighbours, in the order the view itself orders them.
    ///
    /// Every step is a query over the index: the membership, the image's own
    /// sort key, the count of rows the view orders before it, and the two rows
    /// at the resulting offsets — the same statement `Index::view` runs for a
    /// page, only one row long. Nothing walks the view in Rust, and the page
    /// the neighbours come from is by construction the page the grid showed.
    ///
    /// `query.offset` and `query.limit` are ignored: neighbours walk the whole
    /// matching set, not one page of it. `lens_limit` caps that set the way
    /// the Recent lens caps the viewer's grid: an image further in than the
    /// cap answers `None` (the lens never showed it, so it has no neighbours
    /// there).
    pub fn view_neighbours(
        &self,
        query: &ViewQuery,
        image: &str,
        lens_limit: Option<u64>,
    ) -> Result<Option<Neighbours>, IndexError> {
        let sql = view_sql(query);
        let conn = self.conn()?;
        // A view the image is not part of has no neighbours: a stale `v=`
        // on a link, a filter that excludes it, a collection it is not
        // embedded in. Membership is answered by the view's own filters
        // (`count_from` carries the members join when the view is a
        // collection), so it can never disagree with the rows themselves.
        let member_sql = format!(
            "{with}SELECT EXISTS(SELECT 1 {from} WHERE {where_sql} AND f.path=?)",
            with = sql.with,
            from = sql.count_from,
            where_sql = sql.where_sql
        );
        let member_extras: Vec<&dyn rusqlite::ToSql> = vec![&image];
        let member: bool =
            conn.query_row(&member_sql, bound_with(&sql, &member_extras), |row| {
                row.get(0)
            })?;
        if !member {
            return Ok(None);
        }
        let total: u64 = {
            let counted: i64 =
                conn.query_row(&sql.count(), bound(&sql.params), |row| row.get(0))?;
            counted.max(0) as u64
        };
        let (position, previous_offset, next_offset) = match if sql.collection {
            collection_offsets(self, &sql, query, image)?
        } else {
            sort_offsets(self, &sql, query, image)?
        } {
            // The counts above found nothing to place: the image left the
            // index between one query and the next, or a collection that
            // says the image is a member cannot say where. Either way
            // there is no view to walk.
            Some(offsets) => offsets,
            None => return Ok(None),
        };
        // The lens: the cap turns the position into a claim about the whole
        // matching set, so an image further in than the cap was never on
        // screen and has no neighbours there. Otherwise the totals say the
        // same thing the capped grid said (Recent states its own size,
        // W34 audit #10).
        if let Some(cap) = lens_limit {
            if position > cap {
                return Ok(None);
            }
        }
        let total = lens_limit.map_or(total, |cap| total.min(cap));
        Ok(Some(Neighbours {
            position,
            total,
            // `view_sql` builds the same statement the grid page runs, so the
            // row at an offset is the row the grid showed there — and the end
            // of the view answers `None`, never a wrap-around.
            previous: match previous_offset {
                Some(offset) if offset < total => row_at(self, query, offset)?,
                _ => None,
            },
            next: if position < total {
                row_at(self, query, next_offset)?
            } else {
                None
            },
        }))
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
    /// detail asks its `taken_ns` here, and shows [`ImageMeta::taken_reason`]
    /// where an instant would have gone.
    ///
    /// This is a companion to [`Index::view`], not part of its page shape:
    /// the viewer's own page JSON is the index API's hand-mirrored mirror,
    /// and it grows when the viewer does, not when the index does.
    pub fn image_meta(&self, path: &str) -> Result<Option<ImageMeta>, IndexError> {
        let row: Option<MetaRow> = self
            .conn()?
            .query_row(
                "SELECT width,height,taken_ns,taken_reason \
                 FROM files WHERE path=?1 AND kind='image'",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(
            row.map(|(width, height, taken_ns, taken_reason)| ImageMeta {
                width: width.and_then(|value| u32::try_from(value).ok()),
                height: height.and_then(|value| u32::try_from(value).ok()),
                taken_ns,
                taken_reason: reason_from_code(taken_reason),
            }),
        )
    }

    /// What the index knows about image headers, counted: how many images
    /// there are, how many have dimensions, a taken time, or a header that
    /// could not be read (dims unknown), and — for the images with no taken
    /// time — how many each recorded reason accounts for. The counted warning
    /// of a refresh — and of every scan that follows, because the unknowns are
    /// recorded facts about content, not transient failures.
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
        // The reasons are counted in one grouped pass, so a reason this version
        // does not know (a code from a newer dimagine, or a hand-written one)
        // simply names no count here rather than being guessed at.
        let mut reasons = self.conn()?.prepare(
            "SELECT taken_reason, count(*) FROM files \
             WHERE kind='image' AND taken_ns IS NULL AND taken_reason IS NOT NULL \
             GROUP BY taken_reason ORDER BY taken_reason",
        )?;
        let taken_missing = reasons
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(code, count)| {
                reason_from_code(Some(code)).map(|reason| (reason, max_zero(count)))
            })
            .collect();
        Ok(ImageMetaStats {
            images: max_zero(row.0),
            with_dimensions: max_zero(row.1),
            with_taken: max_zero(row.2),
            unknown_dimensions: max_zero(row.3),
            taken_missing,
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
            .appears_in_titled(image_path)?
            .into_iter()
            .map(|collection| collection.note_path)
            .collect())
    }

    /// The same back-links with what each note calls itself: the "Appears in"
    /// list on the image page names a collection the way the sidebar does —
    /// by its `title`, falling back to the file name — instead of by its
    /// path alone (K27 motion 6).
    pub fn appears_in_titled(&self, image_path: &str) -> Result<Vec<AppearsIn>, IndexError> {
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
            .map(|candidate| AppearsIn {
                title: candidate
                    .props
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                note_path: candidate.path,
            })
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

/// The same, with the statement's own parameters first and `extra` after
/// them — the order an appended `AND (?)` places its `?` in. The extras come
/// as references so a handful of locals can ride along without a box.
fn bound_with<'a>(
    sql: &'a ViewSql,
    extra: &'a [&'a dyn rusqlite::ToSql],
) -> impl rusqlite::Params + 'a {
    rusqlite::params_from_iter(
        sql.params
            .iter()
            .map(|value| value.as_ref())
            .chain(extra.iter().copied()),
    )
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

    /// Seed an index whose every neighbour answer is worked out by hand: four
    /// images with deliberate ties — `a`, `b`, `d` share an `added`, `b` and
    /// `d` share a size, `b` and `d` have no rating — and one collection whose
    /// embed order is *not* any sort order and embeds `a` twice, in two
    /// spellings (FORMAT §5), with the duplicate in the middle.
    fn neighbour_library() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |name: &str, size: u64, added_ns: i64, rating: Option<u8>| FileRecord {
            path: format!("x/{name}.png"),
            size,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: Some(added_ns),
            rating,
            width: None,
            height: None,
            taken_ns: None,
            taken_reason: None,
        };
        let note = |path: &str, image_path: &str, props_json: &str| NoteRecord {
            path: path.to_owned(),
            image_path: Some(image_path.to_owned()),
            id: None,
            title: path.to_owned(),
            tags: match props_json {
                json if json.contains("\"red\"") => vec!["red".to_owned()],
                _ => Vec::new(),
            },
            props_json: props_json.to_owned(),
        };
        index.begin_scan().unwrap();
        index.upsert_file(&image("a", 100, 1000, Some(3))).unwrap();
        index.upsert_file(&image("b", 50, 1000, None)).unwrap();
        index.upsert_file(&image("c", 200, 900, Some(5))).unwrap();
        index.upsert_file(&image("d", 50, 1000, None)).unwrap();
        for (image_path, props_json) in [
            ("x/a.png", r#"{"tags":["red"],"rating":3}"#),
            ("x/b.png", r#"{"tags":["red"]}"#),
            ("x/c.png", r#"{"rating":5}"#),
            ("x/d.png", r#"{}"#),
        ] {
            let note_path = format!("{image_path}.md");
            index
                .upsert_note(&note(&note_path, image_path, props_json))
                .unwrap();
            index
                .set_note_body(&note_path, &format!("match {image_path}"))
                .unwrap();
        }
        let embed = |target: &str, raw: &str| crate::LinkRecord {
            src: "set.md".to_owned(),
            raw: raw.to_owned(),
            target: Some(target.to_owned()),
            state: crate::LinkState::Resolved,
            syntax: crate::LinkSyntax::WikiEmbed,
        };
        index
            .upsert_note(&NoteRecord {
                path: "set.md".to_owned(),
                image_path: None,
                id: None,
                title: "set.md".to_owned(),
                tags: Vec::new(),
                props_json: r#"{"kind":"collection","title":"Set"}"#.to_owned(),
            })
            .unwrap();
        index
            .replace_links(
                "set.md",
                &[
                    embed("x/c.png", "![[c.png]]"),
                    embed("x/a.png", "![[a.png]]"),
                    embed("x/d.png", "![[d.png]]"),
                    embed("x/a.png", "![[x/a.png]]"),
                    embed("x/b.png", "![[b.png]]"),
                ],
            )
            .unwrap();
        index
            .finish_scan(&[
                "x/a.png".into(),
                "x/b.png".into(),
                "x/c.png".into(),
                "x/d.png".into(),
                "x/a.png.md".into(),
                "x/b.png.md".into(),
                "x/c.png.md".into(),
                "x/d.png.md".into(),
                "set.md".into(),
            ])
            .unwrap();
        (dir, index)
    }

    /// For every image in a view, the neighbours are exactly the images the
    /// view itself showed on either side — ties, ends and all — and the
    /// position and total are the ones the grid would print (K27: the
    /// neighbours walk the view, never the whole library).
    #[test]
    fn neighbours_match_the_grid_on_every_side_of_every_image() {
        let (_dir, index) = neighbour_library();
        let cases: Vec<(&str, ViewQuery, Option<u64>)> = vec![
            (
                "added descending, a three-way tie on added",
                ViewQuery::default(),
                None,
            ),
            (
                "name ascending",
                ViewQuery {
                    sort: SortKey::Name,
                    descending: false,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "size descending, a tie on size",
                ViewQuery {
                    sort: SortKey::Size,
                    descending: true,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "rating descending, two images without one",
                ViewQuery {
                    sort: SortKey::Rating,
                    descending: true,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "rating ascending",
                ViewQuery {
                    sort: SortKey::Rating,
                    descending: false,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "a recursive folder",
                ViewQuery {
                    folder: Some("x".into()),
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "a non-recursive folder",
                ViewQuery {
                    folder: Some("x".into()),
                    recursive: false,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "a tag",
                ViewQuery {
                    tags: vec!["red".into()],
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "full text",
                ViewQuery {
                    text: Some("match x/c.png".into()),
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            // RW45 review L-6: the case above is a one-item view, so it cannot
            // see an offset move wrongly. Every note's body carries the word, so
            // this is the FTS path holding all four images, with neighbours on
            // both sides of the middle ones.
            (
                "full text over every image",
                ViewQuery {
                    text: Some("match".into()),
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "the untagged lens",
                ViewQuery {
                    untagged: true,
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
            (
                "a collection in embed order, one image twice",
                ViewQuery {
                    collection: Some("set.md".into()),
                    limit: 100,
                    ..ViewQuery::default()
                },
                None,
            ),
        ];
        let mut interior = 0;
        for (name, query, lens) in cases {
            let page = index.view(&query).unwrap();
            assert!(
                !page.items.is_empty(),
                "{name}: a case whose view shows nothing compares nothing"
            );
            let mut asked = std::collections::HashSet::new();
            for (first, item) in page.items.iter().enumerate() {
                // An image embedded more than once is one image: ask once, at
                // its first occurrence (`rposition` below finds the last).
                if !asked.insert(item.path.clone()) {
                    continue;
                }
                let first = first as u64;
                let last = page
                    .items
                    .iter()
                    .rposition(|row| row.path == item.path)
                    .unwrap() as u64;
                let neighbours = index
                    .view_neighbours(&query, &item.path, lens)
                    .unwrap_or_else(|error| panic!("{name}: {error}"))
                    .unwrap_or_else(|| panic!("{name}: {} has no place", item.path));
                let expected_previous = first
                    .checked_sub(1)
                    .map(|before| page.items[before as usize].path.clone());
                let expected_next = page
                    .items
                    .get(last as usize + 1)
                    .cloned()
                    .map(|row| row.path);
                assert_eq!(
                    neighbours.previous, expected_previous,
                    "{name}: previous of {}",
                    item.path
                );
                assert_eq!(
                    neighbours.next, expected_next,
                    "{name}: next of {}",
                    item.path
                );
                assert_eq!(neighbours.position, first + 1, "{name}: {}", item.path);
                if expected_previous.is_some() && expected_next.is_some() {
                    interior += 1;
                }
                assert_eq!(
                    neighbours.total, page.total,
                    "{name}: the total is the grid's own"
                );
            }
        }
        // A case whose view holds one image compares nothing, so the walk as a
        // whole has to have read images with something on either side of them —
        // otherwise the loop above could pass having seen nothing at all.
        assert!(
            interior >= 8,
            "only {interior} images had a neighbour on both sides across every case"
        );
    }

    /// The ends of a view answer `None` rather than wrapping around, and the
    /// position stays inside the view (K27: "3 / 124", never 0 / 0).
    #[test]
    fn the_ends_of_a_view_have_no_neighbour_beyond_them() {
        let (_dir, index) = neighbour_library();
        let first = index
            .view_neighbours(&ViewQuery::default(), "x/a.png", None)
            .unwrap()
            .unwrap();
        assert_eq!(first.previous, None, "the first image has nothing before");
        assert_eq!(first.next.as_deref(), Some("x/b.png"));
        assert_eq!(first.position, 1);
        assert_eq!(first.total, 4);
        let last = index
            .view_neighbours(&ViewQuery::default(), "x/c.png", None)
            .unwrap()
            .unwrap();
        assert_eq!(last.next, None, "the last image has nothing after");
        assert_eq!(last.previous.as_deref(), Some("x/d.png"));
        assert_eq!(last.position, 4);
    }

    /// An image embedded twice keeps one place, held by its first occurrence:
    /// the neighbours stand outside the whole range the duplicates hold.
    #[test]
    fn an_image_embedded_twice_walks_the_range_of_its_occurrences() {
        let (_dir, index) = neighbour_library();
        // Embed order: c, a, d, a, b — `a` holds positions 2 and 4.
        let bedded = index
            .view_neighbours(
                &ViewQuery {
                    collection: Some("set.md".into()),
                    limit: 100,
                    ..ViewQuery::default()
                },
                "x/a.png",
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(bedded.position, 2, "the first occurrence places the image");
        assert_eq!(bedded.total, 5, "the collection counts its embed rows");
        assert_eq!(bedded.previous.as_deref(), Some("x/c.png"));
        assert_eq!(
            bedded.next.as_deref(),
            Some("x/b.png"),
            "the neighbour after `a` is past every one of its occurrences"
        );
    }

    /// A lens limit caps the walk the way it caps the grid (W34 audit #10):
    /// the images past the cap were never shown, and the image at the cap has
    /// no next even though the library holds more.
    #[test]
    fn a_lens_limit_cuts_the_glass_the_way_it_cut_the_grid() {
        let (_dir, index) = neighbour_library();
        // Added descending: a, b, d, c — a two-wide lens shows a, b.
        let inside = index
            .view_neighbours(&ViewQuery::default(), "x/b.png", Some(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                inside.position,
                inside.total,
                inside.previous.as_deref(),
                inside.next
            ),
            (2, 2, Some("x/a.png"), None),
            "the image at the cap has no next, exactly as the capped grid had none"
        );
        for outside in ["x/d.png", "x/c.png"] {
            assert!(
                index
                    .view_neighbours(&ViewQuery::default(), outside, Some(2))
                    .unwrap()
                    .is_none(),
                "{outside} was never in the lens, so it has no neighbours there"
            );
        }
    }

    /// An image a view does not show has no place in it — a filter that
    /// excludes it, a collection it is not embedded in, an index that has
    /// never seen it — and each answers `None` rather than a guess (invariant
    /// 4: an unknown is not an empty row at index 0).
    #[test]
    fn an_image_the_view_does_not_show_has_no_neighbours_there() {
        let (_dir, index) = neighbour_library();
        let red = ViewQuery {
            tags: vec!["red".into()],
            limit: 100,
            ..ViewQuery::default()
        };
        assert!(index
            .view_neighbours(&red, "x/c.png", None)
            .unwrap()
            .is_none());
        assert!(index
            .view_neighbours(&red, "x/e.png", None)
            .unwrap()
            .is_none());
        let set = ViewQuery {
            collection: Some("set.md".into()),
            limit: 100,
            ..ViewQuery::default()
        };
        // `x/a.png` is embedded; a path beside the set is not.
        assert!(index
            .view_neighbours(&set, "x/a.png", None)
            .unwrap()
            .is_some());
        assert!(index
            .view_neighbours(&set, "x/e.png", None)
            .unwrap()
            .is_none());
        // No collection of that name: a view with no members.
        let empty = ViewQuery {
            collection: Some("none.md".into()),
            ..ViewQuery::default()
        };
        assert!(index
            .view_neighbours(&empty, "x/a.png", None)
            .unwrap()
            .is_none());
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
            taken_reason: None,
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
            taken_reason: None,
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
                        taken_reason: (index_in_folder % 3 == 0)
                            .then_some(crate::TakenReason::NoExif),
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
            taken_reason: taken_ns.map_or(Some(TakenReason::NoExif), |_| None),
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
                taken_reason: None,
            })
        );
        assert_eq!(
            index.image_meta("unknown-a.jpg").unwrap(),
            Some(crate::ImageMeta {
                width: Some(4),
                height: Some(3),
                taken_ns: None,
                taken_reason: Some(TakenReason::NoExif),
            }),
            "an unknown taken time is not an unknown dimension, and says what it is"
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
                unknown_dimensions: 0,
                taken_missing: vec![(TakenReason::NoExif, 2)]
            }
        );
    }

    /// W48: the taken-order stats count what they should, including the
    /// headers no reader could parse.
    #[test]
    fn image_meta_stats_count_the_unknown_headers() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str,
                     width: Option<u32>,
                     taken_ns: Option<i64>,
                     taken_reason: Option<TakenReason>| FileRecord {
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
            taken_reason,
        };
        index.begin_scan().unwrap();
        index
            .upsert_file(&image("healthy.jpg", Some(640), Some(1), None))
            .unwrap();
        index
            .upsert_file(&image(
                "bare-dims.jpg",
                Some(12),
                None,
                Some(TakenReason::ExifWithoutDate),
            ))
            .unwrap();
        index
            .upsert_file(&image(
                "broken.jpg",
                None,
                None,
                Some(TakenReason::UnreadableFile),
            ))
            .unwrap();
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
                taken_reason: None,
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
                unknown_dimensions: 1,
                taken_missing: vec![
                    (TakenReason::ExifWithoutDate, 1),
                    (TakenReason::UnreadableFile, 1),
                ]
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
                unknown_dimensions: 0,
                taken_missing: Vec::new()
            }
        );
    }

    // === W47 · the filter panel's facet counts ==============================

    fn facet_map(values: &[(&str, u64)]) -> BTreeMap<String, u64> {
        values
            .iter()
            .map(|(value, count)| ((*value).to_owned(), *count))
            .collect()
    }

    fn facet_embed(src: &str, target: &str, raw: &str) -> crate::LinkRecord {
        crate::LinkRecord {
            src: src.to_owned(),
            raw: raw.to_owned(),
            target: Some(target.to_owned()),
            state: crate::LinkState::Resolved,
            syntax: crate::LinkSyntax::WikiEmbed,
        }
    }

    /// A library whose facet answers are worked out by hand: five images over
    /// three folders (`refs`, `refs/ui`, `other`), tags `red` / `big` / `blue`
    /// and one image with no tag at all, `added` times far apart so a Recent
    /// window of three cuts between `d` and `e`, two listed collections (one of
    /// which embeds `b` twice, in two spellings) and one image note that embeds
    /// a sibling — a collection whose own image is not its member (FORMAT §3.2).
    fn facet_library() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let image = |path: &str, added_ns: i64| FileRecord {
            path: path.to_owned(),
            size: 10,
            mtime_ns: 1,
            sha256: None,
            kind: crate::FileKind::Image,
            note_added_ns: Some(added_ns),
            rating: None,
            width: None,
            height: None,
            taken_ns: None,
            taken_reason: None,
        };
        let note = |path: &str, image_path: Option<&str>, tags: &[&str], props: &str| NoteRecord {
            path: path.to_owned(),
            image_path: image_path.map(str::to_owned),
            id: None,
            title: path.to_owned(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            props_json: props.to_owned(),
        };
        index.begin_scan().unwrap();
        for (path, added_ns) in [
            ("refs/a.png", 5_000),
            ("refs/b.png", 4_000),
            ("refs/ui/c.png", 3_000),
            ("refs/ui/d.png", 2_000),
            ("other/e.png", 1_000),
        ] {
            index.upsert_file(&image(path, added_ns)).unwrap();
        }
        for (path, tags) in [
            ("refs/a.png", ["big", "red"].as_slice()),
            ("refs/b.png", ["red"].as_slice()),
            ("refs/ui/c.png", ["blue"].as_slice()),
            // An image whose note carries no tag: `untagged` is its row.
            ("refs/ui/d.png", [].as_slice()),
            ("other/e.png", ["big"].as_slice()),
        ] {
            let note_path = format!("{path}.md");
            index
                .upsert_note(&note(&note_path, Some(path), tags, "{}"))
                .unwrap();
        }
        index
            .replace_links(
                "refs/a.png.md",
                &[
                    facet_embed("refs/a.png.md", "refs/a.png", "![[a.png]]"),
                    facet_embed("refs/a.png.md", "refs/ui/c.png", "![[ui/c.png]]"),
                ],
            )
            .unwrap();
        index
            .upsert_note(&note(
                "set.md",
                None,
                &[],
                r#"{"kind":"collection","title":"Set"}"#,
            ))
            .unwrap();
        index
            .replace_links(
                "set.md",
                &[
                    facet_embed("set.md", "refs/a.png", "![[refs/a.png]]"),
                    facet_embed("set.md", "refs/ui/c.png", "![[refs/ui/c.png]]"),
                    facet_embed("set.md", "other/e.png", "![[other/e.png]]"),
                ],
            )
            .unwrap();
        index
            .upsert_note(&note(
                "dupe.md",
                None,
                &[],
                r#"{"kind":"collection","title":"Dupe"}"#,
            ))
            .unwrap();
        index
            .replace_links(
                "dupe.md",
                &[
                    facet_embed("dupe.md", "refs/b.png", "![[refs/b.png]]"),
                    facet_embed("dupe.md", "refs/b.png", "![[b.png]]"),
                    facet_embed("dupe.md", "refs/a.png", "![[refs/a.png]]"),
                ],
            )
            .unwrap();
        index
            .finish_scan(&[
                "refs/a.png".into(),
                "refs/b.png".into(),
                "refs/ui/c.png".into(),
                "refs/ui/d.png".into(),
                "other/e.png".into(),
                "refs/a.png.md".into(),
                "refs/b.png.md".into(),
                "refs/ui/c.png.md".into(),
                "refs/ui/d.png.md".into(),
                "other/e.png.md".into(),
                "set.md".into(),
                "dupe.md".into(),
            ])
            .unwrap();
        (dir, index)
    }

    /// Nothing narrowed: every facet counts the library, a folder row counts
    /// its subfolders, and a collection counts members — once each, with a
    /// note's own image not a member of itself.
    #[test]
    fn an_uncharted_facet_panel_counts_the_whole_library() {
        let (_dir, index) = facet_library();
        let counts = index
            .view_facet_counts(&ViewQuery::default(), RecentWindow::Off { limit: 3 })
            .unwrap();
        assert_eq!(
            counts.tags,
            facet_map(&[("big", 2), ("blue", 1), ("red", 2)]),
            "{:?}",
            counts.tags
        );
        assert_eq!(
            counts.folders,
            facet_map(&[("other", 1), ("refs", 4), ("refs/ui", 2)]),
            "a parent counts what its subfolder holds, the root is no row"
        );
        assert_eq!(
            counts.collections,
            facet_map(&[("dupe.md", 2), ("refs/a.png.md", 1), ("set.md", 3)]),
            "dupe.md embeds b twice and still counts it once; a.png's own note \
             is a collection of c alone, not of a"
        );
        assert_eq!(counts.untagged, 1);
        assert_eq!(counts.recent, 3);
    }

    /// One filter is on — the folder scope — and it narrows every facet except
    /// the folder facet, whose own filter steps out. Otherwise its rows would
    /// answer "what is inside the folder you already chose", which is always
    /// the number the toolbar already shows.
    #[test]
    fn one_active_filter_narrows_every_facet_but_its_own() {
        let (_dir, index) = facet_library();
        let view = ViewQuery {
            folder: Some("refs".to_owned()),
            ..ViewQuery::default()
        };
        let counts = index
            .view_facet_counts(&view, RecentWindow::Off { limit: 3 })
            .unwrap();
        assert_eq!(
            counts.tags,
            facet_map(&[("big", 1), ("blue", 1), ("red", 2)]),
            "other/e.png is the second `big` image and it is not in refs"
        );
        assert_eq!(
            counts.folders,
            facet_map(&[("other", 1), ("refs", 4), ("refs/ui", 2)]),
            "the folder facet ignores `in`: its rows are the choices, not the view"
        );
        assert_eq!(
            counts.collections,
            facet_map(&[("dupe.md", 2), ("refs/a.png.md", 1), ("set.md", 2)]),
            "set.md's member `other/e.png` leaves the folder"
        );
        assert_eq!(counts.untagged, 1);
        assert_eq!(
            counts.recent, 3,
            "the three newest are a, b, c, and all three are in refs"
        );
    }

    /// Two filters are on — a tag and a collection — and every facet that is
    /// neither of them counts their intersection, while each of the two counts
    /// the other one.
    #[test]
    fn two_active_filters_narrow_the_facets_that_are_neither_of_them() {
        let (_dir, index) = facet_library();
        let view = ViewQuery {
            tags: vec!["red".to_owned()],
            collection: Some("set.md".to_owned()),
            ..ViewQuery::default()
        };
        let counts = index
            .view_facet_counts(&view, RecentWindow::Off { limit: 3 })
            .unwrap();
        assert_eq!(
            counts.tags,
            facet_map(&[("big", 1), ("red", 1)]),
            "the tag facet keeps `tag=red` and counts set.md's members \
             that carry it: a alone, so `blue` is no row at all"
        );
        assert_eq!(
            counts.folders,
            facet_map(&[("refs", 1)]),
            "a is the only set.md member that carries `red`"
        );
        assert_eq!(
            counts.collections,
            facet_map(&[("dupe.md", 2), ("set.md", 1)]),
            "the collection facet drops `c=set.md` and counts the `red` images a, b"
        );
        assert_eq!(
            counts.untagged, 0,
            "an untagged image cannot carry `red`: an honest zero, not a missing row"
        );
        assert_eq!(counts.recent, 1);
    }

    /// A third filter is on as well, and it is the one no tag can express:
    /// `untagged` beside `red` leaves nothing, and every facet says zero rather
    /// than pretending otherwise.
    #[test]
    fn a_filter_that_leaves_nothing_leaves_every_facet_at_zero() {
        let (_dir, index) = facet_library();
        let view = ViewQuery {
            tags: vec!["red".to_owned()],
            untagged: true,
            folder: Some("refs".to_owned()),
            ..ViewQuery::default()
        };
        let counts = index
            .view_facet_counts(&view, RecentWindow::Off { limit: 3 })
            .unwrap();
        assert!(counts.tags.is_empty(), "{:?}", counts.tags);
        assert!(counts.folders.is_empty(), "{:?}", counts.folders);
        assert!(counts.collections.is_empty(), "{:?}", counts.collections);
        assert_eq!(counts.untagged, 0);
        assert_eq!(counts.recent, 0);

        // The untagged facet is the exception that proves the rule: with only
        // `untagged` on it counts the images no tag touches.
        let only_lens = ViewQuery {
            untagged: true,
            ..ViewQuery::default()
        };
        let counts = index
            .view_facet_counts(&only_lens, RecentWindow::Off { limit: 3 })
            .unwrap();
        assert_eq!(counts.untagged, 1, "the lens steps out of its own row");
        assert!(counts.tags.is_empty(), "no tagged image is untagged");
        assert_eq!(counts.folders, facet_map(&[("refs", 1), ("refs/ui", 1)]));
    }

    /// The Recent lens on: every other facet's count is capped at the
    /// window — the newest `limit` images of the set on screen, not
    /// the library — so a row says what its own link shows, and the
    /// lens's own row says what it leaves of the filters that are on,
    /// which is the grid on screen.
    #[test]
    fn the_recent_lens_caps_the_counts_at_its_window_and_says_what_it_reaches() {
        let (_dir, index) = facet_library();
        let view = ViewQuery {
            folder: Some("refs".to_owned()),
            ..ViewQuery::default()
        };
        let off = index
            .view_facet_counts(&view, RecentWindow::Off { limit: 3 })
            .unwrap();
        let on = index
            .view_facet_counts(&view, RecentWindow::On { limit: 3 })
            .unwrap();

        // The three newest are a, b, c. `d` — the one untagged image in refs —
        // is the fourth, so it is inside the folder and outside the lens. Its
        // row's link (`in=refs&recent=1&untagged=1`) still shows it — the
        // newest three of one image is that one image — so the lens caps the
        // count at the window instead of cutting `d` out of it.
        assert_eq!((off.untagged, on.untagged), (1, 1));
        // `other/e.png` is the fifth: the folder keeps it out, the lens would
        // too, so the `big` tag reads the same either way.
        assert_eq!((off.tags["big"], on.tags["big"]), (1, 1));
        // What is on screen is the same either way, so the lens's own row is
        // too: it names the view it is already part of.
        assert_eq!((off.recent, on.recent), (3, 3));

        // With the lens on, the folder facet counts the window too, so a folder
        // the window does not reach drops out — `other` is `e.png` alone.
        let all = index
            .view_facet_counts(&ViewQuery::default(), RecentWindow::On { limit: 3 })
            .unwrap();
        assert_eq!(
            all.folders,
            facet_map(&[("other", 1), ("refs", 3), ("refs/ui", 2)]),
            "other/e.png is the fifth newest, so `other` keeps its one image \
             and `refs/ui` keeps both of its own"
        );
        assert_eq!(all.recent, 3);
    }

    /// The window is the newest N rows in the grid's own order (the path
    /// breaking a tie), not a threshold on `added` that an equal instant would
    /// widen past the N the lens shows.
    #[test]
    fn a_tie_across_the_window_edge_cuts_between_rows_not_at_an_instant() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        for path in ["m.png", "n.png", "o.png"] {
            index
                .upsert_file(&FileRecord {
                    path: path.to_owned(),
                    size: 10,
                    mtime_ns: 1,
                    sha256: None,
                    kind: crate::FileKind::Image,
                    note_added_ns: Some(1_000),
                    rating: None,
                    width: None,
                    height: None,
                    taken_ns: None,
                    taken_reason: None,
                })
                .unwrap();
        }
        index
            .finish_scan(&["m.png".into(), "n.png".into(), "o.png".into()])
            .unwrap();

        let counts = index
            .view_facet_counts(&ViewQuery::default(), RecentWindow::On { limit: 2 })
            .unwrap();
        assert_eq!(
            counts.recent, 2,
            "three images added at one instant, and the window is still two rows"
        );
    }

    /// An empty library has no facet values, which is a panel with nothing in
    /// it — not a failure to count.
    #[test]
    fn facet_counts_on_an_empty_index_are_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        let counts = index
            .view_facet_counts(&ViewQuery::default(), RecentWindow::On { limit: 200 })
            .unwrap();
        assert_eq!(counts, FacetCounts::default());
    }
}
