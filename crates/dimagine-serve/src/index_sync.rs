//! Index synchronization, startup verification, and background rescan.
//!
//! Spec §5:
//! - On startup: open `.dimagine/` index; if missing or `RebuildRequired` →
//!   run the same sync as `dimagine scan` before accepting traffic (logging
//!   progress); otherwise the periodic sync keeps it current from then on.
//! - Refresh: background incremental sync every `--rescan-interval` (default
//!   300 s; `0` disables).
//! - All list and count queries go through `dimagine_index`.
//! - Counts and sidebar data are cached per index generation and invalidated
//!   after each sync.

use dimagine_core::library::Library;
use dimagine_core::sync_index;
use dimagine_index::{Index, IndexError, ViewPage, ViewQuery};
use serde::Serialize;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// How far back the sidebar's "Recent" view reaches.
pub const RECENT_WINDOW_DAYS: u64 = 30;

const SECONDS_PER_DAY: u64 = 60 * 60 * 24;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// A folder row for `/api/sidebar`, with its recursive image count.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SidebarFolder {
    /// Library-relative folder path.
    pub path: String,
    /// Images in this folder and its subfolders.
    pub count: u64,
}

/// A collection row for `/api/sidebar`.
///
/// One home for the shape agents read: every item carries `path`, `title`
/// and `count`, and `count` is always a number, never `null` — an uncountable
/// member count would be an unknown recorded as absent.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SidebarCollection {
    /// Library-relative path of the collection note.
    pub path: String,
    pub title: String,
    /// Number of image members.
    pub count: u64,
}

/// A tag row for `/api/sidebar`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SidebarTag {
    pub tag: String,
    pub count: u64,
}

/// Everything the sidebar shows, in one read (spec §2 `/api/sidebar`).
///
/// `untagged` and `recent` go beyond the shape the spec sketches, because the
/// sidebar's `VIEWS` section counts both lenses (spec §3) and an agent asking
/// for the sidebar has the same use for them.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SidebarData {
    pub folders: Vec<SidebarFolder>,
    pub collections: Vec<SidebarCollection>,
    pub tags: Vec<SidebarTag>,
    /// Images in the whole library.
    pub total: u64,
    /// Images whose note carries no tag.
    pub untagged: u64,
    /// Images added inside the recent window.
    pub recent: u64,
}

/// What kept the index from being opened.
///
/// Text rather than an [`IndexError`] so the reason can be stored in the
/// handle and shown on a page instead of collapsing into a status code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexUnavailable(pub String);

impl fmt::Display for IndexUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for IndexUnavailable {}

/// The index behind a handle: open, or the reason it is not.
enum Slot {
    Open(Index),
    Closed(IndexUnavailable),
}

/// Thread-safe handle to the SQLite index, with generational caching and a
/// background rescan.
pub struct IndexHandle {
    root: PathBuf,
    slot: Mutex<Slot>,
    generation: AtomicU64,
    cached_sidebar: Mutex<Option<(u64, Arc<SidebarData>)>>,
    rescan_started: AtomicBool,
    /// Whether the index was already current at startup, which decides if a
    /// first background sync is worth running.
    needs_first_background_sync: bool,
}

