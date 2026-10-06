//! Read-only Eagle library importer for dimagine libraries.
//!
//! The source library is never modified. Imported images retain their original
//! bytes and raw Eagle metadata; generated notes follow the dimagine format.

mod backfill;

use chrono::{DateTime, Datelike, FixedOffset, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error as StdError;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use ulid::Ulid;
use unicode_normalization::UnicodeNormalization;

pub use backfill::{backfill_added, BackfillReport};

/// Options controlling the import.
#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// Optional display label for the Eagle library. By default this is its
    /// directory name with a trailing `.library` removed.
    pub name: Option<String>,
}

/// Machine-readable reason why an Eagle item was skipped.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReasonCode {
    MissingMetadata,
    UnreadableMetadata,
    Trash,
    NotImage,
    OriginalMissing,
    CopyFailed,
    Symlink,
}

/// An item skipped during import, including stable reason code and detail.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedItem {
    /// Eagle item directory identifier.
    pub item: String,
    /// Stable machine-readable reason.
    pub reason_code: SkipReasonCode,
    /// Prototype-compatible explanation.
    pub reason: String,
}

/// One imported Eagle item and its path in the destination library.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedItem {
    /// Eagle item identifier.
    pub item: String,
    /// Destination-relative image path using forward slashes.
    pub path: String,
}

/// Summary of a completed import. It can be serialized as JSON with serde.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportReport {
    /// Successfully imported image items.
    pub imported: Vec<ImportedItem>,
    /// Items omitted from the destination.
    pub skipped: Vec<SkippedItem>,
    /// Number of items assigned generated names because their names were generic.
    pub renamed: usize,
    /// Number of references to folders absent from the folder tree.
    pub dangling_folder_refs: usize,
    /// Imported image count by Eagle folder path; `(no Eagle folder)` represents inbox.
    pub folder_counts: BTreeMap<String, usize>,
}

