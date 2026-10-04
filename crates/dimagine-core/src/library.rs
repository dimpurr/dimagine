//! Walk a library folder and classify what is in it (FORMAT §2).
//!
//! Walking rules (FORMAT §2.2): ignore entries whose names start with `.`
//! (including `.dimagine/`), `._*` resource-fork files, `Thumbs.db` and
//! `desktop.ini`; never follow symbolic links. Non-UTF-8 names are handled
//! for identity and I/O as native paths. A lossy UTF-8 spelling is used only
//! for display; non-UTF-8 names never become string-based link lookup keys.
//!
//! A directory that cannot be listed does not abort the walk: it is recorded
//! and the library is reported as NOT fully read. "Unknown is not empty"
//! (FORMAT §1): readers must treat the outcome of such an incomplete walk as
//! unreliable, which is why `unreadable_dirs` exists and why both `scan` and
//! `check` distinguish it from ordinary findings.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::format::{self, IgnoreReason};

/// How a file is classified by its name (FORMAT §2 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    /// An image (`jpg`, `png`, ...).
    Image,
    /// An image note: `<image>.<ext>.md` (FORMAT §3).
    ImageNote,
    /// Any other `*.md` note (collections live here, FORMAT §5).
    Note,
    /// Verbatim source metadata: `<image>.<ext>.<source>.json` (FORMAT §3.4).
    Raw,
    /// A JSON Canvas board (FORMAT §6).
    Canvas,
    /// Anything else.
    Other,
    /// A FIFO, socket, device, or other non-regular filesystem entry.
    Special,
}

/// A library entry, already classified by its name and filesystem type.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Lossy display path relative to the library root, `/`-separated.
    pub rel: String,
    /// File name only.
    pub name: String,
    /// Native library-relative path used for filesystem access and identity.
    pub path: PathBuf,
    /// Native final path component, retained independently of display text.
    pub native_name: OsString,
    pub class: FileClass,
}

/// Open a walked entry without following a replacement symlink, then verify
/// that the opened object is still a regular file before reading its bytes.
pub fn open_regular(root: &Path, entry: &FileEntry) -> std::io::Result<fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(root.join(&entry.path))?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "entry is not a regular file",
        ));
    }
    Ok(file)
}

impl FileEntry {
    /// The library-relative path of the folder holding this file, `""` at root.
    pub fn dir(&self) -> &str {
        match self.rel.rfind('/') {
            Some(i) => &self.rel[..i],
            None => "",
        }
    }

    /// For an image note, the name of the image it belongs to (``x.jpg`` for
    /// ``x.jpg.md``); `None` for anything else (FORMAT §3).
    pub fn paired_image_name(&self) -> Option<&str> {
        match self.class {
            // `classify` guarantees the strip works for this class.
            FileClass::ImageNote => strip_suffix_ignore_case(&self.name, ".md"),
            _ => None,
        }
    }

    /// For an image note, the library-relative path of its image: same
    /// folder, same name (FORMAT §3).
    pub fn paired_image_rel(&self) -> Option<&str> {
        match self.class {
            FileClass::ImageNote => strip_suffix_ignore_case(&self.rel, ".md"),
            _ => None,
        }
    }

    /// Native path of the image paired with this image note.
    pub fn paired_image_path(&self) -> Option<PathBuf> {
        if self.class != FileClass::ImageNote {
            return None;
        }
        let name = paired_name_os(&self.native_name)?;
        Some(self.path.with_file_name(name))
    }
}

#[cfg(unix)]
fn paired_name_os(name: &std::ffi::OsStr) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = name.as_bytes();
    let stem = bytes.get(..bytes.len().checked_sub(3)?)?;
    if !bytes[bytes.len() - 3..].eq_ignore_ascii_case(b".md") {
        return None;
    }
    Some(OsString::from_vec(stem.to_vec()))
}

#[cfg(not(unix))]
fn paired_name_os(name: &std::ffi::OsStr) -> Option<OsString> {
    let name = name.to_str()?;
    strip_suffix_ignore_case(name, ".md").map(OsString::from)
}

/// Counts of ignored entries, by reason (FORMAT §2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct IgnoredCounts {
    pub total: usize,
    pub hidden: usize,
    pub resource_fork: usize,
    pub os_metadata: usize,
    pub symlink: usize,
}

/// A path that could not be read, and why.
#[derive(Clone, Debug, serde::Serialize)]
pub struct UnreadableEntry {
    pub path: String,
    pub path_non_utf8: bool,
    pub reason: String,
}

/// The library root is not usable.
#[derive(Clone, Debug)]
pub struct NotAFolder(pub PathBuf);

impl std::fmt::Display for NotAFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a folder: {}", self.0.display())
    }
}

impl std::error::Error for NotAFolder {}

/// A walked library: the classification of everything found, read-only.
#[derive(Clone, Debug)]
pub struct Library {
    /// The library root. If the user passed a symlink, this is the symlink
    /// path itself (explicit intent); symlinks inside are never followed.
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    pub ignored: IgnoredCounts,
    /// Directories (or listings) that could not be read. Non-empty means the
    /// walk is INCOMPLETE and every "found nothing" statement is unreliable.
    pub unreadable_dirs: Vec<UnreadableEntry>,
}

impl Library {
    /// True when the whole folder tree could be enumerated.
    pub fn fully_read(&self) -> bool {
        self.unreadable_dirs.is_empty()
    }

