//! Read-only tag transformations and library-wide batch planning.
//! A plan is not permission to write: a writer must revalidate it under lock.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::library::{open_regular, FileClass, Library};
use crate::tag_aliases::{TagAliases, ALIAS_PATH};

use crate::fm_edit::{add_tags, editable_tags, remove_exact_tags, FmEditError};
use crate::tag_aliases::{fold, valid_source, valid_target};

/// A tag transformation. Sources are exact stored spellings, including legacy
/// names with spaces. Moves include descendants at a slash boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum TagOperation {
    Rename {
        source: String,
        target: String,
    },
    Merge {
        sources: Vec<String>,
        target: String,
    },
    Move {
        source: String,
        target: String,
    },
    Remove {
        source: String,
    },
    /// Remove a tag only when a slash-delimited descendant is on the same note.
    RemoveRedundantParent,
}

/// The transformed bytes and the number of additions already represented by
/// another tag on the note. Bytes outside the tags property are preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteTagEdit {
    pub bytes: Vec<u8>,
    pub deduplicated: usize,
}

/// Apply operations in order to one note in memory. Invalid requests and
/// unsupported front matter are refused without changing the input.
pub fn edit_note_tags(raw: &[u8], operations: &[TagOperation]) -> Result<NoteTagEdit, String> {
    if operations.is_empty() {
        return Err("no tag operations".into());
    }
    let mut seen = HashSet::new();
    let mut redundant_parent_seen = false;
    for op in operations {
        if matches!(op, TagOperation::RemoveRedundantParent) {
            if redundant_parent_seen {
                return Err("duplicate redundant-parent operation".into());
            }
            redundant_parent_seen = true;
            continue;
        }
        let (sources, target, is_move): (Vec<&str>, Option<&str>, bool) = match op {
            TagOperation::Rename { source, target } => (vec![source], Some(target), false),
            TagOperation::Merge { sources, target } => (
                sources.iter().map(String::as_str).collect(),
                Some(target),
                false,
            ),
            TagOperation::Move { source, target } => (vec![source], Some(target), true),
            TagOperation::Remove { source } => (vec![source], None, false),
            TagOperation::RemoveRedundantParent => unreachable!(),
        };
        if sources.is_empty() {
            return Err("no sources".into());
        }
        if let Some(t) = target {
            if !valid_target(t) || t.starts_with('#') {
                return Err(format!("invalid target: {t:?}"));
            }
        }
        for source in sources {
            if !valid_source(source) {
                return Err(format!("invalid source: {source:?}"));
            }
            if !seen.insert(fold(source)) {
                return Err(format!("duplicate source: {source:?}"));
            }
            if let Some(t) = target {
                if fold(source) == fold(t) {
                    return Err("source and target are the same".into());
                }
                if is_move && fold(t).starts_with(&(fold(source) + "/")) {
                    return Err("move target is inside the source tree".into());
                }
            }
        }
    }
    let original = editable_tags(raw).map_err(|e| e.to_string())?;
    let mut tags = original.clone();
    let mut deduplicated = 0;
    for op in operations {
        deduplicated += transform_tags(&mut tags, op)?;
    }
    let removed: Vec<&str> = original
        .iter()
        .filter(|tag| !tags.contains(tag))
        .map(String::as_str)
        .collect();
    let additions: Vec<&str> = tags
        .iter()
        .filter(|tag| !original.contains(tag))
        .map(String::as_str)
        .collect();
    let bytes = if removed.is_empty() && additions.is_empty() {
        raw.to_vec()
    } else {
        // Add first so replacing the last old tag does not delete and recreate
        // the key at a different location. A temporary tag handles case-folded
        // replacements that cannot be added until the old spelling is removed.
        let needs_placeholder = !additions.is_empty()
            && removed.len() == original.len()
            && additions
                .iter()
                .all(|tag| original.iter().any(|old| fold(old) == fold(tag)));
        let mut placeholder = "dimagine-temporary-tag".to_owned();
        while original
            .iter()
            .chain(tags.iter())
            .any(|tag| fold(tag) == fold(&placeholder))
        {
            placeholder.push('x');
        }
        let bytes = if needs_placeholder {
            add_tags(raw, &[&placeholder]).map_err(|e: FmEditError| e.to_string())?
        } else {
            raw.to_vec()
        };
        let bytes = add_tags(&bytes, &additions).map_err(|e: FmEditError| e.to_string())?;
        let bytes = remove_exact_tags(&bytes, &removed).map_err(|e| e.to_string())?;
        let bytes = add_tags(&bytes, &additions).map_err(|e: FmEditError| e.to_string())?;
        if needs_placeholder {
            remove_exact_tags(&bytes, &[&placeholder]).map_err(|e| e.to_string())?
        } else {
            bytes
        }
    };
    Ok(NoteTagEdit {
        bytes,
        deduplicated,
    })
}

