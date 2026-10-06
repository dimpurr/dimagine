//! Read-only view queries over the index (Viewer Phase 1 backend).
//!
//! These functions answer the questions a viewer UI asks: which images are in
//! a folder (recursively or not), which match some tags and text, how are they
//! ordered, which collections exist and what do they contain. They are pure
//! reads over the SQLite index and never touch the library files.
//!
//! Paths are library-relative and `/`-separated, the same spelling the scan
//! stored. `folder` uses `""` for the library root.

use crate::{Index, IndexError};

/// The highest `rating` FORMAT §3.1 allows.
const MAX_RATING: u8 = 5;

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
}

/// What one note offers as evidence of being a collection (FORMAT §5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CollectionEvidence<'a> {
    /// The note's `kind` property.
    pub kind: Option<&'a str>,
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
    /// epoch. The viewer's "Recent" lens.
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
        sql.order_by = order_by(q);
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

/// The columns that decide whether a note is a collection (FORMAT §5): its
/// path and title source, the number of image members (self-embed excluded,
/// FORMAT §3.2) and whether a strong image embed failed to resolve.
const CANDIDATE_COLUMNS: &str = "notes.path, notes.props_json, \
     (SELECT count(*) FROM links WHERE links.src=notes.path AND links.syntax<>'wiki_link' \
        AND links.target IS NOT NULL \
        AND links.target IN (SELECT path FROM files WHERE kind='image') \
        AND links.target<>COALESCE(notes.image_path,'')) AS members, \
     EXISTS(SELECT 1 FROM links WHERE links.src=notes.path AND links.syntax<>'wiki_link' \
        AND links.target IS NULL) AS unresolved";

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
    members: usize,
    unresolved: usize,
}

impl CollectionCandidate {
    fn evidence(&self) -> CollectionEvidence<'_> {
        CollectionEvidence {
            kind: self.props.get("kind").and_then(serde_json::Value::as_str),
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
        let sql = view_sql(q);
        let total: i64 = self
            .conn()?
            .query_row(&sql.count(), bound(&sql.params), |row| row.get(0))?;
        let (page_sql, page_params) =
            view_sql(q).into_page(i64::from(q.limit), i64::from(q.offset));
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
        let (count_sql, count_params) = {
            let sql = view_sql(q);
            (sql.count(), sql.params)
        };
        let (page_sql, page_params) =
            view_sql(q).into_page(i64::from(q.limit), i64::from(q.offset));
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

    /// Notes that are collections (FORMAT §5), with their image member count.
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
            .map(|candidate| CollectionInfo {
                title: candidate
                    .props
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                note_path: candidate.path,
                member_count: candidate.members as u64,
            })
            .collect())
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
        let collection = |kind, members, unresolved| {
            note_is_collection(&CollectionEvidence {
                kind,
                members,
                unresolved,
            })
        };
        assert!(collection(Some("collection"), 0, 0));
        assert!(collection(None, 1, 0), "an image embed is enough");
        assert!(collection(None, 0, 1), "a broken image embed is enough");
        assert!(!collection(None, 0, 0));
        assert!(!collection(Some("note"), 0, 0));
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
            let rendered = index.view_query_plan(&query).unwrap();
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
}
