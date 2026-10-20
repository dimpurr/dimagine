//! `dimagine previews`: build missing `thumb` and `view` renditions under
//! `.dimagine/cache/` (FORMAT §8.2) with bounded concurrency, using
//! `dimagine_preview`. Compiled when the `previews` feature is on and not
//! switched off at runtime (ADR-013).
//!
//! This command is where images that *decode* badly surface — `check` only
//! sniffs headers, so a truncated image must show up here as a finding.
//! Renditions that already exist and decode cleanly count as cached.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use clap::{Arg, ArgMatches, Command};
use dimagine_core::check::PreviewGaps;
use dimagine_core::library::{FileClass, Library};
use dimagine_preview::{ensure_with_fingerprint, Fingerprint, Kind, PreviewError, Rendition};

use crate::emit_failure;

const SCHEMA: &str = "dimagine.previews/0.1";

/// Walls between huge preview caches and small ones: more than this number of
/// parallel decodes buys nothing and starves other tools.
const DEFAULT_JOBS_CAP: usize = 8;

pub fn command() -> Command {
    Command::new("previews")
        .about("Build missing thumb and view renditions in .dimagine/cache/ (FORMAT §8.2).")
        .arg(
            Arg::new("jobs")
                .long("jobs")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .help(
                    "Maximum images decoded in parallel \
                     (default: the CPU count, capped at 8).",
                ),
        )
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|cpus| cpus.get())
        .unwrap_or(1)
        .clamp(1, DEFAULT_JOBS_CAP)
}

/// One processed image: how many of its renditions were written by this run,
/// already cached, its measured fingerprint, or why the image failed.
struct ImageOutcome {
    /// Lossy display path relative to the library root.
    rel: String,
    generated: usize,
    cached: usize,
    /// The fingerprint measured from the same decode that made the renditions
    /// (W77); `None` when the image failed to decode, so nothing was measured.
    fingerprint: Option<Fingerprint>,
    failure: Option<(&'static str, String)>,
}

struct Failure<'a> {
    path: &'a str,
    code: &'static str,
    reason: String,
}

/// The run's rendition tallies, gathered so the two printers share one argument
/// where they once shared five.
struct Summary {
    images: usize,
    generated: usize,
    cached: usize,
    failed: usize,
    jobs: usize,
}

/// What storing this run's fingerprints into the index came to (W77). The
/// index is derived data, so a run that cannot open it still built the
/// rendition cache; the near-duplicate scan simply has nothing new to read.
struct FingerprintStore {
    /// Fingerprints written to (or refreshed for) their content digest.
    stored: usize,
    /// Images whose write the index refused.
    failed: usize,
    /// The index could not be opened at all, so nothing was written.
    open_error: Option<String>,
}

/// Writes each decoded image's fingerprint to the content-keyed `image_dhash`
/// cache in the index, so `dimagine check` can compare them without decoding.
fn store_fingerprints(root: &Path, outcomes: &[ImageOutcome]) -> FingerprintStore {
    let mut index = match dimagine_index::Index::open(root) {
        Ok(index) => index,
        Err(error) => {
            return FingerprintStore {
                stored: 0,
                failed: 0,
                open_error: Some(error.to_string()),
            }
        }
    };
    let mut stored = 0;
    let mut failed = 0;
    for outcome in outcomes {
        let Some(fingerprint) = &outcome.fingerprint else {
            continue;
        };
        let wrote = index
            .store_image_dhash(
                &fingerprint.sha256,
                fingerprint.dhash,
                fingerprint.signature.width,
                fingerprint.signature.height,
                &fingerprint.signature.gray,
            )
            .is_ok();
        if wrote {
            stored += 1;
        } else {
            failed += 1;
        }
    }
    FingerprintStore {
        stored,
        failed,
        open_error: None,
    }
}