fn transform_tags(tags: &mut Vec<String>, op: &TagOperation) -> Result<usize, String> {
    let mut remove = Vec::new();
    let mut add = Vec::new();
    for tag in tags.iter() {
        let replacement = match op {
            TagOperation::Rename { source, target } if tag == source => Some(Some(target.clone())),
            TagOperation::Merge { sources, target } if sources.contains(tag) => {
                Some(Some(target.clone()))
            }
            TagOperation::Move { source, target } if tag == source => Some(Some(target.clone())),
            TagOperation::Move { source, target } if tag.starts_with(&format!("{source}/")) => {
                Some(Some(format!("{target}{}", &tag[source.len()..])))
            }
            TagOperation::Remove { source } if tag == source => Some(None),
            TagOperation::RemoveRedundantParent
                if tags.iter().any(|child| {
                    child
                        .strip_prefix(tag.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1)
                }) =>
            {
                Some(None)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            remove.push(tag.clone());
            if let Some(target) = replacement {
                if !valid_target(&target) {
                    return Err(format!("invalid moved target: {target:?}"));
                }
                add.push(target);
            }
        }
    }
    tags.retain(|tag| !remove.contains(tag));
    let mut retained: HashSet<String> = tags.iter().map(|t| fold(t)).collect();
    let mut deduplicated = 0;
    for tag in add {
        if retained.insert(fold(&tag)) {
            tags.push(tag);
        } else {
            deduplicated += 1;
        }
    }
    Ok(deduplicated)
}

/// A complete readable note snapshot, including unchanged notes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedNote {
    pub path: String,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
    pub deduplicated: usize,
}

/// A condition that prevents safe application until resolved or confirmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanIssue {
    pub path: String,
    pub message: String,
}

/// The complete proposed alias document, if a change was requested.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AliasSnapshot {
    /// `None` means the file was absent, not unreadable.
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

/// Read-only material for later revalidation and application by a Writer.
/// Any issue prevents application; rename collisions require explicit review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TagBatchPlan {
    pub version: u32,
    pub operations: Vec<TagOperation>,
    pub notes: Vec<PlannedNote>,
    pub aliases: Option<AliasSnapshot>,
    pub unreadable: Vec<PlanIssue>,
    pub conflicts: Vec<PlanIssue>,
    /// SHA-256 of the serialized plan with this field set to the empty string.
    pub digest: String,
}

impl TagBatchPlan {
    fn refresh_digest(&mut self) -> Result<(), String> {
        self.digest.clear();
        let serialized = serde_json::to_vec(&self).map_err(|e| e.to_string())?;
        self.digest = format!("{:x}", Sha256::digest(serialized));
        Ok(())
    }

    /// Attach a validated proposed alias document to this snapshot.
    pub fn set_alias_after(&mut self, bytes: &[u8]) -> Result<(), String> {
        TagAliases::parse(bytes).map_err(|e| format!("invalid proposed aliases: {e}"))?;
        self.aliases
            .as_mut()
            .ok_or("alias table unavailable")?
            .after = Some(bytes.to_vec());
        self.refresh_digest()
    }

