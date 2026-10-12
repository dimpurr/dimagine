//! The library round-trip harness: for every note in a scratch copy of a
//! library, set one scalar key and restore it, asserting the bytes are
//! identical afterwards. Refused notes are counted by reason. This test is
//! ignored by default and must never be pointed at a real library; it only
//! reads files and computes new bytes in memory — but the point of running
//! it is to find editor bugs, and a buggy build should not be tried on the
//! only copy of real data.
//!
//! ```sh
//! DIMAGINE_ROUNDTRIP_LIBRARY=/path/to/scratch-copy cargo test \
//!     --test fm_roundtrip -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use dimagine_core::fm_edit::{delete_key, front_matter_range, set_scalar, FmRange};
use dimagine_core::note::front_matter_text;
use saphyr::{LoadableYamlNode, Yaml};

/// The probe key must be absent from every real note; the harness checks and
/// skips the rare note that carries it.
const PROBE_KEY: &str = "dimagine-roundtrip-probe";
const PROBE_VALUE: &str = "w61";

#[test]
#[ignore = "set DIMAGINE_ROUNDTRIP_LIBRARY to a scratch copy of a library; never a real library"]
fn roundtrip_one_key_over_a_library_copy() {
    let Some(root) = std::env::var_os("DIMAGINE_ROUNDTRIP_LIBRARY") else {
        eprintln!("DIMAGINE_ROUNDTRIP_LIBRARY is not set; nothing to do");
        return;
    };
    let mut notes = Vec::new();
    collect_notes(Path::new(&root), &mut notes);
    eprintln!(
        "{} markdown files under {}",
        notes.len(),
        root.to_string_lossy()
    );

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut identical = 0usize;
    let mut rated_identical = 0usize;
    let mut rated_canonicalized = 0usize;

    for path in &notes {
        let Ok(raw) = fs::read(path) else {
            *counts.entry("unreadable".to_string()).or_default() += 1;
            continue;
        };
        match front_matter_range(&raw) {
            FmRange::NoFrontMatter { .. } => {
                *counts.entry("no front matter".to_string()).or_default() += 1;
                continue;
            }
            FmRange::Unterminated { .. } => {
                *counts.entry("unterminated".to_string()).or_default() += 1;
                continue;
            }
            FmRange::Closed { .. } => {}
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        if probe_key_present(front_matter_text(&text)) {
            *counts
                .entry("probe key already present".to_string())
                .or_default() += 1;
            continue;
        }
        // Set one scalar key, then restore it by deleting it again: every
        // byte must come back.
        let probed = match set_scalar(&raw, PROBE_KEY, PROBE_VALUE) {
            Ok(probed) => probed,
            Err(error) => {
                *counts.entry(format!("set refused: {error}")).or_default() += 1;
                continue;
            }
        };
        let restored = match delete_key(&probed, PROBE_KEY) {
            Ok(restored) => restored,
            Err(error) => {
                // Nothing was written by the harness itself; this is an
                // internal inconsistency and must fail loudly.
                panic!("{}, restore of the probe refused: {error}", path.display());
            }
        };
        assert_eq!(
            restored,
            raw,
            "{}: set and restore must be byte-identical",
            path.display()
        );
        identical += 1;

        // Where the note carries a plain integer rating, bounce it through a
        // different value and back: identical for plain spellings, and for
        // quoted ones ("4") the write is canonicalizing, which is counted,
        // not asserted, and must at least read back the same rating.
        if let Some(rating) = rating_of(front_matter_text(&text)) {
            let other = if rating == 3 { 4 } else { 3 };
            let set = dimagine_core::fm_edit::set_rating(
                &raw,
                dimagine_core::fm_edit::RatingEdit::Set(other),
            );
            let set = match set {
                Ok(set) => set,
                Err(error) => {
                    *counts
                        .entry(format!("rating refused: {error}"))
                        .or_default() += 1;
                    continue;
                }
            };
            let back = match dimagine_core::fm_edit::set_rating(
                &set,
                dimagine_core::fm_edit::RatingEdit::Set(rating),
            ) {
                Ok(back) => back,
                Err(error) => panic!("{}, rating restore refused: {error}", path.display()),
            };
            if back == raw {
                rated_identical += 1;
            } else {
                // The original rating was written in some other spelling
                // (quoted, spaced, commented): check the value came back.
                let back_text = String::from_utf8_lossy(&back).into_owned();
                assert_eq!(
                    rating_of(front_matter_text(&back_text)),
                    Some(rating),
                    "{}: rating value must survive the bounce",
                    path.display()
                );
                rated_canonicalized += 1;
            }
        }
    }

    eprintln!("probe identical: {identical}");
    eprintln!("rating identical: {rated_identical}, canonicalized: {rated_canonicalized}");
    for (reason, count) in &counts {
        eprintln!("{count:6}  {reason}");
    }
    assert!(
        identical > 0 || !counts.is_empty(),
        "the library copy held no notes to probe"
    );
}

/// Is the probe key already used in this front matter?
fn probe_key_present(yaml: Option<&str>) -> bool {
    let Some(yaml) = yaml else { return false };
    let Ok(docs) = Yaml::load_from_str(yaml) else {
        return false;
    };
    let Some(doc) = docs.first() else {
        return false;
    };
    doc.as_mapping().is_some_and(|mapping| {
        mapping
            .iter()
            .any(|(key, _)| key.as_str() == Some(PROBE_KEY))
    })
}

/// The note's rating when it is a plain 0–5 integer value.
fn rating_of(yaml: Option<&str>) -> Option<u8> {
    let yaml = yaml?;
    let docs = Yaml::load_from_str(yaml).ok()?;
    let doc = docs.first()?;
    let rating = doc
        .as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some("rating"))?
        .1;
    match rating {
        Yaml::Value(saphyr::Scalar::Integer(n)) if (0..=5).contains(n) => Some(*n as u8),
        _ => None,
    }
}

fn collect_notes(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<_> = entries.flatten().collect();
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_notes(&path, out);
        } else if name.to_ascii_lowercase().ends_with(".md") {
            out.push(path);
        }
    }
}
