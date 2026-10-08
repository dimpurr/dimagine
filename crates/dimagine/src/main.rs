//! The dimagine command-line interface, version 0.1: `scan` and `check`,
//! both strictly read-only, plus built-in plugin subcommands that are
//! compiled per Cargo feature (ADR-013): `import eagle`, `previews` and
//! `serve`.
//!
//! Exit codes (HLD):
//!
//! - `0` finished, nothing to report;
//! - `1` finished, findings or a failed operation;
//! - `2` usage error;
//! - `3` did not finish reading the library, so every "not found" in the
//!   output proves nothing.

#[cfg(feature = "import-eagle")]
mod import;
mod plugins;
#[cfg(feature = "previews")]
mod previews;
#[cfg(feature = "serve")]
mod serve;
#[cfg(feature = "serve")]
mod user;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use dimagine_core::check::{self, CheckReport, Severity};
use dimagine_core::library::Library;
use dimagine_core::scan::{self, ScanReport};

use plugins::CorePlugins;

// The clap default is a plain literal; keep it in sync with the core constant.
const _: () = assert!(dimagine_core::check::DEFAULT_MAX_NOTE_BYTES == 8_388_608);

fn main() -> ExitCode {
    // The offered subcommands depend on the library's switch file, so the
    // library is resolved before clap runs; a misread falls back to the
    // current directory, which is also clap's default for the flag.
    let plugins = CorePlugins::load(&plugins::library_hint(std::env::args_os()));
    let matches = cli(&plugins).get_matches(); // clap exits with code 2 on usage errors
    let Some((name, sub)) = matches.subcommand() else {
        return ExitCode::from(2);
    };
    let library_dir = resolve_library(sub);
    match name {
        "scan" => run_scan(sub, &library_dir),
        "check" => run_check(sub, &library_dir),
        #[cfg(feature = "import-eagle")]
        "import" => import::run(sub, &library_dir),
        #[cfg(feature = "previews")]
        "previews" => previews::run(sub, &library_dir),
        #[cfg(feature = "serve")]
        "serve" => serve::run(sub, &library_dir),
        #[cfg(feature = "serve")]
        "user" => user::run(sub),
        other => unreachable!("clap already rejected unknown subcommand {other}"),
    }
}