    /// Whether the snapshot is complete and has no unresolved conflicts.
    pub fn ready(&self) -> bool {
        self.unreadable.is_empty() && self.conflicts.is_empty() && self.aliases.is_some()
    }
}

/// Enumerate all non-ignored Markdown notes and the alias table without writing.
/// `alias_after` is an optional complete replacement document, validated before
/// being retained. A caller must re-enumerate and compare the digest under the
/// Writer lock, then use per-file preconditions and a single journal operation.
pub fn plan_tag_batch(
    root: &Path,
    operations: &[TagOperation],
    alias_after: Option<&[u8]>,
) -> Result<TagBatchPlan, String> {
    // Validate requests even when the library contains no notes.
    edit_note_tags(b"---\ntags: []\n---\n", operations)?;
    if let Some(bytes) = alias_after {
        TagAliases::parse(bytes).map_err(|e| format!("invalid proposed aliases: {e}"))?;
    }
    let library = Library::open(root).map_err(|e| e.to_string())?;
    let mut plan = TagBatchPlan {
        version: 1,
        operations: operations.to_vec(),
        notes: Vec::new(),
        aliases: None,
        unreadable: library
            .unreadable_dirs
            .iter()
            .map(|e| PlanIssue {
                path: e.path.clone(),
                message: e.reason.clone(),
            })
            .collect(),
        conflicts: Vec::new(),
        digest: String::new(),
    };
    for entry in &library.files {
        if !matches!(entry.class, FileClass::Note | FileClass::ImageNote) {
            if entry.class == FileClass::Special && entry.name.to_ascii_lowercase().ends_with(".md")
            {
                plan.unreadable.push(PlanIssue {
                    path: entry.rel.clone(),
                    message: "note is not a regular file".into(),
                });
            }
            continue;
        }
        let Some(path) = entry.path.to_str() else {
            plan.unreadable.push(PlanIssue {
                path: entry.rel.clone(),
                message: "note path is not UTF-8".into(),
            });
            continue;
        };
        let mut bytes = Vec::new();
        if let Err(e) = open_regular(root, entry).and_then(|mut f| f.read_to_end(&mut bytes)) {
            plan.unreadable.push(PlanIssue {
                path: path.into(),
                message: format!("cannot read note: {e}"),
            });
            continue;
        }
        match editable_tags(&bytes) {
            Ok(_) => {}
            Err(e) => {
                plan.unreadable.push(PlanIssue {
                    path: path.into(),
                    message: format!("cannot inspect tags: {e}"),
                });
                plan.notes.push(PlannedNote {
                    path: path.into(),
                    before: bytes.clone(),
                    after: bytes,
                    deduplicated: 0,
                });
                continue;
            }
        }
        match edit_note_tags(&bytes, operations) {
            Ok(edit) => plan.notes.push(PlannedNote {
                path: path.into(),
                before: bytes,
                after: edit.bytes,
                deduplicated: edit.deduplicated,
            }),
            Err(e) => {
                plan.conflicts.push(PlanIssue {
                    path: path.into(),
                    message: e,
                });
                plan.notes.push(PlannedNote {
                    path: path.into(),
                    before: bytes.clone(),
                    after: bytes,
                    deduplicated: 0,
                });
            }
        }
    }
    // Detect collisions against the state at each step, including targets
    // introduced by an earlier operation on a different note.
    let mut evolving: Vec<Vec<String>> = plan
        .notes
        .iter()
        .filter_map(|n| editable_tags(&n.before).ok())
        .collect();
    for op in operations {
        let tags: HashSet<String> = evolving.iter().flatten().map(|t| fold(t)).collect();
        if let TagOperation::Rename { source, target } = op {
            if tags.contains(&fold(source)) && tags.contains(&fold(target)) {
                plan.conflicts.push(PlanIssue {
                    path: String::new(),
                    message: format!("rename {source:?} collides with existing target {target:?}; review as a merge"),
                });
            }
        }
        for tags in &mut evolving {
            let _ = transform_tags(tags, op);
        }
    }
    match read_alias_snapshot(root) {
        Ok(before) => {
            plan.aliases = Some(AliasSnapshot {
                before,
                after: alias_after.map(<[u8]>::to_vec),
            })
        }
        Err(message) => plan.unreadable.push(PlanIssue {
            path: ALIAS_PATH.into(),
            message,
        }),
    }
    plan.notes.sort_by(|a, b| a.path.cmp(&b.path));
    plan.unreadable
        .sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    plan.conflicts
        .sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    plan.refresh_digest()?;
    Ok(plan)
}

