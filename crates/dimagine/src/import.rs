//! `dimagine import eagle`: one-time, read-only import of an Eagle library
//! (HLD module `import::eagle`). Compiled when the `import-eagle` feature is
//! on and not switched off at runtime (ADR-013).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use dimagine_eagle::{
    backfill_added, import, ImportError, ImportOptions, ImportReport, SkipReasonCode,
};

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
                        .help(
                            "The Eagle .library folder to import (its files never change), \
                             or the dimagine library to backfill with --backfill-added.",
                        ),
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
                )
                .arg(
                    Arg::new("backfill_added")
                        .long("backfill-added")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Backfill the added property from sibling .eagle.json raw files \
                             into this library's notes instead of importing.",
                        ),
                )
                .arg(
                    Arg::new("apply")
                        .long("apply")
                        .action(ArgAction::SetTrue)
                        .help("Write the backfilled notes (default: dry run, report only)."),
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
    // Both modes are pointed at a folder to read, so both answer the same way
    // when that folder cannot be opened at all: exit code 3, "did not finish
    // reading" (HLD). Exit code 1 would say a reading happened and turned up a
    // problem; here no reading happened.
    if let Some(detail) = unreadable_folder(&source) {
        let message = format!("did not finish reading {}: {detail}", source.display());
        report_unfinished_read(json, library_dir, &message, None);
        return ExitCode::from(3);
    }
    if sub.get_flag("backfill_added") {
        return run_backfill_added(&source, sub);
    }
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
            // The run stopped short of reading the whole source, which is exit
            // code 3 rather than a completed import with problems.
            let message = format!(
                "I/O failure after partial import ({} retained artifacts): {error}",
                retained_artifacts.len()
            );
            report_unfinished_read(json, library_dir, &message, Some(&progress));
            ExitCode::from(3)
        }
        Err(error) => {
            emit_failure(json, &error.to_string());
            ExitCode::from(1)
        }
    }
}

/// Whether `path` cannot be read at all, as raw OS text.
///
/// Only the two kinds that stop a reading before it starts count: a folder that
/// is not there, and a folder that is locked. Anything else — a plain file
/// where a library was expected, say — is left to the importer to describe,
/// because there the reading happened and found the wrong thing.
fn unreadable_folder(path: &Path) -> Option<String> {
    match std::fs::read_dir(path) {
        Ok(_) => None,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Some(error.to_string())
        }
        Err(_) => None,
    }
}

/// Report a reading that stopped short (HLD exit code 3), carrying whatever the
/// run got through first. `read_complete` is the same flag `scan`, `check` and
/// `previews` use, so a machine reader can tell "no findings" from "no reading".
fn report_unfinished_read(
    json: bool,
    library_dir: &Path,
    message: &str,
    progress: Option<&ImportReport>,
) {
    if json {
        let mut document = serde_json::json!({
            "schema": "dimagine.error/0.1",
            "error": message,
            "read_complete": false,
        });
        if let Some(progress) = progress {
            document["import"] = serde_json::to_value(progress).unwrap_or(serde_json::Value::Null);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&document).unwrap_or_default()
        );
    } else {
        if let Some(progress) = progress {
            print_human(library_dir, progress);
        }
        emit_failure(false, message);
    }
}

fn run_backfill_added(library: &Path, sub: &ArgMatches) -> ExitCode {
    let json = sub.get_flag("json");
    let apply = sub.get_flag("apply");
    match backfill_added(library, apply) {
        Ok(report) => {
            if json {
                let document = serde_json::json!({
                    "schema": SCHEMA,
                    "library": library.display().to_string(),
                    "apply": apply,
                    "backfill": report,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document).unwrap_or_default()
                );
            } else {
                let mode = if apply { "applied" } else { "dry run" };
                println!("library: {}", library.display());
                println!("mode: {mode}");
                println!("scanned: {}", report.scanned);
                println!("updated: {}", report.updated);
                println!("already had added: {}", report.already_had);
                println!("no btime: {}", report.no_btime);
                println!("no front matter: {}", report.no_front_matter);
                println!("unreadable: {}", report.unreadable);
            }
            ExitCode::from(0)
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
