//! Index synchronization, startup verification, and background rescan.
//!
//! Spec §5:
//! - On startup: open `.dimagine/` index; if missing or `RebuildRequired` →
//!   run the same sync as `dimagine scan` before accepting traffic (logging
//!   progress); otherwise the periodic sync keeps it current from then on.
//! - Refresh: background incremental sync every `--rescan-interval` (default
//!   300 s; `0` disables).
//! - All list and count queries go through `dimagine_index`.
//! - Counts and sidebar data are cached per index generation — the number of
//!   scans committed to the index, which every committed scan raises in the
//!   same transaction as its content — so each entry retires itself on the
//!   first request after a rescan.
//! - A rescan writes on the index connection while every list request reads a
//!   read-only connection of its own (WAL), so live requests proceed
//!   throughout and never wait for a rescan.

use dimagine_core::library::Library;
use dimagine_core::sync_index;
use dimagine_index::{
    AppearsIn, CollectionInfo, Connection, FacetCounts, ImageMeta, Index, IndexError, Neighbours,
    Reader, RecentWindow, ViewPage, ViewQuery,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// How many digests the handle remembers for files the index has none for.
/// Two grid pages' worth of strangers — [`crate::view_query::PAGE_SIZE`] is
/// 120 — so a page of images the scan has not reached stops re-reading its
/// sources, while request traffic alone cannot grow the process without bound.
const DIGEST_MEMO_LIMIT: usize = 256;

/// What one remembered digest describes: the path as the request spelled it,
/// and the size and nanosecond mtime the digest was read from.
///
/// The path is the URL's own spelling rather than a normalised one because two
/// unicode forms of one name can be two distinct files on a
/// normalisation-sensitive filesystem; keeping them apart is the same reason
/// the media route asks the index for both forms.
#[derive(PartialEq, Eq, Hash)]
struct DigestKey {
    path: String,
    size: u64,
    mtime_ns: i64,
}

impl DigestKey {
    fn new(path: &str, size: u64, mtime_ns: i64) -> Self {
        Self {
            path: path.to_owned(),
            size,
            mtime_ns,
        }
    }
}

/// How many images the "Recent" lens reaches: the `RECENT_LIMIT` most
/// recently added images, however old they are (W34 audit #10: a count, not
/// a window — a 30-day window covered 95% of a library imported inside it).
pub const RECENT_LIMIT: u64 = 200;

/// What the Recent lens calls itself, wherever it is named. The label states
/// the definition, so it cannot drift from the number the lens keeps.
pub fn recent_label() -> String {
    format!("Recent — last {RECENT_LIMIT} added")
}

/// The Recent lens as the index asks for it: the window the grid is cut by, and
/// whether the lens is on. [`RECENT_LIMIT`] is the viewer's number — the index
/// keeps no number of its own — so the grid, the neighbour walk and the filter
/// panel's counts are all cut by the same one.
pub fn recent_window(active: bool) -> RecentWindow {
    if active {
        RecentWindow::On {
            limit: RECENT_LIMIT,
        }
    } else {
        RecentWindow::Off {
            limit: RECENT_LIMIT,
        }
    }
}

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
    /// Images the Recent lens reaches: the most recently added
    /// [`RECENT_LIMIT`], the whole library when it is smaller than that.
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
    /// A read-only connection to the same database, opened beside the `Index`
    /// in `slot`. Every list query answers from it while the rescan writes on
    /// the `Index` connection, so a request never waits for a rescan. `None`
    /// means it could not be opened and reads fall back to the `Index`
    /// connection through `slot` (the pre-W31 path).
    reader: Mutex<Option<Reader>>,
    cached_sidebar: Mutex<Option<(u64, Arc<SidebarData>)>>,
    /// View totals per query (without paging) and per committed index
    /// generation, as `(generation, total)`. A repeated query — every grid
    /// scroll, every sidebar lens — pays only for its page; each query counts
    /// once per committed scan.
    cached_totals: Mutex<HashMap<ViewQuery, (u64, u64)>>,
    /// Test hook: milliseconds to hold the write critical section, so a test
    /// can watch a request answer from the reader while the sync still holds
    /// `slot`. Zero in production.
    sync_delay: AtomicU64,
    rescan_started: AtomicBool,
    /// Whether the index was already current at startup, which decides if a
    /// first background sync is worth running.
    needs_first_background_sync: bool,
    /// The digests this handle read for files the index has none for, so the
    /// next request for the same file asks nothing of it. See
    /// [`Self::remembered_digest`] and [`Self::remember_digest`].
    digest_memo: Mutex<HashMap<DigestKey, String>>,
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
            reader: Mutex::new(None),
            cached_sidebar: Mutex::new(None),
            cached_totals: Mutex::new(HashMap::new()),
            sync_delay: AtomicU64::new(0),
            rescan_started: AtomicBool::new(false),
            needs_first_background_sync: false,
            digest_memo: Mutex::new(HashMap::new()),
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
        // A read-only connection to the same database, opened while the `Index`
        // writer is live (it must be, so the WAL side-files already exist).
        // Every list query answers from it; if it cannot open, reads fall back
        // to the writer connection. The best-effort open keeps a reader
        // hiccup from taking the viewer down.
        let reader = Reader::open(index.database_path())
            .map_err(|error| {
                eprintln!("dimagine: index reader unavailable, reads will serialise: {error}")
            })
            .ok();
        if let Some(reader) = reader {
            *handle.reader.lock().expect("a fresh lock is not poisoned") = Some(reader);
        }
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

    /// Run one sync against the library files.
    ///
    /// `sync_index` walks the folder but only rewrites the rows whose size,
    /// mtime or hash moved, so resyncing an unchanged library is cheap. It
    /// commits on the index connection while the reader answers every request
    /// from a snapshot of its own, so no request waits for this; the committed
    /// scan generation the caches validate against moves in the same
    /// transaction as the content, which is what retires the cached totals and
    /// sidebar on the next request after any change.
    pub fn refresh(&self) -> Result<(), IndexError> {
        self.sync_now()
    }

    fn sync_now(&self) -> Result<(), IndexError> {
        let library = Library::open(&self.root).map_err(|error| {
            IndexError::Io(std::io::Error::other(format!("open library: {error}")))
        })?;
        {
            let mut slot = self.lock()?;
            // Test hook: hold the write critical section the way a slow rescan
            // does, so a test can watch a request answer from the reader while
            // this writer still holds `slot` (W31). Zero in production.
            let delay = self.sync_delay.load(Ordering::SeqCst);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            match &mut *slot {
                Slot::Open(index) => sync_index(&library, index)?,
                Slot::Closed(reason) => {
                    return Err(IndexError::Io(std::io::Error::other(format!(
                        "the index is not open: {reason}"
                    ))))
                }
            }
        }
        self.after_sync();
        Ok(())
    }

    /// One page of images for a view query.
    ///
    /// The page and its committed generation are read together on the reader
    /// connection, in one snapshot. The total is cached per query (without
    /// paging) under that generation and trusted only while the generation
    /// still reads back the same, so a rescan cannot slip between a total and
    /// the list it describes: it commits the generation bump and its content
    /// in one transaction, so the next request fails the check and recounts
    /// from one snapshot. A repeated query then pays only for its page.
    pub fn view(&self, query: &ViewQuery) -> Result<ViewPage, IndexError> {
        if let Ok(reader_slot) = self.reader.lock() {
            if let Some(reader) = reader_slot.as_ref() {
                let key = ViewQuery {
                    offset: 0,
                    limit: 0,
                    ..query.clone()
                };
                // The page and the generation it was read at, in one snapshot.
                let (generation, items) = reader.view_page_with_generation(query)?;
                if let Some(total) = self.cached_total(&key, generation)? {
                    return Ok(ViewPage { total, items });
                }
                // First request for this query at this generation: count once.
                // Total and list come from the recount's own snapshot, so they
                // always describe the same index.
                let (generation, page) = reader.view_with_generation(query)?;
                self.cache_total(&key, generation, page.total)?;
                return Ok(page);
            }
        }
        // No reader connection: read through the index, as before W31.
        match &*self.lock()? {
            Slot::Open(index) => index.view(query),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// One page of images ordered by the EXIF taken time — the "Taken" sort.
    ///
    /// The same query, the same filters and the same paging as [`Self::view`],
    /// ordered by [`ViewQuery::descending`] on the taken time instead of by
    /// [`ViewQuery::sort`] (which this call ignores). Images with no taken time
    /// are an unknown instant and come last in both directions.
    pub fn view_by_taken(&self, query: &ViewQuery) -> Result<ViewPage, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.view_by_taken(query),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// What the index knows about one image's header: its pixel dimensions
    /// and its EXIF taken time, `None` where the header could not be read.
    ///
    /// A viewer's grid asks per tile, so this is the row of an indexed lookup
    /// rather than a scan: the facts are already stored beside the path the
    /// view returned, and no image file is opened.
    pub fn image_meta(&self, path: &str) -> Result<Option<ImageMeta>, IndexError> {
        if let Ok(reader_slot) = self.reader.lock() {
            if let Some(reader) = reader_slot.as_ref() {
                return reader.image_meta(path);
            }
        }
        match &*self.lock()? {
            Slot::Open(index) => index.image_meta(path),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// The content digest the index already holds for one image, or `None`
    /// when it holds none for these facts.
    ///
    /// The reuse that keeps a preview URL off the disk: a rendition is keyed
    /// by the file's SHA-256, the index stores that digest per file, and the
    /// row's own size and nanosecond mtime prove the digest describes the
    /// bytes now on disk. So a reader that must know the hash of an image it
    /// already resolved asks here first and reads the file only when the
    /// answer is "no digest for these facts" — the same reuse
    /// `dimagine_index`'s refresh makes through
    /// [`dimagine_index::Index::reusable_image_meta`].
    ///
    /// A closed index has no digest to give and says so; that is an unknown,
    /// not an empty library.
    pub fn content_hash(&self, path: &str, size: u64, mtime_ns: i64) -> Option<String> {
        if let Ok(reader_slot) = self.reader.lock() {
            if let Some(reader) = reader_slot.as_ref() {
                return reader
                    .reusable_image_meta(path, size, mtime_ns)
                    .ok()
                    .flatten()
                    .map(|(sha256, _meta)| sha256);
            }
        }
        self.with_index(|index| {
            index
                .reusable_image_meta(path, size, mtime_ns)
                .ok()
                .flatten()
                .map(|(sha256, _meta)| sha256)
        })
        .flatten()
    }

    /// A digest this handle already read for exactly these facts, or `None`
    /// because it has not read this file — an unknown, never a claim that the
    /// file has no digest.
    ///
    /// This is the second half of the reuse [`Self::content_hash`] starts. A
    /// file the index holds no digest for (one added between two rescans, or
    /// one a scan has not reached) would otherwise have its whole source read
    /// and hashed once per request, which is the shape of W70 M1 narrowed to
    /// that population (RW71 review M-1).
    ///
    /// The answer never leaves the handle: a content URL is resolved from the
    /// index, so a remembered digest keeps the file's path route answering
    /// directly instead of sending the browser to a `/p/` URL the index would
    /// answer with a 404.
    pub(crate) fn remembered_digest(&self, path: &str, size: u64, mtime_ns: i64) -> Option<String> {
        let key = DigestKey::new(path, size, mtime_ns);
        self.digest_memo
            .lock()
            .ok()
            .and_then(|memo| memo.get(&key).cloned())
    }

    /// Records what one file's bytes hashed to, so the next request for the
    /// same path, size and mtime asks nothing of the disk.
    ///
    /// The memo holds [`DIGEST_MEMO_LIMIT`] entries, and a sync drops it
    /// ([`Self::invalidate`]) because a scan reads these digests into the index
    /// anyway: it is a cache of one interval's worth of strangers, not a second
    /// store of digests.
    pub(crate) fn remember_digest(&self, path: &str, size: u64, mtime_ns: i64, sha256: &str) {
        if let Ok(mut memo) = self.digest_memo.lock() {
            if memo.len() >= DIGEST_MEMO_LIMIT {
                memo.clear();
            }
            memo.insert(DigestKey::new(path, size, mtime_ns), sha256.to_owned());
        }
    }

    /// The library-relative paths of every image the index holds, with the
    /// size and nanosecond mtime of the row, in path order (ADR-020 item 6:
    /// the gap a background preview backfill works from).
    pub fn image_rows(&self) -> Vec<(String, u64, i64)> {
        self.with_index(|index| index.image_rows().unwrap_or_default())
            .unwrap_or_default()
    }

    /// The library-relative paths that carry this content digest, in path
    /// order. Empty means no file in this index has those bytes.
    ///
    /// This is the one question a content-addressed URL is answered by: the
    /// bytes the URL names either exist or they do not, and nothing about a
    /// path is inferred from them.
    pub fn paths_for_content(&self, sha256: &str) -> Vec<String> {
        self.with_index(|index| index.by_sha256(sha256).unwrap_or_default())
            .unwrap_or_default()
    }

    /// The one image row that carries this content digest, if the index
    /// holds one (ADR-017 §11.3: an upload dedupes against the image the
    /// index already knows by hash). `None` is an unknown, never a
    /// "certainly absent": an index that could not be opened answers the
    /// same way a library that holds no such file does, and the upload
    /// copies in that case — the fail-safe direction.
    pub fn image_path_for_content(&self, sha256: &str) -> Option<String> {
        self.with_index(|index| index.image_by_sha256(sha256).ok().flatten())
            .flatten()
    }

    /// Run one read against the open index. `None` from outside is an index
    /// that could not be opened — an unknown, never an empty answer — and the
    /// caller decides what that means for its own question.
    fn with_index<T>(&self, read: impl FnOnce(&Index) -> T) -> Option<T> {
        match self.lock() {
            Ok(slot) => match &*slot {
                Slot::Open(index) => Some(read(index)),
                Slot::Closed(_) => None,
            },
            Err(_) => None,
        }
    }

    /// The counts behind one filter panel: for every facet value, how many
    /// images it would show given the view's other filters.
    ///
    /// Five grouped queries, whatever the library holds — the panel never asks
    /// one question per tag or folder. Nothing is cached, because the answer
    /// belongs to the filters on screen and changes with every one of them.
    pub fn view_facet_counts(
        &self,
        query: &ViewQuery,
        recent: RecentWindow,
    ) -> Result<FacetCounts, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.view_facet_counts(query, recent),
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

    /// The same back-links, with what each collection calls itself (K27
    /// motion 6: the image page names a collection the way the sidebar does,
    /// by its `title`, falling back to the file name).
    pub fn appears_in_titled(&self, image_path: &str) -> Result<Vec<AppearsIn>, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.appears_in_titled(image_path),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// Where one image sits in one view, and the images beside it there (K27
    /// motion 4). `lens_limit` is the Recent lens's window: the cap belongs
    /// to the viewer, so the caller states it in the same sizing the grid
    /// used.
    pub fn view_neighbours(
        &self,
        query: &ViewQuery,
        image_path: &str,
        lens_limit: Option<u64>,
    ) -> Result<Option<Neighbours>, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.view_neighbours(query, image_path, lens_limit),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// The same walk in the "Taken" order (W49): the view a taken-sorted grid
    /// showed, walked by the image page the grid's tiles open. The caller
    /// decides the order is in force ([`crate::view_query::ViewParams::orders_by_taken`]);
    /// the query is the ordinary one, whose `sort` field decides nothing here.
    pub fn view_neighbours_by_taken(
        &self,
        query: &ViewQuery,
        image_path: &str,
        lens_limit: Option<u64>,
    ) -> Result<Option<Neighbours>, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.view_neighbours_by_taken(query, image_path, lens_limit),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// Whether one note is a collection, listed or not.
    ///
    /// The list surfaces filter by `collection_is_listed`; a legacy
    /// `/collection/<path>` asks a different question — *is this a collection*
    /// — and it is a question about one note, so the index answers it from that
    /// note's own row (W39 review M2: reading the whole collection list here
    /// held the index lock on an admission-free route and made unrelated
    /// requests ~5× slower under a burst). The rule is the index's, so an
    /// unlisted collection still resolves (W27f review L-1: reading the
    /// filtered list there turned "unlisted" into 404).
    pub fn is_collection(&self, path: &str) -> Result<bool, IndexError> {
        match &*self.lock()? {
            Slot::Open(index) => index.is_collection(path),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// Every folder, collection and tag with its count, cached per committed
    /// index generation.
    ///
    /// The whole sidebar is one read transaction on the reader connection, so
    /// its folders, collections, tags and lens counts always describe the same
    /// snapshot. The cache entry carries that snapshot's generation — how many
    /// scans had been committed — and every committed scan raises it, so an
    /// entry survives exactly until the first request after a rescan retires
    /// it.
    pub fn sidebar_data(&self) -> Result<Arc<SidebarData>, IndexError> {
        if let Ok(reader_slot) = self.reader.lock() {
            if let Some(reader) = reader_slot.as_ref() {
                let generation = reader.generation()?;
                if let Some(cached) = self.cached_sidebar(generation)? {
                    return Ok(cached);
                }
                let (generation, data) = reader.read_at_generation(Self::sidebar_data_on)?;
                let data = Arc::new(data);
                if let Ok(mut cache) = self.cached_sidebar.lock() {
                    *cache = Some((generation, Arc::clone(&data)));
                }
                return Ok(data);
            }
        }
        // No reader connection: read through the index, uncached (as before W31).
        self.sidebar_from_index().map(Arc::new)
    }

    /// The cached sidebar at `generation`, when one was read from that
    /// generation of the index.
    fn cached_sidebar(&self, generation: u64) -> Result<Option<Arc<SidebarData>>, IndexError> {
        let cache = self.cached_sidebar.lock().map_err(|_| {
            IndexError::Io(std::io::Error::other("the sidebar cache lock is poisoned"))
        })?;
        Ok(cache
            .as_ref()
            .filter(|(cached, _)| *cached == generation)
            .map(|(_, data)| Arc::clone(data)))
    }

    /// Everything the sidebar shows, from one snapshot on `conn`. Public pieces
    /// of `dimagine_index` run on the caller's connection so the folders,
    /// collections, tags and lens counts agree on the index generation the
    /// entry is cached under.
    fn sidebar_data_on(conn: &Connection) -> Result<SidebarData, IndexError> {
        let folders = dimagine_index::folder_counts(conn)?;
        let collections = dimagine_index::collections(conn)?;
        let tags = dimagine_index::tag_counts(conn)?;
        // One row is enough to read each lens's size: the count comes back as a
        // total, so this stays one grouped query per lens.
        let untagged = dimagine_index::view_total(
            conn,
            &ViewQuery {
                untagged: true,
                ..ViewQuery::default()
            },
        )?;
        Ok(Self::sidebar_from_parts(
            folders,
            collections,
            tags,
            untagged,
        ))
    }

    /// The same sidebar built through the index connection, for the case where
    /// the reader could not be opened.
    fn sidebar_from_index(&self) -> Result<SidebarData, IndexError> {
        let slot = self.lock()?;
        let index = match &*slot {
            Slot::Open(index) => index,
            Slot::Closed(reason) => return Err(reason.clone().into()),
        };
        let folders = index.folder_counts()?;
        let collections = index.collections()?;
        let tags = index.tag_counts()?;
        let untagged = count_matching(
            index,
            ViewQuery {
                untagged: true,
                ..ViewQuery::default()
            },
        )?;
        Ok(Self::sidebar_from_parts(
            folders,
            collections,
            tags,
            untagged,
        ))
    }

    /// Assemble [`SidebarData`] from the four counts, whatever connection read
    /// them: the folder and collection filters and the Recent cap are one rule,
    /// written once.
    fn sidebar_from_parts(
        folders: Vec<(String, u64)>,
        collections: Vec<CollectionInfo>,
        tags: Vec<(String, u64)>,
        untagged: u64,
    ) -> SidebarData {
        let total = folders
            .iter()
            .find(|(folder, _)| folder.is_empty())
            .map(|(_, count)| *count)
            .unwrap_or(0);
        // Recent is the `RECENT_LIMIT` most recently added images (W34 audit
        // #10), so its size is the whole library capped at the limit — the same
        // size the view itself reports, 200 or fewer.
        let recent = total.min(RECENT_LIMIT);
        SidebarData {
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
        }
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

    /// After a committed sync, drop the digests read for files the index had
    /// none for: the sync that just ran hashed them into the index, so the memo
    /// has nothing left to save. The count and sidebar caches need no clearing —
    /// a committed scan raised the generation they are validated against, so
    /// they retire themselves on the next read (W31).
    fn after_sync(&self) {
        if let Ok(mut memo) = self.digest_memo.lock() {
            memo.clear();
        }
    }

    /// Lengthen the write critical section of every sync by `millis`, for a test
    /// that proves a request answers from the reader while the sync still holds
    /// the writer. Not for production use.
    #[cfg(test)]
    pub(crate) fn set_sync_delay(&self, millis: u64) {
        self.sync_delay.store(millis, Ordering::SeqCst);
    }

    /// The cached total for `key` when it was counted at `generation`.
    fn cached_total(&self, key: &ViewQuery, generation: u64) -> Result<Option<u64>, IndexError> {
        let cache = self.cached_totals.lock().map_err(|_| {
            IndexError::Io(std::io::Error::other(
                "the view total cache lock is poisoned",
            ))
        })?;
        Ok(cache
            .get(key)
            .filter(|(cached, _)| *cached == generation)
            .map(|(_, total)| *total))
    }

    /// Record `total` for `key` at `generation`, clearing the cache when it is
    /// full so a long-lived process cannot grow it without bound.
    fn cache_total(&self, key: &ViewQuery, generation: u64, total: u64) -> Result<(), IndexError> {
        const VIEW_TOTAL_CACHE_LIMIT: usize = 64;
        let mut cache = self.cached_totals.lock().map_err(|_| {
            IndexError::Io(std::io::Error::other(
                "the view total cache lock is poisoned",
            ))
        })?;
        if cache.len() >= VIEW_TOTAL_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key.clone(), (generation, total));
        Ok(())
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

    /// The current index generation: how many scans have been committed to the
    /// index. Read through the reader in one transaction, so it always
    /// describes committed content; a cached count tagged with this generation
    /// is valid exactly while it reads back the same.
    pub fn generation(&self) -> Result<u64, IndexError> {
        if let Ok(reader_slot) = self.reader.lock() {
            if let Some(reader) = reader_slot.as_ref() {
                return reader.generation();
            }
        }
        // No reader: read it through the index connection.
        match &*self.lock()? {
            Slot::Open(index) => index.generation(),
            Slot::Closed(reason) => Err(reason.clone().into()),
        }
    }

    /// A handle that never opens, for a library whose index could not be built.
    ///
    /// The viewer still runs; every list query reports `reason` instead of
    /// answering from a half-built database.
    pub fn unavailable(root: PathBuf, reason: IndexUnavailable) -> Self {
        Self {
            root,
            slot: Mutex::new(Slot::Closed(reason)),
            reader: Mutex::new(None),
            cached_sidebar: Mutex::new(None),
            cached_totals: Mutex::new(HashMap::new()),
            sync_delay: AtomicU64::new(0),
            rescan_started: AtomicBool::new(false),
            needs_first_background_sync: false,
            digest_memo: Mutex::new(HashMap::new()),
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
        let generation = handle.generation().unwrap();
        let before = handle.sidebar_data().unwrap();
        let folders_before = before.folders.len();

        fs::write(dir.path().join("refs/added.jpg"), JPEG).unwrap();
        assert_eq!(
            handle.sidebar_data().unwrap().total,
            before.total,
            "the cached count must hold until a sync runs"
        );

        handle.refresh().unwrap();
        assert!(
            handle.generation().unwrap() > generation,
            "a committed scan raises the index generation the caches validate against"
        );
        let after = handle.sidebar_data().unwrap();
        assert_eq!(after.total, before.total + 1);
        assert_eq!(
            after.folders.len(),
            folders_before,
            "a file in an existing folder adds no folder row"
        );
    }

    /// W31: the total a view report carries must come from the same state of the
    /// index as its list — before a change, held steady across repeat requests
    /// while no sync runs, and recounted in full on the first request after a
    /// sync. Walking every page and adding up the items is the check: if the
    /// total ever disagrees with the sum of the pages, the walk reports it.
    #[test]
    fn a_view_total_matches_the_list_before_and_after_a_change() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        let walk = |handle: &IndexHandle, query: &ViewQuery| {
            let (mut total, mut items, mut offset) = (None, Vec::new(), 0u32);
            loop {
                let page = handle
                    .view(&ViewQuery {
                        offset,
                        limit: 2,
                        ..query.clone()
                    })
                    .unwrap();
                assert_eq!(
                    page.total,
                    *total.get_or_insert(page.total),
                    "the total must not move between pages of one query"
                );
                let walked = page.items.len();
                items.extend(page.items);
                if walked == 0 || items.len() as u64 >= page.total {
                    return (page.total, items);
                }
                offset += 2;
            }
        };

        let query = ViewQuery {
            folder: Some("refs".into()),
            ..ViewQuery::default()
        };
        let (total, items) = walk(&handle, &query);
        assert_eq!(total, 2);
        assert_eq!(items.len(), 2, "{items:?}");
        // The cached total must survive a repeat request unchanged.
        assert_eq!(handle.view(&query).unwrap().total, 2);

        fs::write(dir.path().join("refs/new-1.jpg"), JPEG).unwrap();
        fs::write(dir.path().join("refs/new-2.jpg"), JPEG).unwrap();
        assert_eq!(
            walk(&handle, &query).0,
            2,
            "no rescan ran, so the cached total is still the truth"
        );
        handle.refresh().unwrap();
        let (total, items) = walk(&handle, &query);
        assert_eq!(total, 4, "the first request after the sync recounts");
        assert_eq!(items.len(), 4);
    }

    /// W31 (RW27's second Medium): a slow rescan used to hold the request lock
    /// for its whole duration, so a live `GET /api/view` waited for the full
    /// rescan. The sync delay injection simulates exactly that hold, on the lock
    /// the old design shared with requests; the view and the sidebar must both
    /// answer long before the slow sync is over, from their own reader snapshot.
    #[test]
    fn a_view_request_completes_while_a_slow_rescan_is_in_progress() {
        let dir = test_library();
        let handle = Arc::new(IndexHandle::open(dir.path()).unwrap());
        handle.set_sync_delay(750);

        let syncing = {
            let handle = Arc::clone(&handle);
            std::thread::spawn(move || handle.refresh())
        };
        // Let the sync reach its long critical section.
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !syncing.is_finished(),
            "the test must measure the request during the sync, not after it"
        );

        let started = std::time::Instant::now();
        let page = handle.view(&ViewQuery::default()).unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 4);
        let view_wait = started.elapsed();
        let sidebar = handle.sidebar_data().unwrap();
        let sidebar_wait = started.elapsed();
        assert_eq!(sidebar.total, 4);

        let slow_sync = Duration::from_millis(500);
        assert!(
            view_wait < slow_sync,
            "a view request waited {view_wait:?} for a rescan that still holds the writer"
        );
        assert!(
            sidebar_wait < slow_sync,
            "the sidebar waited {sidebar_wait:?} for a rescan that still holds the writer"
        );
        syncing.join().unwrap().unwrap();
        assert_eq!(
            handle.generation().unwrap(),
            2,
            "the slow sync still committed"
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

    /// The handle answers "Appears in" with what each collection calls
    /// itself, not only its path: the image page labels a collection the way
    /// the sidebar does (K27 motion 6).
    #[test]
    fn appears_in_also_carries_each_collections_title() {
        use dimagine_index::AppearsIn;
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        assert_eq!(
            handle.appears_in_titled("refs/img1.jpg").unwrap(),
            vec![AppearsIn {
                note_path: "collection.md".to_owned(),
                title: "Sample Collection".to_owned(),
            }]
        );
        let bare = handle.appears_in_titled("plain.png").unwrap();
        assert!(bare.is_empty(), "{bare:?}");
    }

    /// The handle passes the neighbour question through with the lens the
    /// caller states: a member answers its place, a non-member answers
    /// `None`.
    #[test]
    fn the_handle_answers_neighbours_for_a_member_not_for_an_outsider() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        let single = ViewQuery {
            collection: Some("collection.md".to_owned()),
            ..ViewQuery::default()
        };
        let member = handle
            .view_neighbours(&single, "refs/img1.jpg", None)
            .unwrap()
            .unwrap();
        assert_eq!(member.position, 1);
        assert_eq!(member.total, 1);
        assert_eq!(member.previous, None);
        assert_eq!(member.next, None);
        assert!(handle
            .view_neighbours(&single, "refs2/trap.jpg", None)
            .unwrap()
            .is_none());
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

    /// W34 audit #10: Recent is "the last 200 added", not a 30-day window,
    /// so on a library bigger than the limit the lens caps at the limit and
    /// the label says the number the lens keeps.
    #[test]
    fn recent_is_capped_at_the_last_two_hundred_added() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("refs")).unwrap();
        for index in 0..205 {
            fs::write(root.join(format!("refs/img{index:03}.jpg")), JPEG).unwrap();
        }
        let handle = IndexHandle::open(root).unwrap();

        let sidebar = handle.sidebar_data().unwrap();
        assert_eq!(sidebar.total, 205);
        assert_eq!(
            sidebar.recent, 200,
            "Recent promises the last {RECENT_LIMIT} added, not the whole library"
        );
        assert_eq!(recent_label(), "Recent — last 200 added");
    }

    /// A library smaller than the limit is all Recent: the lens reaches every
    /// image, whatever its age.
    #[test]
    fn a_small_library_is_all_recent() {
        let dir = test_library();
        let handle = IndexHandle::open(dir.path()).unwrap();
        let sidebar = handle.sidebar_data().unwrap();
        assert_eq!(sidebar.recent, sidebar.total);
        assert_eq!(sidebar.recent, 4);
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