impl IndexHandle {
    /// Open the index for `library_root` and make the library queryable.
    ///
    /// A missing or unusable index is synchronised here, before the router
    /// exists, so the first page request never waits for a scan. An index that
    /// cannot be opened at all is remembered rather than fatal: the viewer
    /// still serves sign-in, media and notes, and every list query reports the
    /// reason instead of pretending the library is empty.
    pub fn open(library_root: impl AsRef<Path>) -> Result<Self, IndexUnavailable> {
        let root = library_root.as_ref().to_path_buf();
        let db_path = root.join(".dimagine/cache/index.sqlite");
        let was_missing = !db_path.is_file();

        let mut handle = Self {
            root,
            slot: Mutex::new(Slot::Closed(IndexUnavailable(
                "the index has not been opened yet".to_owned(),
            ))),
            generation: AtomicU64::new(1),
            cached_sidebar: Mutex::new(None),
            rescan_started: AtomicBool::new(false),
            needs_first_background_sync: false,
        };

        let (index, was_rebuilt) = match open_usable_index(&handle.root, &db_path) {
            Ok(opened) => opened,
            Err(reason) => {
                eprintln!("dimagine: rebuilding the index: {reason}");
                let index = rebuild_index(&handle.root, &db_path).map_err(|reason| {
                    IndexUnavailable(format!("the index could not be rebuilt: {reason}"))
                })?;
                (index, true)
            }
        };
        *handle.slot.lock().expect("a fresh lock is not poisoned") = Slot::Open(index);

        if was_missing || was_rebuilt {
            // Spec §5: a missing or unusable index is synced before traffic is
            // accepted, not lazily on the first request.
            eprintln!(
                "dimagine: indexing {} (first run, please wait)",
                handle.root.display()
            );
            handle
                .sync_now()
                .map_err(|error| IndexUnavailable(format!("the first sync failed: {error}")))?;
        } else {
            // The index is current; the periodic sync keeps it that way.
            handle.needs_first_background_sync = true;
        }
        Ok(handle)
    }

    /// Run one sync against the library files and invalidate the counts.
    ///
    /// `sync_index` walks the folder but only rewrites the rows whose size,
    /// mtime or hash moved, so resyncing an unchanged library is cheap.
    pub fn refresh(&self) -> Result<(), IndexError> {
        self.sync_now()
    }

    fn sync_now(&self) -> Result<(), IndexError> {
        let library = Library::open(&self.root).map_err(|error| {
            IndexError::Io(std::io::Error::other(format!("open library: {error}")))
        })?;
        {
            let mut slot = self.lock()?;
            match &mut *slot {
                Slot::Open(index) => sync_index(&library, index)?,
                Slot::Closed(reason) => {
                    return Err(IndexError::Io(std::io::Error::other(format!(
                        "the index is not open: {reason}"
                    ))))
                }
            }
        }
        self.invalidate();
        Ok(())
    }

    /// One page of images for a view query.
    pub fn view(&self, query: &ViewQuery) -> Result<ViewPage, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.view(query),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// Collection note paths that embed `image_path`.
    pub fn appears_in(&self, image_path: &str) -> Result<Vec<String>, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.appears_in(image_path),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// Every folder, collection and tag with its count, cached per generation.
    pub fn sidebar_data(&self) -> Result<Arc<SidebarData>, IndexError> {
        let generation = self.generation();
        if let Some(cached) = self.cached_sidebar.lock().ok().and_then(|cache| {
            cache
                .as_ref()
                .filter(|(cached, _)| *cached == generation)
                .map(|(_, data)| Arc::clone(data))
        }) {
            return Ok(cached);
        }

        let data = Arc::new(self.read_sidebar_data()?);
        if let Ok(mut cache) = self.cached_sidebar.lock() {
            *cache = Some((generation, Arc::clone(&data)));
        }
        Ok(data)
    }

