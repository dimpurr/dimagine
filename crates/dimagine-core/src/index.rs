//! Sync the rebuildable SQLite index from a walked library.
//!
//! `dimagine scan` refreshes the index (HLD: "walk the library, refresh the
//! index, print a summary"). This module writes one scan's worth of file,
//! note, body and link rows into an open [`Index`]. It is the only writer;
//! the library files themselves are never touched.
//!
//! `added_ns` is derived per image: the paired note's `added` property, else
//! its `imported`. The index adds the last link of the chain FORMAT §3.1
//! describes for "newest added" — `first_seen_ns` — itself, so the scan does
//! not read that column back and cannot disagree with it.

use std::collections::HashMap;
use std::fs;
use std::time::UNIX_EPOCH;

use dimagine_index::{FileKind, FileRecord, Index, IndexError, LinkRecord, LinkState, NoteRecord};
use saphyr::{LoadableYamlNode, Yaml};
use serde_json::Value;

use crate::library::{FileClass, FileEntry, Library};
use crate::links::{extract_markdown_links, LinkSyntax, Outcome, Resolver};
use crate::note::{front_matter_text, parse_note, IdProperty};

/// Everything the index needs about one note, parsed once.
struct NoteData {
    props_json: String,
    title: String,
    tags: Vec<String>,
    id: Option<String>,
    body: String,
    links: Vec<LinkRecord>,
    /// The note's own "added" time (FORMAT §3.1); `None` when the note has
    /// neither `added` nor `imported`, which the index answers with
    /// `first_seen_ns` instead.
    note_added_ns: Option<i64>,
    /// The note's `rating`, read with the index's own rule so a scan and a
    /// query never disagree about what counts as one.
    rating: Option<u8>,
}

/// Walk `library` and write this scan's rows into `index`.
///
/// An active scan is required (the caller opens the index and calls
/// [`Index::begin_scan`] before this). On success the scan is finished with
/// the library's paths as seen; on error the scan is aborted so a failed sync
/// can never prune the index.
pub fn sync_index(library: &Library, index: &mut Index) -> Result<(), IndexError> {
    index.begin_scan()?;
    let result = sync_index_inner(library, index);
    match result {
        Ok(()) => {
            let seen: Vec<String> = library
                .files
                .iter()
                .map(|entry| entry.rel.clone())
                .collect();
            index.finish_scan(&seen)
        }
        Err(error) => {
            index.abort_scan();
            Err(error)
        }
    }
}