    /// Walk `root` as a library. It must exist and be a directory.
    pub fn open(root: &Path) -> Result<Library, NotAFolder> {
        if !root.is_dir() {
            return Err(NotAFolder(root.to_path_buf()));
        }
        let mut library = Library {
            root: root.to_path_buf(),
            files: Vec::new(),
            ignored: IgnoredCounts::default(),
            unreadable_dirs: Vec::new(),
        };
        library.walk_dir(root);
        Ok(library)
    }

    fn walk_dir(&mut self, dir: &Path) {
        let read = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(err) => {
                self.unreadable_dirs.push(UnreadableEntry {
                    path: dir
                        .strip_prefix(&self.root)
                        .unwrap_or(dir)
                        .display()
                        .to_string(),
                    path_non_utf8: dir
                        .strip_prefix(&self.root)
                        .unwrap_or(dir)
                        .to_str()
                        .is_none(),
                    reason: err.to_string(),
                });
                return;
            }
        };
        // Collect first so the walk order is deterministic on every platform.
        let mut entries: Vec<(OsString, PathBuf)> = Vec::new();
        let mut partial_err: Option<std::io::Error> = None;
        for entry in read {
            match entry {
                Ok(dirent) => {
                    entries.push((dirent.file_name(), dirent.path()));
                }
                Err(err) => partial_err = Some(err),
            }
        }
        if let Some(err) = partial_err {
            self.unreadable_dirs.push(UnreadableEntry {
                path: dir
                    .strip_prefix(&self.root)
                    .unwrap_or(dir)
                    .display()
                    .to_string(),
                path_non_utf8: dir
                    .strip_prefix(&self.root)
                    .unwrap_or(dir)
                    .to_str()
                    .is_none(),
                reason: format!("partial directory listing: {err}"),
            });
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (native_name, path) in entries {
            let name = native_name.to_string_lossy().into_owned();
            // FORMAT §2.2: ignore dot paths, resource forks and OS metadata
            // files, in folders just as much as in files.
            if let Some(reason) = format::ignore_reason(&name) {
                self.count_ignored(reason);
                continue;
            }
            // FORMAT §2.2: do not follow symlinks at all while scanning,
            // whether they point inside or outside the library.
            let metadata = match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    self.ignored.symlink += 1;
                    self.ignored.total += 1;
                    continue;
                }
                Ok(meta) => meta,
                Err(err) => {
                    self.unreadable_dirs.push(UnreadableEntry {
                        path: path
                            .strip_prefix(&self.root)
                            .unwrap_or(&path)
                            .display()
                            .to_string(),
                        path_non_utf8: path
                            .strip_prefix(&self.root)
                            .unwrap_or(&path)
                            .to_str()
                            .is_none(),
                        reason: format!("could not inspect path: {err}"),
                    });
                    continue;
                }
            };
            let rel_path = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            if metadata.is_dir() {
                self.walk_dir(&path);
            } else {
                let class = if metadata.is_file() {
                    classify(&name)
                } else {
                    FileClass::Special
                };
                self.files.push(FileEntry {
                    rel,
                    name,
                    path: rel_path,
                    native_name,
                    class,
                });
            }
        }
    }

    fn count_ignored(&mut self, reason: IgnoreReason) {
        self.ignored.total += 1;
        match reason {
            IgnoreReason::Hidden => self.ignored.hidden += 1,
            IgnoreReason::ResourceFork => self.ignored.resource_fork += 1,
            IgnoreReason::OsMetadata => self.ignored.os_metadata += 1,
        }
    }
}

/// Strip `suffix` from `name` if present, ASCII-case-insensitively.
fn strip_suffix_ignore_case<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let start = name.len().checked_sub(suffix.len())?;
    let tail = name.get(start..)?;
    if tail.eq_ignore_ascii_case(suffix) {
        Some(&name[..start])
    } else {
        None
    }
}

/// Classify a file name per the FORMAT §2 table. Extensions are matched
/// case-insensitively (§2.1).
pub fn classify(name: &str) -> FileClass {
    let Some(ext) = format::file_extension(name) else {
        return FileClass::Other;
    };
    let ext = ext.to_ascii_lowercase();
    if ext == "md" {
        let Some(stem) = strip_suffix_ignore_case(name, ".md") else {
            return FileClass::Other;
        };
        let is_image = format::file_extension(stem).is_some_and(format::is_image_extension);
        return if is_image {
            FileClass::ImageNote
        } else {
            FileClass::Note
        };
    }
    if ext == "canvas" {
        return FileClass::Canvas;
    }
    if ext == "json" {
        // A raw file is `<name>.<ext>.<source>.json` (FORMAT §3.4): strip
        // `.json`, strip the `<source>` segment, then an image extension must
        // remain.
        let Some(rest) = strip_suffix_ignore_case(name, ".json") else {
            return FileClass::Other;
        };
        let Some(source) = format::file_extension(rest) else {
            return FileClass::Other;
        };
        let Some(image_name) = strip_suffix_ignore_case(rest, &format!(".{source}")) else {
            return FileClass::Other;
        };
        let is_raw = format::file_extension(image_name).is_some_and(format::is_image_extension);
        return if is_raw {
            FileClass::Raw
        } else {
            FileClass::Other
        };
    }
    if format::is_image_extension(&ext) {
        return FileClass::Image;
    }
    FileClass::Other
}
