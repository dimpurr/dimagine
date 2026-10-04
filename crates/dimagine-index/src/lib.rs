//! Rebuildable SQLite index for a dimagine library.
//!
//! Library files remain the source of truth. This crate stores derived metadata
//! under `.dimagine/cache/index.sqlite` and never modifies library content.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SCHEMA_VERSION: i64 = 1;
static OPEN_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkState {
    Resolved,
    Missing,
    Ambiguous,
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
}

/// Errors returned by index operations.
#[derive(Debug)]
pub enum IndexError {
    Busy,
    RebuildRequired(String),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "index is already open for this library"),
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
            }),
            Err(IndexError::RebuildRequired(_)) => Ok(Self {
                conn: None,
                db_path,
                root_key: root,
                rebuild_required: true,
                scan_active: false,
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
        self.conn()?
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM scan_seen;")?;
        self.scan_active = true;
        Ok(())
    }

    /// Inserts or updates a file row and marks its path as seen in this scan.
    pub fn upsert_file(&mut self, record: &FileRecord) -> Result<(), IndexError> {
        self.conn()?.execute(
            "INSERT INTO files(path,size,mtime_ns,sha256,kind) VALUES(?1,?2,?3,?4,?5) \
             ON CONFLICT(path) DO UPDATE SET size=excluded.size,mtime_ns=excluded.mtime_ns, \
             sha256=COALESCE(excluded.sha256,files.sha256),kind=excluded.kind",
            params![
                record.path,
                record.size,
                record.mtime_ns,
                record.sha256,
                record.kind.as_str()
            ],
        )?;
        self.conn()?.execute(
            "INSERT OR IGNORE INTO scan_seen(path) VALUES(?1)",
            [&record.path],
        )?;
        Ok(())
    }

    /// Inserts or updates parsed note metadata and its searchable title/tags.
    pub fn upsert_note(&mut self, record: &NoteRecord) -> Result<(), IndexError> {
        let tags = record.tags.join(" ");
        self.conn()?.execute(
            "INSERT INTO notes(path,image_path,id,title,tags,props_json,body) VALUES(?1,?2,?3,?4,?5,?6,'') \
             ON CONFLICT(path) DO UPDATE SET image_path=excluded.image_path,id=excluded.id, \
             title=excluded.title,tags=excluded.tags,props_json=excluded.props_json",
            params![record.path, record.image_path, record.id, record.title, tags, record.props_json],
        )?;
        self.refresh_fts(&record.path)?;
        Ok(())
    }

    /// Stores note body text for full-text search.
    pub fn set_note_body(&mut self, path: &str, body: &str) -> Result<(), IndexError> {
        self.conn()?.execute(
            "UPDATE notes SET body=?2 WHERE path=?1",
            params![path, body],
        )?;
        self.refresh_fts(path)
    }

    fn refresh_fts(&self, path: &str) -> Result<(), IndexError> {
        self.conn()?
            .execute("DELETE FROM notes_fts WHERE path=?1", [path])?;
        self.conn()?.execute(
            "INSERT INTO notes_fts(path,title,tags,body) SELECT path,title,tags,body FROM notes WHERE path=?1",
            [path],
        )?;
        Ok(())
    }

    /// Replaces all outgoing links for one source path.
    pub fn replace_links(&mut self, src: &str, links: &[LinkRecord]) -> Result<(), IndexError> {
        self.conn()?
            .execute("DELETE FROM links WHERE src=?1", [src])?;
        for link in links {
            self.conn()?.execute(
                "INSERT INTO links(src,raw,target,state) VALUES(?1,?2,?3,?4)",
                params![src, link.raw, link.target, link.state.as_str()],
            )?;
        }
        Ok(())
    }

    /// Commits the scan and deletes file/note/link rows whose paths vanished.
    pub fn finish_scan(&mut self, seen_paths: &[String]) -> Result<(), IndexError> {
        let conn = self.conn()?;
        if !self.scan_active {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
        }
        conn.execute("DELETE FROM moved_from", [])?;
        conn.execute(
            "INSERT INTO moved_from(path,id,sha256) \
             SELECT n.path,n.id,f.sha256 FROM notes n LEFT JOIN files f ON f.path=n.image_path \
             WHERE n.path NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO moved_from(path,id,sha256) \
             SELECT f.path,NULL,f.sha256 FROM files f \
             WHERE f.path NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        for path in seen_paths {
            conn.execute("INSERT OR IGNORE INTO scan_seen(path) VALUES(?1)", [path])?;
        }
        conn.execute(
            "DELETE FROM files WHERE path NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        conn.execute(
            "DELETE FROM notes_fts WHERE path NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        conn.execute(
            "DELETE FROM notes WHERE path NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        conn.execute(
            "DELETE FROM links WHERE src NOT IN (SELECT path FROM scan_seen)",
            [],
        )?;
        conn.execute("DELETE FROM scan_seen", [])?;
        conn.execute_batch("COMMIT;")?;
        self.scan_active = false;
        Ok(())
    }

    /// Returns whether content hashing is needed; a cached hash is reusable only
    /// when both size and nanosecond mtime are unchanged.
    pub fn needs_hash(&self, path: &str, size: u64, mtime_ns: i64) -> Result<bool, IndexError> {
        let previous: Option<(i64, i64)> = self
            .conn()?
            .query_row(
                "SELECT size,mtime_ns FROM files WHERE path=?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(previous != Some((size as i64, mtime_ns)))
    }

    /// Finds all paths with the supplied SHA-256 digest.
    pub fn by_sha256(&self, sha256: &str) -> Result<Vec<String>, IndexError> {
        let mut stmt = self
            .conn()?
            .prepare("SELECT path FROM files WHERE sha256=?1 ORDER BY path")?;
        let rows = stmt.query_map([sha256], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns pairs of note IDs or hashes that occur at more than one path.
    pub fn moved_candidates(&self) -> Result<Vec<(String, String, String)>, IndexError> {
        let mut stmt = self.conn()?.prepare(
            "SELECT 'id',m.id,m.path || '|' || n.path FROM moved_from m JOIN notes n ON n.id=m.id \
             WHERE m.id IS NOT NULL AND m.path<>n.path \
             UNION ALL SELECT 'sha256',m.sha256,m.path || '|' || f.path FROM moved_from m JOIN files f ON f.sha256=m.sha256 \
             WHERE m.sha256 IS NOT NULL AND m.path<>f.path ORDER BY 1,2",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Searches note title, tags, and body. Terms shorter than three Unicode
    /// characters use substring matching because the trigram tokenizer needs 3.
    pub fn search_text(&self, query: &str) -> Result<Vec<String>, IndexError> {
        if query.chars().count() < 3 {
            let pattern = format!("%{query}%");
            let mut stmt = self.conn()?.prepare(
                "SELECT path FROM notes WHERE title LIKE ?1 OR tags LIKE ?1 OR body LIKE ?1 ORDER BY path",
            )?;
            let rows = stmt.query_map([pattern], |row| row.get(0))?;
            return Ok(rows.collect::<Result<Vec<_>, _>>()?);
        }
        let mut stmt = self
            .conn()?
            .prepare("SELECT path FROM notes_fts WHERE notes_fts MATCH ?1 ORDER BY path")?;
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
        if version > SCHEMA_VERSION {
            return Err(IndexError::RebuildRequired(format!(
                "schema version {version} is newer than supported version {SCHEMA_VERSION}"
            )));
        }
    }
    if version.unwrap_or(0) < 1 {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,size INTEGER NOT NULL,mtime_ns INTEGER NOT NULL,sha256 TEXT,kind TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS notes(path TEXT PRIMARY KEY,image_path TEXT,id TEXT,title TEXT NOT NULL,tags TEXT NOT NULL,props_json TEXT NOT NULL,body TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS links(src TEXT NOT NULL,raw TEXT NOT NULL,target TEXT,state TEXT NOT NULL,PRIMARY KEY(src,raw));
             CREATE TABLE IF NOT EXISTS scan_seen(path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS moved_from(path TEXT PRIMARY KEY,id TEXT,sha256 TEXT);
             CREATE INDEX IF NOT EXISTS files_sha256_idx ON files(sha256);
             CREATE INDEX IF NOT EXISTS notes_id_idx ON notes(id);
             CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(path UNINDEXED,title,tags,body,tokenize='trigram');
             DELETE FROM schema_version;
             INSERT INTO schema_version(version) VALUES(1);
             COMMIT;",
        )?;
    }
    Ok(())
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
            .upsert_file(&file("old.jpg", 10, 20, Some("abc")))
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
            .upsert_file(&file("new.jpg", 10, 20, Some("abc")))
            .unwrap();
        index
            .upsert_note(&note("new.jpg.md", Some("stable-id"), "海の絵", &["海"]))
            .unwrap();
        index
            .finish_scan(&["new.jpg".into(), "new.jpg.md".into()])
            .unwrap();
        assert_eq!(index.by_sha256("abc").unwrap(), vec!["new.jpg"]);
        assert!(index.by_sha256("none").unwrap().is_empty());
        assert_eq!(index.moved_candidates().unwrap().len(), 2);
        assert!(index.needs_hash("gone.jpg", 5, 7).unwrap());
    }

    #[test]
    fn full_text_cjk_and_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::open(dir.path()).unwrap();
        index
            .upsert_note(&note("cn.md", None, "深海少女", &["水彩"]))
            .unwrap();
        index.set_note_body("cn.md", "海の中で眠る少女").unwrap();
        index
            .upsert_note(&note("jp.md", None, "星空の旅", &["夜空"]))
            .unwrap();
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
}
