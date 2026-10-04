//! `dimagine import eagle`: one-time, read-only import of an Eagle library
//! (HLD module `import::eagle`). Compiled when the `import-eagle` feature is
//! on and not switched off at runtime (ADR-013).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command};
use dimagine_eagle::{import, ImportError, ImportOptions, ImportReport, SkipReasonCode};

use crate::emit_failure;

const SCHEMA: &str = "dimagine.import/0.1";

pub fn command() -> Command {
    Command::new("import")
        .about("One-time import from another tool, with provenance.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("eagle")
                .about("Import an Eagle .library folder into the library; the source is only read.")
                .arg(
                    Arg::new("source")
                        .value_name("NAME.LIBRARY")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("The Eagle .library folder to import (its files never change)."),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .value_name("N")
                        .value_parser(clap::value_parser!(String))
                        .help(
                            "Display label for the imported library \
                             (default: the source folder name without .library).",
                        ),
                ),
        )
}

pub fn run(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    // `subcommand_required` on the import command makes this unreachable.
    let Some(("eagle", sub)) = sub.subcommand() else {
        return ExitCode::from(2);
    };
    let source = sub
        .get_one::<PathBuf>("source")
        .expect("source is required")
        .clone();
    let options = ImportOptions {
        name: sub.get_one::<String>("name").cloned(),
    };
    match import(&source, library_dir, options) {
        Ok(report) => {
            let partial = !report.skipped.is_empty() || report.dangling_folder_refs > 0;
            if json {
                print_json(&source, library_dir, &report);
            } else {
                print_human(library_dir, &report);
            }
            ExitCode::from(u8::from(partial))
        }
        Err(ImportError::PartialIo {
            error,
            progress,
            retained_artifacts,
        }) => {
            // Something was imported before the failure; show both. The
            // retained paths are only meaningful to a human with the folder.
            let message = format!(
                "I/O failure after partial import ({} retained artifacts): {error}",
                retained_artifacts.len()
            );
            if json {
                let error = serde_json::json!({
                    "schema": "dimagine.error/0.1",
                    "error": message,
                    "import": progress,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&error).unwrap_or_default()
                );
            } else {
                print_human(library_dir, &progress);
                emit_failure(false, &message);
            }
            ExitCode::from(1)
        }
        Err(error) => {
            emit_failure(json, &error.to_string());
            ExitCode::from(1)
        }
    }
}

fn code_name(code: &SkipReasonCode) -> &'static str {
    match code {
        SkipReasonCode::MissingMetadata => "missing_metadata",
        SkipReasonCode::UnreadableMetadata => "unreadable_metadata",
        SkipReasonCode::Trash => "trash",
        SkipReasonCode::NotImage => "not_image",
        SkipReasonCode::OriginalMissing => "original_missing",
        SkipReasonCode::CopyFailed => "copy_failed",
        SkipReasonCode::Symlink => "symlink",
    }
}

fn print_json(source: &Path, library_dir: &Path, report: &ImportReport) {
    #[derive(serde::Serialize)]
    struct ImportJson<'a> {
        schema: &'static str,
        source: &'a str,
        library: &'a str,
        #[serde(flatten)]
        report: &'a ImportReport,
    }
    let document = ImportJson {
        schema: SCHEMA,
        source: &source.display().to_string(),
        library: &library_dir.display().to_string(),
        report,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
}

fn print_human(library_dir: &Path, report: &ImportReport) {
    println!("library: {}", library_dir.display());
    println!(
        "imported {} (renamed {})",
        report.imported.len(),
        report.renamed
    );
    println!("skipped {}", report.skipped.len());
    for skipped in &report.skipped {
        println!(
            "  {}: {}: {}",
            code_name(&skipped.reason_code),
            skipped.item,
            skipped.reason
        );
    }
    if report.dangling_folder_refs > 0 {
        println!("dangling folder refs: {}", report.dangling_folder_refs);
    }
    if !report.folder_counts.is_empty() {
        println!("folders:");
        for (folder, count) in &report.folder_counts {
            println!("  {folder}: {count}");
        }
    }
}