/// Import failure caused by invalid input, destination policy, or file I/O.
#[derive(Debug)]
pub enum ImportError {
    /// Source metadata is missing or malformed.
    InvalidSource(String),
    /// Destination contains entries outside the allowed settings directories.
    DestinationNotEmpty(PathBuf),
    /// Source and destination overlap after resolving existing path components.
    OverlappingPaths {
        source: PathBuf,
        destination: PathBuf,
    },
    /// The Eagle folder tree contains an ID more than once.
    DuplicateFolderId(String),
    /// An I/O failure after some outputs were committed, with recoverable progress.
    PartialIo {
        error: io::Error,
        progress: ImportReport,
        retained_artifacts: Vec<PathBuf>,
    },
    /// An underlying filesystem operation failed.
    Io(io::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSource(message) => write!(f, "{message}"),
            Self::DestinationNotEmpty(path) => {
                write!(f, "refuse: {} exists and is not empty", path.display())
            }
            Self::OverlappingPaths {
                source,
                destination,
            } => write!(
                f,
                "refuse overlapping source and destination: {} and {}",
                source.display(),
                destination.display()
            ),
            Self::DuplicateFolderId(id) => write!(f, "duplicate Eagle folder ID: {id}"),
            Self::PartialIo {
                error,
                retained_artifacts,
                ..
            } => write!(
                f,
                "I/O failure after partial import ({} retained artifacts): {error}",
                retained_artifacts.len()
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl StdError for ImportError {}

impl From<io::Error> for ImportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Import images from an Eagle library into an empty or missing destination.
///
/// The source is read only. The destination may contain `.obsidian/` and/or
/// `.dimagine/` settings directories. The returned report records imported paths,
/// stable skip codes, generic-name changes, dangling folder references, and
/// per-folder counts.
pub fn import(
    src_library: &Path,
    dst_library: &Path,
    opts: ImportOptions,
) -> Result<ImportReport, ImportError> {
    let source = fs::canonicalize(src_library)?;
    let destination = canonicalize_future_path(dst_library)?;
    if source.starts_with(&destination) || destination.starts_with(&source) {
        return Err(ImportError::OverlappingPaths {
            source,
            destination,
        });
    }
    let dst_library = destination.as_path();
    let source_meta = source.join("metadata.json");
    if !matches!(fs::symlink_metadata(&source_meta), Ok(ref m) if m.file_type().is_file()) {
        return Err(ImportError::InvalidSource(format!(
            "not an Eagle library (no metadata.json): {}",
            source.display()
        )));
    }
    if dst_library.exists() {
        for entry in fs::read_dir(dst_library)? {
            let entry = entry?;
            let name = entry.file_name();
            let allowed = name == ".obsidian" || name == ".dimagine";
            if !allowed || !entry.file_type()?.is_dir() {
                return Err(ImportError::DestinationNotEmpty(dst_library.to_path_buf()));
            }
        }
    }

    let json_bytes = fs::read(&source_meta)?;
    let mut deserializer = serde_json::Deserializer::from_slice(&json_bytes);
    deserializer.disable_recursion_limit();
    let stacked = serde_stacker::Deserializer::new(&mut deserializer);
    let root = Value::deserialize(stacked).map_err(|error| {
        ImportError::InvalidSource(format!("unreadable metadata.json: {error}"))
    })?;
    deserializer.end().map_err(|error| {
        ImportError::InvalidSource(format!("unreadable metadata.json: {error}"))
    })?;
    let root = IterativeValue::new(root);
    check_json_depth(&root, 512)?;
    let default_name = source
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let display_library = opts.name.unwrap_or_else(|| {
        default_name
            .strip_suffix(".library")
            .unwrap_or(&default_name)
            .to_owned()
    });
    let mut library = safe_component(&display_library, 120);
    if library.is_empty() {
        library = "Eagle".to_owned();
    }
    let mut folder_paths = HashMap::new();
    if let Some(folders) = root.get("folders").and_then(Value::as_array) {
        walk_folders(folders, "", &mut folder_paths)?;
    }

    let now: DateTime<Local> = Local::now();
    let now_text = now.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let mut report = ImportReport::default();
    let mut notes = Vec::new();
    let mut collection_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut taken: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let images_dir = source.join("images");
    if matches!(fs::symlink_metadata(&images_dir), Ok(ref metadata) if metadata.file_type().is_symlink())
    {
        report.skipped.push(skip(
            "images".to_owned(),
            SkipReasonCode::Symlink,
            "images directory is a symlink",
        ));
    }
    let mut item_dirs =
        sorted_info_dirs(&images_dir).map_err(|error| partial_io(error, dst_library, &report))?;

    for item_dir in item_dirs.drain(..) {
        if fs::symlink_metadata(&item_dir)
            .map_err(|error| partial_io(error, dst_library, &report))?
            .file_type()
            .is_symlink()
        {
            let item = item_dir.file_name().unwrap_or_default().to_string_lossy();
            report.skipped.push(skip(
                item.strip_suffix(".info").unwrap_or(&item).to_owned(),
                SkipReasonCode::Symlink,
                "item directory is a symlink",
            ));
            continue;
        }
        let item = item_dir.file_name().unwrap_or_default().to_string_lossy();
        let item = item.strip_suffix(".info").unwrap_or(&item).to_owned();
        let meta_path = item_dir.join("metadata.json");
        if !meta_path.exists() {
            report.skipped.push(skip(
                item,
                SkipReasonCode::MissingMetadata,
                "no metadata.json",
            ));
            continue;
        }
        if !fs::symlink_metadata(&meta_path)
            .map_err(|error| partial_io(error, dst_library, &report))?
            .file_type()
            .is_file()
        {
            report.skipped.push(skip(
                item,
                SkipReasonCode::Symlink,
                "metadata entry is not a regular file",
            ));
            continue;
        }
        let raw = match fs::read(&meta_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.skipped.push(skip(
                    item,
                    SkipReasonCode::UnreadableMetadata,
                    format!("unreadable metadata: {error}"),
                ));
                continue;
            }
        };
        let metadata: Value = match serde_json::from_slice(&raw) {
            Ok(value) => value,
            Err(error) => {
                report.skipped.push(skip(
                    item,
                    SkipReasonCode::UnreadableMetadata,
                    format!("unreadable metadata: {error}"),
                ));
                continue;
            }
        };
        if metadata
            .get("isDeleted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            report
                .skipped
                .push(skip(item, SkipReasonCode::Trash, "in Eagle trash"));
            continue;
        }
        let ext = metadata
            .get("ext")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if !is_image(&ext) {
            let shown = if ext.is_empty() {
                "no ext"
            } else {
                ext.as_str()
            };
            report.skipped.push(skip(
                item,
                SkipReasonCode::NotImage,
                format!("not an image ({shown})"),
            ));
            continue;
        }
        let original_name = metadata.get("name").and_then(Value::as_str).unwrap_or("");
        let raw_ext = metadata.get("ext").and_then(Value::as_str).unwrap_or("");
        let candidate_filename = format!("{original_name}.{raw_ext}");
        let mut name_components = Path::new(original_name).components();
        let original_name_is_normal = matches!(
            name_components.next(),
            Some(std::path::Component::Normal(_))
        ) && name_components.next().is_none();
        let mut candidate_components = Path::new(&candidate_filename).components();
        let candidate_is_normal = matches!(
            candidate_components.next(),
            Some(std::path::Component::Normal(_))
        ) && candidate_components.next().is_none();

        let mut expected_is_file = false;
        let mut expected_path = None;
        if original_name_is_normal && candidate_is_normal {
            let expected = item_dir.join(&candidate_filename);
            let expected_metadata = fs::symlink_metadata(&expected);
            if matches!(expected_metadata, Ok(ref metadata) if metadata.file_type().is_symlink()) {
                report.skipped.push(skip(
                    item,
                    SkipReasonCode::Symlink,
                    "original image is a symlink",
                ));
                continue;
            }
            if matches!(expected_metadata, Ok(ref metadata) if metadata.file_type().is_file()) {
                expected_is_file = true;
                expected_path = Some(expected);
            }
        }
        let original = if expected_is_file {
            expected_path.expect("expected path must exist when is_file is true")
        } else {
            match fallback_original(&item_dir)
                .map_err(|error| partial_io(error, dst_library, &report))?
            {
                Some(path) => path,
                None => {
                    let has_symlink = fs::read_dir(&item_dir)
                        .map_err(|error| partial_io(error, dst_library, &report))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| partial_io(error, dst_library, &report))?
                        .iter()
                        .any(|entry| {
                            entry.file_type().is_ok_and(|kind| kind.is_symlink())
                                && entry.file_name() != "metadata.json"
                                && !entry
                                    .file_name()
                                    .to_string_lossy()
                                    .ends_with("_thumbnail.png")
                        });
                    if has_symlink {
                        report.skipped.push(skip(
                            item,
                            SkipReasonCode::Symlink,
                            "original candidate is a symlink",
                        ));
                        continue;
                    }
                    report.skipped.push(skip(
                        item,
                        SkipReasonCode::OriginalMissing,
                        "original file missing",
                    ));
                    continue;
                }
            }
        };

        let folder_ids = metadata
            .get("folders")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let paths: Vec<String> = folder_ids
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|id| folder_paths.get(id).cloned())
            .collect();
        report.dangling_folder_refs += folder_ids.len().saturating_sub(paths.len());
        let home = if let Some(path) = paths.first() {
            dst_library.join("Eagle").join(&library).join(path)
        } else {
            dst_library.join("inbox")
        };
        create_confined_dirs(dst_library, &home)
            .map_err(|error| partial_io(error, dst_library, &report))?;
        let mut name = clean(original_name);
        let generated_name = name.is_empty() || is_generic(&name);
        if generated_name {
            name = metadata
                .get("url")
                .and_then(Value::as_str)
                .and_then(name_from_url)
                .map(|derived| clean(&derived))
                .unwrap_or_else(|| {
                    format!(
                        "{}-{}",
                        now.format("%Y%m%d-%H%M%S"),
                        &Ulid::new().to_string().to_lowercase()[22..]
                    )
                });
            // Count generated names only after the image and raw metadata commit.
        }
        let filename = unique_name(&home, &name, &ext, &mut taken)
            .map_err(|error| partial_io(error, dst_library, &report))?;
        let destination_image = home.join(&filename);
        match copy_item_transactionally(&original, &destination_image, &raw) {
            Ok(()) => {}
            Err(CopyItemError::Skipped(error)) => {
                report.skipped.push(skip(
                    item,
                    SkipReasonCode::CopyFailed,
                    format!("copy failed (not downloaded?): {error}"),
                ));
                continue;
            }
            Err(CopyItemError::CleanupFailed { error, retained }) => {
                let mut retained_list = retained_artifacts(dst_library, &report);
                retained_list.extend(retained);
                return Err(ImportError::PartialIo {
                    error,
                    progress: report,
                    retained_artifacts: retained_list,
                });
            }
        }
        let raw_filename = format!("{filename}.eagle.json");
        if generated_name {
            report.renamed += 1;
        }
        let rel = relative_string(dst_library, &destination_image);
        let note = make_note(
            &metadata,
            &display_library,
            &item,
            &paths,
            &filename,
            &raw_filename,
            &now_text,
        );
        notes.push(Note {
            path: destination_image.with_file_name(format!("{filename}.md")),
            frontmatter_and_body: note,
            filename,
            rel: rel.clone(),
        });
        report.imported.push(ImportedItem {
            item,
            path: rel.clone(),
        });
        for path in if paths.is_empty() {
            vec!["(no Eagle folder)".to_owned()]
        } else {
            paths
        } {
            *report.folder_counts.entry(path.clone()).or_default() += 1;
            collection_members
                .entry(path)
                .or_default()
                .push(rel.clone());
        }
    }