pub fn run(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let jobs = sub
        .get_one::<usize>("jobs")
        .copied()
        .unwrap_or_else(default_jobs)
        .max(1);
    let library = match Library::open(library_dir) {
        Ok(library) => library,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    let images: Vec<(String, PathBuf)> = library
        .files
        .iter()
        .filter(|file| file.class == FileClass::Image)
        .map(|file| (file.rel.clone(), library.root.join(&file.path)))
        .collect();

    let cached_at_start = cached_at_start(&library.root);
    let next = AtomicUsize::new(0);
    let outcomes = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some((rel, path)) = images.get(index) else {
                    break;
                };
                let outcome = build(&library.root, rel, path, &cached_at_start);
                outcomes.lock().unwrap().push(outcome);
            });
        }
    });
    let mut outcomes = outcomes.into_inner().unwrap();
    outcomes.sort_by(|a, b| a.rel.cmp(&b.rel));

    // Every decoded image also carried a perceptual fingerprint (W77). The
    // single-threaded store after the parallel phase keeps the index writes
    // serial (one connection) without holding it across any decode.
    let fingerprints = store_fingerprints(&library.root, &outcomes);

    let summary = Summary {
        images: images.len(),
        generated: outcomes.iter().map(|o| o.generated).sum(),
        cached: outcomes.iter().map(|o| o.cached).sum(),
        failed: outcomes.iter().filter(|o| o.failure.is_some()).count(),
        jobs,
    };
    let findings: Vec<Failure> = outcomes
        .iter()
        .filter_map(|o| {
            o.failure.as_ref().map(|(code, reason)| Failure {
                path: &o.rel,
                code,
                reason: reason.clone(),
            })
        })
        .collect();
    let read_complete = library.fully_read();

    if json {
        print_json(&library, &summary, &fingerprints, &findings);
    } else {
        print_human(&library, &summary, &fingerprints, &findings);
    }
    if !read_complete {
        ExitCode::from(3)
    } else if summary.failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

/// The renditions the cache does not hold, counted from the index's own rows —
/// `dimagine check`'s new item (ADR-020 item 6).
///
/// The count is a read of the index and a stat per candidate cache entry:
/// nothing is decoded, generated or written, so it belongs in `check`, which
/// never changes a file. The digest each rendition is keyed by is the one the
/// index already stores for the image's size and nanosecond mtime — the same
/// reuse the routes make (W71 M1) — and an image whose digest the index does
/// not hold is left out of the population rather than counted as missing,
/// because a digest no one has cannot name the entry to look for.
///
/// `None` is "not counted", never "nothing missing": no index to read (a
/// library that has never been scanned), an index that could not be opened, or
/// one that has to be rebuilt first. A library that is fully rendered answers
/// `Some` with both counts at zero.
pub fn gaps(library_dir: &Path) -> Option<PreviewGaps> {
    // The index is a cache, and `check` may not create one: counting from a
    // database that is not there yet is not this command's business, and
    // opening it would write the library's first cache entry during a
    // read-only run.
    let database = library_dir
        .join(".dimagine")
        .join("cache")
        .join("index.sqlite");
    if !database.is_file() {
        return None;
    }
    let root = fs::canonicalize(library_dir).unwrap_or_else(|_| library_dir.to_path_buf());
    let index = dimagine_index::Index::open(&root).ok()?;
    if index.rebuild_required() {
        return None;
    }
    let rows = index.image_rows().ok()?;
    if rows.is_empty() {
        // No row at all is an index that has not seen this library yet, not a
        // library with nothing in it — an empty answer here would read as
        // "everything is rendered" (invariant 4).
        return None;
    }
    let mut gaps = PreviewGaps {
        images: 0,
        thumb: 0,
        view: 0,
    };
    for (path, size, mtime_ns) in &rows {
        let Some(hash) = index
            .reusable_image_meta(path, *size, *mtime_ns)
            .ok()
            .flatten()
            .map(|(sha256, _meta)| sha256)
        else {
            continue;
        };
        gaps.images += 1;
        for (kind, missing) in [(Kind::Thumb, &mut gaps.thumb), (Kind::View, &mut gaps.view)] {
            if dimagine_preview::existing_rendition(&root, &hash, kind).is_none() {
                *missing += 1;
            }
        }
    }
    Some(gaps)
}