    fn read_sidebar_data(&self) -> Result<SidebarData, IndexError> {
        let slot = self.lock()?;
        let index = match &*slot {
            Slot::Open(index) => index,
            Slot::Closed(reason) => return Err(reason.clone().into()),
        };
        let folders = index.folder_counts()?;
        let collections = index.collections()?;
        let tags = index.tag_counts()?;
        let total = folders
            .iter()
            .find(|(folder, _)| folder.is_empty())
            .map(|(_, count)| *count)
            .unwrap_or(0);
        // One row is enough to read each lens's size: the count comes back in
        // `ViewPage::total`, so this stays one grouped query per lens.
        let untagged = count_matching(
            index,
            ViewQuery {
                untagged: true,
                ..ViewQuery::default()
            },
        )?;
        let recent = count_matching(
            index,
            ViewQuery {
                added_after_ns: Some(recent_cutoff_ns()),
                ..ViewQuery::default()
            },
        )?;

        Ok(SidebarData {
            folders: folders
                .into_iter()
                .filter(|(folder, _)| !folder.is_empty())
                .map(|(path, count)| SidebarFolder { path, count })
                .collect(),
            collections: collections
                .into_iter()
                // The collection list carries only the notes that mean to
                // collect: `kind: collection` always, plus a non-image note
                // with an image embed (`dimagine_index::collection_is_listed`,
                // FORMAT §3.2/§5). An image note that embeds its siblings is
                // still a collection; it is just not listed.
                .filter(|collection| collection.listed)
                .map(|collection| SidebarCollection {
                    path: collection.note_path,
                    title: collection.title,
                    count: collection.member_count,
                })
                .collect(),
            tags: tags
                .into_iter()
                .map(|(tag, count)| SidebarTag { tag, count })
                .collect(),
            total,
            untagged,
            recent,
        })
    }

    /// Start the periodic background sync when `interval_secs` is non-zero.
    ///
    /// Calling this more than once starts one task. With no Tokio runtime
    /// current — the router being built on a plain thread, as the tests do —
    /// nothing is spawned and the viewer serves the startup index alone.
    pub fn ensure_background_rescan(self: &Arc<Self>, interval_secs: u64) {
        if interval_secs == 0 {
            return;
        }
        if self
            .rescan_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let handle = Arc::clone(self);
        runtime.spawn(async move {
            if handle.needs_first_background_sync {
                handle.background_sync().await;
            }
            let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
            // A Tokio interval's first tick completes immediately, and that is
            // the startup sync's job, not the loop's.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                handle.background_sync().await;
            }
        });
    }

    async fn background_sync(self: &Arc<Self>) {
        let handle = Arc::clone(self);
        match tokio::task::spawn_blocking(move || handle.refresh()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("dimagine: background index sync failed: {error}"),
            Err(error) => eprintln!("dimagine: background index sync panicked: {error}"),
        }
    }

    /// Drop the cached counts so the next read recounts.
    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut cache) = self.cached_sidebar.lock() {
            *cache = None;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Slot>, IndexError> {
        self.slot
            .lock()
            .map_err(|_| IndexError::Io(std::io::Error::other("the index lock is poisoned")))
    }

    /// The library root this handle indexes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current index generation, bumped by every successful sync.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// A handle that never opens, for a library whose index could not be built.
    ///
    /// The viewer still runs; every list query reports `reason` instead of
    /// answering from a half-built database.
    pub fn unavailable(root: PathBuf, reason: IndexUnavailable) -> Self {
        Self {
            root,
            slot: Mutex::new(Slot::Closed(reason)),
            generation: AtomicU64::new(0),
            cached_sidebar: Mutex::new(None),
            rescan_started: AtomicBool::new(false),
            needs_first_background_sync: false,
        }
    }
}

impl From<IndexUnavailable> for IndexError {
    fn from(value: IndexUnavailable) -> Self {
        IndexError::Io(std::io::Error::other(value.0))
    }
}

/// Open the index, discarding a database that reports it must be rebuilt.
///
/// Returns the index and whether it had to be rebuilt, because an empty index
/// is not the same as a current one.
fn open_usable_index(root: &Path, db_path: &Path) -> Result<(Index, bool), IndexUnavailable> {
    match Index::open(root) {
        Ok(index) if index.rebuild_required() => {
            // `Index::open` answers an unusable database with a handle that
            // only knows it has to be rebuilt. Dropping it releases the
            // process-wide claim on the file before the rebuild reopens it.
            drop(index);
            rebuild_index(root, db_path).map(|index| (index, true))
        }
        Ok(index) => Ok((index, false)),
        Err(IndexError::RebuildRequired(_)) => {
            rebuild_index(root, db_path).map(|index| (index, true))
        }
        Err(error) => Err(IndexUnavailable(error.to_string())),
    }
}

