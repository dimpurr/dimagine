//! Validation and input-only resolution of the library's tag aliases.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

/// The visible, optional alias table, relative to the library root.
pub const ALIAS_PATH: &str = "_dimagine/tag-aliases.json";

pub(crate) fn fold(s: &str) -> String {
    s.case_fold().nfc().collect()
}

pub(crate) fn valid_source(s: &str) -> bool {
    !s.is_empty()
        && s.split('/').all(|segment| !segment.is_empty())
        && !s
            .chars()
            .any(|c| c.is_control() || matches!(c, ',' | '[' | ']' | '{' | '}' | '#' | '|'))
}

/// Whether a tag is valid as a new canonical target.
pub fn valid_target(s: &str) -> bool {
    valid_source(s) && !s.chars().any(char::is_whitespace)
}

#[derive(Debug)]
struct Entries(Vec<(String, String)>);

impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = Entries;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of alias strings")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Entries, M::Error> {
                let mut entries = Vec::new();
                let mut keys = HashSet::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if !keys.insert(fold(&key)) {
                        return Err(de::Error::custom("ambiguous alias keys"));
                    }
                    entries.push((key, value));
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u64,
    aliases: Entries,
}

/// A validated alias table. It changes inputs only, never existing notes.
#[derive(Debug, Default)]
pub struct TagAliases {
    entries: HashMap<String, String>,
}

impl TagAliases {
    /// Parse and validate a version 1 JSON document, including every chain.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let doc: Document = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if doc.version != 1 {
            return Err("unsupported alias version".into());
        }
        let mut entries = HashMap::new();
        for (key, target) in doc.aliases.0 {
            if !valid_source(&key) || !valid_target(&target) {
                return Err(format!("invalid alias: {key:?} → {target:?}"));
            }
            entries.insert(fold(&key), target);
        }
        let table = Self { entries };
        for key in table.entries.keys() {
            table.resolve(key)?;
        }
        Ok(table)
    }

    /// Read the table. Only a missing file is treated as an empty table.
    pub fn load(root: &Path) -> Result<Self, String> {
        let dir = root.join("_dimagine");
        for path in [&dir, &root.join(ALIAS_PATH)] {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err("alias path contains a symlink".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
                Err(e) => return Err(format!("cannot inspect alias table: {e}")),
            }
        }
        let bytes = std::fs::read(root.join(ALIAS_PATH))
            .map_err(|e| format!("cannot read alias table: {e}"))?;
        Self::parse(&bytes)
    }

    /// Return validated input aliases and their terminal targets.
    pub fn suggestions(&self) -> Result<Vec<(String, String)>, String> {
        let mut suggestions = self
            .entries
            .keys()
            .map(|key| Ok((key.clone(), self.resolve(key)?)))
            .collect::<Result<Vec<_>, String>>()?;
        suggestions.sort();
        Ok(suggestions)
    }

    /// Resolve a chain to its terminal spelling; reject cycles and excessive chains.
    pub fn resolve(&self, input: &str) -> Result<String, String> {
        let mut current = input.to_owned();
        let mut seen = HashSet::new();
        while let Some(next) = self.entries.get(&fold(&current)) {
            if seen.len() >= 128 || !seen.insert(fold(&current)) {
                return Err("alias cycle or chain too long".into());
            }
            current.clone_from(next);
        }
        Ok(current)
    }
}
