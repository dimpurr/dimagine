//! `dimagine check near_duplicate`: report images that are the same picture at
//! different sizes, from the fingerprints `dimagine previews` stored (W77).
//!
//! This is a read of the index only — nothing is decoded, compared against
//! pixels on disk, or changed. Two images are *candidates* when their shape
//! hashes are close (`dhash` within [`DHASH_MAX`]) and their aspect ratios
//! match within [`ASPECT_MAX_DIFF`]; the fixed-width grayscale signature then
//! decides between the two verdicts: a [`P99_SAME_MAX`] or smaller 99th-percentile
//! pixel difference is a `same` pair (one picture, re-encoded — mergeable), a
//! larger one is a `variant` (similar but different — reported, never merged).
//!
//! A library the index has not measured yet (never scanned, or previews not run)
//! answers `measured: false`: that is "not counted", never "no duplicates".

use clap::{ArgMatches, Command};
use std::path::Path;
use std::process::ExitCode;

use dimagine_core::library::Library;
use dimagine_index::{ImageFingerprint, Index};
use dimagine_preview::{dhash_distance, p99, Signature};

use crate::emit_failure;

const SCHEMA: &str = "dimagine.near-duplicate/0.1";

/// The most that sixty-four shape gradients may disagree for two images to be
/// compared at all: six of them.
const DHASH_MAX: u32 = 6;
/// How far apart two aspect ratios may be, as a fraction, before a pair is a
/// crop rather than the same picture at a different size.
const ASPECT_MAX_DIFF: f64 = 0.03;
/// The 99th-percentile grayscale difference that still reads as one picture
/// re-encoded rather than two similar pictures.
const P99_SAME_MAX: u8 = 40;

pub fn command() -> Command {
    Command::new("near_duplicate")
        .about("Report images that are the same picture at different sizes (read-only).")
}

/// One candidate pair, with the two numbers that put it in its verdict.
#[derive(serde::Serialize)]
struct Pair {
    a: String,
    b: String,
    dhash_distance: u32,
    p99: u8,
    suggestion: &'static str,
}

/// A set of images linked by `same` verdicts — the unit `dimagine merge` works
/// on. It is a transitive closure, so a group may hold three or more files.
#[derive(serde::Serialize)]
struct Group {
    members: Vec<String>,
}

/// The whole report.
#[derive(serde::Serialize)]
struct Report {
    schema: &'static str,
    library: String,
    /// Whether the index held any fingerprints to compare. `false` means the
    /// scan could not run (no index, or none measured), not that nothing is a
    /// duplicate.
    measured: bool,
    /// Images the scan compared.
    images: usize,
    /// Mergeable groups, each two or more files, in path order.
    groups: Vec<Group>,
    /// Every candidate pair, mergeable or only similar, in path order.
    pairs: Vec<Pair>,
}

pub fn run(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let library = match Library::open(library_dir) {
        Ok(library) => library,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    // The fingerprint scan is a read of an index that must already exist: like
    // the rendition gap, `check` never creates one, so a library that has never
    // been scanned (or has never had its previews built) is "not measured".
    let entries = match measured_fingerprints(&library) {
        Ok(entries) => entries,
        Err(error) => {
            emit_failure(json, &error);
            return ExitCode::from(1);
        }
    };
    let report = build_report(library.root.display().to_string(), entries);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print_human(&report);
    }
    if !library.fully_read() {
        ExitCode::from(3)
    } else {
        ExitCode::from(0)
    }
}

/// The stored fingerprints, or a human-readable reason the scan cannot run.
fn measured_fingerprints(library: &Library) -> Result<Vec<ImageFingerprint>, String> {
    let database = library
        .root
        .join(".dimagine")
        .join("cache")
        .join("index.sqlite");
    if !database.is_file() {
        return Ok(Vec::new());
    }
    let index = Index::open(&library.root).map_err(|e| e.to_string())?;
    if index.rebuild_required() {
        return Ok(Vec::new());
    }
    index.image_fingerprints().map_err(|e| e.to_string())
}

fn build_report(library: String, entries: Vec<ImageFingerprint>) -> Report {
    let measured = !entries.is_empty();
    let mut pairs: Vec<Pair> = Vec::new();
    // Group over the mergeable (`same`) links by path. Every entry has a live
    // image row, so its path is its identity; the union-find keys on the path.
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    for i in 0..entries.len() {
        for j in i + 1..entries.len() {
            let (a, b) = (&entries[i], &entries[j]);
            let Some(distance) = distance_if_candidate(a, b) else {
                continue;
            };
            let Some(p99) = p99(&signature(a), &signature(b)) else {
                continue;
            };
            let same = p99 <= P99_SAME_MAX;
            if same {
                union(&mut parent, i, j);
            }
            pairs.push(Pair {
                a: a.path.clone(),
                b: b.path.clone(),
                dhash_distance: distance,
                p99,
                suggestion: if same { "same" } else { "variant" },
            });
        }
    }
    pairs.sort_by(|x, y| (&x.a, &x.b).cmp(&(&y.a, &y.b)));
    let groups = merge_groups(&entries, &parent);
    Report {
        schema: SCHEMA,
        library,
        measured,
        images: entries.len(),
        groups,
        pairs,
    }
}

