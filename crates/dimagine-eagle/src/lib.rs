//! Read-only Eagle library importer for dimagine libraries.
//!
//! The source library is never modified. Imported images retain their original
//! bytes and raw Eagle metadata; generated notes follow the dimagine format.

use chrono::{DateTime, Local};
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
            name = format!(
                "{}-{}",
                now.format("%Y%m%d-%H%M%S"),
                &Ulid::new().to_string().to_lowercase()[22..]
            );
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

fn copy_item_transactionally(
    source: &Path,
    image: &Path,
    raw: &[u8],
) -> Result<(), CopyItemError> {
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

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().expect("output file has parent");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".dimagine-tmp-{nonce}.note"));
    let result = (|| {
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, path)
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
    let stem = result.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        result.insert(0, '_');
    }
    result
}

fn suffixed_component(base: &str, suffix: usize, max_bytes: usize) -> String {
    let suffix = format!("-{suffix}");
    let shortened = safe_component(base, max_bytes.saturating_sub(suffix.len()));
    safe_component(&format!("{shortened}{suffix}"), max_bytes)
}

fn is_generic(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "image"
        || lower == "download"
        || lower == "untitled"
        || lower.starts_with("pasted image")
        || lower.starts_with("screenshot")
        || {
            let digits = lower
                .strip_prefix("img")
                .unwrap_or("")
                .trim_start_matches(['_', '-']);
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        }
        || (lower.len() >= 16 && lower.chars().all(|c| c.is_ascii_hexdigit()))
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

fn yaml_quote(value: &Value) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    json.replace('\u{007f}', "\\u007F")
        .replace('\u{0085}', "\\u0085")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}
fn yaml_string(value: &str) -> String {
    yaml_quote(&Value::String(value.to_owned()))
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
    lines.extend([
        format!("imported: {now}"),
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
    use super::{copy_item_transactionally, CopyItemError};
    use std::fs;
    use tempfile::TempDir;

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
        assert!(!image.exists(), "published image must be rolled back on companion failure");
    }
}