/// Build a complete alias document for tags actually changed by a plan.
/// Existing keys are replaced case-insensitively, retaining their spelling.
pub fn remembered_aliases(plan: &TagBatchPlan) -> Result<Option<Vec<u8>>, String> {
    let snapshot = plan.aliases.as_ref().ok_or("alias table unavailable")?;
    let mut doc: serde_json::Value = match &snapshot.before {
        Some(bytes) => {
            serde_json::from_slice(bytes).map_err(|e| format!("invalid aliases: {e}"))?
        }
        None => serde_json::json!({"version": 1, "aliases": {}}),
    };
    let aliases = doc["aliases"]
        .as_object_mut()
        .ok_or("invalid aliases object")?;
    let mut changes = Vec::new();
    for note in &plan.notes {
        if note.before == note.after {
            continue;
        }
        let original = editable_tags(&note.before).map_err(|e| e.to_string())?;
        for tag in &original {
            let mut context = original.clone();
            let mut current = vec![tag.clone()];
            let mut renamed = false;
            for op in &plan.operations {
                let before = current.clone();
                if matches!(op, TagOperation::RemoveRedundantParent) {
                    current.retain(|parent| {
                        !context.iter().any(|child| {
                            child
                                .strip_prefix(parent.as_str())
                                .is_some_and(|suffix| suffix.starts_with('/') && suffix.len() > 1)
                        })
                    });
                } else {
                    transform_tags(&mut current, op)?;
                }
                transform_tags(&mut context, op)?;
                if !matches!(op, TagOperation::Remove { .. }) && current != before {
                    renamed = true;
                }
            }
            if renamed {
                if let Some(target) = current.pop() {
                    if target != *tag {
                        changes.push((tag.clone(), target));
                    }
                }
            }
        }
    }
    changes.sort();
    changes.dedup();
    if changes.is_empty() {
        return Ok(None);
    }
    for (source, target) in changes {
        let key = aliases
            .keys()
            .find(|key| fold(key) == fold(&source))
            .cloned()
            .unwrap_or(source);
        aliases.insert(key, serde_json::Value::String(target));
    }
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    TagAliases::parse(&bytes).map_err(|e| format!("invalid proposed aliases: {e}"))?;
    Ok(Some(bytes))
}