fn sync_index_inner(library: &Library, index: &mut Index) -> Result<(), IndexError> {
    let resolver = Resolver::new(&library.files);
    let mut notes: HashMap<String, NoteData> = HashMap::new();
    let mut added_by_image: HashMap<String, i64> = HashMap::new();
    let mut rating_by_image: HashMap<String, u8> = HashMap::new();
    for entry in &library.files {
        if !matches!(entry.class, FileClass::ImageNote | FileClass::Note) {
            continue;
        }
        let data = parse_note_entry(library, entry, &resolver, &library.files)?;
        if entry.class == FileClass::ImageNote {
            if let Some(image_rel) = entry.paired_image_rel() {
                if let Some(note_added_ns) = data.note_added_ns {
                    added_by_image.insert(image_rel.to_owned(), note_added_ns);
                }
                if let Some(rating) = data.rating {
                    rating_by_image.insert(image_rel.to_owned(), rating);
                }
            }
        }
        notes.insert(entry.rel.clone(), data);
    }
    for entry in &library.files {
        match entry.class {
            FileClass::Image => {
                let (size, mtime_ns) = file_facts(library, entry);
                index.upsert_file(&FileRecord {
                    path: entry.rel.clone(),
                    size,
                    mtime_ns,
                    sha256: None,
                    kind: FileKind::Image,
                    note_added_ns: added_by_image.get(entry.rel.as_str()).copied(),
                    rating: rating_by_image.get(entry.rel.as_str()).copied(),
                })?;
            }
            FileClass::ImageNote | FileClass::Note => {
                let data = notes
                    .get(entry.rel.as_str())
                    .expect("note parsed in the first pass");
                let image_path = if entry.class == FileClass::ImageNote {
                    entry.paired_image_rel().map(str::to_owned)
                } else {
                    None
                };
                index.upsert_note(&NoteRecord {
                    path: entry.rel.clone(),
                    image_path,
                    id: data.id.clone(),
                    title: data.title.clone(),
                    tags: data.tags.clone(),
                    props_json: data.props_json.clone(),
                })?;
                index.set_note_body(&entry.rel, &data.body)?;
                index.replace_links(&entry.rel, &data.links)?;
            }
            FileClass::Raw => {
                let (size, mtime_ns) = file_facts(library, entry);
                index.upsert_file(&FileRecord {
                    path: entry.rel.clone(),
                    size,
                    mtime_ns,
                    sha256: None,
                    kind: FileKind::Raw,
                    note_added_ns: None,
                    rating: None,
                })?;
            }
            FileClass::Canvas => {
                let (size, mtime_ns) = file_facts(library, entry);
                index.upsert_file(&FileRecord {
                    path: entry.rel.clone(),
                    size,
                    mtime_ns,
                    sha256: None,
                    kind: FileKind::Canvas,
                    note_added_ns: None,
                    rating: None,
                })?;
            }
            FileClass::Other => {
                let (size, mtime_ns) = file_facts(library, entry);
                index.upsert_file(&FileRecord {
                    path: entry.rel.clone(),
                    size,
                    mtime_ns,
                    sha256: None,
                    kind: FileKind::Other,
                    note_added_ns: None,
                    rating: None,
                })?;
            }
            FileClass::Special => {}
        }
    }
    Ok(())
}

fn parse_note_entry(
    library: &Library,
    entry: &FileEntry,
    resolver: &Resolver,
    files: &[FileEntry],
) -> Result<NoteData, IndexError> {
    let path = library.root.join(&entry.path);
    let text = fs::read_to_string(&path)?;
    let parsed = parse_note(&text);
    let props_json = front_matter_json(&text);
    let props: Value = serde_json::from_str(&props_json).unwrap_or(Value::Null);
    let title = props
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tags = props
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let id = match &parsed.id {
        Some(IdProperty::Text(id)) => Some(id.clone()),
        _ => None,
    };
    // FORMAT §3.1: "newest added" sorts by `added`, else `imported`. A value
    // that does not parse falls through to the next link of the chain instead
    // of ending it, so a corrupt `added` never hides a good `imported`.
    let note_added_ns = ["added", "imported"]
        .iter()
        .filter_map(|key| props.get(*key).and_then(Value::as_str))
        .find_map(parse_iso8601_ns);
    let rating = dimagine_index::note_rating(&props);
    let links = resolve_links(&parsed.body, parsed.body_line, entry, resolver, files);
    Ok(NoteData {
        props_json,
        title,
        tags,
        id,
        body: parsed.body,
        links,
        note_added_ns,
        rating,
    })
}

/// Serialize the note's front matter as a JSON object so the view queries can
/// read `title`, `rating`, `tags`, `kind` and the datetimes without a YAML
/// parser. Notes without a usable front matter store `{}`.
fn front_matter_json(text: &str) -> String {
    let Some(yaml) = front_matter_text(text) else {
        return "{}".to_owned();
    };
    let docs = match Yaml::load_from_str(yaml) {
        Ok(docs) => docs,
        Err(_) => return "{}".to_owned(),
    };
    let Some(doc) = docs.first() else {
        return "{}".to_owned();
    };
    if !doc.is_mapping() {
        return "{}".to_owned();
    }
    yaml_to_json(doc)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "{}".to_owned())
}

