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
use dimagine_core::library::{FileClass, Library};
use dimagine_preview::{ensure, Kind, PreviewError, Rendition};

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
/// already cached, or why the image failed.
struct ImageOutcome {
    /// Lossy display path relative to the library root.
    rel: String,
    generated: usize,
    cached: usize,
    failure: Option<(&'static str, String)>,
}

struct Failure<'a> {
    path: &'a str,
    code: &'static str,
    reason: String,
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

    let generated: usize = outcomes.iter().map(|o| o.generated).sum();
    let cached: usize = outcomes.iter().map(|o| o.cached).sum();
    let failed: usize = outcomes.iter().filter(|o| o.failure.is_some()).count();
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
        print_json(
            &library,
            images.len(),
            generated,
            cached,
            failed,
            jobs,
            &findings,
        );
    } else {
        print_human(&library, images.len(), generated, cached, failed, &findings);
    }
    if !read_complete {
        ExitCode::from(3)
    } else if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn build(root: &Path, rel: &str, path: &Path, cached: &HashSet<PathBuf>) -> ImageOutcome {
    match ensure(root, path, &[Kind::Thumb, Kind::View]) {
        Ok(renditions) => {
            let (generated, hits) = classify(&renditions, cached);
            ImageOutcome {
                rel: rel.to_string(),
                generated,
                cached: hits,
                failure: None,
            }
        }
        Err(error) => ImageOutcome {
            rel: rel.to_string(),
            generated: 0,
            cached: 0,
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
    images: usize,
    generated: usize,
    cached: usize,
    failed: usize,
    jobs: usize,
    findings: &[Failure],
) {
    let document = serde_json::json!({
        "schema": SCHEMA,
        "library": library.root.display().to_string(),
        "images": images,
        "generated": generated,
        "cached": cached,
        "failed": failed,
        "jobs": jobs,
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
    images: usize,
    generated: usize,
    cached: usize,
    failed: usize,
    findings: &[Failure],
) {
    println!("library: {}", library.root.display());
    println!("previews: {images} images ({generated} generated, {cached} cached, {failed} failed)");
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
