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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use dimagine_core::check::{self, CheckReport, Severity};
use dimagine_core::library::Library;
use dimagine_core::scan::{self, ScanReport};

// The clap default is a plain literal; keep it in sync with the core constant.
const _: () = assert!(dimagine_core::check::DEFAULT_MAX_NOTE_BYTES == 8_388_608);

fn main() -> ExitCode {
    let matches = cli().get_matches(); // clap exits with code 2 on usage errors
    let Some((name, sub)) = matches.subcommand() else {
        return ExitCode::from(2);
    };
    let library_dir = resolve_library(sub);
    match name {
        "scan" => run_scan(sub, &library_dir),
        "check" => run_check(sub, &library_dir),
        #[cfg(feature = "import-eagle")]
        "import" => import::run(sub, &library_dir),
        other => unreachable!("clap already rejected unknown subcommand {other}"),
    }
}

fn cli() -> Command {
    let mut command = Command::new("dimagine")
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
    #[cfg(feature = "import-eagle")]
    {
        command = command.subcommand(import::command());
    }
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
    if json {
        print_scan_json(&report);
    } else {
        print_scan_human(&report);
    }
    ExitCode::from(report.exit_code().try_into().unwrap_or(1))
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

fn print_scan_json(report: &ScanReport) {
    #[derive(serde::Serialize)]
    struct ScanJson<'a> {
        schema: &'a str,
        #[serde(flatten)]
        report: &'a ScanReport,
    }
    let document = ScanJson {
        schema: scan::SCHEMA,
        report,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}

fn print_scan_human(report: &ScanReport) {
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