    let name_counts =
        image_name_counts(dst_library).map_err(|error| partial_io(error, dst_library, &report))?;
    for note in notes {
        let embed = if name_counts
            .get(&note.filename.to_lowercase())
            .copied()
            .unwrap_or(0)
            == 1
        {
            note.filename
        } else {
            note.rel
        };
        let text = format!("{}![[{}]]\n", note.frontmatter_and_body, embed);
        if let Err(error) = write_atomically(&note.path, text.as_bytes()) {
            let retained_artifacts = retained_artifacts(dst_library, &report);
            return Err(ImportError::PartialIo {
                error,
                progress: report,
                retained_artifacts,
            });
        }
    }

    if let Err(error) = write_collection_and_report(
        dst_library,
        &source,
        &library,
        &now,
        &report,
        &collection_members,
    ) {
        let retained_artifacts = retained_artifacts(dst_library, &report);
        return Err(ImportError::PartialIo {
            error,
            progress: report,
            retained_artifacts,
        });
    }
    if let Err(error) = write_obsidian_gallery(dst_library) {
        let retained_artifacts = retained_artifacts(dst_library, &report);
        return Err(ImportError::PartialIo {
            error,
            progress: report,
            retained_artifacts,
        });
    }
    Ok(report)
}

fn skip(item: String, reason_code: SkipReasonCode, reason: impl Into<String>) -> SkippedItem {
    SkippedItem {
        item,
        reason_code,
        reason: reason.into(),
    }
}

fn canonicalize_future_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "destination has no existing ancestor",
            )
        })?;
        missing.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "destination has no existing ancestor",
            )
        })?;
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn create_confined_dirs(root: &Path, target: &Path) -> io::Result<()> {
    let relative = target.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "write path escaped destination",
        )
    })?;
    let mut current = root.to_path_buf();
    if !current.exists() {
        fs::create_dir_all(&current)?;
    }
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe destination path",
            ));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination path contains a symlink",
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination component is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Debug)]
enum CopyItemError {
    Skipped(io::Error),
    CleanupFailed {
        error: io::Error,
        retained: Vec<PathBuf>,
    },
}

fn copy_item_transactionally(source: &Path, image: &Path, raw: &[u8]) -> Result<(), CopyItemError> {
    let parent = image.parent().expect("image destination has parent");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let image_tmp = parent.join(format!(".dimagine-tmp-{nonce}.image"));
    let raw_path = image.with_file_name(format!(
        "{}.eagle.json",
        image.file_name().unwrap().to_string_lossy()
    ));
    let raw_tmp = parent.join(format!(".dimagine-tmp-{nonce}.raw"));
    let mut published_image = false;
    let result = (|| -> io::Result<()> {
        fs::copy(source, &image_tmp)?;
        fs::write(&raw_tmp, raw)?;
        fs::rename(&image_tmp, image)?;
        published_image = true;
        fs::rename(&raw_tmp, &raw_path)?;
        Ok(())
    })();

    if let Err(error) = result {
        let mut cleanup_error = None;
        let mut retained = Vec::new();

        if published_image {
            if let Err(err) = fs::remove_file(image) {
                if err.kind() != io::ErrorKind::NotFound {
                    cleanup_error = Some(err);
                    retained.push(image.to_path_buf());
                }
            }
        }
        if image_tmp.exists() {
            if let Err(err) = fs::remove_file(&image_tmp) {
                if err.kind() != io::ErrorKind::NotFound {
                    if cleanup_error.is_none() {
                        cleanup_error = Some(err);
                    }
                    retained.push(image_tmp.clone());
                }
            }
        }
        if raw_tmp.exists() {
            if let Err(err) = fs::remove_file(&raw_tmp) {
                if err.kind() != io::ErrorKind::NotFound {
                    if cleanup_error.is_none() {
                        cleanup_error = Some(err);
                    }
                    retained.push(raw_tmp.clone());
                }
            }
        }

        if let Some(err) = cleanup_error {
            return Err(CopyItemError::CleanupFailed {
                error: io::Error::new(
                    err.kind(),
                    format!("cleanup failed: {err}; initial error: {error}"),
                ),
                retained,
            });
        }

        return Err(CopyItemError::Skipped(error));
    }

    let clean_img = match fs::remove_file(&image_tmp) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    };
    let clean_raw = match fs::remove_file(&raw_tmp) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    };
    if let Err(err) = clean_img {
        return Err(CopyItemError::CleanupFailed {
            error: err,
            retained: vec![image_tmp],
        });
    }
    if let Err(err) = clean_raw {
        return Err(CopyItemError::CleanupFailed {
            error: err,
            retained: vec![raw_tmp],
        });
    }

    Ok(())
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().expect("output file has parent");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".dimagine-tmp-{nonce}.note"));
    let result = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        // Durable before the rename: a crash after the rename must not leave
        // the note empty or half-written.
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        // The rename itself has to reach the disk, or a crash can undo it.
        sync_directory(parent)
    })();
    let clean = match fs::remove_file(&temporary) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => clean,
        Err(err) => {
            if let Err(cleanup_err) = clean {
                return Err(io::Error::new(
                    cleanup_err.kind(),
                    format!(
                        "failed to clean temporary file {}: {cleanup_err}; write error: {err}",
                        temporary.display()
                    ),
                ));
            }
            Err(err)
        }
    }
}

