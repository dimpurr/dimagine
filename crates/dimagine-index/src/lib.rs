//! Rebuildable SQLite index for a dimagine library.
//!
//! Library files remain the source of truth. This crate stores derived metadata
//! under `.dimagine/cache/index.sqlite` and never modifies library content.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

const SCHEMA_VERSION: i64 = 2;
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
        self.conn()?
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM scan_seen;")?;
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
    pub fn upsert_file(&mut self, record: &FileRecord) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            self.conn()?.execute(
                "INSERT INTO files(path,size,mtime_ns,sha256,kind) VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(path) DO UPDATE SET \
                 sha256=CASE WHEN files.size=excluded.size AND files.mtime_ns=excluded.mtime_ns \
                   THEN COALESCE(excluded.sha256,files.sha256) ELSE excluded.sha256 END, \
                 size=excluded.size,mtime_ns=excluded.mtime_ns,kind=excluded.kind",
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
        })();
        self.fail_scan_on_error(result)
    }

    /// Inserts or updates parsed note metadata and marks the note path seen.
    pub fn upsert_note(&mut self, record: &NoteRecord) -> Result<(), IndexError> {
        self.ensure_scan_active()?;
        let result = (|| {
            let title = searchable(&record.title);
            let tags = searchable(&record.tags.join(" "));
            self.conn()?.execute(
                "INSERT INTO notes(path,image_path,id,title,tags,props_json,body) VALUES(?1,?2,?3,?4,?5,?6,'') \
             ON CONFLICT(path) DO UPDATE SET image_path=excluded.image_path,id=excluded.id, \
             title=excluded.title,tags=excluded.tags,props_json=excluded.props_json",
                params![record.path, record.image_path, record.id, title, tags, record.props_json],
            )?;
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
                if unique.insert((link.raw.as_str(), link.target.as_deref(), link.state)) {
                    self.conn()?.execute(
                        "INSERT INTO links(src,raw,target,state) VALUES(?1,?2,?3,?4)",
                        params![src, link.raw, link.target, link.state.as_str()],
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
            conn.execute(
                "DELETE FROM files WHERE path NOT IN (SELECT path FROM scan_seen)",
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

    /// Searches note title, tags, and body. Terms shorter than three Unicode
    /// characters use substring matching because the trigram tokenizer needs 3.
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
        if version != SCHEMA_VERSION {
            return Err(IndexError::RebuildRequired(format!(
                "schema version {version} does not match supported version {SCHEMA_VERSION}"
            )));
        }
    }
    if version.is_none() {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,size INTEGER NOT NULL,mtime_ns INTEGER NOT NULL,sha256 TEXT,kind TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS notes(note_id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,image_path TEXT,id TEXT,title TEXT NOT NULL,tags TEXT NOT NULL,props_json TEXT NOT NULL,body TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS links(src TEXT NOT NULL,raw TEXT NOT NULL,target TEXT,state TEXT NOT NULL,PRIMARY KEY(src,raw));
             CREATE TABLE IF NOT EXISTS scan_seen(path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS moved_from(path TEXT PRIMARY KEY,id TEXT,sha256 TEXT,kind TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS files_sha256_idx ON files(sha256);
             CREATE INDEX IF NOT EXISTS notes_id_idx ON notes(id);
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
             INSERT INTO schema_version(version) VALUES(2);
             COMMIT;",
        )?;
    }
    validate_schema(conn)?;
    Ok(())
}

fn validate_schema(conn: &Connection) -> Result<(), IndexError> {
    let required = [
        "files",
        "notes",
        "links",
        "scan_seen",
        "moved_from",
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
        ("files", &["path", "size", "mtime_ns", "sha256", "kind"][..]),
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
        ("links", &["src", "raw", "target", "state"][..]),
        ("scan_seen", &["path"][..]),
        ("moved_from", &["path", "id", "sha256", "kind"][..]),
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
        ("scan_seen", vec!["path"]),
        ("moved_from", vec!["path"]),
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

fn searchable(text: &str) -> String {
    text.case_fold().nfc().collect()
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
