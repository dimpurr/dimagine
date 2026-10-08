//! Rebuildable SQLite index for a dimagine library.
//!
//! Library files remain the source of truth. This crate stores derived metadata
//! under `.dimagine/cache/index.sqlite` and never modifies library content.

mod view;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

pub use view::{
    collection_is_listed, note_is_collection, note_rating, AppearsIn, CollectionEvidence,
    CollectionInfo, ImageMetaStats, Neighbours, SortKey, ViewItem, ViewPage, ViewQuery,
};

const SCHEMA_VERSION: i64 = 7;
/// The schema version immediately below [`SCHEMA_VERSION`]. A database at
/// this version migrates in place: columns and tables it lacks are added,
/// every existing row is kept, and the first refresh backfills the new
/// per-image columns. Anything older stays on the rebuild path — a migration
/// only ever steps forward one version.
const MIGRATABLE_FROM: i64 = 6;
/// The `files` columns one migration step adds. All three hold the values a
/// refresh read from the image header; `NULL` is an honest "unknown", never
/// a zero (FORMAT §0: an unknown is not empty).
const MIGRATION_COLUMNS: [&str; 3] = ["width", "height", "taken_ns"];
static OPEN_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// The move pairings [`Index::finish_scan`] tries, strongest key first. Each
/// statement inserts `(new_path, old_path, first_seen_ns)` for an image that
/// appeared this scan and took over the `first_seen_ns` of a vanished image.
///
/// A key only pairs when it names exactly one vanished image and exactly one
/// appeared image. A key that two rows on either side share pairs nothing, so a
/// copy made in the same scan as a move is dated like the new file it is. An
/// image the index already held before this scan never appears on the right of
/// a pair, and a row a stronger key already paired is out of reach of the keys
/// that run after it.
const MOVE_PAIR_SQL: [&str; 2] = [
    // The paired note kept its id, so the image moved with it (FORMAT §3.3).
    // The id is unique per note, so one vanished and one appeared note share an
    // id exactly when that image moved; a copy of a note duplicates its id and
    // leaves both sides ambiguous, which pairs nothing at all.
    "WITH vanished AS ( \
       SELECT n.id AS id, n.image_path AS old_path, old.first_seen_ns AS first_seen_ns \
       FROM notes n \
       JOIN moved_from m ON m.kind='note' AND m.path=n.path \
       JOIN moved_from g ON g.kind='image' AND g.path=n.image_path \
       JOIN files old ON old.kind='image' AND old.path=n.image_path \
       WHERE n.id IS NOT NULL AND n.id<>'' \
     ), \
     appeared AS ( \
       SELECT n.id AS id, n.image_path AS new_path \
       FROM notes n \
       JOIN files f ON f.kind='image' AND f.path=n.image_path \
       WHERE n.id IS NOT NULL AND n.id<>'' \
         AND n.path IN (SELECT path FROM scan_seen) \
         AND f.path IN (SELECT path FROM temp.appeared_images) \
     ), \
     pairs AS ( \
       SELECT sole_vanished.old_path AS old_path, \
              sole_vanished.first_seen_ns AS first_seen_ns, \
              sole_appeared.new_path AS new_path \
       FROM (SELECT id, old_path, first_seen_ns FROM vanished GROUP BY id HAVING COUNT(*)=1) sole_vanished \
       JOIN (SELECT id, new_path FROM appeared GROUP BY id HAVING COUNT(*)=1) sole_appeared \
         ON sole_appeared.id=sole_vanished.id \
     ) \
     INSERT OR IGNORE INTO temp.move_pair(new_path,old_path,first_seen_ns) \
     SELECT new_path, old_path, first_seen_ns FROM pairs \
     WHERE new_path NOT IN (SELECT new_path FROM pairs GROUP BY new_path HAVING COUNT(*)>1) \
       AND old_path NOT IN (SELECT old_path FROM pairs GROUP BY old_path HAVING COUNT(*)>1)",
    // The same size and nanosecond mtime: moving a file keeps both, so this is
    // the key for what the id above did not pair — an image without a note, a
    // note without an id, and an id a copy made ambiguous. `HAVING COUNT(*)=1`
    // leaves one row per size and mtime on either side, which makes the join
    // below a pairing and not a cross product.
    "WITH sole_vanished AS ( \
       SELECT old.size AS size, old.mtime_ns AS mtime_ns, old.path AS old_path, \
              old.first_seen_ns AS first_seen_ns \
       FROM moved_from m JOIN files old ON old.path=m.path \
       WHERE m.kind='image' AND old.path NOT IN (SELECT old_path FROM temp.move_pair) \
       GROUP BY old.size, old.mtime_ns HAVING COUNT(*)=1 \
     ), \
     sole_appeared AS ( \
       SELECT f.size AS size, f.mtime_ns AS mtime_ns, f.path AS new_path \
       FROM files f \
       WHERE f.path IN (SELECT path FROM temp.appeared_images) \
         AND f.path NOT IN (SELECT new_path FROM temp.move_pair) \
       GROUP BY f.size, f.mtime_ns HAVING COUNT(*)=1 \
     ) \
     INSERT OR IGNORE INTO temp.move_pair(new_path,old_path,first_seen_ns) \
     SELECT sole_appeared.new_path, sole_vanished.old_path, sole_vanished.first_seen_ns \
     FROM sole_vanished JOIN sole_appeared \
     ON sole_appeared.size=sole_vanished.size AND sole_appeared.mtime_ns=sole_vanished.mtime_ns",
];

/// File classification recorded by a scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileKind {
    Image,
    Note,
    Raw,
    Canvas,
    Other,
}

impl FileKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Note => "note",
            Self::Raw => "raw",
            Self::Canvas => "canvas",
            Self::Other => "other",
        }
    }
}

/// File facts collected during a scan. Paths are relative to the library root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub sha256: Option<String>,
    pub kind: FileKind,
    /// The image note's own "added" time in ns since the Unix epoch: the note
    /// property `added`, else its `imported`. `None` means the note has no
    /// datetime, which is not "keep the stored value": the index then derives
    /// `added_ns` from `first_seen_ns` again, so removing the property clears
    /// it.
    pub note_added_ns: Option<i64>,
    /// The image note's `rating`, or `None` when it has none or an invalid one
    /// (FORMAT §3.1). Stored on the image row so sorting and filtering never
    /// have to read the note's properties.
    pub rating: Option<u8>,
    /// Pixel width after EXIF orientation, read from the image header, or
    /// `None` when the header could not be read. `None` is "unknown", never 0.
    pub width: Option<u32>,
    /// Pixel height after EXIF orientation; `None` under the same terms as
    /// [`FileRecord::width`].
    pub height: Option<u32>,
    /// The EXIF `DateTimeOriginal` (with `OffsetTimeOriginal` when present)
    /// as nanoseconds since the Unix epoch, or `None` when the image has no
    /// readable one. A missing date is an unknown, never a zero instant.
    pub taken_ns: Option<i64>,
}

/// Per-image metadata extracted from the image header, cached by the SHA-256
/// digest of the file content the way previews are keyed (HLD `preview`):
/// content that already exists in the cache is never parsed twice, whatever
/// path or mtime it arrives under. `None` fields are honest unknowns; a row
/// with all three `None` is the recorded fact that this particular content
/// has no readable header, so a duplicate or a later refresh neither re-reads
/// it nor mistakes the unknown for a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageMeta {
    /// Pixel width after EXIF orientation.
    pub width: Option<u32>,
    /// Pixel height after EXIF orientation.
    pub height: Option<u32>,
    /// EXIF `DateTimeOriginal` (with `OffsetTimeOriginal` when present) in
    /// ns since the Unix epoch.
    pub taken_ns: Option<i64>,
}

/// Parsed note fields. Body text is supplied separately with [`Index::set_note_body`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteRecord {
    pub path: String,
    pub image_path: Option<String>,
    pub id: Option<String>,
    pub title: String,
    pub tags: Vec<String>,
    pub props_json: String,
}

/// Link resolution status.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinkState {
    Resolved,
    Missing,
    Ambiguous,
}

/// Basis used to identify a possible move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveBasis {
    Id,
    Sha256,
}

/// How a link was written. Only a strong image embed makes a note a collection
/// (FORMAT §5), so the syntax has to survive into the index.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinkSyntax {
    /// `![[target]]`
    WikiEmbed,
    /// `[[target]]`
    WikiLink,
    /// `![alt](target)`
    MarkdownImage,
    /// A JSON Canvas `file` node.
    CanvasFileNode,
}

impl LinkSyntax {
    fn as_str(self) -> &'static str {
        match self {
            Self::WikiEmbed => "wiki_embed",
            Self::WikiLink => "wiki_link",
            Self::MarkdownImage => "markdown_image",
            Self::CanvasFileNode => "canvas_file_node",
        }
    }
}

/// A possible move, with paths kept as independent values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoveCandidate {
    pub basis: MoveBasis,
    pub key: String,
    pub old_path: String,
    pub new_path: String,
}

impl LinkState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// Link extracted from a note or board.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkRecord {
    pub src: String,
    pub raw: String,
    pub target: Option<String>,
    pub state: LinkState,
    /// How the link was written (FORMAT §5).
    pub syntax: LinkSyntax,
}

/// Errors returned by index operations.
#[derive(Debug)]
pub enum IndexError {
    Busy,
    ScanNotActive,
    ScanAborted,
    RebuildRequired(String),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "index is already open for this library"),
            Self::ScanNotActive => write!(f, "index write requires an active scan"),
            Self::ScanAborted => {
                write!(f, "index scan was aborted; begin a new scan before writing")
            }
            Self::RebuildRequired(reason) => write!(f, "index rebuild required: {reason}"),
            Self::Sqlite(error) => write!(f, "SQLite index error: {error}"),
            Self::Io(error) => write!(f, "index filesystem error: {error}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<rusqlite::Error> for IndexError {
    fn from(value: rusqlite::Error) -> Self {
        if matches!(value, rusqlite::Error::SqliteFailure(ref e, _) if e.code == rusqlite::ErrorCode::DatabaseBusy || e.code == rusqlite::ErrorCode::DatabaseLocked)
        {
            Self::Busy
        } else if matches!(value, rusqlite::Error::SqliteFailure(ref e, _) if e.code == rusqlite::ErrorCode::DatabaseCorrupt || e.code == rusqlite::ErrorCode::NotADatabase)
        {
            Self::RebuildRequired(value.to_string())
        } else {
            Self::Sqlite(value)
        }
    }
}