/// Flush a directory entry so a rename survives a crash. Not every platform
/// lets a directory be opened for this (Windows refuses), and there the rename
/// is still atomic; only an unexpected failure is reported.
fn sync_directory(directory: &Path) -> io::Result<()> {
    match fs::File::open(directory) {
        Ok(handle) => handle.sync_all(),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn retained_artifacts(root: &Path, report: &ImportReport) -> Vec<PathBuf> {
    let mut artifacts: Vec<PathBuf> = report
        .imported
        .iter()
        .flat_map(|item| {
            let image = root.join(&item.path);
            [
                image.clone(),
                image.with_file_name(format!(
                    "{}.eagle.json",
                    image.file_name().unwrap().to_string_lossy()
                )),
                image.with_file_name(format!(
                    "{}.md",
                    image.file_name().unwrap().to_string_lossy()
                )),
            ]
        })
        .filter(|path| path.exists())
        .collect();

    let collections_dir = root.join("collections");
    if collections_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&collections_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    artifacts.push(path);
                }
            }
        }
    }

    artifacts
}

fn partial_io(error: io::Error, root: &Path, report: &ImportReport) -> ImportError {
    ImportError::PartialIo {
        error,
        progress: report.clone(),
        retained_artifacts: retained_artifacts(root, report),
    }
}

struct IterativeValue(Option<Value>);

impl IterativeValue {
    fn new(value: Value) -> Self {
        Self(Some(value))
    }
}

impl std::ops::Deref for IterativeValue {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("value present")
    }
}

impl Drop for IterativeValue {
    fn drop(&mut self) {
        if let Some(val) = self.0.take() {
            drop_value_iteratively(val);
        }
    }
}

fn drop_value_iteratively(mut value: Value) {
    let mut stack = Vec::new();
    stack.push(std::mem::replace(&mut value, Value::Null));
    while let Some(mut current) = stack.pop() {
        match current {
            Value::Array(ref mut vec) => {
                for item in vec.drain(..) {
                    stack.push(item);
                }
            }
            Value::Object(ref mut map) => {
                for (_, item) in std::mem::take(map) {
                    stack.push(item);
                }
            }
            _ => {}
        }
    }
}

fn check_json_depth(root: &Value, max_depth: usize) -> Result<(), ImportError> {
    let mut stack = vec![(root, 1)];
    while let Some((node, depth)) = stack.pop() {
        if depth > max_depth {
            return Err(ImportError::InvalidSource(format!(
                "JSON nesting exceeds maximum supported depth of {max_depth}"
            )));
        }
        match node {
            Value::Array(arr) => {
                for item in arr {
                    stack.push((item, depth + 1));
                }
            }
            Value::Object(map) => {
                for item in map.values() {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn walk_folders(
    folders: &[Value],
    parent: &str,
    output: &mut HashMap<String, String>,
) -> Result<(), ImportError> {
    walk_folders_at(folders, parent, output, &mut HashSet::new(), 0)
}

fn walk_folders_at(
    folders: &[Value],
    parent: &str,
    output: &mut HashMap<String, String>,
    ids: &mut HashSet<String>,
    depth: usize,
) -> Result<(), ImportError> {
    if depth > 128 {
        return Err(ImportError::InvalidSource(
            "folder nesting exceeds the supported maximum of 128".to_owned(),
        ));
    }
    let mut siblings = HashSet::new();
    for folder in folders {
        let Some(id) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !ids.insert(id.to_owned()) {
            return Err(ImportError::DuplicateFolderId(id.to_owned()));
        }
        let name = folder.get("name").and_then(Value::as_str).unwrap_or("");
        let base = safe_component(name, 120);
        let base = if base.is_empty() {
            let fallback = safe_component(id, 80);
            if fallback.is_empty() {
                "unnamed-folder".to_owned()
            } else {
                fallback
            }
        } else {
            base
        };
        let mut segment = base.clone();
        let mut suffix = 2;
        while !siblings.insert(segment.to_lowercase()) {
            segment = suffixed_component(&base, suffix, 120);
            suffix += 1;
        }
        let path = if parent.is_empty() {
            segment
        } else {
            format!("{parent}/{segment}")
        };
        output.insert(id.to_owned(), path.clone());
        if let Some(children) = folder.get("children").and_then(Value::as_array) {
            walk_folders_at(children, &path, output, ids, depth + 1)?;
        }
    }
    Ok(())
}

fn sorted_info_dirs(images: &Path) -> io::Result<Vec<PathBuf>> {
    if !matches!(fs::symlink_metadata(images), Ok(ref metadata) if metadata.is_dir()) {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(images)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "info"))
        .collect())
}

fn fallback_original(item_dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut files = fs::read_dir(item_dir)?.collect::<Result<Vec<_>, _>>()?;
    files.sort_by_key(|entry| entry.file_name());
    Ok(files
        .into_iter()
        .find(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name() != "metadata.json"
                && !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with("_thumbnail.png")
        })
        .map(|entry| entry.path()))
}

fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "tif" | "tiff" | "bmp"
    )
}

fn clean(input: &str) -> String {
    safe_component(input, 180)
}

fn safe_component(input: &str, max_bytes: usize) -> String {
    let normalized: String = input.nfc().collect();
    let mut replaced = String::new();
    for character in normalized.chars() {
        let character = if matches!(
            character,
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '[' | ']' | '#' | '^'
        ) || character.is_control()
        {
            '-'
        } else {
            character
        };
        if replaced.len() + character.len_utf8() > max_bytes {
            break;
        }
        replaced.push(character);
    }
    let mut result = replaced.trim().trim_matches(['.', ' ']).to_owned();
    if result.is_empty() {
        return result;
    }
    if is_windows_reserved(&result) {
        result.insert(0, '_');
    }
    result
}