/// Discard the database files and open a fresh one.
fn rebuild_index(root: &Path, db_path: &Path) -> Result<Index, IndexUnavailable> {
    let name = db_path.display().to_string();
    for stale in [name.clone(), format!("{name}-wal"), format!("{name}-shm")] {
        if let Err(error) = fs::remove_file(&stale) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(IndexUnavailable(format!("cannot remove {stale}: {error}")));
            }
        }
    }
    Index::open(root).map_err(|error| IndexUnavailable(error.to_string()))
}

/// The `added` cutoff of the recent window, in ns since the Unix epoch.
pub fn recent_cutoff_ns() -> i64 {
    recent_cutoff_ns_at(std::time::SystemTime::now())
}

/// The same cutoff, from an explicit instant so a test can place itself in
/// time.
pub fn recent_cutoff_ns_at(now: std::time::SystemTime) -> i64 {
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / SECONDS_PER_DAY).min(RECENT_WINDOW_DAYS);
    (seconds - days * SECONDS_PER_DAY) as i64 * NANOS_PER_SECOND
}

/// How many images a view query matches, without reading the page itself.
fn count_matching(index: &Index, query: ViewQuery) -> Result<u64, IndexError> {
    Ok(index.view(&query)?.total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    const JPEG: &[u8] = b"\xff\xd8\xffsynthetic image bytes";

    /// A library with nested folders, a prefix-trap sibling, a collection,
    /// tags, and a note with an explicit `added` beside notes without one
    /// (spec §6).
    fn test_library() -> TempDir {
        let dir = tempdir().unwrap();
        let refs = dir.path().join("refs");
        fs::create_dir_all(refs.join("ui")).unwrap();
        fs::create_dir_all(dir.path().join("refs2")).unwrap();
        fs::write(refs.join("img1.jpg"), JPEG).unwrap();
        fs::write(
            refs.join("img1.jpg.md"),
            "---\ntags:\n  - test\n  - demo\n---\nNote for img1\n",
        )
        .unwrap();
        fs::write(refs.join("ui/deep.jpg"), JPEG).unwrap();
        fs::write(dir.path().join("refs2/trap.jpg"), JPEG).unwrap();
        fs::write(
            dir.path().join("plain.png"),
            b"\x89PNG\r\n\x1a\nno note here",
        )
        .unwrap();
        fs::write(
            dir.path().join("collection.md"),
            "---\ntitle: Sample Collection\n---\n![[refs/img1.jpg]]\nCaption for img1\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn startup_indexes_a_library_that_has_no_index_yet() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();

        let sidebar = handle.sidebar_data().unwrap();
        assert_eq!(sidebar.total, 4);
        assert!(sidebar
            .folders
            .iter()
            .any(|f| f.path == "refs" && f.count == 2));
        assert!(sidebar
            .collections
            .iter()
            .any(|c| c.title == "Sample Collection" && c.count == 1));
        assert!(sidebar.tags.iter().any(|t| t.tag == "test" && t.count == 1));
        assert_eq!(sidebar.untagged, 3);
        assert_eq!(handle.view(&ViewQuery::default()).unwrap().total, 4);
    }

    #[test]
    fn folder_counts_do_not_leak_into_a_prefix_sibling() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        let view = handle
            .view(&ViewQuery {
                folder: Some("refs".into()),
                ..ViewQuery::default()
            })
            .unwrap();
        assert_eq!(view.total, 2, "`refs` must not reach `refs2`");
        assert!(view.items.iter().all(|item| item.path.starts_with("refs/")));
    }

    #[test]
    fn a_refresh_sees_new_files_and_invalidates_the_counts() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        let generation = handle.generation();
        let before = handle.sidebar_data().unwrap();
        let folders_before = before.folders.len();

        fs::write(dir.path().join("refs/added.jpg"), JPEG).unwrap();
        assert_eq!(
            handle.sidebar_data().unwrap().total,
            before.total,
            "the cached count must hold until a sync runs"
        );

        handle.refresh().unwrap();
        assert!(handle.generation() > generation);
        let after = handle.sidebar_data().unwrap();
        assert_eq!(after.total, before.total + 1);
        assert_eq!(
            after.folders.len(),
            folders_before,
            "a file in an existing folder adds no folder row"
        );
    }

    #[test]
    fn an_unusable_database_is_discarded_and_reindexed() {
        let dir = test_library();
        let cache = dir.path().join(".dimagine/cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("index.sqlite"), b"not a sqlite database").unwrap();

        let handle = IndexHandle::open(dir.path()).unwrap();
        assert_eq!(handle.sidebar_data().unwrap().total, 4);
    }

    #[test]
    fn appears_in_names_the_collections_that_embed_the_image() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        assert_eq!(
            handle.appears_in("refs/img1.jpg").unwrap(),
            vec!["collection.md".to_owned()]
        );
        assert!(handle.appears_in("plain.png").unwrap().is_empty());
    }

    /// W27f: an image note that embeds its siblings is a valid collection
    /// (FORMAT §5), but listing one per image buried the deliberate ones, so
    /// the list — and only the list — drops it
    /// (`dimagine_index::collection_is_listed`, FORMAT §3.2/§5). "Appears in"
    /// still names it.
    #[test]
    fn the_sidebar_list_drops_image_note_collections_but_appears_in_keeps_them() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("refs")).unwrap();
        fs::write(root.join("refs/page-01.png"), JPEG).unwrap();
        fs::write(root.join("refs/page-02.png"), JPEG).unwrap();
        fs::write(
            root.join("refs/page-01.png.md"),
            "---\ntitle: Page one\n---\n\nAn image note embedding a sibling and its preview.\n\n\
             ![[page-02.png]]\n![[page-01.png]]\n",
        )
        .unwrap();
        fs::write(
            root.join("guide.md"),
            "---\nkind: collection\ntitle: Guide\n---\nPlanned shots go here.\n",
        )
        .unwrap();
        fs::write(
            root.join("roundup.md"),
            "![[refs/page-01.png]]\nA plain note, so a listed collection.\n",
        )
        .unwrap();
        let handle = IndexHandle::open(root).unwrap();

        let sidebar = handle.sidebar_data().unwrap();
        let listed: Vec<(&str, u64)> = sidebar
            .collections
            .iter()
            .map(|collection| (collection.path.as_str(), collection.count))
            .collect();
        assert_eq!(
            listed,
            [("guide.md", 0), ("roundup.md", 1)],
            "the image note is a collection, but not one the list carries"
        );
        assert_eq!(
            handle.appears_in("refs/page-02.png").unwrap(),
            vec!["refs/page-01.png.md".to_owned()],
            "the image page still names the image note in appears in"
        );
    }

    #[test]
    fn the_recent_window_is_thirty_days_wide() {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let cutoff = recent_cutoff_ns() / NANOS_PER_SECOND;
        assert_eq!(
            seconds - cutoff,
            (RECENT_WINDOW_DAYS * SECONDS_PER_DAY) as i64
        );
    }

    #[tokio::test]
    async fn a_rescan_interval_of_zero_starts_no_task() {
        let dir = test_library();
        let handle = Arc::new(IndexHandle::open(dir.path()).unwrap());
        handle.ensure_background_rescan(0);
        assert!(
            !handle.rescan_started.load(Ordering::SeqCst),
            "0 must switch the background sync off"
        );
    }

    #[tokio::test]
    async fn the_background_sync_is_started_once_and_picks_up_new_files() {
        let dir = test_library();
        let handle = Arc::new(IndexHandle::open(dir.path()).unwrap());
        handle.ensure_background_rescan(60);
        handle.ensure_background_rescan(60);
        assert!(handle.rescan_started.load(Ordering::SeqCst));

        fs::write(dir.path().join("refs/appeared.jpg"), JPEG).unwrap();
        handle.background_sync().await;
        assert_eq!(handle.sidebar_data().unwrap().total, 5);
    }
}