/// The Hamming distance of two shape hashes when the pair clears the cheap
/// gates (close shape, matching aspect), or `None` to skip it before the
/// pixel comparison.
fn distance_if_candidate(a: &ImageFingerprint, b: &ImageFingerprint) -> Option<u32> {
    let distance = dhash_distance(a.dhash, b.dhash);
    if distance > DHASH_MAX {
        return None;
    }
    let (Some(aw), Some(ah)) = (a.file_width, a.file_height) else {
        return None;
    };
    let (Some(bw), Some(bh)) = (b.file_width, b.file_height) else {
        return None;
    };
    if ah == 0 || bh == 0 {
        return None;
    }
    if !aspect_close(aw as f64 / ah as f64, bw as f64 / bh as f64) {
        return None;
    }
    Some(distance)
}

fn aspect_close(a: f64, b: f64) -> bool {
    let larger = a.max(b);
    if larger == 0.0 {
        return true;
    }
    (a - b).abs() / larger <= ASPECT_MAX_DIFF
}

fn signature(entry: &ImageFingerprint) -> Signature {
    Signature {
        width: entry.signature.width,
        height: entry.signature.height,
        gray: entry.signature.gray.clone(),
    }
}

fn merge_groups(entries: &[ImageFingerprint], parent: &[usize]) -> Vec<Group> {
    let mut buckets: std::collections::BTreeMap<usize, Vec<&str>> =
        std::collections::BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        buckets
            .entry(find(parent, i))
            .or_default()
            .push(&entry.path);
    }
    let mut groups: Vec<Group> = buckets
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|members| {
            let mut members: Vec<String> = members.iter().map(|p| (*p).to_string()).collect();
            members.sort();
            Group { members }
        })
        .collect();
    groups.sort_by(|a, b| a.members.cmp(&b.members));
    groups
}

fn find(parent: &[usize], mut i: usize) -> usize {
    while parent[i] != i {
        i = parent[i];
    }
    i
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb] = ra;
    }
}

fn print_human(report: &Report) {
    println!("library: {}", report.library);
    if !report.measured {
        println!(
            "near-duplicates: not measured (scan the library and run `dimagine previews` first)"
        );
        return;
    }
    println!(
        "near-duplicates: {images} images measured, {mergeable} mergeable group(s), {pairs} candidate pair(s)",
        images = report.images,
        mergeable = report.groups.len(),
        pairs = report.pairs.len()
    );
    for group in &report.groups {
        println!("  group: {}", group.members.join("  "));
    }
    for pair in &report.pairs {
        println!(
            "  {suggestion}: {a}  {b}  dhash={distance}  p99={pixel}",
            suggestion = pair.suggestion,
            a = pair.a,
            b = pair.b,
            distance = pair.dhash_distance,
            pixel = pair.p99
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dimagine_index::SignatureBlob;

    fn entry(path: &str, width: u32, height: u32, dhash: u64, tone: u8) -> ImageFingerprint {
        // A flat 128 x 100 grayscale signature of one tone, the way a re-encode
        // of the same flat region would look.
        ImageFingerprint {
            path: path.into(),
            file_width: Some(width),
            file_height: Some(height),
            dhash,
            signature: SignatureBlob {
                width: 128,
                height: 100,
                gray: vec![tone; 128 * 100],
            },
        }
    }

    fn report(entries: Vec<ImageFingerprint>) -> Report {
        build_report("library".into(), entries)
    }

    #[test]
    fn a_reencode_is_a_same_pair_and_a_merge_group() {
        let report = report(vec![
            entry("a.jpg", 1200, 800, 0, 120),
            entry("b.jpg", 2400, 1600, 0, 121),
        ]);
        assert!(report.measured);
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].suggestion, "same");
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].members, vec!["a.jpg", "b.jpg"]);
    }

    #[test]
    fn a_far_apart_signature_is_a_variant_and_not_a_group() {
        let report = report(vec![
            entry("a.jpg", 1200, 800, 0, 10),
            entry("b.jpg", 1200, 800, 0, 250),
        ]);
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].suggestion, "variant");
        assert!(report.groups.is_empty());
    }

    #[test]
    fn a_different_aspect_ratio_is_never_a_candidate() {
        let report = report(vec![
            entry("a.jpg", 1200, 800, 0, 120),
            // Same shape hash, but a 1:2 aspect the >3% gate rejects.
            entry("b.jpg", 800, 1600, 0, 120),
        ]);
        assert!(report.pairs.is_empty());
    }

    #[test]
    fn a_distant_shape_hash_is_never_a_candidate() {
        let report = report(vec![
            entry("a.jpg", 1200, 800, 0, 120),
            entry("b.jpg", 1200, 800, u64::MAX, 120),
        ]);
        assert!(report.pairs.is_empty());
    }

    #[test]
    fn same_links_transitively_close_one_group_of_three() {
        let report = report(vec![
            entry("a.jpg", 1200, 800, 0, 120),
            entry("b.jpg", 1200, 800, 0, 122),
            entry("c.jpg", 1200, 800, 0, 124),
        ]);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].members, vec!["a.jpg", "b.jpg", "c.jpg"]);
    }

    #[test]
    fn no_fingerprints_is_not_measured_rather_than_clean() {
        let report = report(Vec::new());
        assert!(!report.measured);
        assert_eq!(report.images, 0);
        assert!(report.pairs.is_empty());
    }
}