/// Whether a name would collide with a reserved Windows device name.
///
/// Windows refuses to create `CON`, `PRN`, `AUX`, `NUL`, `COM1` to `COM9` and
/// `LPT1` to `LPT9` with any extension, and it also accepts the superscript
/// digit spellings of `COM1` to `COM3` and `LPT1` to `LPT3` (`COM¹`, `COM²`,
/// `COM³`, `LPT¹`, `LPT²`, `LPT³`) as the very same devices. Matching is on the
/// ASCII-uppercased stem, the part before the first dot, so the case matters but
/// the extension does not.
///
/// A library has to stay usable when it is synced to a Windows machine, so
/// [`safe_component`] prefixes such a name with `_`. The Python prototype
/// mirrors this in `is_windows_reserved`; both are checked against the shared
/// rows in `tests/fixtures/reserved-names.json`.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = stem.strip_prefix(prefix) {
            let mut characters = suffix.chars();
            if let (Some(index), None) = (characters.next(), characters.next()) {
                return matches!(index, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}');
            }
        }
    }
    false
}

fn suffixed_component(base: &str, suffix: usize, max_bytes: usize) -> String {
    let suffix = format!("-{suffix}");
    let shortened = safe_component(base, max_bytes.saturating_sub(suffix.len()));
    safe_component(&format!("{shortened}{suffix}"), max_bytes)
}

/// Whether an Eagle item name is a placeholder that carries no information.
///
/// A name is generic when, after Unicode lowercasing, it is exactly `image`,
/// `download` or `untitled`; starts with `pasted image` or `screenshot`; is
/// `img` plus at most one `_` or `-` separator plus ASCII digits; or is at
/// least 16 ASCII hex digits (a hash).
///
/// Deliberately ASCII-only where the earlier implementations disagreed:
/// separators are exactly one `_` or `-` (`img__12` is a real name, not a
/// placeholder) and digits are ASCII (`img١٢` is a real name, not a
/// placeholder), and the match never tolerates a trailing newline. The Python
/// prototype mirrors this in `is_generic`; both are checked against the shared
/// rows in `tests/fixtures/generic-names.json`.
fn is_generic(name: &str) -> bool {
    let lower = name.to_lowercase();
    if matches!(lower.as_str(), "image" | "download" | "untitled") {
        return true;
    }
    if lower.starts_with("pasted image") || lower.starts_with("screenshot") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("img") {
        let digits = rest.strip_prefix(['_', '-']).unwrap_or(rest);
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return true;
        }
    }
    lower.len() >= 16 && lower.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Derive a generic `<site>-<id>` name from a source URL.
///
/// Only the URL path is considered (query and fragment are dropped), path
/// segments containing `=` are skipped, and remaining segments are split into
/// alphanumeric tokens. The first all-digit identifier (5-20 chars) wins,
/// otherwise the first mixed letter/digit identifier (6-24 chars). Tokens
/// longer than 24 characters are treated as hashes and ignored. Returns `None`
/// for non-http(s) URLs or when no identifier-shaped token is present.
///
/// An IP literal host yields no site label (see [`site_from_authority`]), so
/// such URLs also return `None` and the importer falls back to its time-based
/// generated name.
pub fn name_from_url(url: &str) -> Option<String> {
    let rest = strip_scheme(url)?;
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let site = site_from_authority(authority)?;
    let mut first_digits: Option<&str> = None;
    let mut first_mixed: Option<&str> = None;
    for segment in path.split('/') {
        if segment.is_empty() || segment.contains('=') {
            continue;
        }
        for token in segment.split(|c: char| !c.is_ascii_alphanumeric()) {
            if token.is_empty() || token.len() > 24 {
                continue;
            }
            if token.bytes().all(|b| b.is_ascii_digit()) {
                if (5..=20).contains(&token.len()) && first_digits.is_none() {
                    first_digits = Some(token);
                }
            } else if token.bytes().any(|b| b.is_ascii_alphabetic())
                && token.bytes().any(|b| b.is_ascii_digit())
                && (6..=24).contains(&token.len())
                && first_mixed.is_none()
            {
                first_mixed = Some(token);
            }
        }
    }
    let id = first_digits.or(first_mixed)?;
    Some(format!("{site}-{id}"))
}

fn strip_scheme(url: &str) -> Option<&str> {
    for scheme in ["https://", "http://"] {
        if let Some(prefix) = url.get(..scheme.len()) {
            if prefix.eq_ignore_ascii_case(scheme) {
                return Some(&url[scheme.len()..]);
            }
        }
    }
    None
}

/// Site label for the host part of a URL authority, or `None`.
///
/// Userinfo is dropped and any port is ignored, including the port that follows
/// a bracketed IPv6 literal. An IP literal host never produces a label: neither
/// a bracketed IPv6 literal (`[::1]:3000`) nor a dotted-quad IPv4 literal
/// (`192.168.1.5`), and neither a bare, unbracketed IPv6 literal such as
/// `2001:db8::1`, which cannot appear in a legal authority but is common in
/// pasted URLs. Callers treat `None` as "no site in this URL" and fall back to
/// the time-based generated name; they never substitute the host itself.
///
/// The Python prototype mirrors this in `_site_from_authority`.
fn site_from_authority(authority: &str) -> Option<String> {
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, ip_literal) = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((host, _port)) => (host, true),
            None => return None,
        },
        None => {
            if authority.matches(':').count() > 1 {
                return None;
            }
            let host = authority.split(':').next().unwrap_or(authority);
            (host, is_ipv4_literal(host))
        }
    };
    if ip_literal {
        return None;
    }
    site_label(&host.to_ascii_lowercase())
}