fn cli(plugins: &CorePlugins) -> Command {
    let command = Command::new("dimagine")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Tools for a dimagine image library: a plain folder of images with optional Markdown notes.")
        .after_help("Every command takes --library <dir> (default: the current directory) and --json for machine-readable output. Commands only read the library; derived data lives in .dimagine/ and can always be deleted.")
        .arg(
            Arg::new("library")
                .long("library")
                .value_name("DIR")
                .global(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("The library folder to read (default: the current directory)."),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .global(true)
                .action(ArgAction::SetTrue)
                .help("Print one machine-readable JSON document instead of human text."),
        )
        .arg(
            Arg::new("max_note_bytes")
                .long("max-note-bytes")
                .value_name("BYTES")
                .global(true)
                .value_parser(clap::value_parser!(u64))
                .default_value("8388608")
                .help("Maximum bytes read from one Markdown note (default: 8388608)."),
        )
        .subcommand(
            Command::new("scan").about("Summarise the library: images, notes, collections, boards."),
        )
        .subcommand(
            Command::new("check").about("Report problems in the library; never changes files."),
        )
        .subcommand_required(true)
        .arg_required_else_help(true);
    // Built-in plugins: compiled per Cargo feature, then switched per library
    // (ADR-013). Disabled plugins do not appear in help or usage. Each block
    // rebinds `command`, so no feature combination leaves an unused `mut`.
    #[cfg(feature = "import-eagle")]
    let command = if plugins.import_eagle {
        command.subcommand(import::command())
    } else {
        command
    };
    #[cfg(not(feature = "import-eagle"))]
    let _ = &plugins.import_eagle;
    #[cfg(feature = "previews")]
    let command = if plugins.previews {
        command.subcommand(previews::command())
    } else {
        command
    };
    #[cfg(not(feature = "previews"))]
    let _ = &plugins.previews;
    #[cfg(feature = "serve")]
    let command = if plugins.serve {
        command
            .subcommand(serve::command())
            .subcommand(user::command())
    } else {
        command
    };
    #[cfg(not(feature = "serve"))]
    let _ = &plugins.serve;
    command
}

fn resolve_library(sub: &ArgMatches) -> PathBuf {
    sub.get_one::<PathBuf>("library")
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn run_scan(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let max_note_bytes = sub
        .get_one::<u64>("max_note_bytes")
        .copied()
        .unwrap_or(dimagine_core::check::DEFAULT_MAX_NOTE_BYTES);
    let library = match Library::open(library_dir) {
        Ok(library) => library,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    let report = scan::run_with_limit(&library, max_note_bytes);
    // Refresh the index (HLD: "walk the library, refresh the index, print a
    // summary"). A rebuild-required index is reported through the same message
    // as any other failure. The summary is still printed and the exit code is
    // still the summary's, but a stale index is never left unmentioned: the
    // viewer would otherwise serve pre-truncation results with no hint.
    let index_stats = refresh_index(&library);
    let index_stats = match index_stats {
        Ok(stats) => Some(stats),
        Err(error) => {
            eprintln!("dimagine: index not refreshed: {error}");
            None
        }
    };
    if json {
        print_scan_json(&report, index_stats.as_ref());
    } else {
        print_scan_human(&report, index_stats.as_ref());
    }
    ExitCode::from(report.exit_code().try_into().unwrap_or(1))
}

fn refresh_index(
    library: &Library,
) -> Result<dimagine_index::ImageMetaStats, dimagine_index::IndexError> {
    let mut index = dimagine_index::Index::open(&library.root)?;
    if index.rebuild_required() {
        return Err(dimagine_index::IndexError::RebuildRequired(
            "delete .dimagine/cache/index.sqlite and rescan".into(),
        ));
    }
    dimagine_core::sync_index(library, &mut index)?;
    // What the index now holds, counted from its own rows: dimensions that
    // stayed unknown are a recorded fact about content, so every scan reports
    // them.
    index.image_meta_stats()
}

fn run_check(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let max_note_bytes = sub
        .get_one::<u64>("max_note_bytes")
        .copied()
        .unwrap_or(dimagine_core::check::DEFAULT_MAX_NOTE_BYTES);
    let library = match Library::open(library_dir) {
        Ok(library) => library,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    let report = check::run_with_limit(&library, max_note_bytes);
    if json {
        print_check_json(&report);
    } else {
        print_check_human(&report);
    }
    ExitCode::from(report.exit_code().try_into().unwrap_or(1))
}

fn emit_failure(json: bool, message: &str) {
    if json {
        let error = serde_json::json!({
            "schema": "dimagine.error/0.1",
            "error": message,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&error).unwrap_or_default()
        );
    } else {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "dimagine: {message}");
    }
}

fn print_scan_json(report: &ScanReport, index: Option<&dimagine_index::ImageMetaStats>) {
    #[derive(serde::Serialize)]
    struct ScanJson<'a> {
        schema: &'a str,
        #[serde(flatten)]
        report: &'a ScanReport,
        /// Per-image header facts of the index, present whenever the index
        /// was refreshed: images, how many have dimensions, a taken time,
        /// or dimensions that stayed unknown (a header that could not be read,
        /// or a format this build has no reader for). Absent is a refresh that
        /// did not happen, which is not "zero images have dimensions".
        #[serde(skip_serializing_if = "Option::is_none")]
        index: Option<IndexStatsJson>,
    }
    #[derive(serde::Serialize)]
    struct IndexStatsJson {
        images: u64,
        with_dimensions: u64,
        with_taken: u64,
        unknown_dimensions: u64,
        /// Images with no taken time, counted by the reason their own bytes
        /// gave (`no-exif`, `exif-without-date`, `exif-unreadable`,
        /// `date-unreadable`, `file-unreadable`). The counts add up to
        /// `images - with_taken`: an image whose reason is *unrecorded* — a
        /// foreign or hand-edited index, or one written by a build whose
        /// reason code this version does not know — is counted under
        /// `reason not recorded` rather than left out of the sum. Empty is
        /// every image having a time; a reason that counted nothing is simply
        /// absent.
        taken_missing: std::collections::BTreeMap<&'static str, u64>,
    }
    let document = ScanJson {
        schema: scan::SCHEMA,
        report,
        index: index.map(|stats| IndexStatsJson {
            images: stats.images,
            with_dimensions: stats.with_dimensions,
            with_taken: stats.with_taken,
            unknown_dimensions: stats.unknown_dimensions,
            taken_missing: {
                let mut missing: std::collections::BTreeMap<&'static str, u64> = stats
                    .taken_missing
                    .iter()
                    .map(|(reason, count)| (reason.as_str(), *count))
                    .collect();
                let unrecorded = unrecorded_taken_reasons(stats);
                if unrecorded > 0 {
                    missing.insert("reason not recorded", unrecorded);
                }
                missing
            },
        }),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}

/// The images with no taken time whose reason is none of the recorded kinds:
/// `images - with_taken` less what the breakdown accounts for. It is a row
/// nobody recorded a reason for — a foreign or hand-edited index, or one
/// written by a build whose reason code this version does not know — and both
/// the human summary and the JSON name it rather than leaving it out of a sum
/// that then disagrees with its own total (RW50 L1).
fn unrecorded_taken_reasons(stats: &dimagine_index::ImageMetaStats) -> u64 {
    let missing = stats.images.saturating_sub(stats.with_taken);
    let recorded: u64 = stats.taken_missing.iter().map(|(_, count)| count).sum();
    missing.saturating_sub(recorded)
}

fn print_scan_human(report: &ScanReport, index: Option<&dimagine_index::ImageMetaStats>) {
    println!("library: {}", report.library);
    let formats = if report.images.is_empty() {
        "none".to_string()
    } else {
        report
            .images
            .iter()
            .map(|(format, count)| format!("{format} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("images: {} ({})", report.image_total, formats);
    if let Some(index) = index {
        println!(
            "index: {images} images ({dims} with dimensions, {taken} with taken time)",
            images = index.images,
            dims = index.with_dimensions,
            taken = index.with_taken
        );
        if index.unknown_dimensions > 0 {
            // Not "unreadable headers": a format this build has no reader for
            // (AVIF, HEIF) lands here beside a header that failed to parse,
            // and the row does not record which of the two it was.
            println!(
                "index: dimensions unknown for {} images",
                index.unknown_dimensions
            );
        }
        if index.with_taken < index.images {
            // The count without the kinds is the number that started this: an
            // operator can see dates are missing but not whether the files
            // carry none, carry one that could not be read, or carry nothing
            // we could read at all. The kinds add up to that count; whatever
            // is left is an image whose reason nobody recorded, and it is
            // named rather than quietly dropped from the sum (RW50 L1).
            let mut kinds = index
                .taken_missing
                .iter()
                .map(|(reason, count)| format!("{reason} {count}"))
                .collect::<Vec<_>>();
            let unrecorded = unrecorded_taken_reasons(index);
            if unrecorded > 0 {
                kinds.push(format!("reason not recorded {unrecorded}"));
            }
            println!(
                "index: taken time missing for {} images ({})",
                index.images - index.with_taken,
                kinds.join(", ")
            );
        }
    }
    println!(
        "image notes: {} (paired {}, unpaired {})",
        report.image_notes.total, report.image_notes.paired, report.image_notes.unpaired
    );
    println!(
        "notes: {} ({} explicit collections; {} embed collections; {} unknown)",
        report.notes_total,
        report.collections_explicit,
        report.collections_embedded,
        report.collections_unknown
    );
    println!("canvases: {}", report.canvases);
    println!("raw source files: {}", report.raw_files);
    println!("other files: {}", report.other_files);
    println!(
        "ignored entries: {} (hidden {}, resource-fork {}, os metadata {}, symlink {})",
        report.ignored.total,
        report.ignored.hidden,
        report.ignored.resource_fork,
        report.ignored.os_metadata,
        report.ignored.symlink
    );
    if report.unreadable_files.is_empty() {
        println!("unreadable: 0");
    } else {
        println!("unreadable: {}", report.unreadable_files.len());
        for unreadable in &report.unreadable_files {
            println!("  {} ({})", unreadable.path, unreadable.reason);
        }
    }
    if !report.read_complete {
        println!();
        for dir in &report.unreadable_dirs {
            println!("could not read folder {}: {}", dir.path, dir.reason);
        }
        println!(
            "reading incomplete: some library content could not be read, \
             so the counts above may miss files and any \"not found\" proves nothing"
        );
    }
}

fn print_check_json(report: &CheckReport) {
    #[derive(serde::Serialize)]
    struct CheckJson<'a> {
        schema: &'a str,
        #[serde(flatten)]
        report: &'a CheckReport,
    }
    let document = CheckJson {
        schema: check::SCHEMA,
        report,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}

fn print_check_human(report: &CheckReport) {
    for finding in &report.findings {
        let severity = match finding.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        println!(
            "{} {}: {}: {}",
            severity, finding.code, finding.path, finding.message
        );
    }
    let [errors, warnings, infos] = report.counts();
    if report.findings.is_empty() {
        println!("no problems found");
    } else {
        println!(
            "{error} error{es}, {warning} warning{ws}, {info} info{is}",
            error = errors,
            es = if errors == 1 { "" } else { "s" },
            warning = warnings,
            ws = if warnings == 1 { "" } else { "s" },
            info = infos,
            is = if infos == 1 { "" } else { "s" },
        );
    }
    if !report.read_complete {
        println!();
        for dir in &report.unreadable_dirs {
            println!("could not read folder {}: {}", dir.path, dir.reason);
        }
        println!(
            "reading incomplete: some library content could not be read, \
             so missing and ambiguous link findings prove nothing"
        );
    }
}