/// Resolve a note's links into the index's link records. Only image links
/// are stored: the view queries use the links table for collection
/// membership, which is always an image embed.
fn resolve_links(
    body: &str,
    body_line: usize,
    entry: &FileEntry,
    resolver: &Resolver,
    files: &[FileEntry],
) -> Vec<LinkRecord> {
    extract_markdown_links(body, body_line)
        .into_iter()
        .filter_map(|link| {
            let outcome = resolver.resolve(&link.target, entry, link.syntax);
            let (target, state) = match outcome {
                Outcome::Resolved(index) => (
                    files.get(index).map(|target| target.rel.clone()),
                    LinkState::Resolved,
                ),
                Outcome::Ambiguous(_) => (None, LinkState::Ambiguous),
                Outcome::NotFound => (None, LinkState::Missing),
                Outcome::NotImageTarget => return None,
            };
            Some(LinkRecord {
                src: entry.rel.clone(),
                raw: link.raw,
                target,
                state,
                syntax: link.syntax.into(),
            })
        })
        .collect()
}

/// The index stores the syntax a link was written with, because only a strong
/// image embed makes a note a collection (FORMAT §5).
impl From<LinkSyntax> for dimagine_index::LinkSyntax {
    fn from(syntax: LinkSyntax) -> Self {
        match syntax {
            LinkSyntax::WikiEmbed => Self::WikiEmbed,
            LinkSyntax::WikiLink => Self::WikiLink,
            LinkSyntax::MarkdownImage => Self::MarkdownImage,
            LinkSyntax::CanvasFileNode => Self::CanvasFileNode,
        }
    }
}

/// Size and nanosecond mtime for a regular file, or zeros when unavailable.
fn file_facts(library: &Library, entry: &FileEntry) -> (u64, i64) {
    let Ok(metadata) = fs::symlink_metadata(library.root.join(&entry.path)) else {
        return (0, 0);
    };
    let mtime_ns = metadata
        .modified()
        .map(|modified| match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos() as i64,
            Err(error) => -(error.duration().as_nanos() as i64),
        })
        .unwrap_or(0);
    (metadata.len(), mtime_ns)
}

/// Parse an ISO 8601 datetime with a UTC offset into ns since the Unix epoch.
/// Returns `None` for anything else, so the caller falls back to the next
/// link in the chain.
fn parse_iso8601_ns(text: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    Some(parsed.timestamp() * 1_000_000_000 + i64::from(parsed.timestamp_subsec_nanos()))
}

/// Convert a parsed saphyr node into a JSON value, preserving the front
/// matter's property order and unknown keys.
fn yaml_to_json(yaml: &Yaml) -> Option<Value> {
    match yaml {
        Yaml::Value(scalar) => Some(scalar_to_json(scalar)),
        Yaml::Representation(text, _, _) => Some(Value::String(text.to_string())),
        Yaml::Sequence(sequence) => Some(Value::Array(
            sequence.iter().filter_map(yaml_to_json).collect(),
        )),
        Yaml::Mapping(mapping) => {
            let mut object = serde_json::Map::new();
            for (key, value) in mapping {
                let key = match key {
                    Yaml::Value(saphyr::Scalar::String(text)) => text.to_string(),
                    Yaml::Representation(text, _, _) => text.to_string(),
                    _ => continue,
                };
                if let Some(value) = yaml_to_json(value) {
                    object.insert(key, value);
                }
            }
            Some(Value::Object(object))
        }
        Yaml::Tagged(_, inner) => yaml_to_json(inner),
        Yaml::Alias(_) | Yaml::BadValue => None,
    }
}