/// True for dotted-quad IPv4 literals such as `192.168.1.5`.
///
/// Deliberately lenient: four labels of one to three ASCII digits are enough,
/// so a malformed literal such as `999.999.999.999` also yields no site label.
/// The Python prototype mirrors this in `IPV4_LITERAL`.
fn is_ipv4_literal(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() == 4
        && labels.iter().all(|label| {
            (1..=3).contains(&label.len()) && label.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn site_label(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let suffix = match labels.len() {
        0 => return None,
        1 => 0,
        _ => {
            if matches!(
                format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]).as_str(),
                "co.uk" | "co.jp" | "com.cn" | "com.au"
            ) {
                2
            } else {
                1
            }
        }
    };
    let index = labels.len().checked_sub(suffix + 1)?;
    labels.get(index).map(|label| (*label).to_owned())
}

fn unique_name(
    home: &Path,
    name: &str,
    ext: &str,
    taken: &mut HashMap<PathBuf, HashSet<String>>,
) -> io::Result<String> {
    if !taken.contains_key(home) {
        let names = if home.is_dir() {
            fs::read_dir(home)?
                .map(|entry| entry.map(|e| e.file_name().to_string_lossy().to_lowercase()))
                .collect::<io::Result<HashSet<_>>>()?
        } else {
            HashSet::new()
        };
        taken.insert(home.to_path_buf(), names);
    }
    let names = taken.get_mut(home).expect("inserted above");
    let mut candidate = name.to_owned();
    let mut suffix = 1;
    while names.contains(&format!("{candidate}.{ext}").to_lowercase()) {
        suffix += 1;
        candidate = suffixed_component(name, suffix, 180);
    }
    names.insert(format!("{candidate}.{ext}").to_lowercase());
    Ok(format!("{candidate}.{ext}"))
}

/// Render a JSON value as a YAML scalar, escaping everything YAML forbids.
///
/// The value is serialized as JSON first — that keeps quoting, escaping and
/// non-string types consistent — and then every character YAML does not accept
/// is replaced by a `\uXXXX` escape, see [`escape_yaml_forbidden`].
fn yaml_quote(value: &Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    escape_yaml_forbidden(&json)
}

/// Escape every character YAML forbids, in a JSON-serialized value.
///
/// YAML printable characters are tab, LF, CR, U+0020..U+007E, U+0085,
/// U+00A0..U+D7FF, U+E000..U+FFFD and U+10000..U+10FFFF. Everything else has
/// to reach the reader as an escape inside the double-quoted scalar: the C0
/// controls (already escaped by the JSON serializer), DEL, the C1 control block
/// U+0080..U+009F, the non-characters U+FFFE and U+FFFF, and the line
/// separators U+2028 and U+2029, which also break JavaScript readers. An
/// escaped character is preserved exactly, so the reader sees the original
/// scalar.
///
/// Backslash sequences the JSON serializer produced are copied through
/// untouched, so their escapes are not escaped again. The Python prototype
/// mirrors this in `yaml_scalar`.
fn escape_yaml_forbidden(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    let mut characters = json.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            escaped.push('\\');
            if let Some(escape) = characters.next() {
                escaped.push(escape);
            }
        } else if yaml_printable(character) {
            escaped.push(character);
        } else {
            escaped.push_str(&format!("\\u{:04X}", character as u32));
        }
    }
    escaped
}

fn yaml_printable(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{0085}')
        || ('\u{0020}'..='\u{007e}').contains(&character)
        || ('\u{00a0}'..='\u{d7ff}').contains(&character)
        || ('\u{e000}'..='\u{fffd}').contains(&character)
        || ('\u{10000}'..='\u{10ffff}').contains(&character)
}

fn yaml_string(value: &str) -> String {
    yaml_quote(&Value::String(value.to_owned()))
}

/// Render an Eagle `btime` (epoch milliseconds) as ISO 8601 with the local
/// UTC offset, the same formatting `imported` uses. `None` when `btime` is
/// missing or invalid, so the note simply omits `added` (FORMAT §3.1).
pub(crate) fn btime_text(metadata: &Value) -> Option<String> {
    let instant = DateTime::from_timestamp_millis(btime_millis(metadata)?)?;
    let offset = *instant.with_timezone(&Local).offset();
    added_text(instant, offset)
}

/// The same value as [`btime_text`], rendered at a chosen UTC offset. Only the
/// tests need a fixed offset: the local zone is resolved once per process, so
/// a test cannot walk a matrix of zones inside one process.
#[cfg(test)]
pub(crate) fn btime_text_at(metadata: &Value, offset_seconds: i32) -> Option<String> {
    let instant = DateTime::from_timestamp_millis(btime_millis(metadata)?)?;
    added_text(instant, FixedOffset::east_opt(offset_seconds)?)
}

/// The `added:` value for an instant rendered at `offset`.
///
/// ISO 8601 has four-digit years, so a year outside 0..=9999 is not a value any
/// reader accepts: year 10000 formats, but as an unreadable five-digit year, so
/// the note would carry an `added` nothing can parse back. The range is decided
/// on the UTC instant, which is the same decision in every timezone, and the
/// rendering is checked as well, because a zone off UTC turns an instant inside
/// the range into year 10000 or year 0 on the clock it writes.
fn added_text(instant: DateTime<Utc>, offset: FixedOffset) -> Option<String> {
    if !(0..=9999).contains(&instant.year()) {
        return None;
    }
    let local = instant.with_timezone(&offset);
    if !(0..=9999).contains(&local.year()) {
        return None;
    }
    Some(local.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
}

/// Eagle's `btime` in epoch milliseconds. A JSON number is either an integer
/// or a float, and Eagle writes floats (`1689200000000.0`), so an integral
/// float is a valid btime. A fractional or non-numeric value is not.
fn btime_millis(metadata: &Value) -> Option<i64> {
    let value = metadata.get("btime")?;
    if let Some(millis) = value.as_i64() {
        return Some(millis);
    }
    let millis = value.as_f64()?;
    if millis.is_finite() && millis.fract() == 0.0 && millis.abs() <= i64::MAX as f64 {
        Some(millis as i64)
    } else {
        None
    }
}

fn make_note(
    metadata: &Value,
    library: &str,
    item: &str,
    paths: &[String],
    filename: &str,
    raw: &str,
    now: &str,
) -> String {
    let title = metadata
        .get("name")
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| Value::String(filename.to_owned()));
    let mut lines = vec![
        "---".to_owned(),
        format!("id: {}", Ulid::new()),
        format!("title: {}", yaml_quote(&title)),
    ];
    if let Some(tags) = metadata
        .get("tags")
        .filter(|v| v.as_array().is_some_and(|items| !items.is_empty()))
    {
        lines.push(format!("tags: {}", yaml_quote(tags)));
    }
    if let Some(star) = metadata.get("star").filter(|v| truthy(v)) {
        let rating = star
            .as_i64()
            .or_else(|| star.as_f64().map(|value| value as i64))
            .or_else(|| star.as_str().and_then(|value| value.parse().ok()))
            .unwrap_or(0);
        lines.push(format!("rating: {rating}"));
    }
    if let Some(url) = metadata.get("url").filter(|v| truthy(v)) {
        lines.push(format!("source: {}", yaml_quote(url)));
    }
    if let (Some(width), Some(height)) = (
        metadata.get("width").filter(|v| truthy(v)),
        metadata.get("height").filter(|v| truthy(v)),
    ) {
        lines.push(format!("width: {}", width));
        lines.push(format!("height: {}", height));
    }
    lines.push(format!("imported: {now}"));
    if let Some(added) = btime_text(metadata) {
        lines.push(format!("added: {added}"));
    }
    lines.extend([
        "sources:".to_owned(),
        "  - type: eagle".to_owned(),
        format!("    library: {}", yaml_string(library)),
        format!(
            "    item: {}",
            yaml_string(metadata.get("id").and_then(Value::as_str).unwrap_or(item))
        ),
        format!(
            "    folders: {}",
            yaml_quote(&Value::Array(
                paths.iter().cloned().map(Value::String).collect()
            ))
        ),
        format!("    imported: {now}"),
        "    importer: \"eagle-import prototype 0.2\"".to_owned(),
        format!("    raw: {}", yaml_string(raw)),
        "---".to_owned(),
        "".to_owned(),
    ]);
    let annotation = metadata
        .get("annotation")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let prefix = format!("{}\n", lines.join("\n"));
    if annotation.is_empty() {
        prefix
    } else {
        format!("{prefix}{annotation}\n\n")
    }
}

