//! The dimagine command-line interface, version 0.1: `scan` and `check`,
//! both strictly read-only.
//!
//! Exit codes (HLD):
//!
//! - `0` finished, nothing to report;
//! - `1` finished, findings or a failed operation;
//! - `2` usage error;
//! - `3` did not finish reading the library, so every "not found" in the
//!   output proves nothing.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dimagine_core::check::{self, CheckReport, Severity};
use dimagine_core::library::Library;
use dimagine_core::scan::{self, ScanReport};

#[derive(Parser)]
#[command(
    name = "dimagine",
    version,
    about = "Tools for a dimagine image library: a plain folder of images with optional Markdown notes.",
    after_help = "Both commands read the library and never modify it. Every command takes --library <dir> (default: the current directory) and --json for machine-readable output."
)]
struct Cli {
    /// The library folder to read (default: the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    library: Option<PathBuf>,

    /// Print one machine-readable JSON document instead of human text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise the library: images, notes, collections, boards.
    Scan,
    /// Report problems in the library; never changes files.
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse(); // clap exits with code 2 on usage errors
    let library_dir = resolve_library(&cli.library);
    let json = cli.json;
    let library = match Library::open(&library_dir) {
        Ok(library) => library,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    match cli.command {
        Command::Scan => {
            let report = scan::run(&library);
            if json {
                print_scan_json(&report);
            } else {
                print_scan_human(&report);
            }
            ExitCode::from(report.exit_code().try_into().unwrap_or(1))
        }
        Command::Check => {
            let report = check::run(&library);
            if json {
                print_check_json(&report);
            } else {
                print_check_human(&report);
            }
            ExitCode::from(report.exit_code().try_into().unwrap_or(1))
        }
    }
}

fn resolve_library(flag: &Option<PathBuf>) -> PathBuf {
    match flag {
        Some(path) => path.clone(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
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
        "notes: {} ({} collection{})",
        report.notes_total,
        report.collections,
        if report.collections == 1 { "" } else { "s" }
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
            "reading incomplete: the walk did not cover the whole library, \
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
            "reading incomplete: the walk did not cover the whole library, \
             so missing and ambiguous link findings prove nothing"
        );
    }
}
