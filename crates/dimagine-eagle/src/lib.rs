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
use std::path::{Path, PathBuf};
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
    /// Destination has visible content and is therefore not empty.
    DestinationNotEmpty(PathBuf),
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
/// The source is read only. The destination may contain hidden entries, matching
/// the prototype's empty-target rule. The returned report records imported paths,
/// stable skip codes, generic-name changes, dangling folder references, and
/// per-folder counts.
pub fn import(
    src_library: &Path,
    dst_library: &Path,
    opts: ImportOptions,
) -> Result<ImportReport, ImportError> {
    let source_meta = src_library.join("metadata.json");
    if !source_meta.exists() {
        return Err(ImportError::InvalidSource(format!(
            "not an Eagle library (no metadata.json): {}",
            src_library.display()
        )));
    }
    if dst_library.exists()
        && fs::read_dir(dst_library)?
            .filter_map(Result::ok)
            .any(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
    {
        return Err(ImportError::DestinationNotEmpty(dst_library.to_path_buf()));
    }

    let root: Value = serde_json::from_slice(&fs::read(&source_meta)?).map_err(|error| {
        ImportError::InvalidSource(format!("unreadable metadata.json: {error}"))
    })?;
    let default_name = src_library
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let library = opts.name.unwrap_or_else(|| {
        default_name
            .strip_suffix(".library")
            .unwrap_or(&default_name)
            .to_owned()
    });
    let mut folder_paths = HashMap::new();
    if let Some(folders) = root.get("folders").and_then(Value::as_array) {
        walk_folders(folders, "", &mut folder_paths);
    }

    let now: DateTime<Local> = Local::now();
    let now_text = now.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let mut report = ImportReport::default();
    let mut notes = Vec::new();
    let mut collection_members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut taken: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut item_dirs = sorted_info_dirs(&src_library.join("images"))?;

    for item_dir in item_dirs.drain(..) {
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
        let expected = item_dir.join(format!(
            "{original_name}.{}",
            metadata.get("ext").and_then(Value::as_str).unwrap_or("")
        ));
        let original = if expected.exists() {
            expected
        } else {
            match fallback_original(&item_dir)? {
                Some(path) => path,
                None => {
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
        fs::create_dir_all(&home)?;
        let mut name = clean(original_name);
        if name.is_empty() || is_generic(&name) {
            name = format!(
                "{}-{}",
                now.format("%Y%m%d-%H%M%S"),
                &Ulid::new().to_string().to_lowercase()[22..]
            );
            report.renamed += 1;
        }
        let filename = unique_name(&home, &name, &ext, &mut taken)?;
        let destination_image = home.join(&filename);
        if let Err(error) = fs::copy(&original, &destination_image) {
            report.skipped.push(skip(
                item,
                SkipReasonCode::CopyFailed,
                format!("copy failed (not downloaded?): {error}"),
            ));
            continue;
        }
        let raw_filename = format!("{filename}.eagle.json");
        fs::write(home.join(&raw_filename), &raw)?;
        let rel = relative_string(dst_library, &destination_image);
        let note = make_note(
            &metadata,
            &library,
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

    let name_counts = image_name_counts(dst_library)?;
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
        fs::write(
            note.path,
            format!("{}![[{}]]\n", note.frontmatter_and_body, embed),
        )?;
    }

    write_collection_and_report(
        dst_library,
        src_library,
        &library,
        &now,
        &report,
        &collection_members,
    )?;
    write_obsidian_gallery(dst_library)?;
    Ok(report)
}

fn skip(item: String, reason_code: SkipReasonCode, reason: impl Into<String>) -> SkippedItem {
    SkippedItem {
        item,
        reason_code,
        reason: reason.into(),
    }
}

fn walk_folders(folders: &[Value], parent: &str, output: &mut HashMap<String, String>) {
    for folder in folders {
        let Some(id) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = folder.get("name").and_then(Value::as_str).unwrap_or("");
        let segment = clean(name);
        let segment = if segment.is_empty() {
            id.to_owned()
        } else {
            segment
        };
        let path = if parent.is_empty() {
            segment
        } else {
            format!("{parent}/{segment}")
        };
        output.insert(id.to_owned(), path.clone());
        if let Some(children) = folder.get("children").and_then(Value::as_array) {
            walk_folders(children, &path, output);
        }
    }
}

fn sorted_info_dirs(images: &Path) -> io::Result<Vec<PathBuf>> {
    if !images.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(images)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "info") && path.is_dir())
        .collect())
}

fn fallback_original(item_dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut files = fs::read_dir(item_dir)?.collect::<Result<Vec<_>, _>>()?;
    files.sort_by_key(|entry| entry.file_name());
    Ok(files.into_iter().map(|entry| entry.path()).find(|path| {
        path.file_name().is_some_and(|name| {
            name != "metadata.json" && !name.to_string_lossy().ends_with("_thumbnail.png")
        })
    }))
}

fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "tif" | "tiff" | "bmp"
    )
}

fn clean(input: &str) -> String {
    let normalized: String = input.nfc().collect();
    let replaced: String = normalized
        .chars()
        .map(|c| {
            if matches!(
                c,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '[' | ']' | '#' | '^'
            ) || (c as u32) < 32
            {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim().trim_matches('.');
    trimmed.chars().take(120).collect()
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
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_lowercase())
                .collect()
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
        candidate = format!("{name}-{suffix}");
    }
    names.insert(format!("{candidate}.{ext}").to_lowercase());
    Ok(format!("{candidate}.{ext}"))
}

fn json_quote(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
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
        format!("title: {}", json_quote(&title)),
    ];
    if let Some(tags) = metadata.get("tags").filter(|v| truthy(v)) {
        lines.push(format!("tags: {}", json_quote(tags)));
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
        lines.push(format!("source: {}", json_quote(url)));
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
        format!("    library: {}", json_string(library)),
        format!(
            "    item: {}",
            json_string(metadata.get("id").and_then(Value::as_str).unwrap_or(item))
        ),
        format!(
            "    folders: {}",
            json_quote(&Value::Array(
                paths.iter().cloned().map(Value::String).collect()
            ))
        ),
        format!("    imported: {now}"),
        "    importer: \"eagle-import prototype 0.2\"".to_owned(),
        format!("    raw: {}", json_string(raw)),
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
    fs::create_dir_all(&collection_dir)?;
    let mut collection = vec![
        "---".to_owned(),
        format!("kind: collection"),
        format!(
            "title: {}",
            json_string(&format!("{library} (Eagle import)"))
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
    fs::write(
        collection_dir.join(format!("{library}.md")),
        collection.join("\n"),
    )?;

    let date = now.format("%Y%m%d");
    let mut lines = vec![
        "---".to_owned(),
        format!(
            "title: {}",
            json_string(&format!("Import report {library}"))
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
    fs::write(
        collection_dir.join(format!("_import-{library}-{date}.md")),
        format!("{}\n", lines.join("\n")),
    )
}

fn write_obsidian_gallery(root: &Path) -> io::Result<()> {
    let snippets = root.join(".obsidian").join("snippets");
    fs::create_dir_all(&snippets)?;
    fs::write(snippets.join("dimagine-gallery.css"), ".dimagine-gallery .image-embed { display:inline-block; width:24%; margin:0.4%; vertical-align:top; }\n.dimagine-gallery .image-embed img { width:100%; height:auto; border-radius:4px; }\n")?;
    let appearance = root.join(".obsidian").join("appearance.json");
    if !appearance.exists() {
        fs::write(
            appearance,
            "{\"enabledCssSnippets\": [\"dimagine-gallery\"]}",
        )?;
    }
    Ok(())
}