fn truthy(value: &Value) -> bool {
    !value.is_null()
        && value != &Value::Bool(false)
        && value != &Value::Number(0.into())
        && value != ""
}

fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn image_name_counts(root: &Path) -> io::Result<HashMap<String, usize>> {
    fn visit(path: &Path, counts: &mut HashMap<String, usize>) -> io::Result<()> {
        if !path.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                visit(&entry.path(), counts)?;
            } else if is_image(
                entry
                    .path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .as_str(),
            ) {
                *counts
                    .entry(name.to_string_lossy().to_lowercase())
                    .or_default() += 1;
            }
        }
        Ok(())
    }
    let mut counts = HashMap::new();
    visit(root, &mut counts)?;
    Ok(counts)
}

struct Note {
    path: PathBuf,
    frontmatter_and_body: String,
    filename: String,
    rel: String,
}

fn write_collection_and_report(
    root: &Path,
    source: &Path,
    library: &str,
    now: &DateTime<Local>,
    report: &ImportReport,
    members: &BTreeMap<String, Vec<String>>,
) -> io::Result<()> {
    let collection_dir = root.join("Eagle").join(library);
    create_confined_dirs(root, &collection_dir)?;
    let mut collection = vec![
        "---".to_owned(),
        format!("kind: collection"),
        format!(
            "title: {}",
            yaml_string(&format!("{library} (Eagle import)"))
        ),
        "cssclasses: [dimagine-gallery]".to_owned(),
        "---".to_owned(),
        "".to_owned(),
        format!(
            "All images imported from the Eagle library {library}, grouped by their Eagle folder."
        ),
        "".to_owned(),
    ];
    let mut ordered_members = members.iter().collect::<Vec<_>>();
    ordered_members.sort_by_key(|(folder, _)| (folder.starts_with('('), (*folder).clone()));
    for (folder, items) in ordered_members {
        collection.push(format!("## {folder} ({})", items.len()));
        collection.push("".to_owned());
        for item in items {
            collection.push(format!("![[{item}]]"));
        }
        collection.push("".to_owned());
    }
    write_atomically(
        &collection_dir.join(format!("{library}.md")),
        collection.join("\n").as_bytes(),
    )?;

    let date = now.format("%Y%m%d");
    let mut lines = vec![
        "---".to_owned(),
        format!(
            "title: {}",
            yaml_string(&format!("Import report {library}"))
        ),
        format!("imported: {}", now.format("%Y-%m-%dT%H:%M:%S%:z")),
        "---".to_owned(),
        "".to_owned(),
        format!("# Import report: Eagle → {library}"),
        "".to_owned(),
        format!(
            "- Source: Eagle library `{}` (read only, not modified)",
            source.file_name().unwrap_or_default().to_string_lossy()
        ),
        format!("- Imported: {}", report.imported.len()),
        format!("- Skipped: {}", report.skipped.len()),
        format!("- Renamed (no meaningful name): {}", report.renamed),
        format!(
            "- Folder references that pointed to deleted Eagle folders: {}",
            report.dangling_folder_refs
        ),
        "".to_owned(),
        "| Eagle item | Reason skipped |".to_owned(),
        "|---|---|".to_owned(),
    ];
    for skipped in &report.skipped {
        lines.push(format!("| {} | {} |", skipped.item, skipped.reason));
    }
    write_atomically(
        &collection_dir.join(format!("_import-{library}-{date}.md")),
        format!("{}\n", lines.join("\n")).as_bytes(),
    )
}

fn write_absent_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() {
            return Ok(());
        }
    }
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    file.write_all(bytes)
}