fn build(root: &Path, rel: &str, path: &Path, cached: &HashSet<PathBuf>) -> ImageOutcome {
    match ensure_with_fingerprint(root, path, &[Kind::Thumb, Kind::View]) {
        Ok((renditions, fingerprint)) => {
            let (generated, hits) = classify(&renditions, cached);
            ImageOutcome {
                rel: rel.to_string(),
                generated,
                cached: hits,
                fingerprint: Some(fingerprint),
                failure: None,
            }
        }
        Err(error) => ImageOutcome {
            rel: rel.to_string(),
            generated: 0,
            cached: 0,
            fingerprint: None,
            failure: Some((code_for(&error), error.to_string())),
        },
    }
}

/// The derived-data folder as it stands when the run starts: every file path,
/// whichever shard it lives in.
///
/// A missing folder is not an error — it is the first run on a library, and
/// everything is then generated.
fn cached_at_start(root: &Path) -> HashSet<PathBuf> {
    let mut existing = HashSet::new();
    let mut pending = vec![root.join(".dimagine").join("cache")];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else {
                existing.insert(path);
            }
        }
    }
    existing
}

/// `ensure` returns existing and freshly written renditions alike, and each is
/// named after the content hash of the image it came from, so a path that was
/// not in the cache at the start is one this run had to make. No timestamp can
/// stand in for that: a file written in the second the run began looks as old
/// as one that was cached, and a file that predates the run by a year can still
/// be a rendition this run rewrote.
///
/// A stored rendition that `ensure` found unusable and rewrote keeps its name,
/// so it counts as cached: the two numbers say how much of the cache this run
/// had to build from the images, not how many bytes reached the disk.
fn classify(renditions: &[Rendition], cached: &HashSet<PathBuf>) -> (usize, usize) {
    let mut generated = 0;
    let mut hits = 0;
    for rendition in renditions {
        if cached.contains(&rendition.path) {
            hits += 1;
        } else {
            generated += 1;
        }
    }
    (generated, hits)
}

fn code_for(error: &PreviewError) -> &'static str {
    match error {
        PreviewError::Io(_) => "io_error",
        PreviewError::Unsupported(_) => "unsupported_format",
        PreviewError::Decode(_) => "decode_failed",
        PreviewError::PixelLimit { .. } => "pixel_limit",
        PreviewError::ResourceLimit(_) => "resource_limit",
    }
}

fn print_json(
    library: &Library,
    summary: &Summary,
    fingerprints: &FingerprintStore,
    findings: &[Failure],
) {
    let document = serde_json::json!({
        "schema": SCHEMA,
        "library": library.root.display().to_string(),
        "images": summary.images,
        "generated": summary.generated,
        "cached": summary.cached,
        "failed": summary.failed,
        "jobs": summary.jobs,
        "fingerprints": {
            "stored": fingerprints.stored,
            "failed": fingerprints.failed,
            "index_error": fingerprints.open_error,
        },
        "read_complete": library.fully_read(),
        "unreadable_dirs": library.unreadable_dirs,
        "findings": findings
            .iter()
            .map(|f| serde_json::json!({
                "path": f.path,
                "code": f.code,
                "reason": f.reason,
            }))
            .collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}

fn print_human(
    library: &Library,
    summary: &Summary,
    fingerprints: &FingerprintStore,
    findings: &[Failure],
) {
    println!("library: {}", library.root.display());
    println!(
        "previews: {images} images ({generated} generated, {cached} cached, {failed} failed)",
        images = summary.images,
        generated = summary.generated,
        cached = summary.cached,
        failed = summary.failed
    );
    match &fingerprints.open_error {
        Some(error) => println!("fingerprints: not stored, index unavailable ({error})"),
        None if fingerprints.failed > 0 => println!(
            "fingerprints: {stored} stored, {failed} failed",
            stored = fingerprints.stored,
            failed = fingerprints.failed
        ),
        None => println!(
            "fingerprints: {stored} stored",
            stored = fingerprints.stored
        ),
    }
    for failure in findings {
        println!(
            "error {}: {}: {}",
            failure.path, failure.code, failure.reason
        );
    }
    if !library.fully_read() {
        println!();
        for dir in &library.unreadable_dirs {
            println!("could not read folder {}: {}", dir.path, dir.reason);
        }
        println!(
            "reading incomplete: some library content could not be read, \
             so the counts above may miss files and any \"not found\" proves nothing"
        );
    }
}