fn scalar_to_json(scalar: &saphyr::Scalar) -> Value {
    match scalar {
        saphyr::Scalar::Null => Value::Null,
        saphyr::Scalar::Boolean(value) => Value::Bool(*value),
        saphyr::Scalar::Integer(value) => Value::Number((*value).into()),
        saphyr::Scalar::FloatingPoint(value) => {
            serde_json::Number::from_f64(value.0).map_or(Value::Null, Value::Number)
        }
        saphyr::Scalar::String(text) => Value::String(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso8601_ns_handles_offsets_and_z() {
        assert_eq!(
            parse_iso8601_ns("2023-07-12T20:54:07+01:00"),
            Some(1_689_191_647_000_000_000)
        );
        assert_eq!(
            parse_iso8601_ns("2026-10-04T14:30:12+01:00"),
            Some(1_791_120_612_000_000_000)
        );
        assert_eq!(
            parse_iso8601_ns("2023-07-12T19:54:07Z"),
            Some(1_689_191_647_000_000_000)
        );
        assert_eq!(parse_iso8601_ns("2021-10-15"), None);
        assert_eq!(parse_iso8601_ns("not a date"), None);
    }

    #[test]
    fn yaml_to_json_preserves_unknown_keys_and_order() {
        let yaml = Yaml::load_from_str(
            "title: Girl\nrating: 4\nunknown: [a, b]\ntags: [underwater, sketch]\n",
        )
        .unwrap();
        let doc = yaml.into_iter().next().unwrap();
        let json = yaml_to_json(&doc).unwrap();
        assert_eq!(json["title"], "Girl");
        assert_eq!(json["rating"], 4);
        assert_eq!(json["unknown"], serde_json::json!(["a", "b"]));
        assert_eq!(json["tags"], serde_json::json!(["underwater", "sketch"]));
    }

    #[test]
    fn yaml_to_json_handles_non_mapping_front_matter() {
        let yaml = Yaml::load_from_str("- just\n- a list\n").unwrap();
        let doc = yaml.into_iter().next().unwrap();
        assert_eq!(
            yaml_to_json(&doc),
            Some(serde_json::json!(["just", "a list"]))
        );
    }

    #[test]
    fn front_matter_json_round_trips_properties() {
        let text = "---\ntitle: Girl\nrating: 4\nunknown: keep\n---\n\nBody.\n";
        let json = front_matter_json(text);
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["title"], "Girl");
        assert_eq!(value["rating"], 4);
        assert_eq!(value["unknown"], "keep");
        assert_eq!(front_matter_json("no front matter"), "{}");
        assert_eq!(front_matter_json("---\n---\n"), "{}");
    }

    #[test]
    fn front_matter_json_tolerates_invalid_yaml() {
        assert_eq!(front_matter_json("---\nnot: [valid\n---\n"), "{}");
    }

    #[test]
    fn sync_requires_an_existing_library() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        let library = Library::open(&missing);
        assert!(library.is_err());
    }

    /// RW26 M-1: `["added", "imported"].find_map(as_str).and_then(parse)`
    /// stopped the chain at the first key that was a string, so a corrupt
    /// `added` swallowed the whole chain and the image sorted as brand new.
    #[test]
    fn an_unparseable_added_falls_through_to_imported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("refs")).unwrap();
        for (name, added) in [
            ("a.png", "added: yesterday"),
            ("b.png", "added: 2023-13-45T99:99:99+01:00"),
        ] {
            std::fs::write(root.join(format!("refs/{name}")), b"png").unwrap();
            std::fs::write(
                root.join(format!("refs/{name}.md")),
                format!(
                    "---\ntitle: Bad\n{added}\nimported: 2020-01-01T00:00:00+00:00\n---\n\nBody.\n\n\
                     ![[{name}]]\n"
                ),
            )
            .unwrap();
        }
        let library = Library::open(root).unwrap();
        let mut index = Index::open(root).unwrap();
        sync_index(&library, &mut index).unwrap();
        let page = index
            .view(&dimagine_index::ViewQuery {
                sort: dimagine_index::SortKey::Added,
                ..dimagine_index::ViewQuery::default()
            })
            .unwrap();
        let expected = parse_iso8601_ns("2020-01-01T00:00:00+00:00").unwrap();
        assert_eq!(page.total, 2);
        for item in &page.items {
            assert_eq!(
                item.added_ns, expected,
                "{} falls back to imported, not to first_seen",
                item.path
            );
        }
    }

    #[test]
    fn file_facts_reads_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("a.jpg"), b"12345").unwrap();
        let library = Library::open(root).unwrap();
        let entry = library
            .files
            .iter()
            .find(|entry| entry.rel == "a.jpg")
            .expect("a.jpg walked");
        let (size, mtime_ns) = file_facts(&library, entry);
        assert_eq!(size, 5);
        assert!(mtime_ns > 0);
    }
}