fn read_alias_snapshot(root: &Path) -> Result<Option<Vec<u8>>, String> {
    let dir = root.join("_dimagine");
    let path = root.join(ALIAS_PATH);
    for component in [&dir, &path] {
        match std::fs::symlink_metadata(component) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err("alias path contains a symlink".into())
            }
            Ok(m) if component == &dir && !m.is_dir() => {
                return Err("alias directory is not a directory".into())
            }
            Ok(m) if component == &path && !m.is_file() => {
                return Err("alias table is not a regular file".into())
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot inspect alias table: {e}")),
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot read alias table: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("alias table is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read alias table: {e}"))?;
    TagAliases::parse(&bytes).map_err(|e| format!("invalid alias table: {e}"))?;
    Ok(Some(bytes))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TagList {
    version: u32,
    entries: Vec<TagListEntry>,
    notes: Vec<String>,
    optional_operations: Vec<TagListOptional>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TagListEntry {
    source: String,
    target: String,
    action: TagListAction,
    selected: bool,
    image_count: u64,
    reason: String,
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    alternative_action: Option<TagListAction>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TagListAction {
    Rename,
    Merge,
    Move,
    Remove,
    Keep,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TagListOptional {
    operation: String,
    selected: bool,
    description: String,
}

pub fn parse_tag_list(bytes: &[u8]) -> Result<Vec<TagOperation>, String> {
    let list: TagList =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid tag list: {e}"))?;
    if list.version != 1 {
        return Err("unsupported tag list version".into());
    }
    let _ = &list.notes;
    let mut optional_selected = false;
    let mut optional_seen = false;
    for optional in &list.optional_operations {
        if optional.operation != "remove-redundant-parent" {
            return Err("unknown optional operation".into());
        }
        if optional_seen {
            return Err("duplicate optional operation".into());
        }
        optional_seen = true;
        optional_selected = optional.selected;
        let _ = &optional.description;
    }
    let mut operations = Vec::new();
    let mut all_sources = Vec::new();
    for entry in list.entries {
        let _ = (entry.image_count, &entry.reason, &entry.alternative_action);
        // Validate every source, including unselected entries, for duplicates using the core's Unicode rules.
        all_sources.push(TagOperation::Remove {
            source: entry.source.clone(),
        });
        let op = match entry.action {
            TagListAction::Rename => TagOperation::Rename {
                source: entry.source,
                target: entry.target,
            },
            TagListAction::Merge => TagOperation::Merge {
                sources: vec![entry.source],
                target: entry.target,
            },
            TagListAction::Move => TagOperation::Move {
                source: entry.source,
                target: entry.target,
            },
            TagListAction::Remove => TagOperation::Remove {
                source: entry.source,
            },
            TagListAction::Keep => {
                if entry.source != entry.target {
                    return Err("keep requires identical source and target".into());
                }
                continue;
            }
        };
        edit_note_tags(b"---\ntags: []\n---\n", std::slice::from_ref(&op))?;
        if entry.selected {
            operations.push(op);
        }
    }
    if !all_sources.is_empty() {
        edit_note_tags(b"---\ntags: []\n---\n", &all_sources)?;
    }
    if optional_selected {
        operations.push(TagOperation::RemoveRedundantParent);
    }
    if operations.is_empty() {
        return Err("no selected operations".into());
    }
    edit_note_tags(b"---\ntags: []\n---\n", &operations)?;
    // An output consumed by another operation makes results depend on order.
    for (i, op) in operations.iter().enumerate() {
        let target = match op {
            TagOperation::Rename { target, .. }
            | TagOperation::Merge { target, .. }
            | TagOperation::Move { target, .. } => Some(target),
            TagOperation::Remove { .. } | TagOperation::RemoveRedundantParent => None,
        };
        if let Some(target) = target {
            for (j, other) in operations.iter().enumerate() {
                if i == j {
                    continue;
                }
                let sources: Vec<&String> = match other {
                    TagOperation::Rename { source, .. }
                    | TagOperation::Move { source, .. }
                    | TagOperation::Remove { source } => vec![source],
                    TagOperation::Merge { sources, .. } => sources.iter().collect(),
                    TagOperation::RemoveRedundantParent => Vec::new(),
                };
                for source in sources {
                    let pair = [
                        TagOperation::Remove {
                            source: target.clone(),
                        },
                        TagOperation::Remove {
                            source: source.clone(),
                        },
                    ];
                    if edit_note_tags(b"---\ntags: []\n---\n", &pair).is_err() {
                        return Err("an operation target is another operation's source".into());
                    }
                }
            }
        }
    }
    Ok(operations)
}