fn write_obsidian_gallery(root: &Path) -> io::Result<()> {
    let snippets = root.join(".obsidian").join("snippets");
    create_confined_dirs(root, &snippets)?;
    write_absent_file(
        &snippets.join("dimagine-gallery.css"),
        b".dimagine-gallery .image-embed { display:inline-block; width:24%; margin:0.4%; vertical-align:top; }\n.dimagine-gallery .image-embed img { width:100%; height:auto; border-radius:4px; }\n",
    )?;
    write_absent_file(
        &root.join(".obsidian").join("appearance.json"),
        b"{\"enabledCssSnippets\": [\"dimagine-gallery\"]}",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        btime_text_at, copy_item_transactionally, is_generic, is_windows_reserved,
        write_atomically, CopyItemError,
    };
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// RW26 L-8: the write is now fsynced before the rename, so the exact bytes
    /// still land and no temporary file is left behind.
    #[test]
    fn atomic_note_write_publishes_the_exact_bytes_and_no_temporary() {
        let temp = TempDir::new().unwrap();
        let note = temp.path().join("girl-underwater.jpg.md");
        let bytes = b"---\ntitle: Girl\n---\n\nBody.\n\n![[girl-underwater.jpg]]\n";
        write_atomically(&note, bytes).unwrap();
        assert_eq!(fs::read(&note).unwrap(), bytes);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);

        // Overwriting an existing note replaces it whole.
        write_atomically(&note, b"second\n").unwrap();
        assert_eq!(fs::read(&note).unwrap(), b"second\n");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    /// A failed write reports the error and leaves nothing behind, so a
    /// library never collects `.dimagine-tmp-` litter from a failed import.
    #[test]
    fn atomic_note_write_reports_failure_without_litter() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("absent/note.md");
        assert!(write_atomically(&missing, b"content").is_err());
        assert!(!missing.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    fn shared_rows(fixture: &str) -> Vec<(String, bool)> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        let bytes =
            fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let rows: Value = serde_json::from_slice(&bytes).expect("fixture is JSON");
        let rows = rows.as_array().expect("fixture is a JSON array");
        assert!(rows.len() >= 40, "expected at least 40 rows in {fixture}");
        rows.iter()
            .map(|row| {
                let name = row["name"].as_str().expect("name string").to_owned();
                let expected = row["generic"]
                    .as_bool()
                    .or_else(|| row["reserved"].as_bool())
                    .expect("generic or reserved boolean");
                (name, expected)
            })
            .collect()
    }

    #[test]
    fn generic_name_gate_matches_shared_fixture() {
        for (name, expected) in shared_rows("generic-names.json") {
            assert_eq!(is_generic(&name), expected, "name: {name:?}");
        }
    }

    #[test]
    fn windows_reserved_name_gate_matches_shared_fixture() {
        for (name, expected) in shared_rows("reserved-names.json") {
            assert_eq!(is_windows_reserved(&name), expected, "name: {name:?}");
        }
    }

    /// The offsets the two importers have to agree on, with the zones whose
    /// local time they are: UTC, both offsets `America/New_York` uses,
    /// `Asia/Kolkata`'s half hour, `America/Santiago` and the two offsets
    /// `Pacific/Chatham` uses, plus the last zone west of UTC.
    const OFFSET_MATRIX: [(&str, i32); 7] = [
        ("UTC", 0),
        ("America/New_York in winter", -5 * 3600),
        ("America/New_York in summer", -4 * 3600),
        ("America/Santiago in summer", -4 * 3600),
        ("Pacific/Midway", -11 * 3600),
        ("Asia/Kolkata", 5 * 3600 + 1800),
        ("Pacific/Chatham in summer", 13 * 3600 + 2700),
    ];

    /// The instant a written `added:` names, in UTC, so one fixture row is
    /// compared the same way from every offset. Mirrors Python `utc_of`.
    fn utc_of(text: &str) -> String {
        let moment = DateTime::parse_from_rfc3339(text)
            .expect("a written value is RFC 3339")
            .with_timezone(&Utc);
        moment.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// RW26 M-3: the same fixture rows the Python prototype's `--selftest`
    /// reads, rendered at every offset in the matrix instead of at whatever
    /// zone the machine happens to be in, so the check cannot depend on the
    /// timezone it runs under.
    #[test]
    fn added_matches_the_python_prototype_at_every_offset() {
        let rows: Value = serde_json::from_slice(
            &fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/added-btime.json"),
            )
            .unwrap(),
        )
        .expect("fixture is JSON");
        for row in rows.as_array().expect("fixture is an array") {
            let note = row["note"].as_str().unwrap_or("fixture row");
            let expected = row["utc"].as_str();
            for (zone, offset) in OFFSET_MATRIX {
                let written = btime_text_at(&row["metadata"], offset).map(|text| utc_of(&text));
                // The last representable instant is the one row an offset can
                // change the answer for: rendered east of UTC it is year 10000,
                // which is not ISO 8601, so both importers omit it there.
                let expected_here = match (expected, offset) {
                    (Some(instant), offset) if offset > 0 && instant.starts_with("9999-") => None,
                    (expected, _) => expected,
                };
                assert_eq!(written.as_deref(), expected_here, "{note} at {zone}");
            }
        }
    }

    /// The row the boundary rule exists for: `10000-01-01T00:00:00Z` is outside
    /// the four-digit years in every timezone, and a zone west of UTC used to
    /// write the local year 9999 of an instant no reader can name.
    #[test]
    fn the_year_10000_btime_is_omitted_at_every_offset() {
        let metadata: Value = serde_json::from_str(r#"{"btime":253402300800000}"#).unwrap();
        for (zone, offset) in OFFSET_MATRIX {
            assert_eq!(
                btime_text_at(&metadata, offset),
                None,
                "year 10000 at {zone}"
            );
        }
        // The instant just inside the range is still written where it fits.
        let last: Value = serde_json::from_str(r#"{"btime":253402300799000}"#).unwrap();
        assert_eq!(
            btime_text_at(&last, 0).as_deref(),
            Some("9999-12-31T23:59:59+00:00")
        );
        assert_eq!(btime_text_at(&last, 5 * 3600 + 1800), None);
    }

    #[test]
    fn failed_item_copy_leaves_no_published_or_temporary_artifacts() {
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("source-directory");
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("child"), b"not a regular image").unwrap();
        let image = temp.path().join("photo.png");
        assert!(copy_item_transactionally(&source_dir, &image, b"raw metadata").is_err());
        assert!(!image.exists());
        assert!(!temp.path().join("photo.png.eagle.json").exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_item_copy_rolls_back_published_image_when_companion_rename_fails() {
        let temp = TempDir::new().unwrap();
        let source_file = temp.path().join("source.png");
        fs::write(&source_file, b"content").unwrap();
        let image = temp.path().join("photo.png");
        let raw_path = temp.path().join("photo.png.eagle.json");
        fs::create_dir(&raw_path).unwrap();
        fs::write(raw_path.join("blocker"), b"blocker").unwrap();

        let err = copy_item_transactionally(&source_file, &image, b"raw").unwrap_err();
        match err {
            CopyItemError::Skipped(_) => {}
            CopyItemError::CleanupFailed { .. } => {
                panic!("expected clean rollback (Skipped), got CleanupFailed");
            }
        }
        assert!(
            !image.exists(),
            "published image must be rolled back on companion failure"
        );
    }

    #[test]
    fn iterative_drop_handles_deep_nesting_without_stack_overflow() {
        use super::drop_value_iteratively;

        let mut val = Value::Null;
        for _ in 0..10_000 {
            val = Value::Array(vec![val]);
        }
        drop_value_iteratively(val);
    }
}