impl From<std::io::Error> for IndexError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Open index handle. Only one handle per canonical library root may be open in
/// this process; a competing opener receives [`IndexError::Busy`].
pub struct Index {
    conn: Option<Connection>,
    db_path: PathBuf,
    root_key: PathBuf,
    rebuild_required: bool,
    scan_active: bool,
    scan_aborted: bool,
}

impl Index {
    /// Opens (and creates when needed) `.dimagine/cache/index.sqlite`.
    pub fn open(library_root: impl AsRef<Path>) -> Result<Self, IndexError> {
        std::fs::create_dir_all(library_root.as_ref())?;
        let root = std::fs::canonicalize(library_root.as_ref())?;
        let db_path = root.join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db_path.parent().expect("database path has a parent"))?;
        let registry = OPEN_PATHS.get_or_init(|| Mutex::new(HashSet::new()));
        let mut open_paths = registry.lock().map_err(|_| IndexError::Busy)?;
        if !open_paths.insert(db_path.clone()) {
            return Err(IndexError::Busy);
        }
        drop(open_paths);

        let open_result = (|| {
            let conn = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )?;
            conn.busy_timeout(std::time::Duration::from_millis(0))?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            migrate(&conn)?;
            Ok(conn)
        })();
        match open_result {
            Ok(conn) => Ok(Self {
                conn: Some(conn),
                db_path,
                root_key: root,
                rebuild_required: false,
                scan_active: false,
                scan_aborted: false,
            }),
            Err(IndexError::RebuildRequired(_)) => Ok(Self {
                conn: None,
                db_path,
                root_key: root,
                rebuild_required: true,
                scan_active: false,
                scan_aborted: false,
            }),
            Err(error) => {
                if let Ok(mut paths) = registry.lock() {
                    paths.remove(&db_path);
                }
                Err(error)
            }
        }
    }

    /// True when this handle represents a database that must be discarded and rebuilt.
    pub fn rebuild_required(&self) -> bool {
        self.rebuild_required
    }

    fn conn(&self) -> Result<&Connection, IndexError> {
        self.conn
            .as_ref()
            .ok_or_else(|| IndexError::RebuildRequired("database is unavailable".into()))
    }

    /// Begins a scan transaction, clearing the previous seen-path set.
    pub fn begin_scan(&mut self) -> Result<(), IndexError> {
        if self.scan_active {
            self.abort_scan();
            return Err(IndexError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        self.conn()?.execute_batch(
            "BEGIN IMMEDIATE; \
             DELETE FROM scan_seen; \
             DROP TABLE IF EXISTS temp.pre_scan_images; \
             CREATE TEMP TABLE pre_scan_images(path TEXT PRIMARY KEY); \
             INSERT INTO temp.pre_scan_images(path) SELECT path FROM files WHERE kind='image';",
        )?;
        self.scan_active = true;
        self.scan_aborted = false;
        Ok(())
    }

    /// Cancels the current scan and rolls back every write made during it.
    pub fn abort_scan(&mut self) {
        if self.scan_active {
            if let Some(conn) = self.conn.as_ref() {
                let _ = conn.execute_batch("ROLLBACK;");
            }
            self.scan_active = false;
            self.scan_aborted = true;
        }
    }

    /// Inserts or updates a file row and marks its path as seen in this scan.
    ///
    /// `first_seen_ns` is set when the path is first inserted and never
    /// changed by an update; `added_ns` is the note's own "added" time (note
    /// `added`, else `imported`) with `first_seen_ns` as the fallback, kept in
    /// step with it by [`Index::finish_scan`] after a move is applied.
    /// Image metadata is stored as given, unlike `note_added_ns`: the
    /// refresh reads it from the same bytes whose digest the row records, so
    /// `None` is a fact about the content — later scans reuse it — and never
    /// a way to say "keep the stored value".
    pub fn upsert_file(&mut self, record: &FileRecord) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            let first_seen_ns = now_ns();
            let folder = folder_of(&record.path);
            let name_key = name_key(&record.path);
            self.conn()?.execute(
                "INSERT INTO files(path,size,mtime_ns,sha256,kind,first_seen_ns,note_added_ns,added_ns,folder,name_key,rating,width,height,taken_ns) \
                  VALUES(?1,?2,?3,?4,?5,?6,?7,COALESCE(?7,?6),?8,?9,?10,?11,?12,?13) \
                  ON CONFLICT(path) DO UPDATE SET \
                  sha256=CASE WHEN files.size=excluded.size AND files.mtime_ns=excluded.mtime_ns \
                    THEN COALESCE(excluded.sha256,files.sha256) ELSE excluded.sha256 END, \
                  size=excluded.size,mtime_ns=excluded.mtime_ns,kind=excluded.kind, \
                  note_added_ns=excluded.note_added_ns, \
                  added_ns=COALESCE(excluded.note_added_ns,files.first_seen_ns), \
                  folder=excluded.folder,name_key=excluded.name_key,rating=excluded.rating, \
                  width=excluded.width,height=excluded.height,taken_ns=excluded.taken_ns",
                params![
                    record.path,
                    record.size,
                    record.mtime_ns,
                    record.sha256,
                    record.kind.as_str(),
                    first_seen_ns,
                    record.note_added_ns,
                    folder,
                    name_key,
                    record.rating.map(i64::from),
                    record.width.map(i64::from),
                    record.height.map(i64::from),
                    record.taken_ns,
                ],
            )?;
            self.conn()?.execute(
                "INSERT OR IGNORE INTO scan_seen(path) VALUES(?1)",
                [&record.path],
            )?;
            Ok(())
        })();
        self.fail_scan_on_error(result)
    }

    /// Inserts or updates parsed note metadata and marks the note path seen.
    ///
    /// The note's tags also go into `note_tags`, one row per case-folded tag
    /// of an image note, so a tag filter and the tag counts are index lookups
    /// instead of a scan over every note's properties.
    pub fn upsert_note(&mut self, record: &NoteRecord) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            let title = searchable(&record.title);
            let tags = searchable(&record.tags.join(" "));
            // Tags belong to the image, not to the note: a note that stopped
            // being an image note must not leave its tags behind, and a note
            // that became one must not inherit them. Both the stored image and
            // the incoming one are cleared, before the note row is updated.
            self.conn()?.execute(
                "DELETE FROM note_tags WHERE image_path IN ( \
                   COALESCE((SELECT image_path FROM notes WHERE path=?1),''), COALESCE(?2,''))",
                params![record.path, record.image_path],
            )?;
            self.conn()?.execute(
                "INSERT INTO notes(path,image_path,id,title,tags,props_json,body) VALUES(?1,?2,?3,?4,?5,?6,'') \
             ON CONFLICT(path) DO UPDATE SET image_path=excluded.image_path,id=excluded.id, \
             title=excluded.title,tags=excluded.tags,props_json=excluded.props_json",
                params![record.path, record.image_path, record.id, title, tags, record.props_json],
            )?;
            if let Some(image_path) = &record.image_path {
                for tag in &record.tags {
                    let tag = searchable(tag);
                    self.conn()?.execute(
                        "INSERT OR IGNORE INTO note_tags(image_path,tag) VALUES(?1,?2)",
                        params![image_path, tag],
                    )?;
                }
            }
            self.conn()?.execute(
                "INSERT OR IGNORE INTO scan_seen(path) VALUES(?1)",
                [&record.path],
            )?;
            Ok(())
        })();
        self.fail_scan_on_error(result)
    }

    /// Stores note body text for full-text search.
    pub fn set_note_body(&mut self, path: &str, body: &str) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = self
            .conn()?
            .execute(
                "UPDATE notes SET body=?2 WHERE path=?1",
                params![path, searchable(body)],
            )
            .map(|_| ())
            .map_err(IndexError::from);
        self.fail_scan_on_error(result)
    }

    fn fail_scan_on_error<T>(&mut self, result: Result<T, IndexError>) -> Result<T, IndexError> {
        if result.is_err() {
            self.abort_scan();
        }
        result
    }

    fn ensure_scan_active(&self) -> Result<(), IndexError> {
        if self.scan_aborted {
            Err(IndexError::ScanAborted)
        } else if self.scan_active {
            Ok(())
        } else {
            Err(IndexError::ScanNotActive)
        }
    }

    /// Replaces all outgoing links for one source path.
    pub fn replace_links(&mut self, src: &str, links: &[LinkRecord]) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            self.conn()?
                .execute("DELETE FROM links WHERE src=?1", [src])?;
            let mut unique = HashSet::new();
            for link in links {
                if unique.insert((
                    link.raw.as_str(),
                    link.target.as_deref(),
                    link.state,
                    link.syntax,
                )) {
                    self.conn()?.execute(
                        "INSERT INTO links(src,raw,target,state,syntax) VALUES(?1,?2,?3,?4,?5)",
                        params![
                            src,
                            link.raw,
                            link.target,
                            link.state.as_str(),
                            link.syntax.as_str()
                        ],
                    )?;
                }
            }
            Ok(())
        })();
        self.fail_scan_on_error(result)
    }

    /// Commits the scan and deletes file/note/link rows whose paths vanished.
    ///
    /// An active scan is required: call [`Index::begin_scan`] first. Finishing without one
    /// returns [`IndexError::ScanNotActive`] (or [`IndexError::ScanAborted`] after
    /// [`Index::abort_scan`]) and prunes nothing, so a caller that lost its `begin_scan`
    /// can never empty the index.
    ///
    /// The row that appeared at a new path inherits the `first_seen_ns` of the
    /// row that vanished, so a move never resets "time in the index". The
    /// pairing keys are the paired note's `id` (FORMAT §3.3) and an identical
    /// size plus nanosecond mtime, and a key is used only when it names one
    /// vanished and one appeared image: an ambiguous match hands `first_seen_ns`
    /// to none of them, so a copy made in the same scan as a move keeps its own.
    /// Both keys are facts the scan already has, so [`Index::needs_hash`]
    /// hashing is never required.
    pub fn finish_scan(&mut self, seen_paths: &[String]) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            let conn = self.conn()?;
            for path in seen_paths {
                conn.execute("INSERT OR IGNORE INTO scan_seen(path) VALUES(?1)", [path])?;
            }
            conn.execute("DELETE FROM moved_from", [])?;
            conn.execute(
                "INSERT INTO moved_from(path,id,sha256,kind) \
                 SELECT n.path,n.id,NULL,'note' FROM notes n \
                 WHERE n.path NOT IN (SELECT path FROM scan_seen)",
                [],
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO moved_from(path,id,sha256,kind) \
                 SELECT f.path,NULL,f.sha256,'image' FROM files f WHERE f.kind='image' \
                 AND f.path NOT IN (SELECT path FROM scan_seen)",
                [],
            )?;
            // A detected move keeps the old first_seen: the file at its new
            // path inherits the first_seen of the vanished row, so "time in
            // the index" survives a move. Only images the index did not hold
            // before this scan can be on the receiving end of such a pair.
            conn.execute_batch(
                "DROP TABLE IF EXISTS temp.appeared_images; \
                 CREATE TEMP TABLE appeared_images(path TEXT PRIMARY KEY); \
                 INSERT INTO temp.appeared_images(path) SELECT path FROM files \
                 WHERE kind='image' AND path IN (SELECT path FROM scan_seen) \
                 AND path NOT IN (SELECT path FROM temp.pre_scan_images); \
                 DROP TABLE IF EXISTS temp.move_pair; \
                 CREATE TEMP TABLE move_pair(new_path TEXT PRIMARY KEY, old_path TEXT UNIQUE, first_seen_ns INTEGER NOT NULL);",
            )?;
            for statement in MOVE_PAIR_SQL {
                conn.execute(statement, [])?;
            }
            conn.execute(
                "UPDATE files SET first_seen_ns=COALESCE(( \
                   SELECT p.first_seen_ns FROM move_pair p WHERE p.new_path=files.path \
                 ), first_seen_ns) \
                 WHERE kind='image' AND path IN (SELECT new_path FROM move_pair)",
                [],
            )?;
            conn.execute_batch(
                "DROP TABLE IF EXISTS temp.move_pair; \
                 DROP TABLE IF EXISTS temp.appeared_images;",
            )?;
            // The inherited first_seen is also the added position of an image
            // whose note has no datetime, so derive added_ns from the two
            // columns again (FORMAT §3.1: added, else imported, else
            // first_seen).
            conn.execute(
                "UPDATE files SET added_ns=COALESCE(note_added_ns,first_seen_ns) WHERE kind='image'",
                [],
            )?;
            conn.execute(
                "DELETE FROM files WHERE path NOT IN (SELECT path FROM scan_seen)",
                [],
            )?;
            conn.execute(
                "DELETE FROM notes WHERE path NOT IN (SELECT path FROM scan_seen)",
                [],
            )?;
            conn.execute(
                "DELETE FROM note_tags WHERE image_path NOT IN ( \
                   SELECT image_path FROM notes WHERE image_path IS NOT NULL)",
                [],
            )?;
            conn.execute(
                "DELETE FROM links WHERE src NOT IN (SELECT path FROM scan_seen)",
                [],
            )?;
            conn.execute("DELETE FROM scan_seen", [])?;
            conn.execute_batch("COMMIT;")?;
            Ok(())
        })();
        if result.is_err() {
            self.abort_scan();
        } else {
            self.scan_active = false;
            self.scan_aborted = false;
        }
        result
    }

    /// Returns whether content hashing is needed; reuse requires unchanged size
    /// and nanosecond mtime plus a valid cached SHA-256 digest.
    pub fn needs_hash(&self, path: &str, size: u64, mtime_ns: i64) -> Result<bool, IndexError> {
        let previous: Option<(i64, i64, Option<String>)> = self
            .conn()?
            .query_row(
                "SELECT size,mtime_ns,sha256 FROM files WHERE path=?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(
            !matches!(previous, Some((s, m, Some(ref hash))) if s == size as i64 && m == mtime_ns && valid_sha256(hash)),
        )
    }

    /// The image metadata a refresh can reuse because the file did not change:
    /// the stored SHA-256 digest plus the per-image header columns, when the
    /// row exists with the same size and nanosecond mtime and already carries
    /// a valid digest (the digest is what proves a previous refresh read this
    /// file's content; before it existed the columns are all `NULL` and the
    /// caller must read the file once). `None` dimensions in a `Some` row are
    /// the recorded fact that this content has no readable header, so they
    /// are reused like any other value instead of triggering a re-read.
    pub fn reusable_image_meta(
        &self,
        path: &str,
        size: u64,
        mtime_ns: i64,
    ) -> Result<Option<(String, ImageMeta)>, IndexError> {
        /// One stored files row, before its digest is judged usable.
        struct Stored {
            sha256: Option<String>,
            width: Option<i64>,
            height: Option<i64>,
            taken_ns: Option<i64>,
        }
        let previous: Option<Stored> = self
            .conn()?
            .query_row(
                "SELECT sha256,width,height,taken_ns FROM files \
                 WHERE path=?1 AND size=?2 AND mtime_ns=?3",
                params![path, size as i64, mtime_ns],
                |row| {
                    Ok(Stored {
                        sha256: row.get(0)?,
                        width: row.get(1)?,
                        height: row.get(2)?,
                        taken_ns: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(previous.and_then(|stored| {
            stored
                .sha256
                .filter(|hash| valid_sha256(hash))
                .map(|sha256| {
                    (
                        sha256,
                        ImageMeta {
                            width: stored.width.and_then(|value| u32::try_from(value).ok()),
                            height: stored.height.and_then(|value| u32::try_from(value).ok()),
                            taken_ns: stored.taken_ns,
                        },
                    )
                })
        }))
    }

    /// The header metadata cached for one SHA-256 digest ([`ImageMeta`]).
    /// `Some` of all-`NULL` is a hit: this content's header is known
    /// unreadable, which is exactly what a duplicate should reuse.
    pub fn cached_image_meta(&self, sha256: &str) -> Result<Option<ImageMeta>, IndexError> {
        let cached: Option<(Option<i64>, Option<i64>, Option<i64>)> = self
            .conn()?
            .query_row(
                "SELECT width,height,taken_ns FROM image_meta WHERE sha256=?1",
                [sha256],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(cached.map(|(width, height, taken_ns)| ImageMeta {
            width: width.and_then(|value| u32::try_from(value).ok()),
            height: height.and_then(|value| u32::try_from(value).ok()),
            taken_ns,
        }))
    }

    /// Records the header metadata for one SHA-256 digest, so the same content
    /// elsewhere in the library — a duplicate, a move, a touched file — never
    /// parses it twice. The last writer for a digest wins; reads see rows
    /// written earlier in the same scan, which is what lets one refresh
    /// share work between identical files.
    pub fn cache_image_meta(&mut self, sha256: &str, meta: &ImageMeta) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = self
            .conn()?
            .execute(
                "INSERT INTO image_meta(sha256,width,height,taken_ns) VALUES(?1,?2,?3,?4) \
                 ON CONFLICT(sha256) DO UPDATE SET \
                 width=excluded.width,height=excluded.height,taken_ns=excluded.taken_ns",
                params![
                    sha256,
                    meta.width.map(i64::from),
                    meta.height.map(i64::from),
                    meta.taken_ns
                ],
            )
            .map(|_| ())
            .map_err(IndexError::from);
        self.fail_scan_on_error(result)
    }

    /// Finds all paths with the supplied SHA-256 digest.
    pub fn by_sha256(&self, sha256: &str) -> Result<Vec<String>, IndexError> {
        let mut stmt = self
            .conn()?
            .prepare("SELECT path FROM files WHERE sha256=?1 ORDER BY path")?;
        let rows = stmt.query_map([sha256], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns possible moves keyed by a note ID or image SHA-256 digest.
    pub fn moved_candidates(&self) -> Result<Vec<MoveCandidate>, IndexError> {
        let mut stmt = self.conn()?.prepare(
            "SELECT 'id',m.id,m.path,n.path FROM moved_from m JOIN notes n ON n.id=m.id \
             WHERE m.kind='note' AND m.id IS NOT NULL AND m.path<>n.path \
             UNION ALL SELECT 'sha256',m.sha256,m.path,f.path FROM moved_from m JOIN files f ON f.sha256=m.sha256 \
             WHERE m.kind='image' AND m.sha256 IS NOT NULL AND f.kind='image' AND m.path<>f.path ORDER BY 1,2,3,4",
        )?;
        let rows = stmt.query_map([], |row| {
            let basis: String = row.get(0)?;
            Ok(MoveCandidate {
                basis: if basis == "id" {
                    MoveBasis::Id
                } else {
                    MoveBasis::Sha256
                },
                key: row.get(1)?,
                old_path: row.get(2)?,
                new_path: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Searches note title, tags, and body, and returns the matching note
    /// paths. Terms shorter than three Unicode characters use substring
    /// matching because the trigram tokenizer needs 3.
    ///
    /// This is the standalone search; the `text` filter of
    /// [`crate::ViewQuery`] applies the same two rules inside the view's own
    /// SQL, so a filtered page and a search always agree.
    pub fn search_text(&self, query: &str) -> Result<Vec<String>, IndexError> {
        let query = searchable(query);
        if query.chars().count() < 3 {
            let escaped = query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            let mut stmt = self.conn()?.prepare(
                "SELECT path FROM notes WHERE title LIKE ?1 ESCAPE '\\' OR tags LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\' ORDER BY path",
            )?;
            let rows = stmt.query_map([pattern], |row| row.get(0))?;
            return Ok(rows.collect::<Result<Vec<_>, _>>()?);
        }
        let mut stmt = self
            .conn()?
            .prepare("SELECT notes.path FROM notes_fts JOIN notes ON notes.note_id=notes_fts.rowid WHERE notes_fts MATCH ?1 ORDER BY notes.path")?;
        let rows = stmt.query_map([format!("\"{}\"", query.replace('"', "\"\""))], |row| {
            row.get(0)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Database path, useful for diagnostics and caller-managed rebuilds.
    pub fn database_path(&self) -> &Path {
        &self.db_path
    }

    /// Library root associated with this handle.
    pub fn library_root(&self) -> &Path {
        &self.root_key
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        self.abort_scan();
        if let Some(registry) = OPEN_PATHS.get() {
            if let Ok(mut paths) = registry.lock() {
                paths.remove(&self.db_path);
            }
        }
    }
}

fn migrate(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);")?;
    let version: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(version) = version {
        if version != SCHEMA_VERSION && version != MIGRATABLE_FROM {
            return Err(IndexError::RebuildRequired(format!(
                "schema version {version} does not match supported version {SCHEMA_VERSION}"
            )));
        }
    }
    if version.is_none() {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,size INTEGER NOT NULL,mtime_ns INTEGER NOT NULL,sha256 TEXT,kind TEXT NOT NULL,first_seen_ns INTEGER NOT NULL,note_added_ns INTEGER,added_ns INTEGER NOT NULL,folder TEXT NOT NULL DEFAULT '',name_key TEXT NOT NULL DEFAULT '',rating INTEGER,width INTEGER,height INTEGER,taken_ns INTEGER);
             CREATE TABLE IF NOT EXISTS notes(note_id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,image_path TEXT,id TEXT,title TEXT NOT NULL,tags TEXT NOT NULL,props_json TEXT NOT NULL,body TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS links(src TEXT NOT NULL,raw TEXT NOT NULL,target TEXT,state TEXT NOT NULL,syntax TEXT NOT NULL DEFAULT 'wiki_link',PRIMARY KEY(src,raw));
             CREATE TABLE IF NOT EXISTS note_tags(image_path TEXT NOT NULL,tag TEXT NOT NULL,PRIMARY KEY(image_path,tag));
             CREATE TABLE IF NOT EXISTS scan_seen(path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS moved_from(path TEXT PRIMARY KEY,id TEXT,sha256 TEXT,kind TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS image_meta(sha256 TEXT PRIMARY KEY,width INTEGER,height INTEGER,taken_ns INTEGER);
             CREATE INDEX IF NOT EXISTS files_sha256_idx ON files(sha256);
             CREATE INDEX IF NOT EXISTS files_added_ns_idx ON files(added_ns);
             CREATE INDEX IF NOT EXISTS files_size_idx ON files(size);
             CREATE INDEX IF NOT EXISTS files_mtime_ns_idx ON files(mtime_ns);
             CREATE INDEX IF NOT EXISTS files_folder_idx ON files(folder);
             CREATE INDEX IF NOT EXISTS files_kind_path_idx ON files(kind,path);
             CREATE INDEX IF NOT EXISTS files_rating_idx ON files(rating);
             CREATE INDEX IF NOT EXISTS files_taken_ns_idx ON files(taken_ns);
             CREATE INDEX IF NOT EXISTS notes_id_idx ON notes(id);
             CREATE INDEX IF NOT EXISTS notes_image_path_idx ON notes(image_path);
             CREATE INDEX IF NOT EXISTS note_tags_tag_idx ON note_tags(tag);
             CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title,tags,body,content='notes',content_rowid='note_id',tokenize='trigram');
             CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
               INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body);
             END;
             CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
               INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body);
             END;
             CREATE TRIGGER notes_au AFTER UPDATE OF title,tags,body ON notes BEGIN
               INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body);
               INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body);
             END;
             DELETE FROM schema_version;
             INSERT INTO schema_version(version) VALUES(7);
             COMMIT;",
        )?;
    } else if version == Some(MIGRATABLE_FROM) {
        upgrade_from_previous(conn)?;
    }
    validate_schema(conn)?;
    Ok(())
}

/// Bring a [`MIGRATABLE_FROM`] database up to [`SCHEMA_VERSION`] in place:
/// add the columns and tables the newer schema has, keep every row, and bump
/// the recorded version. All in one transaction, so a crash leaves the old
/// schema fully intact. Adding only what is missing makes re-running the
/// step harmless, which matters because an interrupted upgrade is never
/// visible as a half-changed schema.
fn upgrade_from_previous(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| {
        for column in MIGRATION_COLUMNS {
            if !table_has_column(conn, "files", column)? {
                conn.execute(
                    &format!("ALTER TABLE files ADD COLUMN {column} INTEGER"),
                    [],
                )?;
            }
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS image_meta(sha256 TEXT PRIMARY KEY,width INTEGER,height INTEGER,taken_ns INTEGER);
             CREATE INDEX IF NOT EXISTS files_taken_ns_idx ON files(taken_ns);
             DELETE FROM schema_version;
             INSERT INTO schema_version(version) VALUES(7);
             COMMIT;",
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
    }
    result
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, IndexError> {
    let has: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name=?)",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(has)
}

fn validate_schema(conn: &Connection) -> Result<(), IndexError> {
    let required = [
        "files",
        "notes",
        "links",
        "note_tags",
        "scan_seen",
        "moved_from",
        "image_meta",
        "notes_fts",
    ];
    for table in required {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 AND type IN ('table','view'))",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(IndexError::RebuildRequired(format!(
                "required table {table} is missing"
            )));
        }
    }
    for (table, expected_columns) in [
        (
            "files",
            &[
                "path",
                "size",
                "mtime_ns",
                "sha256",
                "kind",
                "first_seen_ns",
                "note_added_ns",
                "added_ns",
                "folder",
                "name_key",
                "rating",
                "width",
                "height",
                "taken_ns",
            ][..],
        ),
        (
            "notes",
            &[
                "note_id",
                "path",
                "image_path",
                "id",
                "title",
                "tags",
                "props_json",
                "body",
            ][..],
        ),
        ("links", &["src", "raw", "target", "state", "syntax"][..]),
        ("note_tags", &["image_path", "tag"][..]),
        ("scan_seen", &["path"][..]),
        ("moved_from", &["path", "id", "sha256", "kind"][..]),
        ("image_meta", &["sha256", "width", "height", "taken_ns"][..]),
    ] {
        let columns: HashSet<String> = {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let values = stmt
                .query_map([], |row| row.get(1))?
                .collect::<Result<_, _>>()?;
            values
        };
        for column in expected_columns {
            if !columns.contains(*column) {
                return Err(IndexError::RebuildRequired(format!(
                    "required {table} column {column} is missing"
                )));
            }
        }
    }
    for (table, expected_key) in [
        ("files", vec!["path"]),
        ("notes", vec!["note_id"]),
        ("links", vec!["src", "raw"]),
        ("note_tags", vec!["image_path", "tag"]),
        ("scan_seen", vec!["path"]),
        ("moved_from", vec!["path"]),
        ("image_meta", vec!["sha256"]),
    ] {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut key_columns = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        key_columns.retain(|(position, _)| *position > 0);
        key_columns.sort_by_key(|(position, _)| *position);
        if key_columns
            .into_iter()
            .map(|(_, column)| column)
            .collect::<Vec<_>>()
            != expected_key
        {
            return Err(IndexError::RebuildRequired(format!(
                "{table} has an incompatible primary key"
            )));
        }
    }
    if !has_unique_index(conn, "notes", &["path"])? {
        return Err(IndexError::RebuildRequired(
            "notes.path must be unique".into(),
        ));
    }
    let fts_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='notes_fts'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if !fts_sql.is_some_and(|sql| {
        sql.contains("content='notes'")
            && sql.contains("content_rowid='note_id'")
            && sql.contains("trigram")
    }) {
        return Err(IndexError::RebuildRequired(
            "notes_fts has an incompatible configuration".into(),
        ));
    }
    for (trigger, expected_sql) in [
        (
            "notes_ai",
            "CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body); END",
        ),
        (
            "notes_ad",
            "CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body); END",
        ),
        (
            "notes_au",
            "CREATE TRIGGER notes_au AFTER UPDATE OF title,tags,body ON notes BEGIN INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body); INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body); END",
        ),
    ] {
        let trigger_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name=?1 AND type='trigger' AND tbl_name='notes'",
                [trigger],
                |row| row.get(0),
            )
            .optional()?;
        if trigger_sql.is_none_or(|sql| normalize_sql(&sql) != normalize_sql(expected_sql)) {
            return Err(IndexError::RebuildRequired(format!(
                "required FTS synchronization trigger {trigger} is missing or incompatible"
            )));
        }
    }
    Ok(())
}

fn has_unique_index(
    conn: &Connection,
    table: &str,
    expected_columns: &[&str],
) -> Result<bool, IndexError> {
    let mut indexes = conn.prepare(&format!("PRAGMA index_list({table})"))?;
    let names = indexes
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, unique) in names {
        if !unique {
            continue;
        }
        let mut columns = conn.prepare(&format!(
            "PRAGMA index_info('{}')",
            name.replace('\'', "''")
        ))?;
        let actual = columns
            .query_map([], |row| row.get::<_, String>(2))?
            .collect::<Result<Vec<_>, _>>()?;
        if actual.iter().map(String::as_str).collect::<Vec<_>>() == expected_columns {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_sql(sql: &str) -> String {
    sql.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// The folder a library-relative path sits in: `""` for the library root.
fn folder_of(path: &str) -> &str {
    path.rfind('/').map_or("", |index| &path[..index])
}

/// The file name of a library-relative path, case-folded for a name sort that
/// does not depend on SQLite's ASCII-only `lower()`.
fn name_key(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.case_fold().collect()
}

fn searchable(text: &str) -> String {
    text.case_fold().nfc().collect()
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn file(path: &str, size: u64, mtime_ns: i64, sha256: Option<&str>) -> FileRecord {
        FileRecord {
            path: path.into(),
            size,
            mtime_ns,
            sha256: sha256.map(str::to_owned),
            kind: FileKind::Image,
            note_added_ns: None,
            rating: None,
            width: None,
            height: None,
            taken_ns: None,
        }
    }

    fn note(path: &str, id: Option<&str>, title: &str, tags: &[&str]) -> NoteRecord {
        NoteRecord {
            path: path.into(),
            image_path: None,
            id: id.map(str::to_owned),
            title: title.into(),
            tags: tags.iter().map(|s| (*s).into()).collect(),
            props_json: "{}".into(),
        }
    }

    #[test]
    fn incremental_scan_hash_move_and_vanish() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("old.jpg", 10, 20, Some(&"a".repeat(64))))
            .unwrap();
        index.upsert_file(&file("gone.jpg", 5, 7, None)).unwrap();
        index
            .upsert_note(&note("old.jpg.md", Some("stable-id"), "海の絵", &["海"]))
            .unwrap();
        index
            .finish_scan(&["old.jpg".into(), "old.jpg.md".into(), "gone.jpg".into()])
            .unwrap();
        assert!(!index.needs_hash("old.jpg", 10, 20).unwrap());
        assert!(index.needs_hash("old.jpg", 11, 20).unwrap());
        assert!(index.needs_hash("absent.jpg", 10, 20).unwrap());
        drop(index);

        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("new.jpg", 10, 20, Some(&"a".repeat(64))))
            .unwrap();
        index
            .upsert_note(&note("new.jpg.md", Some("stable-id"), "海の絵", &["海"]))
            .unwrap();
        index
            .finish_scan(&["new.jpg".into(), "new.jpg.md".into()])
            .unwrap();
        assert_eq!(index.by_sha256(&"a".repeat(64)).unwrap(), vec!["new.jpg"]);
        assert!(index.by_sha256("none").unwrap().is_empty());
        assert_eq!(index.moved_candidates().unwrap().len(), 2);
        assert!(index.needs_hash("gone.jpg", 5, 7).unwrap());
    }

    #[test]
    fn full_text_cjk_and_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_note(&note("cn.md", None, "深海少女", &["水彩"]))
            .unwrap();
        index.set_note_body("cn.md", "海の中で眠る少女").unwrap();
        index
            .upsert_note(&note("jp.md", None, "星空の旅", &["夜空"]))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        assert_eq!(index.search_text("海の中").unwrap(), vec!["cn.md"]);
        assert_eq!(index.search_text("少女").unwrap(), vec!["cn.md"]);
        assert_eq!(index.search_text("星空").unwrap(), vec!["jp.md"]);
    }

    #[test]
    fn corrupt_database_returns_rebuild_signal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"not a sqlite database").unwrap();
        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
        assert!(matches!(
            index.needs_hash("x", 1, 1),
            Err(IndexError::RebuildRequired(_))
        ));
    }

    #[test]
    fn first_seen_survives_rescans_and_moves() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("a.jpg", 10, 20, Some(&"a".repeat(64))))
            .unwrap();
        index.finish_scan(&["a.jpg".into()]).unwrap();
        let first_seen: i64 = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT first_seen_ns FROM files WHERE path='a.jpg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(first_seen > 0);

        index.begin_scan().unwrap();
        let mut record = file("a.jpg", 10, 20, Some(&"a".repeat(64)));
        record.note_added_ns = Some(123);
        index.upsert_file(&record).unwrap();
        index.finish_scan(&["a.jpg".into()]).unwrap();
        let (after_rescan, added): (i64, i64) = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT first_seen_ns, added_ns FROM files WHERE path='a.jpg'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after_rescan, first_seen, "a rescan keeps first_seen");
        assert_eq!(added, 123);

        index.begin_scan().unwrap();
        index
            .upsert_file(&file("b.jpg", 10, 20, Some(&"a".repeat(64))))
            .unwrap();
        index.finish_scan(&["b.jpg".into()]).unwrap();
        let moved: i64 = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT first_seen_ns FROM files WHERE path='b.jpg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            moved, first_seen,
            "a detected move keeps the old first_seen"
        );
    }

    fn first_seen_of(index: &Index, path: &str) -> i64 {
        index
            .conn()
            .unwrap()
            .query_row(
                "SELECT first_seen_ns FROM files WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// A production scan never hashes (`sync_index` sends no `sha256`), so a
    /// move has to be recognised from the facts it does have.
    #[test]
    fn first_seen_follows_a_move_without_a_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/a.jpg", 10, 20, None))
            .unwrap();
        index
            .upsert_note(&note("refs/a.jpg.md", None, "moved", &[]))
            .unwrap();
        index
            .finish_scan(&["refs/a.jpg".into(), "refs/a.jpg.md".into()])
            .unwrap();
        let first_seen = first_seen_of(&index, "refs/a.jpg");
        assert!(first_seen > 0);

        index.begin_scan().unwrap();
        // The same bytes at a new path, with the same size and mtime, which is
        // what moving a file within a library leaves behind.
        index
            .upsert_file(&file("refs/b.jpg", 10, 20, None))
            .unwrap();
        index
            .upsert_note(&note("refs/b.jpg.md", None, "moved", &[]))
            .unwrap();
        index
            .finish_scan(&["refs/b.jpg".into(), "refs/b.jpg.md".into()])
            .unwrap();
        assert_eq!(
            first_seen_of(&index, "refs/b.jpg"),
            first_seen,
            "size and mtime pair a move, so first_seen survives it"
        );
    }

    #[test]
    fn first_seen_follows_the_paired_note_id_across_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index.upsert_file(&file("a.jpg", 10, 20, None)).unwrap();
        let mut paired = note("a.jpg.md", Some("stable-id"), "moved", &[]);
        paired.image_path = Some("a.jpg".into());
        index.upsert_note(&paired).unwrap();
        index
            .finish_scan(&["a.jpg".into(), "a.jpg.md".into()])
            .unwrap();
        let first_seen = first_seen_of(&index, "a.jpg");

        // The image moved and was edited, so size and mtime no longer match;
        // the note kept its id, which is the key that survives.
        index.begin_scan().unwrap();
        index.upsert_file(&file("b.jpg", 99, 20, None)).unwrap();
        let mut paired = note("b.jpg.md", Some("stable-id"), "moved", &[]);
        paired.image_path = Some("b.jpg".into());
        index.upsert_note(&paired).unwrap();
        index
            .finish_scan(&["b.jpg".into(), "b.jpg.md".into()])
            .unwrap();
        assert_eq!(
            first_seen_of(&index, "b.jpg"),
            first_seen,
            "the paired note's id pairs a move even when the bytes changed"
        );
    }

    #[test]
    fn an_unrelated_new_image_keeps_its_own_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index.upsert_file(&file("gone.jpg", 10, 20, None)).unwrap();
        index.finish_scan(&["gone.jpg".into()]).unwrap();
        let vanished_first_seen = first_seen_of(&index, "gone.jpg");

        index.begin_scan().unwrap();
        index.upsert_file(&file("new.jpg", 77, 88, None)).unwrap();
        index.finish_scan(&["new.jpg".into()]).unwrap();
        assert_ne!(
            first_seen_of(&index, "new.jpg"),
            vanished_first_seen,
            "different size and mtime is not a move"
        );
    }

    /// RW26 H-1: Reviewer workflow (a) - copy image+note, then rename the
    /// original pair in the same scan. Note id pairing is ambiguous (1 vanished
    /// note id matches 2 appeared notes), so neither note inherits by id.
    /// The renamed original inherits via unambiguous 1:1 size+mtime, while the
    /// copy (new mtime) receives its own fresh first_seen_ns.
    #[test]
    fn copy_image_and_note_then_rename_original_does_not_transfer_to_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/a.png", 10, 20, None))
            .unwrap();
        let mut note_a = note("refs/a.png.md", Some("stable-0001"), "original", &[]);
        note_a.image_path = Some("refs/a.png".into());
        index.upsert_note(&note_a).unwrap();
        index
            .finish_scan(&["refs/a.png".into(), "refs/a.png.md".into()])
            .unwrap();
        let first_seen_a = first_seen_of(&index, "refs/a.png");
        assert!(first_seen_a > 0);

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        // Renamed original: keeps size 10 and mtime 20, note retains id: stable-0001
        index
            .upsert_file(&file("refs/b.png", 10, 20, None))
            .unwrap();
        let mut note_b = note("refs/b.png.md", Some("stable-0001"), "renamed", &[]);
        note_b.image_path = Some("refs/b.png".into());
        index.upsert_note(&note_b).unwrap();
        // Duplicate copy: size 10, new mtime 30 (from cp), duplicate note id: stable-0001
        index
            .upsert_file(&file("refs/c.png", 10, 30, None))
            .unwrap();
        let mut note_c = note("refs/c.png.md", Some("stable-0001"), "copy", &[]);
        note_c.image_path = Some("refs/c.png".into());
        index.upsert_note(&note_c).unwrap();

        index
            .finish_scan(&[
                "refs/b.png".into(),
                "refs/b.png.md".into(),
                "refs/c.png".into(),
                "refs/c.png.md".into(),
            ])
            .unwrap();

        assert_eq!(
            first_seen_of(&index, "refs/b.png"),
            first_seen_a,
            "renamed original inherits first_seen via 1:1 size+mtime"
        );
        assert_ne!(
            first_seen_of(&index, "refs/c.png"),
            first_seen_a,
            "the copy must not inherit first_seen"
        );
    }

    /// RW26 H-1: Reviewer workflow (b) - `cp -p` beside a move in one scan.
    /// One vanished row matches 2 appeared rows on size+mtime; because the
    /// pairing is ambiguous, first_seen_ns is transferred to neither.
    #[test]
    fn preserving_copy_beside_a_move_transfers_to_neither() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/x.png", 50, 60, None))
            .unwrap();
        index.finish_scan(&["refs/x.png".into()]).unwrap();
        let first_seen_x = first_seen_of(&index, "refs/x.png");
        assert!(first_seen_x > 0);

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/xcopy.png", 50, 60, None))
            .unwrap();
        index
            .upsert_file(&file("refs/y.png", 50, 60, None))
            .unwrap();
        index
            .finish_scan(&["refs/xcopy.png".into(), "refs/y.png".into()])
            .unwrap();

        assert_ne!(
            first_seen_of(&index, "refs/xcopy.png"),
            first_seen_x,
            "preserving copy does not inherit when ambiguous"
        );
        assert_ne!(
            first_seen_of(&index, "refs/y.png"),
            first_seen_x,
            "moved file does not inherit when copy creates ambiguous size+mtime"
        );
    }

    /// Several vanished rows matching one appeared row must transfer to none.
    #[test]
    fn several_vanished_rows_matching_one_appeared_transfers_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/v1.png", 40, 50, None))
            .unwrap();
        index
            .upsert_file(&file("refs/v2.png", 40, 50, None))
            .unwrap();
        index
            .finish_scan(&["refs/v1.png".into(), "refs/v2.png".into()])
            .unwrap();
        let first_seen_v1 = first_seen_of(&index, "refs/v1.png");
        let first_seen_v2 = first_seen_of(&index, "refs/v2.png");

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/a1.png", 40, 50, None))
            .unwrap();
        index.finish_scan(&["refs/a1.png".into()]).unwrap();

        let first_seen_a1 = first_seen_of(&index, "refs/a1.png");
        assert_ne!(first_seen_a1, first_seen_v1);
        assert_ne!(first_seen_a1, first_seen_v2);
    }

    /// Plain move without a note still inherits first_seen via 1:1 size+mtime.
    #[test]
    fn plain_move_without_note_still_inherits_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/plain.png", 100, 200, None))
            .unwrap();
        index.finish_scan(&["refs/plain.png".into()]).unwrap();
        let first_seen = first_seen_of(&index, "refs/plain.png");
        assert!(first_seen > 0);

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/moved.png", 100, 200, None))
            .unwrap();
        index.finish_scan(&["refs/moved.png".into()]).unwrap();

        assert_eq!(
            first_seen_of(&index, "refs/moved.png"),
            first_seen,
            "plain move with 1:1 size+mtime inherits first_seen"
        );
    }

    /// An image the index already held is not new, so it never receives a
    /// vanished row's first_seen_ns even when its size and mtime match one.
    #[test]
    fn an_image_the_index_already_held_never_takes_over_a_vanished_first_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/gone.png", 10, 20, None))
            .unwrap();
        index
            .upsert_file(&file("refs/kept.png", 10, 20, None))
            .unwrap();
        index
            .finish_scan(&["refs/gone.png".into(), "refs/kept.png".into()])
            .unwrap();
        let first_seen_gone = first_seen_of(&index, "refs/gone.png");
        let first_seen_kept = first_seen_of(&index, "refs/kept.png");

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        // refs/gone.png is deleted, refs/kept.png stays and something else is
        // added: only the added image is a candidate for the vanished row.
        index
            .upsert_file(&file("refs/new.png", 99, 88, None))
            .unwrap();
        index
            .finish_scan(&["refs/kept.png".into(), "refs/new.png".into()])
            .unwrap();

        assert_ne!(
            first_seen_of(&index, "refs/kept.png"),
            first_seen_gone,
            "an image that was already in the index keeps its own first_seen_ns"
        );
        assert_ne!(
            first_seen_of(&index, "refs/new.png"),
            first_seen_gone,
            "a different size and mtime is not a move"
        );
        assert_eq!(
            first_seen_of(&index, "refs/kept.png"),
            first_seen_kept,
            "an untouched row is not rewritten by the move pairing"
        );
    }

    /// A note that vanished while its image stayed is not a move: the image did
    /// not vanish, so nothing can hand its first_seen_ns to a new row.
    #[test]
    fn a_vanished_note_beside_a_staying_image_pairs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("refs/kept.png", 10, 20, None))
            .unwrap();
        let mut paired = note("refs/kept.png.md", Some("stable-0001"), "original", &[]);
        paired.image_path = Some("refs/kept.png".into());
        index.upsert_note(&paired).unwrap();
        index
            .finish_scan(&["refs/kept.png".into(), "refs/kept.png.md".into()])
            .unwrap();
        let first_seen_kept = first_seen_of(&index, "refs/kept.png");

        std::thread::sleep(std::time::Duration::from_millis(20));
        index.begin_scan().unwrap();
        // The note moved to a new image and the note id moved with it, but the
        // image it described never left refs/kept.png.
        index
            .upsert_file(&file("refs/other.png", 77, 88, None))
            .unwrap();
        let mut paired = note("refs/other.png.md", Some("stable-0001"), "moved", &[]);
        paired.image_path = Some("refs/other.png".into());
        index.upsert_note(&paired).unwrap();
        index
            .finish_scan(&[
                "refs/kept.png".into(),
                "refs/other.png".into(),
                "refs/other.png.md".into(),
            ])
            .unwrap();

        assert_ne!(
            first_seen_of(&index, "refs/other.png"),
            first_seen_kept,
            "an image that never vanished cannot donate its first_seen_ns"
        );
    }

    fn added_ns_of(index: &Index, path: &str) -> i64 {
        index
            .conn()
            .unwrap()
            .query_row("SELECT added_ns FROM files WHERE path=?1", [path], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// RW26 L-3: `added_ns=COALESCE(excluded.added_ns,files.added_ns)` meant a
    /// derived `added_ns` could never be cleared: delete the `added:` line from
    /// a note and the index kept the value forever, so only a rebuild fixed it.
    #[test]
    fn removing_the_note_datetime_clears_the_derived_added_ns() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index.upsert_file(&file("a.jpg", 1, 1, None)).unwrap();
        index.finish_scan(&["a.jpg".into()]).unwrap();
        let first_seen = first_seen_of(&index, "a.jpg");
        assert_eq!(
            added_ns_of(&index, "a.jpg"),
            first_seen,
            "an image with no note datetime falls back to first_seen"
        );

        index.begin_scan().unwrap();
        let mut record = file("a.jpg", 1, 1, None);
        record.note_added_ns = Some(42);
        index.upsert_file(&record).unwrap();
        index.finish_scan(&["a.jpg".into()]).unwrap();
        assert_eq!(added_ns_of(&index, "a.jpg"), 42, "the note's added wins");

        index.begin_scan().unwrap();
        index.upsert_file(&file("a.jpg", 1, 1, None)).unwrap();
        index.finish_scan(&["a.jpg".into()]).unwrap();
        assert_eq!(
            added_ns_of(&index, "a.jpg"),
            first_seen,
            "a removed property falls back to first_seen again"
        );
        let stored: Option<i64> = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT note_added_ns FROM files WHERE path='a.jpg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, None, "the note value itself is cleared too");
    }

    #[test]
    fn old_schema_version_requires_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE schema_version(version INTEGER NOT NULL)", [])
            .unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(2)", [])
            .unwrap();
        drop(conn);
        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
    }

    /// The schema of version 6 exactly as that code wrote it, so a database
    /// built by the previous dimagine can be faked faithfully: no per-image
    /// metadata columns, no probe cache, and rows written like the old
    /// `upsert_file` did.
    const V6_SCHEMA: &str = "CREATE TABLE schema_version(version INTEGER NOT NULL);
             CREATE TABLE files(path TEXT PRIMARY KEY,size INTEGER NOT NULL,mtime_ns INTEGER NOT NULL,sha256 TEXT,kind TEXT NOT NULL,first_seen_ns INTEGER NOT NULL,note_added_ns INTEGER,added_ns INTEGER NOT NULL,folder TEXT NOT NULL DEFAULT '',name_key TEXT NOT NULL DEFAULT '',rating INTEGER);
             CREATE TABLE notes(note_id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,image_path TEXT,id TEXT,title TEXT NOT NULL,tags TEXT NOT NULL,props_json TEXT NOT NULL,body TEXT NOT NULL DEFAULT '');
             CREATE TABLE links(src TEXT NOT NULL,raw TEXT NOT NULL,target TEXT,state TEXT NOT NULL,syntax TEXT NOT NULL DEFAULT 'wiki_link',PRIMARY KEY(src,raw));
             CREATE TABLE note_tags(image_path TEXT NOT NULL,tag TEXT NOT NULL,PRIMARY KEY(image_path,tag));
             CREATE TABLE scan_seen(path TEXT PRIMARY KEY);
             CREATE TABLE moved_from(path TEXT PRIMARY KEY,id TEXT,sha256 TEXT,kind TEXT NOT NULL);
             CREATE INDEX files_sha256_idx ON files(sha256);
             CREATE INDEX files_added_ns_idx ON files(added_ns);
             CREATE INDEX files_size_idx ON files(size);
             CREATE INDEX files_mtime_ns_idx ON files(mtime_ns);
             CREATE INDEX files_folder_idx ON files(folder);
             CREATE INDEX files_kind_path_idx ON files(kind,path);
             CREATE INDEX files_rating_idx ON files(rating);
             CREATE INDEX notes_id_idx ON notes(id);
             CREATE INDEX notes_image_path_idx ON notes(image_path);
             CREATE INDEX note_tags_tag_idx ON note_tags(tag);
             CREATE VIRTUAL TABLE notes_fts USING fts5(title,tags,body,content='notes',content_rowid='note_id',tokenize='trigram');
             CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
               INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body);
             END;
             CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
               INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body);
             END;
             CREATE TRIGGER notes_au AFTER UPDATE OF title,tags,body ON notes BEGIN
               INSERT INTO notes_fts(notes_fts,rowid,title,tags,body) VALUES('delete',old.note_id,old.title,old.tags,old.body);
               INSERT INTO notes_fts(rowid,title,tags,body) VALUES(new.note_id,new.title,new.tags,new.body);
             END;
             INSERT INTO schema_version(version) VALUES(6);
             INSERT INTO files(path,size,mtime_ns,sha256,kind,first_seen_ns,note_added_ns,added_ns,folder,name_key,rating) \
               VALUES('refs/old.jpg',7,8,NULL,'image',1000,NULL,1000,'refs','old.jpg',3);";

    /// W48: a database from schema version 6 — the version immediately before
    /// the per-image metadata columns — opens, migrates in place, and keeps
    /// every row. The FIRST SCAN afterwards fills the new columns, because a
    /// v6 image row has no content digest to reuse and the refresh must read
    /// the file once.
    #[test]
    fn previous_schema_version_migrates_in_place_keeping_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(V6_SCHEMA).unwrap();
        drop(conn);

        let mut index = Index::open(dir.path()).unwrap();
        assert!(
            !index.rebuild_required(),
            "a v6 database must not require a rebuild"
        );
        let version: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 7, "the schema version was bumped");
        let (size, rating, width, taken): (i64, Option<i64>, Option<i64>, Option<i64>) = index
            .conn()
            .unwrap()
            .query_row(
                "SELECT size,rating,width,taken_ns FROM files WHERE path='refs/old.jpg'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (size, rating, width, taken),
            (7, Some(3), None, None),
            "the old row is kept with its values, the new columns start unknown"
        );

        // The refresh that follows works exactly as on a fresh database: the
        // fresh columns are written, and the next refresh reuses the digest
        // now that the row has one.
        index.begin_scan().unwrap();
        index
            .upsert_file(&FileRecord {
                path: "refs/old.jpg".into(),
                size: 7,
                mtime_ns: 8,
                sha256: Some("a".repeat(64)),
                kind: FileKind::Image,
                note_added_ns: None,
                rating: Some(3),
                width: Some(64),
                height: Some(48),
                taken_ns: Some(1_689_191_647_000_000_000),
            })
            .unwrap();
        index.finish_scan(&["refs/old.jpg".into()]).unwrap();
        assert_eq!(
            index.reusable_image_meta("refs/old.jpg", 7, 8).unwrap(),
            Some((
                "a".repeat(64),
                ImageMeta {
                    width: Some(64),
                    height: Some(48),
                    taken_ns: Some(1_689_191_647_000_000_000),
                },
            )),
            "after the first refresh the columns are reusable like any other row's"
        );
        drop(index);

        // Idempotence: opening the migrated database again finds the current
        // version and changes nothing.
        let index = Index::open(dir.path()).unwrap();
        assert!(!index.rebuild_required());
        let version: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 7);
    }

    /// A v6 database that someone already hand-patched with some or all of
    /// the new columns still migrates: the step only adds what is missing.
    #[test]
    fn migrating_a_partially_newer_v6_database_adds_only_missing_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(V6_SCHEMA).unwrap();
        // A strange half-state: `width` already exists, the other two do not.
        conn.execute("ALTER TABLE files ADD COLUMN width INTEGER", [])
            .unwrap();
        drop(conn);
        let index = Index::open(dir.path()).unwrap();
        assert!(!index.rebuild_required());
        for (table, expected_key) in [("files", 11 + 3), ("image_meta", 4)] {
            let columns: i64 = index
                .conn()
                .unwrap()
                .query_row(
                    &format!("SELECT count(*) FROM pragma_table_info('{table}')"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(columns, expected_key as i64, "{table} has all columns");
        }
    }

    #[test]
    fn newer_schema_returns_rebuild_signal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE schema_version(version INTEGER NOT NULL)", [])
            .unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(999)", [])
            .unwrap();
        drop(conn);

        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
    }

    #[test]
    fn competing_opener_is_typed_busy_error() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Index::open(dir.path()).unwrap();
        assert!(matches!(Index::open(dir.path()), Err(IndexError::Busy)));
    }

    #[test]
    fn benchmark_insert_100k_synthetic_file_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        let started = Instant::now();
        index.begin_scan().unwrap();
        for i in 0..100_000 {
            index
                .upsert_file(&file(&format!("synthetic/{i:06}.jpg"), i, i as i64, None))
                .unwrap();
        }
        index.finish_scan(&[]).unwrap();
        eprintln!("inserted 100000 synthetic rows in {:?}", started.elapsed());
        assert_eq!(
            index
                .conn()
                .unwrap()
                .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            100_000
        );
    }

    #[test]
    #[ignore = "wall-clock scaling is not stable under parallel load; run on a quiet machine with scripts/dev/bench-index.sh"]
    fn ten_thousand_notes_refresh_fts_roughly_linearly() {
        fn index_notes(count: usize) -> (std::time::Duration, usize) {
            let dir = tempfile::tempdir().unwrap();
            let mut index = Index::open(dir.path()).unwrap();
            let started = Instant::now();
            index.begin_scan().unwrap();
            for i in 0..count {
                let path = format!("notes/{i:05}.md");
                index
                    .upsert_note(&note(
                        &path,
                        None,
                        &format!("Synthetic searchable title {i}"),
                        &["linear"],
                    ))
                    .unwrap();
                index
                    .set_note_body(&path, &format!("synthetic body for note {i}"))
                    .unwrap();
            }
            index.finish_scan(&[]).unwrap();
            let elapsed = started.elapsed();
            let rows = index.search_text("synthetic").unwrap().len();
            (elapsed, rows)
        }
        let (small, small_rows) = index_notes(1_000);
        let (large, large_rows) = index_notes(10_000);
        eprintln!("indexed 1,000 notes in {small:?}; 10,000 notes in {large:?}");
        assert_eq!(small_rows, 1_000);
        assert_eq!(large_rows, 10_000);
        assert!(
            large <= small.saturating_mul(20) + std::time::Duration::from_millis(100),
            "10x more notes took more than the 20x scaling allowance: {small:?} vs {large:?}"
        );
    }

    #[test]
    fn changed_or_missing_hash_is_never_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("a.jpg", 10, 20, Some(&"b".repeat(64))))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        assert!(!index.needs_hash("a.jpg", 10, 20).unwrap());
        index.begin_scan().unwrap();
        index.upsert_file(&file("a.jpg", 11, 21, None)).unwrap();
        index.finish_scan(&[]).unwrap();
        assert!(index.needs_hash("a.jpg", 11, 21).unwrap());
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("hashless.jpg", 4, 8, None))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        assert!(index.needs_hash("hashless.jpg", 4, 8).unwrap());
    }

    /// W48: the reuse rule for per-image metadata is the incremental-hashing
    /// rule — same size and nanosecond mtime plus a valid content digest —
    /// and the digest pinned there is what makes the columns trustworthy: a
    /// row that never got one (a migrated v6 row, a row the old scan wrote)
    /// must be read once instead of reusing `NULL`.
    #[test]
    fn image_meta_reuse_requires_matching_facts_and_a_valid_digest() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&FileRecord {
                path: "a.jpg".into(),
                size: 10,
                mtime_ns: 20,
                sha256: Some("b".repeat(64)),
                kind: FileKind::Image,
                note_added_ns: None,
                rating: None,
                width: Some(640),
                height: Some(480),
                taken_ns: Some(1_689_191_647_000_000_000),
            })
            .unwrap();
        // The production shape of an old row: no digest, so nothing is known
        // about the content and the caller must read the file.
        index.upsert_file(&file("legacy.jpg", 4, 8, None)).unwrap();
        index.finish_scan(&[]).unwrap();

        assert_eq!(
            index.reusable_image_meta("a.jpg", 10, 20).unwrap(),
            Some((
                "b".repeat(64),
                ImageMeta {
                    width: Some(640),
                    height: Some(480),
                    taken_ns: Some(1_689_191_647_000_000_000),
                },
            ))
        );
        assert!(
            index
                .reusable_image_meta("a.jpg", 11, 20)
                .unwrap()
                .is_none(),
            "a changed size is not the same file"
        );
        assert!(
            index
                .reusable_image_meta("a.jpg", 10, 21)
                .unwrap()
                .is_none(),
            "a changed mtime is not the same file"
        );
        assert!(
            index
                .reusable_image_meta("absent.jpg", 10, 20)
                .unwrap()
                .is_none(),
            "a path the index never saw is a fresh read"
        );
        assert!(
            index
                .reusable_image_meta("legacy.jpg", 4, 8)
                .unwrap()
                .is_none(),
            "a row without a valid digest has nothing reusable to say about its content"
        );
    }

    /// W48: the content-hash probe cache. A digest seen for the first time is
    /// parsed and recorded; the recording is visible inside the same scan, so
    /// a duplicate content shares it at once; and a recorded unreadable
    /// header is a hit stating "unknown", which is what stops a broken file
    /// from being re-read on every refresh.
    #[test]
    fn the_probe_cache_is_keyed_by_content_digest_and_reused_within_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        let digest = "c".repeat(64);
        assert!(index.cached_image_meta(&digest).unwrap().is_none());
        index
            .cache_image_meta(
                &digest,
                &ImageMeta {
                    width: Some(12),
                    height: Some(1),
                    taken_ns: None,
                },
            )
            .unwrap();
        assert_eq!(
            index.cached_image_meta(&digest).unwrap(),
            Some(ImageMeta {
                width: Some(12),
                height: Some(1),
                taken_ns: None,
            }),
            "read back inside the same scan"
        );
        index
            .cache_image_meta(
                &digest,
                &ImageMeta {
                    width: None,
                    height: None,
                    taken_ns: None,
                },
            )
            .unwrap();
        assert_eq!(
            index.cached_image_meta(&digest).unwrap(),
            Some(ImageMeta {
                width: None,
                height: None,
                taken_ns: None,
            }),
            "the last writer for a digest wins"
        );
        index.finish_scan(&[]).unwrap();
        assert!(matches!(
            index.cache_image_meta(
                "d".repeat(64).as_str(),
                &ImageMeta {
                    width: None,
                    height: None,
                    taken_ns: None,
                }
            ),
            Err(IndexError::ScanNotActive),
        ));
    }

    #[test]
    fn failed_or_aborted_scan_rolls_back_and_cannot_prune() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index.upsert_file(&file("keep.jpg", 3, 4, None)).unwrap();
        index.finish_scan(&[]).unwrap();
        index.begin_scan().unwrap();
        index.upsert_file(&file("keep.jpg", 99, 99, None)).unwrap();
        let duplicate = LinkRecord {
            src: "keep.jpg.md".into(),
            raw: "same".into(),
            target: None,
            state: LinkState::Missing,
            syntax: LinkSyntax::WikiEmbed,
        };
        let conflict = LinkRecord {
            state: LinkState::Resolved,
            ..duplicate.clone()
        };
        assert!(index
            .replace_links("keep.jpg.md", &[duplicate, conflict])
            .is_err());
        assert!(matches!(
            index.upsert_file(&file("escaped.jpg", 1, 1, None)),
            Err(IndexError::ScanAborted)
        ));
        assert!(index.finish_scan(&[]).is_err());
        let retained_size: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT size FROM files WHERE path='keep.jpg'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained_size, 3);
        index.begin_scan().unwrap();
        index.upsert_file(&file("keep.jpg", 7, 8, None)).unwrap();
        index.abort_scan();
        assert!(index.finish_scan(&[]).is_err());
        let retained_size: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT size FROM files WHERE path='keep.jpg'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained_size, 3);
        index.begin_scan().unwrap();
        index.upsert_file(&file("keep.jpg", 17, 18, None)).unwrap();
        drop(index);
        let index = Index::open(dir.path()).unwrap();
        let retained_size: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT size FROM files WHERE path='keep.jpg'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained_size, 3);
    }

    #[test]
    fn repeated_identical_links_are_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        let link = LinkRecord {
            src: "album.md".into(),
            raw: "a.jpg".into(),
            target: Some("a.jpg".into()),
            state: LinkState::Resolved,
            syntax: LinkSyntax::WikiEmbed,
        };
        index
            .replace_links("album.md", &[link.clone(), link])
            .unwrap();
        index.finish_scan(&["album.md".into()]).unwrap();
        let count: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT count(*) FROM links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn retained_paths_are_not_move_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("same.jpg", 2, 3, Some(&"c".repeat(64))))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("copy.jpg", 2, 3, Some(&"c".repeat(64))))
            .unwrap();
        index.finish_scan(&["same.jpg".into()]).unwrap();
        assert!(index.moved_candidates().unwrap().is_empty());
    }

    #[test]
    fn image_hash_candidates_never_pair_image_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("old.jpg", 2, 3, Some(&"d".repeat(64))))
            .unwrap();
        let mut old = note("old.jpg.md", Some("note-id"), "old", &[]);
        old.image_path = Some("old.jpg".into());
        index.upsert_note(&old).unwrap();
        index.finish_scan(&[]).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("new.jpg", 2, 3, Some(&"d".repeat(64))))
            .unwrap();
        let mut new = note("new.jpg.md", Some("note-id"), "new", &[]);
        new.image_path = Some("new.jpg".into());
        index.upsert_note(&new).unwrap();
        index.finish_scan(&[]).unwrap();
        let pairs = index.moved_candidates().unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs
            .iter()
            .all(|pair| pair.old_path.ends_with(".jpg.md") == (pair.basis == MoveBasis::Id)));
    }

    #[test]
    fn move_candidates_preserve_pipe_paths_as_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("a|b", 1, 1, Some(&"e".repeat(64))))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("c", 1, 1, Some(&"e".repeat(64))))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        let pair = index.moved_candidates().unwrap().pop().unwrap();
        assert_eq!(
            (pair.old_path.as_str(), pair.new_path.as_str()),
            ("a|b", "c")
        );
        assert_eq!(pair.basis, MoveBasis::Sha256);
    }

    #[test]
    fn short_search_treats_wildcards_literally() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_note(&note("percent.md", None, "100% literal", &[]))
            .unwrap();
        index
            .upsert_note(&note("under.md", None, "a_b", &[]))
            .unwrap();
        index
            .upsert_note(&note("slash.md", None, "a\\b", &[]))
            .unwrap();
        index
            .upsert_note(&note("ordinary.md", None, "abc", &[]))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        assert_eq!(index.search_text("%").unwrap(), vec!["percent.md"]);
        assert_eq!(index.search_text("_").unwrap(), vec!["under.md"]);
        assert_eq!(index.search_text("\\").unwrap(), vec!["slash.md"]);
    }

    #[test]
    fn incomplete_current_schema_requires_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE schema_version(version INTEGER NOT NULL)", [])
            .unwrap();
        conn.execute("CREATE TABLE files(path TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute("INSERT INTO schema_version(version) VALUES(2)", [])
            .unwrap();
        drop(conn);
        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
        assert!(matches!(
            index.needs_hash("x", 1, 1),
            Err(IndexError::RebuildRequired(_))
        ));
    }

    #[test]
    fn missing_fts_sync_trigger_requires_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        drop(index);
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        let conn = Connection::open(db).unwrap();
        conn.execute("DROP TRIGGER notes_ai", []).unwrap();
        drop(conn);
        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
    }

    #[test]
    fn missing_note_path_unique_constraint_requires_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path()).unwrap();
        drop(index);
        let db = dir.path().join(".dimagine/cache/index.sqlite");
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "PRAGMA writable_schema=ON;
             UPDATE sqlite_master SET sql=replace(sql,'path TEXT NOT NULL UNIQUE','path TEXT NOT NULL') WHERE type='table' AND name='notes';
             PRAGMA schema_version=222;",
        )
        .unwrap();
        drop(conn);
        let index = Index::open(dir.path()).unwrap();
        assert!(index.rebuild_required());
    }

    #[test]
    fn unicode_search_normalizes_queries_and_text_but_keeps_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        let path = "notes/cafe\u{301}.md";
        index
            .upsert_note(&note(path, None, "École Café", &[]))
            .unwrap();
        index.set_note_body(path, "Cafe\u{301} noir").unwrap();
        index
            .upsert_note(&note("greek.md", None, "ΟΣΟΝ", &[]))
            .unwrap();
        index.finish_scan(&[]).unwrap();
        assert_eq!(index.search_text("é").unwrap(), vec![path]);
        assert_eq!(index.search_text("café").unwrap(), vec![path]);
        assert_eq!(index.search_text("CAFÉ").unwrap(), vec![path]);
        assert_eq!(index.search_text("ΟΣ").unwrap(), vec!["greek.md"]);
    }

    #[test]
    fn writes_require_an_active_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        assert!(matches!(
            index.upsert_file(&file("outside.jpg", 1, 1, None)),
            Err(IndexError::ScanNotActive)
        ));
        assert!(matches!(
            index.upsert_note(&note("outside.md", None, "outside", &[])),
            Err(IndexError::ScanNotActive)
        ));
        assert!(matches!(
            index.set_note_body("outside.md", "body"),
            Err(IndexError::ScanNotActive)
        ));
        assert!(matches!(
            index.replace_links("outside.md", &[]),
            Err(IndexError::ScanNotActive)
        ));
        let count: i64 = index
            .conn()
            .unwrap()
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanNotActive)
        ));
    }

    /// Populates files, a note, and links through a real scan so later tests have rows
    /// that a mis-guarded `finish_scan` would delete.
    fn seeded_index(dir: &tempfile::TempDir) -> Index {
        let mut index = Index::open(dir.path()).unwrap();
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("keep.jpg", 3, 4, Some(&"a".repeat(64))))
            .unwrap();
        index
            .upsert_file(&file("second.jpg", 5, 6, Some(&"b".repeat(64))))
            .unwrap();
        index
            .upsert_note(&note("keep.jpg.md", Some("stable-id"), "keep", &[]))
            .unwrap();
        index.set_note_body("keep.jpg.md", "body text").unwrap();
        index
            .replace_links(
                "keep.jpg.md",
                &[LinkRecord {
                    src: "keep.jpg.md".into(),
                    raw: "[[second.jpg]]".into(),
                    target: Some("second.jpg".into()),
                    state: LinkState::Resolved,
                    syntax: LinkSyntax::WikiEmbed,
                }],
            )
            .unwrap();
        index
            .finish_scan(&["keep.jpg".into(), "second.jpg".into(), "keep.jpg.md".into()])
            .unwrap();
        index
    }

    fn row_counts(index: &Index) -> (i64, i64, i64, i64) {
        let conn = index.conn().unwrap();
        let count = |sql: &str| conn.query_row(sql, [], |row| row.get(0)).unwrap();
        (
            count("SELECT count(*) FROM files"),
            count("SELECT count(*) FROM notes"),
            count("SELECT count(*) FROM links"),
            count("SELECT count(*) FROM moved_from"),
        )
    }

    fn moved_from_paths(index: &Index) -> Vec<String> {
        let conn = index.conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT path FROM moved_from ORDER BY path")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<Vec<String>, _>>().unwrap()
    }

    #[test]
    fn finish_scan_without_an_active_scan_never_prunes_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = seeded_index(&dir);
        assert_eq!(row_counts(&index), (2, 1, 1, 0));

        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanNotActive)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
        assert!(moved_from_paths(&index).is_empty());
        assert!(index.moved_candidates().unwrap().is_empty());

        assert!(matches!(
            index.finish_scan(&["keep.jpg".into()]),
            Err(IndexError::ScanNotActive)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
        assert!(moved_from_paths(&index).is_empty());
        assert!(!index.needs_hash("keep.jpg", 3, 4).unwrap());
        assert_eq!(index.by_sha256(&"a".repeat(64)).unwrap(), ["keep.jpg"]);
    }

    #[test]
    fn finish_scan_on_a_fresh_index_cannot_empty_a_later_scan() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut index = Index::open(dir.path()).unwrap();
            assert!(matches!(
                index.finish_scan(&["never-scanned.jpg".into()]),
                Err(IndexError::ScanNotActive)
            ));
        }

        let mut index = seeded_index(&dir);
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanNotActive)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
    }

    #[test]
    fn finish_scan_twice_needs_a_new_scan_and_keeps_the_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = seeded_index(&dir);
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanNotActive)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));

        index.begin_scan().unwrap();
        index.finish_scan(&["keep.jpg".into()]).unwrap();
        assert_eq!(row_counts(&index), (1, 0, 0, 2));
        assert_eq!(moved_from_paths(&index), ["keep.jpg.md", "second.jpg"]);
        assert!(index.moved_candidates().unwrap().is_empty());
    }

    #[test]
    fn abort_scan_without_an_active_scan_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = seeded_index(&dir);

        index.abort_scan();
        index.abort_scan();
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
        assert!(matches!(
            index.upsert_file(&file("outside.jpg", 1, 1, None)),
            Err(IndexError::ScanNotActive)
        ));
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanNotActive)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));

        index.begin_scan().unwrap();
        index.abort_scan();
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanAborted)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
        assert!(matches!(
            index.finish_scan(&[]),
            Err(IndexError::ScanAborted)
        ));
        assert_eq!(row_counts(&index), (2, 1, 1, 0));

        index.begin_scan().unwrap();
        index.finish_scan(&["keep.jpg".into()]).unwrap();
        assert_eq!(row_counts(&index), (1, 0, 0, 2));
    }

    #[test]
    fn dropping_an_index_mid_scan_keeps_every_committed_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = seeded_index(&dir);
        index.begin_scan().unwrap();
        index
            .upsert_file(&file("keep.jpg", 99, 100, Some(&"a".repeat(64))))
            .unwrap();
        drop(index);

        let index = Index::open(dir.path()).unwrap();
        assert_eq!(row_counts(&index), (2, 1, 1, 0));
        assert!(!index.needs_hash("keep.jpg", 3, 4).unwrap());
    }
}
