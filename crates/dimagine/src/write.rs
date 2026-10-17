//! The dimagine write commands (ADR-016, ADR-017): `tag` `rate` `set`
//! `collect` `mv` `trash` `restore` `undo` `config`, and the trash group's own
//! `trash purge` / `trash empty`.
//!
//! Every mutation goes through the `dimagine-write` op path — a planned set of
//! journaled steps run by [`Writer::execute_op`] — never a direct file write.
//! The intent (which image, which tag) is built here from a read of the library
//! and the pure editors in `dimagine-core` (front matter, links, collections);
//! the bytes land only through the write core, which holds the one lock,
//! compares hashes and journals each step so a crash recovers and an operation
//! can be undone (ADR-016 §6–§8).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};

use dimagine_core::body_edit::{self, BodyCtx, Embed, FolderSource};
use dimagine_core::check::{CheckReport, Finding, Severity};
use dimagine_core::fm_edit::{
    add_tags, delete_key, ensure_id, ensure_self_embed, front_matter_range, remove_tags,
    set_rating, set_scalar, FmEditError, FmRange, RatingEdit,
};
use dimagine_core::library::{classify, FileClass, FileEntry, Library};
use dimagine_core::links::Resolver;
use dimagine_core::settings::{self, Settings};

use dimagine_write::journal::{StepKind, StepRecord};
use dimagine_write::paths::{RelPath, StepPath, TrashPath, TRASH_DIR};
use dimagine_write::trash::{bumped_name, TrashRecord, TrashedFile};
use dimagine_write::{Fingerprint, ScannedRecord, UndoOutcome, UndoneItem, Writer};

/// The `source` field of every operation the CLI records.
const SOURCE: &str = "cli";

/// The `--json` schema of `trash purge` and of `trash empty`. They differ so a
/// caller can tell the two permanent-delete commands' results apart.
const PURGE_SCHEMA: &str = "dimagine.purge/0.1";
const TRASH_EMPTY_SCHEMA: &str = "dimagine.trash-empty/0.1";

/// The clap subcommands to add to the top-level parser. They are always
/// available: the CLI is the public write contract (ADR-016 §1).
pub fn commands() -> Vec<Command> {
    vec![
        Command::new("tag")
            .about("Add or remove tags in an image's note front matter.")
            .arg(
                Arg::new("image")
                    .value_name("IMG")
                    .required(true)
                    .help("The image, by library-relative path or unambiguous bare name."),
            )
            .arg(
                Arg::new("add")
                    .long("add")
                    .value_name("TAG")
                    .action(ArgAction::Append)
                    .help("A tag to add (repeatable)."),
            )
            .arg(
                Arg::new("remove")
                    .long("remove")
                    .value_name("TAG")
                    .action(ArgAction::Append)
                    .help("A tag to remove (repeatable)."),
            ),
        Command::new("rate")
            .about("Set or clear an image's rating (0-5, or none).")
            .arg(Arg::new("image").value_name("IMG").required(true))
            .arg(
                Arg::new("rating")
                    .value_name("RATING")
                    .required(true)
                    .help("An integer 0-5, or none to clear."),
            ),
        Command::new("set")
            .about("Set or clear scalar front-matter properties of an image's note.")
            .arg(Arg::new("image").value_name("IMG").required(true))
            .arg(
                Arg::new("assign")
                    .value_name("KEY=VALUE")
                    .num_args(1..)
                    .required(true)
                    .help("One or more KEY=VALUE pairs (scalars only); KEY= with no value clears the key."),
            ),
        Command::new("collect")
            .about("Create a collection note, add/remove images, reorder its members, or edit a caption.")
            .arg(
                Arg::new("new")
                    .long("new")
                    .value_name("TITLE")
                    .help("Create a new collection note with this title."),
            )
            .arg(
                Arg::new("add")
                    .long("add")
                    .value_name("COLLECTION")
                    .help("The existing collection note to add images to."),
            )
            .arg(
                Arg::new("remove")
                    .long("remove")
                    .value_name("COLLECTION")
                    .help("The existing collection note to remove images from."),
            )
            .arg(
                Arg::new("reorder")
                    .long("reorder")
                    .value_name("COLLECTION")
                    .help(
                        "Reorder the member blocks of an existing collection. Give the new \
                         order as 1-based block ordinals, e.g. 3,1,2: the third block moves \
                         to the first slot, and so on. Every block must appear exactly once.",
                    ),
            )
            .arg(
                Arg::new("caption")
                    .long("caption")
                    .value_name("COLLECTION")
                    .help(
                        "Edit a caption line of a member block. Give <BLOCK> <LINE> <TEXT> as \
                         operands: BLOCK is a 1-based block ordinal, LINE is 0 for the member \
                         line itself (refused here — use --remove) or the 1-based caption line \
                         number, and TEXT is the new caption or `-` to delete the line. One past \
                         the last caption line appends a new one.",
                    ),
            )
            .arg(
                Arg::new("images")
                    .value_name("OPERAND")
                    .num_args(0..)
                    .help(
                        "With --new/--add/--remove: images, by library-relative path or bare \
                         name. With --reorder: the new 1-based block order. With --caption: \
                         <BLOCK> <LINE> <TEXT>.",
                    ),
            ),
        Command::new("mv")
            .about("Move or rename an image and its group, repairing the links that point at it.")
            .arg(
                Arg::new("src")
                    .value_name("SRC")
                    .required(true)
                    .help("The image, by library-relative path or unambiguous bare name."),
            )
            .arg(
                Arg::new("dst")
                    .value_name("DST")
                    .required(true)
                    .help("The new library-relative path of the image; missing folders are created."),
            )
            .arg(
                Arg::new("dry_run")
                    .long("dry-run")
                    .action(ArgAction::SetTrue)
                    .help(
                        "Print the renames and link edits; change no library byte. \
                         Opening the library may still finish a crashed operation's \
                         recovery, and reports it.",
                    ),
            ),
        Command::new("trash")
            .about("Move image groups (image, note, raw metadata) into .trash/.")
            .arg(
                Arg::new("images")
                    .value_name("IMG")
                    .num_args(0..)
                    .help("Images, by library-relative path or bare name."),
            )
            .subcommand_required(false)
            .arg_required_else_help(true)
            .args_conflicts_with_subcommands(true)
            .subcommand(
                Command::new("purge")
                    .about("Permanently delete trashed groups past the retention period.")
                    .arg(
                        Arg::new("dry_run")
                            .long("dry-run")
                            .action(ArgAction::SetTrue)
                            .help("Print what would be deleted; delete nothing."),
                    )
                    .arg(
                        Arg::new("yes")
                            .long("yes")
                            .action(ArgAction::SetTrue)
                            .help("Actually delete (without it the command is a no-op)."),
                    ),
            )
            .subcommand(
                Command::new("empty")
                    .about("Permanently delete every trashed group now (ADR-017 §7).")
                    .arg(
                        Arg::new("yes")
                            .long("yes")
                            .action(ArgAction::SetTrue)
                            .help("Actually delete (without it the command only lists)."),
                    ),
            ),
        Command::new("restore")
            .about("Restore trashed image groups back to their original paths.")
            .arg(
                Arg::new("images")
                    .value_name("IMG")
                    .num_args(1..)
                    .required(true),
            ),
        Command::new("undo")
            .about("Undo the last operation, or one named by its op id (ADR-016 §8).")
            .arg(
                Arg::new("op")
                    .value_name("OP-ID")
                    .help("The operation to undo (default: the most recent)."),
            )
            .arg(
                Arg::new("force")
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help("Undo only the participants still at their recorded hash."),
            ),
        Command::new("config")
            .about("Show or change the library settings (ADR-019).")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(
                Command::new("list").about("List every setting with its value and default."),
            )
            .subcommand(
                Command::new("get")
                    .about("Print a setting's effective value.")
                    .arg(Arg::new("key").value_name("KEY").required(true)),
            )
            .subcommand(
                Command::new("set")
                    .about("Set a setting to a value.")
                    .arg(Arg::new("key").value_name("KEY").required(true))
                    .arg(Arg::new("value").value_name("VALUE").required(true)),
            )
            .subcommand(
                Command::new("reset")
                    .about("Reset a setting to its default.")
                    .arg(Arg::new("key").value_name("KEY").required(true)),
            ),
    ]
}

/// Run a write command, returning its exit code, or `None` for a command this
/// module does not own.
pub fn dispatch(name: &str, sub: &ArgMatches, library_dir: &Path) -> Option<ExitCode> {
    let code = match name {
        "tag" => run_tag(sub, library_dir),
        "rate" => run_rate(sub, library_dir),
        "set" => run_set(sub, library_dir),
        "collect" => run_collect(sub, library_dir),
        "mv" => run_move(sub, library_dir),
        "trash" => run_trash(sub, library_dir),
        "restore" => run_restore(sub, library_dir),
        "undo" => run_undo(sub, library_dir),
        "config" => run_config(sub, library_dir),
        _ => return None,
    };
    Some(code)
}

// ---------------------------------------------------------------------------
// Shared plumbing.
// ---------------------------------------------------------------------------

fn fail(json: bool, message: &str) -> ExitCode {
    crate::emit_failure(json, message);
    ExitCode::from(1)
}

fn library_walk(library_dir: &Path, json: bool) -> Result<Library, ExitCode> {
    Library::open(library_dir).map_err(|e| fail(json, &e.to_string()))
}

fn open_writer(library_dir: &Path, json: bool) -> Result<Writer, ExitCode> {
    Writer::open(library_dir).map_err(|e| fail(json, &error_message(&e)))
}

fn new_op_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Read a library file, `None` when it is simply not there or unreadable.
fn read_opt(writer: &Writer, path: &RelPath) -> Option<(Vec<u8>, Fingerprint)> {
    writer.read(path).ok()
}

/// One image the caller named, with whether it already has a note.
struct Target<'a> {
    entry: &'a FileEntry,
    note_rel: String,
    note_present: bool,
}

impl Target<'_> {
    fn note_relpath(&self) -> Result<RelPath, String> {
        RelPath::parse(&self.note_rel).map_err(|e| e.to_string())
    }

    /// The self-embed text a freshly created note gets (ADR-017 §2, §4).
    fn embed_text(&self, ctx: &BodyCtx) -> Result<String, String> {
        let index = self.index(ctx)?;
        match body_edit::link_text_for(index, ctx).map_err(|e| e.to_string())? {
            Embed::Text(text) => Ok(text),
            Embed::RenameOrMove { name, .. } => Err(format!(
                "bare name {name} is ambiguous; move it into a folder or rename it first"
            )),
        }
    }

    fn index(&self, ctx: &BodyCtx) -> Result<usize, String> {
        ctx.index_of(&self.entry.rel)
            .ok_or_else(|| format!("{} is not in the library walk", self.entry.rel))
    }
}

/// Resolve one `<img>` argument to an image file: an exact library-relative
/// path, else an unambiguous bare name.
fn resolve_image<'a>(
    files: &'a [FileEntry],
    resolver: &Resolver,
    arg: &str,
) -> Result<&'a FileEntry, String> {
    let trimmed = arg.trim_start_matches("./");
    if let Some(entry) = files
        .iter()
        .find(|e| e.class == FileClass::Image && e.rel == trimmed)
    {
        return Ok(entry);
    }
    if trimmed.contains('/') {
        return Err(format!("no image at library path {trimmed}"));
    }
    let hits: Vec<&FileEntry> = resolver
        .bare_name_hits(trimmed)
        .iter()
        .copied()
        .filter_map(|i| files.get(i))
        .filter(|e| e.class == FileClass::Image)
        .collect();
    match hits.len() {
        0 => Err(format!("no image named {trimmed}")),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "bare name {trimmed} is ambiguous ({} matches); name it by its library path",
            hits.len()
        )),
    }
}

fn target<'a>(
    files: &'a [FileEntry],
    resolver: &Resolver,
    arg: &str,
) -> Result<Target<'a>, String> {
    let entry = resolve_image(files, resolver, arg)?;
    let note_rel = format!("{}.md", entry.rel);
    let note_present = files
        .iter()
        .any(|e| e.class == FileClass::ImageNote && e.rel == note_rel);
    Ok(Target {
        entry,
        note_rel,
        note_present,
    })
}

/// Resolve every `<img>` argument, stopping at the first that does not name a
/// single image.
fn resolve_all<'a>(
    files: &'a [FileEntry],
    resolver: &Resolver,
    args: &[String],
    json: bool,
) -> Result<Vec<Target<'a>>, ExitCode> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match target(files, resolver, arg) {
            Ok(t) => out.push(t),
            Err(message) => return Err(fail(json, &message)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// tag / rate / set
// ---------------------------------------------------------------------------

fn run_tag(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let adds: Vec<String> = sub
        .get_many::<String>("add")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    let removes: Vec<String> = sub
        .get_many::<String>("remove")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if adds.is_empty() && removes.is_empty() {
        return fail(json, "give at least one --add or --remove tag");
    }
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let mut targets = match resolve_all(
        &library.files,
        &resolver,
        std::slice::from_ref(sub.get_one::<String>("image").unwrap()),
        json,
    ) {
        Ok(targets) => targets,
        Err(code) => return code,
    };
    let target = targets.remove(0);
    let note_rel = match target.note_relpath() {
        Ok(rel) => rel,
        Err(message) => return fail(json, &message),
    };
    let transform = move |bytes: &[u8]| -> Result<Vec<u8>, FmEditError> {
        let mut bytes = if adds.is_empty() {
            bytes.to_vec()
        } else {
            let refs: Vec<&str> = adds.iter().map(String::as_str).collect();
            add_tags(bytes, &refs)?
        };
        if !removes.is_empty() {
            let refs: Vec<&str> = removes.iter().map(String::as_str).collect();
            bytes = remove_tags(&bytes, &refs)?;
        }
        Ok(bytes)
    };
    apply_note_edit(json, &writer, &target, &ctx, note_rel, transform)
}

fn run_rate(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let word = sub.get_one::<String>("rating").unwrap().as_str();
    let edit = match word {
        "none" | "clear" => RatingEdit::Clear,
        other => match other.parse::<u8>() {
            Ok(value) if value <= 5 => RatingEdit::Set(value),
            Ok(value) => return fail(json, &format!("rating {value} is outside 0-5")),
            Err(_) => return fail(json, &format!("{other} is not 0-5 or none")),
        },
    };
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let mut targets = match resolve_all(
        &library.files,
        &resolver,
        std::slice::from_ref(sub.get_one::<String>("image").unwrap()),
        json,
    ) {
        Ok(targets) => targets,
        Err(code) => return code,
    };
    let target = targets.remove(0);
    let note_rel = match target.note_relpath() {
        Ok(rel) => rel,
        Err(message) => return fail(json, &message),
    };
    let transform = move |bytes: &[u8]| -> Result<Vec<u8>, FmEditError> { set_rating(bytes, edit) };
    apply_note_edit(json, &writer, &target, &ctx, note_rel, transform)
}

fn run_set(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let mut pairs: Vec<(String, String)> = Vec::new();
    for assign in sub.get_many::<String>("assign").unwrap() {
        match assign.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                pairs.push((key.to_string(), value.to_string()))
            }
            _ => return fail(json, &format!("{assign} is not KEY=VALUE")),
        }
    }
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let mut targets = match resolve_all(
        &library.files,
        &resolver,
        std::slice::from_ref(sub.get_one::<String>("image").unwrap()),
        json,
    ) {
        Ok(targets) => targets,
        Err(code) => return code,
    };
    let target = targets.remove(0);
    let note_rel = match target.note_relpath() {
        Ok(rel) => rel,
        Err(message) => return fail(json, &message),
    };
    let transform = move |bytes: &[u8]| -> Result<Vec<u8>, FmEditError> {
        let mut bytes = bytes.to_vec();
        for (key, value) in &pairs {
            // An empty value clears the property, which is a deleted key and
            // never `key: ""` (ADR-017 item 1, RW61 L5).
            bytes = if value.is_empty() {
                delete_key(&bytes, key)?
            } else {
                set_scalar(&bytes, key, value)?
            };
        }
        Ok(bytes)
    };
    apply_note_edit(json, &writer, &target, &ctx, note_rel, transform)
}

/// Apply a front-matter transform to an image's note through a single journaled
/// operation: assign an `id` when the note gains one (ADR-017 §3), create the
/// note with a blank line and a self-embed when it is new (ADR-017 §2), and
/// refuse without touching the file when the transform changes nothing.
fn apply_note_edit<F>(
    json: bool,
    writer: &Writer,
    target: &Target,
    ctx: &BodyCtx,
    note_rel: RelPath,
    transform: F,
) -> ExitCode
where
    F: Fn(&[u8]) -> Result<Vec<u8>, FmEditError>,
{
    let current = read_opt(writer, &note_rel);
    let note_exists = current.is_some();
    let base = current.map(|(bytes, _)| bytes).unwrap_or_default();

    let edited = match transform(&base) {
        Ok(bytes) => bytes,
        Err(error) => return fail(json, &error.to_string()),
    };
    if edited == base {
        return note_unchanged(json, &target.entry.rel);
    }
    let with_id = match ensure_id(&edited, &new_op_id()) {
        Ok(bytes) => bytes,
        Err(error) => return fail(json, &error.to_string()),
    };
    let final_bytes = if note_exists {
        with_id
    } else {
        // A new note: front matter, a blank line, then the self-embed
        // (ADR-017 §2). The blank line is the body's first newline.
        let mut seeded = with_id;
        seeded.push(b'\n');
        let embed = match target.embed_text(ctx) {
            Ok(embed) => embed,
            Err(message) => return fail(json, &message),
        };
        match ensure_self_embed(&seeded, &embed) {
            Ok(bytes) => bytes,
            Err(error) => return fail(json, &error.to_string()),
        }
    };

    let op_id = new_op_id();
    let step = StepRecord {
        step_idx: 0,
        kind: if note_exists {
            StepKind::Modify
        } else {
            StepKind::Create
        },
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(final_bytes.clone()),
    };
    let intent = serde_json::json!({"action": "note_edit", "image": target.entry.rel});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, vec![step]) {
        return fail(json, &error_message(&error));
    }
    print_front_matter(json, &op_id, &target.entry.rel, &final_bytes);
    ExitCode::from(0)
}

fn note_unchanged(json: bool, image_rel: &str) -> ExitCode {
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.note/0.1",
            "image": image_rel,
            "changed": false,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("{image_rel}: nothing to change");
    }
    ExitCode::from(0)
}

/// Print the note's front-matter block after a successful write (the task asks
/// `tag`/`rate`/`set` to print the resulting front matter), with the op id so
/// the caller can `dimagine undo <op-id>` it (ADR-016 §8).
fn print_front_matter(json: bool, op_id: &str, image_rel: &str, bytes: &[u8]) {
    let block = match front_matter_range(bytes) {
        FmRange::Closed { close_start, .. } => {
            let end = bytes[close_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|off| close_start + off + 1)
                .unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
        _ => String::new(),
    };
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.note/0.1",
            "image": image_rel,
            "changed": true,
            "op": op_id,
            "front_matter": block,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        print!("{block}");
    }
}

// ---------------------------------------------------------------------------
// collect
// ---------------------------------------------------------------------------

fn run_collect(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let images: Vec<String> = sub
        .get_many::<String>("images")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);

    // Reorder and caption address blocks and lines, not images, so they take
    // the operands raw and never go through image resolution.
    if let Some(collection) = sub.get_one::<String>("reorder") {
        return collect_reorder(json, &writer, collection, &images, &ctx);
    }
    if let Some(collection) = sub.get_one::<String>("caption") {
        return collect_caption(json, &writer, collection, &images, &ctx);
    }

    let targets = match resolve_all(&library.files, &resolver, &images, json) {
        Ok(targets) => targets,
        Err(code) => return code,
    };

    if let Some(title) = sub.get_one::<String>("new") {
        return collect_new(json, &writer, title, &targets, &ctx);
    }
    if let Some(collection) = sub.get_one::<String>("add") {
        return collect_edit(json, &writer, collection, &targets, &ctx, false);
    }
    if let Some(collection) = sub.get_one::<String>("remove") {
        return collect_edit(json, &writer, collection, &targets, &ctx, true);
    }
    fail(
        json,
        "give one of --new, --add, --remove, --reorder or --caption",
    )
}

fn collect_new(
    json: bool,
    writer: &Writer,
    title: &str,
    targets: &[Target],
    ctx: &BodyCtx,
) -> ExitCode {
    let indices: Vec<usize> = match targets.iter().map(|t| t.index(ctx)).collect() {
        Ok(indices) => indices,
        Err(message) => return fail(json, &message),
    };
    let body = match body_edit::new_collection_body(title, &indices, ctx) {
        Ok(body) => body,
        Err(error) => return fail(json, &error.to_string()),
    };
    let folder = collection_folder_choice(writer, ctx);
    let file_name = body_edit::safe_file_name(title);
    let stem = file_name.strip_suffix(".md").unwrap_or(&file_name);
    let parent = if folder.is_empty() {
        None
    } else {
        match RelPath::parse(&folder) {
            Ok(rel) => Some(rel),
            Err(error) => return fail(json, &error_message(&error)),
        }
    };
    let note_rel = match writer.next_free_name(parent.as_ref(), stem, "md") {
        Ok(rel) => rel,
        Err(error) => return fail(json, &error_message(&error)),
    };

    let mut steps = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Create,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(body),
    }];
    if let Err(message) = push_image_id_steps(writer, &mut steps, targets, ctx) {
        return fail(json, &message);
    }
    let op_id = new_op_id();
    let intent =
        serde_json::json!({"action": "collect_new", "title": title, "note": note_rel.text()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, steps) {
        return fail(json, &error_message(&error));
    }
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.collect/0.1",
            "collection": note_rel.text(),
            "created": true,
            "op": op_id,
            "images": targets.iter().map(|t| t.entry.rel.clone()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("created collection {}", note_rel.text());
    }
    ExitCode::from(0)
}

fn collect_edit(
    json: bool,
    writer: &Writer,
    collection: &str,
    targets: &[Target],
    ctx: &BodyCtx,
    remove: bool,
) -> ExitCode {
    let note_rel = match RelPath::parse(collection.trim_start_matches("./")) {
        Ok(rel) => rel,
        Err(error) => return fail(json, &error_message(&error)),
    };
    let note_index = match ctx.index_of(&note_rel.text()) {
        Some(index) => index,
        None => return fail(json, &format!("no note at {collection}")),
    };
    if !matches!(
        ctx.files[note_index].class,
        FileClass::Note | FileClass::ImageNote
    ) {
        return fail(json, &format!("{collection} is not a note"));
    }
    let (body, _) = match writer.read(&note_rel) {
        Ok(got) => got,
        Err(error) => return fail(json, &error_message(&error)),
    };

    let mut steps: Vec<StepRecord> = Vec::new();
    // Joining a collection assigns each image an id (ADR-017 §3), so each
    // image's own note is created or updated in the same operation.
    if !remove {
        if let Err(message) = push_image_id_steps(writer, &mut steps, targets, ctx) {
            return fail(json, &message);
        }
    }

    let mut new_body = body.clone();
    let mut touched = false;
    for target in targets {
        let index = match target.index(ctx) {
            Ok(index) => index,
            Err(message) => return fail(json, &message),
        };
        let result = if remove {
            body_edit::remove_member(&new_body, note_index, index, ctx).map(|removed| removed.bytes)
        } else {
            body_edit::append_member(&new_body, note_index, index, ctx).map(|added| added.bytes)
        };
        match result {
            Ok(bytes) => {
                if bytes != new_body {
                    touched = true;
                }
                new_body = bytes;
            }
            Err(error) => return fail(json, &error.to_string()),
        }
    }

    if !touched && steps.is_empty() {
        return collect_unchanged(json, &note_rel.text());
    }
    if touched {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Modify,
            src: None,
            dst: note_rel.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(new_body),
        });
    }
    let op_id = new_op_id();
    let action = if remove {
        "collect_remove"
    } else {
        "collect_add"
    };
    let intent = serde_json::json!({"action": action, "collection": note_rel.text()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, steps) {
        return fail(json, &error_message(&error));
    }
    if json {
        let doc = serde_json::json!({"schema": "dimagine.collect/0.1", "collection": note_rel.text(), "changed": true, "op": op_id});
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        let verb = if remove { "removed from" } else { "updated" };
        println!("{verb} {}", note_rel.text());
    }
    ExitCode::from(0)
}

fn collect_unchanged(json: bool, collection: &str) -> ExitCode {
    if json {
        let doc = serde_json::json!({"schema": "dimagine.collect/0.1", "collection": collection, "changed": false});
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("{collection}: nothing to change");
    }
    ExitCode::from(0)
}

/// Resolve an existing collection note and read its body and fingerprint. The
/// block and caption editors carry the open-hash precondition into the write
/// (ADR-016 §3), so they need the fingerprint the read produced.
fn read_collection(
    json: bool,
    writer: &Writer,
    collection: &str,
    ctx: &BodyCtx,
) -> Result<(RelPath, usize, Vec<u8>, Fingerprint), ExitCode> {
    let note_rel = match RelPath::parse(collection.trim_start_matches("./")) {
        Ok(rel) => rel,
        Err(error) => return Err(fail(json, &error_message(&error))),
    };
    let note_index = match ctx.index_of(&note_rel.text()) {
        Some(index) => index,
        None => return Err(fail(json, &format!("no note at {collection}"))),
    };
    if !matches!(
        ctx.files[note_index].class,
        FileClass::Note | FileClass::ImageNote
    ) {
        return Err(fail(json, &format!("{collection} is not a note")));
    }
    let (body, fp) = match writer.read(&note_rel) {
        Ok(got) => got,
        Err(error) => return Err(fail(json, &error_message(&error))),
    };
    Ok((note_rel, note_index, body, fp))
}

/// A 1-based block ordinal as a 0-based block index.
fn parse_ordinal(part: &str) -> Result<usize, String> {
    match part.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n - 1),
        Ok(_) => Err(format!("block ordinal {part} is not 1 or more")),
        Err(_) => Err(format!("block ordinal {part:?} is not a whole number")),
    }
}

/// Parse the `--reorder` operands into a 0-based order. Ordinals are 1-based
/// and may be comma-joined in one operand (`3,1,2`) or spread across operands.
fn parse_order(operands: &[String]) -> Result<Vec<usize>, String> {
    let mut order = Vec::new();
    for operand in operands {
        for part in operand.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            order.push(parse_ordinal(part)?);
        }
    }
    Ok(order)
}

/// `collect --reorder COLLECTION ORDER…`: permute the member blocks, leaving
/// every other byte of the note where it is (ADR-017 §9).
fn collect_reorder(
    json: bool,
    writer: &Writer,
    collection: &str,
    operands: &[String],
    ctx: &BodyCtx,
) -> ExitCode {
    let (note_rel, note_index, body, fp) = match read_collection(json, writer, collection, ctx) {
        Ok(got) => got,
        Err(code) => return code,
    };
    let blocks = match body_edit::member_blocks(&body, note_index, ctx) {
        Ok(blocks) => blocks,
        Err(error) => return fail(json, &error.to_string()),
    };
    let order = match parse_order(operands) {
        Ok(order) => order,
        Err(message) => return fail(json, &message),
    };
    let new_body = match body_edit::reorder_members(&body, note_index, ctx, &order) {
        Ok(bytes) => bytes,
        Err(error) => return fail(json, &error.to_string()),
    };
    if new_body == body {
        return collect_unchanged(json, &note_rel.text());
    }
    let step = StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(new_body),
    };
    let op_id = new_op_id();
    let intent = serde_json::json!({"action": "collect_reorder", "collection": note_rel.text()});
    if let Err(error) =
        writer.execute_op_guarded(&op_id, SOURCE, None, intent, vec![step], (&note_rel, &fp))
    {
        return fail(json, &error_message(&error));
    }
    // `order` is a validated permutation by now, so every slot is in range.
    let order_images: Vec<&str> = order
        .iter()
        .map(|&slot| blocks[slot].image.as_str())
        .collect();
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.collect/0.1",
            "collection": note_rel.text(),
            "changed": true,
            "op": op_id,
            "order": order_images,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("reordered {}: {}", note_rel.text(), order_images.join(", "));
    }
    ExitCode::from(0)
}

/// `collect --caption COLLECTION BLOCK LINE TEXT`: add, edit or delete one
/// caption line of a member block (ADR-017 §9), as one whole-replace step
/// guarded by the note's open hash.
fn collect_caption(
    json: bool,
    writer: &Writer,
    collection: &str,
    operands: &[String],
    ctx: &BodyCtx,
) -> ExitCode {
    let (note_rel, note_index, body, fp) = match read_collection(json, writer, collection, ctx) {
        Ok(got) => got,
        Err(code) => return code,
    };
    if operands.len() != 3 {
        return fail(json, "give <BLOCK> <LINE> <TEXT> after --caption");
    }
    let block = match parse_ordinal(&operands[0]) {
        Ok(block) => block,
        Err(message) => return fail(json, &message),
    };
    let line = match operands[1].parse::<usize>() {
        Ok(line) => line,
        Err(_) => {
            return fail(
                json,
                &format!("line offset {:?} is not a whole number", operands[1]),
            )
        }
    };
    let text = if operands[2] == "-" {
        None
    } else {
        Some(operands[2].as_str())
    };
    let (new_body, change) =
        match body_edit::edit_caption(&body, note_index, ctx, block, line, text) {
            Ok(result) => result,
            Err(error) => return fail(json, &error.to_string()),
        };
    if new_body == body {
        return collect_unchanged(json, &note_rel.text());
    }
    let step = StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(new_body),
    };
    let op_id = new_op_id();
    let intent = serde_json::json!({"action": "collect_caption", "collection": note_rel.text()});
    if let Err(error) =
        writer.execute_op_guarded(&op_id, SOURCE, None, intent, vec![step], (&note_rel, &fp))
    {
        return fail(json, &error_message(&error));
    }
    let verb = match change {
        body_edit::CaptionChange::Edited => "edited",
        body_edit::CaptionChange::Added => "added",
        body_edit::CaptionChange::Deleted => "deleted",
    };
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.collect/0.1",
            "collection": note_rel.text(),
            "changed": true,
            "op": op_id,
            "caption": verb,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("{verb} caption in {}", note_rel.text());
    }
    ExitCode::from(0)
}

/// Ensure every targeted image's own note carries an `id`, appending a create
/// or modify step for each note that needs one (used by `collect`).
fn push_image_id_steps(
    writer: &Writer,
    steps: &mut Vec<StepRecord>,
    targets: &[Target],
    ctx: &BodyCtx,
) -> Result<(), String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for target in targets {
        if !seen.insert(target.note_rel.clone()) {
            continue;
        }
        let note_rel = target.note_relpath()?;
        let exists = target.note_present;
        let base = if exists {
            read_opt(writer, &note_rel)
                .map(|(bytes, _)| bytes)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let edited = ensure_id(&base, &new_op_id()).map_err(|e| e.to_string())?;
        if exists && edited == base {
            continue;
        }
        let final_bytes = if exists {
            edited
        } else {
            let mut seeded = edited;
            seeded.push(b'\n');
            let embed = target.embed_text(ctx)?;
            ensure_self_embed(&seeded, &embed).map_err(|e| e.to_string())?
        };
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: if exists {
                StepKind::Modify
            } else {
                StepKind::Create
            },
            src: None,
            dst: note_rel.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(final_bytes),
        });
    }
    Ok(())
}

/// The new-collection folder from `.obsidian/app.json`, read-only (ADR-017 §6).
fn collection_folder_choice(writer: &Writer, ctx: &BodyCtx) -> String {
    let app_json =
        std::fs::read_to_string(writer.root_path().join(".obsidian").join("app.json")).ok();
    let choice = body_edit::collection_folder(app_json.as_deref(), "", ctx);
    if choice.source == FolderSource::RootFallback {
        String::new()
    } else {
        choice.folder
    }
}

// ---------------------------------------------------------------------------
// trash / restore
// ---------------------------------------------------------------------------

/// The library-relative paths of one image's group: the image, its note, and
/// every raw metadata sidecar that hangs off the image name (ADR-017 §7).
fn group_members(files: &[FileEntry], image: &FileEntry) -> Vec<RelPath> {
    let mut members = Vec::new();
    if let Ok(rel) = RelPath::parse(&image.rel) {
        members.push(rel);
    }
    let prefix = format!("{}.", image.name);
    for entry in files {
        if entry.dir_path() != image.dir_path() {
            continue;
        }
        let member = match entry.class {
            FileClass::ImageNote => {
                entry.paired_image_path().as_deref() == Some(image.path.as_path())
            }
            FileClass::Raw => entry.name.starts_with(&prefix),
            _ => false,
        };
        if member {
            if let Ok(rel) = RelPath::parse(&entry.rel) {
                members.push(rel);
            }
        }
    }
    members
}

/// Land a [`MovePlan`](dimagine_core::move_plan::MovePlan) as steps, in the
/// order ADR-017 §8 gives: every rename, then the created note, then the byte
/// edits, the moved note's own edit among them at its new path. Its before
/// bytes ride the step, because the file only reaches that path through this
/// operation's rename and the write core journals every step's before-state
/// before it touches any file (ADR-016 §7, §8).
fn move_steps(plan: &dimagine_core::move_plan::MovePlan) -> Vec<StepRecord> {
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut push = |kind: StepKind,
                    src: Option<String>,
                    dst: String,
                    before: Option<Vec<u8>>,
                    bytes: Option<Vec<u8>>| {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind,
            src,
            dst,
            before_hash: None,
            after_hash: String::new(),
            before_bytes: before,
            after_bytes: bytes,
        })
    };
    for rename in &plan.renames {
        push(
            StepKind::Move,
            Some(rename.src.clone()),
            rename.dst.clone(),
            None,
            None,
        );
    }
    if let Some(created) = &plan.new_note {
        push(
            StepKind::Create,
            None,
            created.path.clone(),
            None,
            Some(created.bytes.clone()),
        );
    }
    for edit in plan.moved_note.iter().chain(plan.edits.iter()) {
        push(
            StepKind::Modify,
            None,
            edit.path.clone(),
            Some(edit.before.clone()),
            Some(edit.bytes.clone()),
        );
    }
    steps
}

fn run_move(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let limit = sub
        .get_one::<u64>("max_note_bytes")
        .copied()
        .unwrap_or(dimagine_core::check::DEFAULT_MAX_NOTE_BYTES);
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);
    let src = sub
        .get_one::<String>("src")
        .map(String::as_str)
        .unwrap_or("");
    let entry = match resolve_image(&library.files, &resolver, src) {
        Ok(entry) => entry,
        Err(message) => return fail(json, &message),
    };
    let dst = sub
        .get_one::<String>("dst")
        .map(|d| d.trim_start_matches("./").to_string())
        .unwrap_or_default();
    let root = library.root.clone();
    let disk_root = root.clone();
    let read = move |entry: &FileEntry| -> Result<Vec<u8>, String> {
        let path = root.join(&entry.path);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a regular file".to_string());
        }
        if meta.len() > limit {
            return Err(format!("larger than the {limit}-byte note limit"));
        }
        std::fs::read(&path).map_err(|e| e.to_string())
    };
    // The one destination a move may displace is the file it moves, which
    // only the filesystem can confirm (a case-only self-rename on a
    // case-insensitive volume): see `same_file_probe`.
    let same_file = dimagine_core::move_plan::same_file_probe(disk_root.clone());
    let occupied = move |rel: &str| disk_root.join(rel).symlink_metadata().is_ok();
    // One op id for both the journal and the id a new
    // note gains (ADR-017 §3): the same logical operation
    // carries the same id on every surface, like the serve
    // endpoint.
    let op_id = new_op_id();
    let plan = match dimagine_core::move_plan::plan_move(
        &library.files,
        &resolver,
        &entry.rel,
        &dst,
        &op_id,
        &read,
        &occupied,
        &same_file,
    ) {
        Ok(plan) => plan,
        Err(refusal) => return fail(json, &refusal.to_string()),
    };
    for warning in &plan.warnings {
        eprintln!("warning: {}: {}", warning.path, warning.message);
    }
    for ambiguous in &plan.ambiguous {
        eprintln!(
            "ambiguous link, not rewritten: {}:{} `{}` could be {}",
            ambiguous.note,
            ambiguous.line,
            ambiguous.target,
            ambiguous.matches.join(" or ")
        );
    }
    let dry_run = sub.get_flag("dry_run");
    if dry_run {
        if json {
            let doc = move_plan_json(&plan, None);
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            print_move_plan(&plan);
        }
        return ExitCode::from(0);
    }
    let steps = move_steps(&plan);
    let intent = serde_json::json!({"action": "move", "from": plan.src, "to": plan.dst});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, steps) {
        return fail(json, &error_message(&error));
    }
    if json {
        let doc = move_plan_json(&plan, Some(op_id.as_str()));
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("moved {} -> {}", plan.src, plan.dst);
        print_move_details(&plan);
        println!("undo with: dimagine undo {op_id}");
    }
    ExitCode::from(0)
}

fn move_plan_json(
    plan: &dimagine_core::move_plan::MovePlan,
    op: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "dimagine.move/0.1",
        "dry_run": op.is_none(),
        "from": plan.src,
        "to": plan.dst,
        "renames": plan.renames.iter().map(|r| serde_json::json!({"from": r.src, "to": r.dst})).collect::<Vec<_>>(),
        "created_note": plan.new_note.as_ref().map(|n| n.path.clone()),
        "id_assigned": plan.id_assigned,
        "edited": plan
            .moved_note
            .iter()
            .chain(plan.edits.iter())
            .map(|e| serde_json::json!({
                "path": e.path,
                "self_embed_appended": e.self_embed_appended,
                "links": e.replaced.iter().map(|s| serde_json::json!({
                    "line": s.line,
                    "from": s.old_text,
                    "to": s.new_text,
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "ambiguous": plan
            .ambiguous
            .iter()
            .map(|a| serde_json::json!({
                "note": a.note,
                "line": a.line,
                "target": a.target,
                "matches": a.matches,
            }))
            .collect::<Vec<_>>(),
        "warnings": plan
            .warnings
            .iter()
            .map(|w| serde_json::json!({"path": w.path, "message": w.message}))
            .collect::<Vec<_>>(),
        "op": op,
    })
}

fn print_move_plan(plan: &dimagine_core::move_plan::MovePlan) {
    println!(
        "move {} -> {} (dry run, nothing changed)",
        plan.src, plan.dst
    );
    print_move_details(plan);
}

/// The per-file results both surfaces print: every rename, the
/// note the group gains, and every byte edit with what changed
/// in it — the same lines a real run reports.
fn print_move_details(plan: &dimagine_core::move_plan::MovePlan) {
    for rename in &plan.renames {
        println!("  rename {} -> {}", rename.src, rename.dst);
    }
    if let Some(created) = &plan.new_note {
        println!(
            "  create {} (the group needs a note for its id)",
            created.path
        );
    }
    for edit in plan.moved_note.iter().chain(plan.edits.iter()) {
        let mut notes: Vec<String> = Vec::new();
        if plan.id_assigned
            && plan
                .moved_note
                .as_ref()
                .is_some_and(|m| m.path == edit.path)
        {
            notes.push("id assigned".to_string());
        }
        if edit.self_embed_appended {
            notes.push("self-embed appended".to_string());
        }
        notes.push(format!(
            "{} link{} rewritten",
            edit.replaced.len(),
            if edit.replaced.len() == 1 { "" } else { "s" }
        ));
        println!("  edit {}: {}", edit.path, notes.join("; "));
        for replaced in &edit.replaced {
            println!(
                "    line {}: {} -> {}",
                replaced.line, replaced.old_text, replaced.new_text
            );
        }
    }
}

fn run_trash(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    // `trash` is the group ADR-017 §7 names: the move itself, plus the two
    // permanent-delete forms (`trash purge`, `trash empty`).
    match sub.subcommand() {
        Some(("purge", nested)) => return run_purge(nested, library_dir),
        Some(("empty", nested)) => return run_empty(nested, library_dir),
        Some((other, _)) => {
            return fail(
                sub.get_flag("json"),
                &format!("unknown trash subcommand {other}"),
            )
        }
        None => {}
    }
    let json = sub.get_flag("json");
    let images: Vec<String> = sub
        .get_many::<String>("images")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    if images.is_empty() {
        return fail(json, "give at least one image to trash");
    }
    let library = match library_walk(library_dir, json) {
        Ok(library) => library,
        Err(code) => return code,
    };
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let resolver = Resolver::new(&library.files);

    let trashed_at = chrono::Utc::now().to_rfc3339();
    let op_id = new_op_id();
    let (steps, trashed) = match plan_trash_steps(
        &writer,
        &library.files,
        &resolver,
        &images,
        &op_id,
        &trashed_at,
    ) {
        Ok(planned) => planned,
        Err(message) => return fail(json, &message),
    };
    let intent = serde_json::json!({"action": "trash", "images": trashed.clone()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, steps) {
        return fail(json, &error_message(&error));
    }
    if json {
        let doc =
            serde_json::json!({"schema": "dimagine.trash/0.1", "trashed": trashed, "op": op_id});
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        for image in trashed {
            println!("trashed {image}");
        }
    }
    ExitCode::from(0)
}

/// Plan one trash operation: each named image's group becomes a block of
/// steps, numbered in a single sequence across the whole operation.
///
/// The journal names every step file from `step_idx` (`step_{:04}.json`), so
/// numbering per group would let a later group's step files rename-replace an
/// earlier group's and hide those groups from crash recovery entirely
/// (ADR-016 §7). The numbering is therefore the operation's, not a group's.
fn plan_trash_steps(
    writer: &Writer,
    files: &[FileEntry],
    resolver: &Resolver,
    images: &[String],
    op_id: &str,
    trashed_at: &str,
) -> Result<(Vec<StepRecord>, Vec<String>), String> {
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut trashed: Vec<String> = Vec::new();
    for image in images {
        let entry = resolve_image(files, resolver, image)?;
        // A repeated argument names one group once (restore skips an already
        // restored record the same way): planning the same moves twice would
        // fail the second block and report the operation as partially applied.
        if trashed.contains(&entry.rel) {
            continue;
        }
        let members = group_members(files, entry);
        let image_rel = RelPath::parse(&entry.rel).map_err(|e| e.to_string())?;
        let round = free_trash_round(writer, &entry.name, &image_rel, &members)?;
        let group_steps = build_trash_steps(
            writer,
            op_id,
            trashed_at,
            entry,
            &members,
            round,
            steps.len(),
        )?;
        steps.extend(group_steps);
        trashed.push(entry.rel.clone());
    }
    Ok((steps, trashed))
}

/// The smallest collision round (ADR-017 §7) whose every target is free: each
/// member's bumped trash path *and* the bumped record file that names the
/// group. The record and the group move as one failure domain, so a round
/// whose record name is already taken is not free — otherwise the moves could
/// land with no record, leaving the group unreachable from the CLI.
fn free_trash_round(
    writer: &Writer,
    image_name: &str,
    image_rel: &RelPath,
    members: &[RelPath],
) -> Result<usize, String> {
    let mut round = 1usize;
    loop {
        let bumped = bumped_name(image_name, round);
        let all_free = members.iter().all(|member| {
            let suffix = member.name().strip_prefix(image_name).unwrap_or("");
            let target = TrashPath::for_original(member)
                .sibling(&format!("{bumped}{suffix}"))
                .expect("a bumped sibling of a validated path");
            writer.read_step(&StepPath::Trash(target)).is_err()
        });
        if all_free && record_slot_free(writer, image_rel, &bumped)? {
            return Ok(round);
        }
        round += 1;
    }
}

/// Whether `.trash/<the image's folder>/<bumped>.trash.json` is free.
fn record_slot_free(writer: &Writer, image_rel: &RelPath, bumped: &str) -> Result<bool, String> {
    let record = TrashRecord::path_for(image_rel)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;
    Ok(writer.read_step(&StepPath::Trash(record)).is_err())
}

/// The create-record + move-each-member steps of one group's part of a trash
/// operation (ADR-017 §7). `first_idx` is where this group's steps begin in the
/// operation's one sequence, so no two steps of the operation share a journal
/// file name.
fn build_trash_steps(
    writer: &Writer,
    op_id: &str,
    trashed_at: &str,
    image: &FileEntry,
    members: &[RelPath],
    round: usize,
    first_idx: usize,
) -> Result<Vec<StepRecord>, String> {
    let image_name = image.name.clone();
    let bumped = bumped_name(&image_name, round);
    let mut files = Vec::new();
    let mut moves = Vec::new();
    for member in members {
        let (_, fp) = match writer.read(member) {
            Ok(read) => read,
            Err(dimagine_write::Error::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let suffix = member.name().strip_prefix(&image_name).unwrap_or("");
        let trash = TrashPath::for_original(member)
            .sibling(&format!("{bumped}{suffix}"))
            .map_err(|e| e.to_string())?;
        files.push(TrashedFile {
            original: member.text(),
            trash: trash.text(),
            mtime_ns: fp.mtime_ns,
        });
        moves.push((member.text(), trash.text()));
    }
    if files.is_empty() {
        return Err("nothing to trash".to_string());
    }
    let record = TrashRecord {
        op_id: op_id.to_string(),
        trashed_at: trashed_at.to_string(),
        files,
    };
    let record_bytes = record.to_bytes().map_err(|e| e.to_string())?;
    let image_rel = RelPath::parse(&image.rel).map_err(|e| e.to_string())?;
    let record_trash = TrashPath::for_original(&image_rel)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;

    let mut steps = vec![StepRecord {
        step_idx: first_idx,
        kind: StepKind::Create,
        src: None,
        dst: record_trash.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(record_bytes),
    }];
    for (index, (src, dst)) in moves.into_iter().enumerate() {
        steps.push(StepRecord {
            step_idx: first_idx + index + 1,
            kind: StepKind::Move,
            src: Some(src),
            dst,
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        });
    }
    Ok(steps)
}

fn run_restore(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let images: Vec<String> = sub
        .get_many::<String>("images")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let records = match writer.trash_records() {
        Ok(records) => records,
        Err(error) => return fail(json, &error_message(&error)),
    };

    let op_id = new_op_id();
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut restored: Vec<(String, Vec<TrashedFile>)> = Vec::new();
    for image in &images {
        // The group is resolved against the trash records, not the library
        // walk: a trashed image is no longer in the walk, which is the whole
        // point of the command.
        let record = match find_record_for(&records, image) {
            Ok(record) => record,
            Err(message) => {
                // ADR-017 §7 keeps the old location readable. A name that is
                // only there is named as such, rather than reported as absent:
                // the write core has no door into `.dimagine/`, so the file is
                // reported, never moved.
                return fail(json, &legacy_hint(library_dir, &message));
            }
        };
        if restored.iter().any(|(rel, _)| *rel == record.label) {
            continue;
        }
        // The original path must be free before the group moves back
        // (ADR-017 §7).
        for file in &record.files {
            match RelPath::parse(&file.original) {
                Ok(original) if read_opt(&writer, &original).is_some() => {
                    return fail(
                        json,
                        &format!(
                            "cannot restore {}: {} already exists",
                            record.label, file.original
                        ),
                    )
                }
                Ok(_) => {}
                Err(error) => return fail(json, &error_message(&error)),
            }
        }
        for file in &record.files {
            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Move,
                src: Some(file.trash.clone()),
                dst: file.original.clone(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: None,
            });
        }
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Delete,
            src: None,
            dst: record.path.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: Some(record.record_bytes.clone()),
            after_bytes: None,
        });
        restored.push((record.label.clone(), record.files.clone()));
    }
    if restored.is_empty() {
        return fail(json, "give at least one trashed image to restore");
    }
    let intent = serde_json::json!({"action": "restore", "images": restored.iter().map(|(rel, _)| rel.clone()).collect::<Vec<_>>()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, None, intent, steps) {
        return fail(json, &error_message(&error));
    }
    // A rename preserves mtime, but a restore still puts the recorded times
    // back for the belt-and-braces half of the promise (ADR-017 §7).
    for (_, files) in &restored {
        for file in files {
            if let Ok(original) = RelPath::parse(&file.original) {
                let _ = writer.set_mtime(&StepPath::Library(original), file.mtime_ns);
            }
        }
    }
    if json {
        let doc = serde_json::json!({"schema": "dimagine.trash/0.1", "restored": restored.iter().map(|(rel, _)| rel.clone()).collect::<Vec<_>>(), "op": op_id});
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        for (rel, _) in &restored {
            println!("restored {rel}");
        }
    }
    ExitCode::from(0)
}

/// A scanned trash record, materialised with its bytes and path for restore or
/// purge.
struct FoundRecord {
    path: TrashPath,
    record_bytes: Vec<u8>,
    files: Vec<TrashedFile>,
    /// The group's image, library-relative: the label the command speaks.
    label: String,
}

fn found_record(scanned: &ScannedRecord, record: &TrashRecord) -> Result<FoundRecord, String> {
    // A record built here lists the image first (ADR-017 §7); a hand-written
    // one need not, so fall back to the shortest path, which is the image of
    // any group whose note and sidecars hang off the image name.
    let label = record
        .files
        .first()
        .map(|file| file.original.clone())
        .or_else(|| {
            record
                .files
                .iter()
                .map(|file| file.original.as_str())
                .min_by_key(|path| path.len())
                .map(str::to_owned)
        })
        .ok_or_else(|| format!("{} records no files", scanned.path.text()))?;
    Ok(FoundRecord {
        path: scanned.path.clone(),
        record_bytes: record.to_bytes().map_err(|e| e.to_string())?,
        files: record.files.clone(),
        label,
    })
}

/// Resolve a `restore` argument to the trash record of its group: an exact
/// library-relative original path, else an unambiguous bare name. A group
/// trashed, restored and trashed again leaves two records over the same
/// originals; the newest one holds the files, so it wins.
fn find_record_for(records: &[ScannedRecord], arg: &str) -> Result<FoundRecord, String> {
    let trimmed = arg.trim_start_matches("./");
    let mut hits: Vec<(&ScannedRecord, &TrashRecord)> = Vec::new();
    for scanned in records {
        let Ok(record) = scanned.record.as_ref() else {
            continue;
        };
        let exact = record.files.iter().any(|file| file.original == trimmed);
        let named = !trimmed.contains('/')
            && record
                .files
                .iter()
                .any(|file| file.original.rsplit('/').next() == Some(trimmed));
        if exact || named {
            hits.push((scanned, record));
        }
    }
    match hits.len() {
        0 => Err(if trimmed.contains('/') {
            format!("nothing trashed at library path {trimmed}")
        } else {
            format!("nothing trashed named {trimmed}")
        }),
        1 => found_record(hits[0].0, hits[0].1),
        _ => {
            let first: Vec<&str> = hits[0]
                .1
                .files
                .iter()
                .map(|f| f.original.as_str())
                .collect();
            let same_group = hits.iter().all(|(_, record)| {
                record
                    .files
                    .iter()
                    .map(|f| f.original.as_str())
                    .collect::<Vec<_>>()
                    == first
            });
            if same_group {
                let (scanned, record) = hits
                    .iter()
                    .max_by_key(|(_, record)| record.trashed_at.as_str())
                    .copied()
                    .unwrap_or(hits[0]);
                found_record(scanned, record)
            } else {
                Err(format!(
                    "bare name {trimmed} is ambiguous ({} trashed groups); name it by its library path",
                    hits.len()
                ))
            }
        }
    }
}

/// Add a pointer to the old trash location when the name a `restore` missed is
/// sitting in it (ADR-017 §7 keeps that location readable). Purely a message:
/// nothing here writes, moves or deletes, because the write core refuses
/// `.dimagine/` paths (ADR-018 §4) and this ADR makes the location read-only.
fn legacy_hint(library_dir: &Path, message: &str) -> String {
    let legacy_root = library_dir.join(".dimagine").join("trash");
    if !legacy_root.is_dir() {
        return message.to_string();
    }
    // The argument is the tail of "nothing trashed named X" /
    // "nothing trashed at library path X".
    let Some(arg) = message
        .strip_prefix("nothing trashed named ")
        .or_else(|| message.strip_prefix("nothing trashed at library path "))
    else {
        return message.to_string();
    };
    let needle = arg.rsplit('/').next().unwrap_or(arg);
    let mut files: Vec<PathBuf> = Vec::new();
    collect_trash_files(&legacy_root, &mut files);
    let Some(hit) = files
        .iter()
        .find(|path| path.file_name().map(|n| n == needle).unwrap_or(false))
    else {
        return message.to_string();
    };
    let rel = display_rel(library_dir, hit);
    format!(
        "{message} — but {rel} is in the old trash location, which dimagine no longer writes \
         to or restores from; move it back into the library by hand"
    )
}

// ---------------------------------------------------------------------------
// undo
// ---------------------------------------------------------------------------
fn run_undo(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let force = sub.get_flag("force");
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let op_id = match sub.get_one::<String>("op") {
        Some(explicit) => explicit.clone(),
        None => match last_op_id(&writer) {
            Some(id) => id,
            None => return fail(json, "no recorded operation to undo"),
        },
    };
    let outcome = if force {
        writer.undo_force(&op_id, None)
    } else {
        writer.undo(&op_id, None)
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return fail(json, &error_message(&error)),
    };

    // A created file is undone by moving it to the trash, never a hard delete
    // (ADR-016 §8): hand each back to a trash operation of its own.
    let hand_back_failures = hand_back_created(&writer, &outcome);
    if !hand_back_failures.is_empty() {
        let mut message = format!(
            "undid {} but {} created file(s) could not be moved to the trash:",
            outcome.op_id,
            hand_back_failures.len()
        );
        for failure in &hand_back_failures {
            message.push_str(&format!("\n  {failure}"));
        }
        return fail(json, &message);
    }
    if json {
        let doc = serde_json::json!({
            "schema": "dimagine.undo/0.1",
            "op": outcome.op_id,
            "undone": outcome.undone.len(),
            "skipped": outcome.skipped.len(),
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        println!("undid {}", outcome.op_id);
        for skipped in &outcome.skipped {
            eprintln!(
                "  skipped {}: {}",
                skipped.path.display(),
                undo_reason_text(skipped.reason)
            );
        }
    }
    ExitCode::from(0)
}

/// The most recent op id in the audit log (undo with no explicit id).
fn last_op_id(writer: &Writer) -> Option<String> {
    writer
        .op_log_entries()
        .ok()?
        .into_iter()
        .next_back()
        .map(|entry| entry.op_id)
}

/// Hand every file a created operation left behind back to the trash, one
/// operation each (ADR-016 §8: a create is undone through the trash, never a
/// hard delete).
///
/// Every hand-back is attempted: one failing must not abandon the rest, which
/// would leave the earlier files trashed and the later ones still in the
/// library. The messages of the ones that failed come back together for the
/// caller to report.
fn hand_back_created(writer: &Writer, outcome: &UndoOutcome) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    for item in &outcome.undone {
        if let UndoneItem::TrashThis(created) = item {
            if let Err(message) = trash_one(writer, created) {
                failures.push(format!("{}: {message}", created.text()));
            }
        }
    }
    failures
}

/// Move one file into the trash as its own group (the undo of a create).
///
/// The slot is chosen before the move, exactly as `run_trash` chooses one: the
/// file's own name is the group name, and when the round-1 name is taken the
/// hand-back takes the next free round. Without that, a taken slot left the
/// record naming another group's trash file — two records over one file, which
/// the restore index cannot resolve.
fn trash_one(writer: &Writer, path: &RelPath) -> Result<(), String> {
    let (_, fp) = writer.read(path).map_err(|error| error_message(&error))?;
    let op_id = new_op_id();
    let trashed_at = chrono::Utc::now().to_rfc3339();
    let round = free_trash_round(writer, path.name(), path, std::slice::from_ref(path))?;
    let bumped = bumped_name(path.name(), round);
    let trash = TrashPath::for_original(path)
        .sibling(&bumped)
        .map_err(|e| e.to_string())?;
    let record_path = TrashRecord::path_for(path)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;
    let record = TrashRecord {
        op_id: op_id.clone(),
        trashed_at,
        files: vec![TrashedFile {
            original: path.text(),
            trash: trash.text(),
            mtime_ns: fp.mtime_ns,
        }],
    };
    let record_bytes = record.to_bytes().map_err(|e| error_message(&e))?;
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: record_path.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(record_bytes),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Move,
            src: Some(path.text()),
            dst: trash.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        },
    ];
    writer
        .execute_op(
            &op_id,
            SOURCE,
            None,
            serde_json::json!({"action": "trash", "undo_of_create": true}),
            steps,
        )
        .map_err(|error| error_message(&error))
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn run_config(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let file_text = match writer.read_settings() {
        Ok(Some(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => None,
        Err(error) => return fail(json, &error.to_string()),
    };
    match sub.subcommand() {
        Some(("list", _)) => {
            let settings = Settings::resolve(file_text.as_deref());
            if json {
                print_settings_json(&settings);
            } else {
                print_settings_human(&settings);
            }
            ExitCode::from(0)
        }
        Some(("get", get)) => {
            let key = get.get_one::<String>("key").unwrap();
            let settings = Settings::resolve(file_text.as_deref());
            match settings.get(key) {
                Some(value) => {
                    if json {
                        let doc = serde_json::json!({
                            "schema": "dimagine.config/0.1", "key": key, "value": value,
                        });
                        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
                    } else {
                        println!("{} = {}", key, value);
                    }
                    ExitCode::from(0)
                }
                None => fail(json, &unknown_key_message(key)),
            }
        }
        Some(("set", set)) => {
            let key = set.get_one::<String>("key").unwrap();
            let raw = set.get_one::<String>("value").unwrap();
            let value = match raw.parse::<i64>() {
                Ok(number) => serde_json::Value::from(number),
                Err(_) => return fail(json, &format!("{key} takes an integer, not {raw}")),
            };
            let change = settings::Change::Set(value);
            match settings::apply(file_text.as_deref(), &[(key.as_str(), change)]) {
                Ok(text) => write_settings(json, &writer, &text),
                Err(error) => fail(json, &apply_error_message(&error)),
            }
        }
        Some(("reset", reset)) => {
            let key = reset.get_one::<String>("key").unwrap();
            match settings::apply(
                file_text.as_deref(),
                &[(key.as_str(), settings::Change::Reset)],
            ) {
                Ok(text) => write_settings(json, &writer, &text),
                Err(error) => fail(json, &apply_error_message(&error)),
            }
        }
        _ => ExitCode::from(2),
    }
}

fn write_settings(json: bool, writer: &Writer, text: &str) -> ExitCode {
    match writer.write_settings(text.as_bytes()) {
        Ok(()) => {
            if json {
                let doc = serde_json::json!({"schema": "dimagine.config/0.1", "written": text});
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
            } else {
                println!("settings written");
            }
            ExitCode::from(0)
        }
        Err(error) => fail(json, &error_message(&error)),
    }
}

fn unknown_key_message(key: &str) -> String {
    let allowed: Vec<&str> = settings::schema().iter().map(|spec| spec.key).collect();
    format!("unknown setting {key}; allowed: {}", allowed.join(", "))
}

fn apply_error_message(error: &settings::ApplyError) -> String {
    match error {
        settings::ApplyError::Unparsable { message } => {
            format!("settings file is not parsable: {message}")
        }
        settings::ApplyError::UnknownKey { key, allowed } => format!(
            "unknown setting {key}; allowed: {}",
            if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            }
        ),
        settings::ApplyError::InvalidValue {
            key,
            value,
            message,
        } => format!("value {value} for {key} is not allowed: {message}"),
    }
}

fn print_settings_human(settings: &Settings) {
    for spec in settings::schema() {
        let value = settings.get(spec.key).cloned().unwrap_or_default();
        let changed = value != spec.default;
        let mark = if changed { ", changed" } else { "" };
        println!(
            "{} = {}  (default {}{})",
            spec.key, value, spec.default, mark
        );
    }
    for key in &settings.report.invalid {
        eprintln!("warning: {key} is set to an invalid value and uses its default");
    }
    for key in &settings.report.unknown {
        eprintln!("note: {key} is not a known setting");
    }
    if let Some(message) = &settings.report.parse_error {
        eprintln!("warning: settings.json is not parsable, all defaults apply: {message}");
    }
}

fn print_settings_json(settings: &Settings) {
    let entries: Vec<serde_json::Value> = settings::schema()
        .iter()
        .map(|spec| {
            let value = settings.get(spec.key).cloned().unwrap_or_default();
            serde_json::json!({
                "key": spec.key,
                "value": value,
                "default": spec.default,
                "changed": value != spec.default,
                "description": spec.description_en,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "schema": "dimagine.config/0.1",
        "settings": entries,
        "invalid": settings.report.invalid,
        "unknown": settings.report.unknown,
        "parse_error": settings.report.parse_error,
    });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
}

// ---------------------------------------------------------------------------
// trash purge / trash empty
// ---------------------------------------------------------------------------

fn run_purge(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let dry_run = sub.get_flag("dry_run");
    let yes = sub.get_flag("yes");
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let retention = match read_retention_days(&writer, json) {
        Ok(days) => days,
        Err(code) => return code,
    };
    let records = match writer.trash_records() {
        Ok(records) => records,
        Err(error) => return fail(json, &error_message(&error)),
    };

    let now = chrono::Utc::now();
    let due: Vec<FoundRecord> = records
        .iter()
        .filter_map(|scanned| {
            let record = scanned.record.as_ref().ok()?;
            if !retention_days_elapsed(now, &record.trashed_at, retention) {
                return None;
            }
            found_record(scanned, record).ok()
        })
        .collect();

    if dry_run {
        report_purge(json, &due, true, PURGE_SCHEMA);
        return ExitCode::from(0);
    }
    if !yes {
        if json {
            let doc = serde_json::json!({
                "schema": PURGE_SCHEMA, "due": due.len(), "deleted": 0,
                "note": "no group deleted: pass --yes to delete, --dry-run to list",
            });
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else if due.is_empty() {
            println!("nothing to purge");
        } else {
            println!(
                "{} group(s) are past retention; re-run with --yes to delete them",
                due.len()
            );
        }
        return ExitCode::from(0);
    }

    let intent = serde_json::json!({"action": "purge", "retention_days": retention});
    match delete_groups(&writer, json, &due, intent) {
        Ok(()) => {}
        Err(code) => return code,
    }
    report_purge(json, &due, false, PURGE_SCHEMA);
    ExitCode::from(0)
}

/// `trash empty`: delete every recorded group now, whatever its age
/// (ADR-017 §7 "立即清空"). One of only two irreversible operations in the
/// product, so it takes an explicit `--yes`; without one it only lists what
/// would go. Groups with no record (Obsidian's own trash files) are never
/// touched — the same rule retention purge follows.
fn run_empty(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    let yes = sub.get_flag("yes");
    let writer = match open_writer(library_dir, json) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let records = match writer.trash_records() {
        Ok(records) => records,
        Err(error) => return fail(json, &error_message(&error)),
    };
    let due: Vec<FoundRecord> = records
        .iter()
        .filter_map(|scanned| {
            let record = scanned.record.as_ref().ok()?;
            found_record(scanned, record).ok()
        })
        .collect();

    if !yes {
        if json {
            let doc = serde_json::json!({
                "schema": TRASH_EMPTY_SCHEMA, "due": due.len(), "deleted": 0,
                "note": "nothing deleted: pass --yes to empty the trash permanently",
            });
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else if due.is_empty() {
            println!("the trash is already empty");
        } else {
            println!(
                "{} group(s) in the trash; re-run with --yes to delete them permanently",
                due.len()
            );
        }
        return ExitCode::from(0);
    }

    let intent = serde_json::json!({"action": "trash_empty"});
    match delete_groups(&writer, json, &due, intent) {
        Ok(()) => {}
        Err(code) => return code,
    }
    report_purge(json, &due, false, TRASH_EMPTY_SCHEMA);
    ExitCode::from(0)
}

/// The retention setting, read once per purge run (ADR-019).
fn read_retention_days(writer: &Writer, json: bool) -> Result<i64, ExitCode> {
    let file_text = match writer.read_settings() {
        Ok(Some(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => None,
        Err(error) => return Err(fail(json, &error_message(&error))),
    };
    Ok(Settings::resolve(file_text.as_deref()).trash_retention_days())
}

/// Permanently delete whole groups (every file plus the record) as one
/// journaled operation. No bytes are recorded, so undoing one refuses as
/// `Permanent` — these are the two irreversible operations (ADR-018 §8).
///
/// A group can be broken: a file its record names may already be gone (the
/// state `check` reports as a missing file). That file's deletion is already
/// done, so its step is left out rather than refusing the whole batch — one
/// broken group must never keep the others in the trash forever.
fn delete_groups(
    writer: &Writer,
    json: bool,
    groups: &[FoundRecord],
    intent: serde_json::Value,
) -> Result<(), ExitCode> {
    let mut steps: Vec<StepRecord> = Vec::new();
    for record in groups {
        for file in &record.files {
            if !step_target_exists(writer, &file.trash) {
                continue;
            }
            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Delete,
                src: None,
                dst: file.trash.clone(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: None,
            });
        }
        if step_target_exists(writer, &record.path.text()) {
            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Delete,
                src: None,
                dst: record.path.text(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: None,
            });
        }
    }
    if steps.is_empty() {
        return Ok(());
    }
    let op_id = new_op_id();
    writer
        .execute_op(&op_id, SOURCE, None, intent, steps)
        .map_err(|error| fail(json, &error_message(&error)))
}

/// Whether a step's target is on disk. A delete step can only be journaled for
/// a file that exists (its before-hash is read from it), so a missing target
/// means the deletion already happened.
fn step_target_exists(writer: &Writer, text: &str) -> bool {
    StepPath::parse(text)
        .map(|path| writer.read_step(&path).is_ok())
        .unwrap_or(false)
}

/// Whether a trashed group has passed the retention window (ADR-017 §7).
/// A retention of 0 never deletes; an unparsable timestamp is never deleted.
fn retention_days_elapsed(
    now: chrono::DateTime<chrono::Utc>,
    trashed_at: &str,
    retention: i64,
) -> bool {
    if retention <= 0 {
        return false;
    }
    let Ok(trashed) = chrono::DateTime::parse_from_rfc3339(trashed_at) else {
        return false;
    };
    (now - trashed.with_timezone(&chrono::Utc)).num_days() >= retention
}

fn report_purge(json: bool, due: &[FoundRecord], dry_run: bool, schema: &str) {
    let groups: Vec<String> = due.iter().map(|record| record.path.text()).collect();
    if json {
        let doc = serde_json::json!({
            "schema": schema,
            "dry_run": dry_run,
            "groups": groups,
        });
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
    } else {
        if groups.is_empty() {
            println!("nothing to purge");
        }
        for group in groups {
            let verb = if dry_run { "would delete" } else { "deleted" };
            println!("{verb} {group}");
        }
    }
}

// ---------------------------------------------------------------------------
// check — write-state health (ADR-016 §7, ADR-017 §7, ADR-018 §7).
// ---------------------------------------------------------------------------

/// The codes the write-state checks emit, snake_case like the core codes.
pub mod wcodes {
    pub const JOURNAL_LEFTOVER: &str = "write_journal_leftover";
    pub const ORPHAN_TEMP: &str = "write_orphan_temp";
    pub const TRASH_SYMLINK: &str = "write_trash_symlink";
    pub const TRASH_RECORD_MISSING_FILE: &str = "write_trash_record_missing_file";
    pub const TRASH_FILE_NO_RECORD: &str = "write_trash_file_no_record";
    pub const TRASH_DOUBLE_CLAIM: &str = "write_trash_double_claim";
    pub const UNPARSABLE_RECORD: &str = "write_trash_unparsable_record";
    pub const LEGACY_TRASH: &str = "write_legacy_trash";
    pub const SERVICE_UNWRITABLE: &str = "write_service_unwritable";
}

/// Report what the service user could not write (ADR-018 §7): the fourth of
/// the four deployment checks. A write lands by writing a dot-temp in a note's
/// directory and renaming it over the target, and by keeping its lock, journal
/// and log under `.dimagine/` — so a directory the service cannot write makes
/// every file in it unwritable, and an unwritable note cannot be updated even
/// when its directory is fine.
///
/// Read-only: it asks the kernel with `access(W_OK)` and never touches a file.
/// It reports as the process runs it — the exact answer when `check` runs as
/// the service user, and the operator's own view otherwise.
pub fn append_service_writability(report: &mut CheckReport, library_dir: &Path, library: &Library) {
    // The derived folder holds the lock, journal and log every operation
    // needs. Without it the write core cannot commit anything, so it is the
    // first thing to test; when it does not exist yet, creating it needs the
    // library root itself.
    let state_dir = library_dir.join(".dimagine");
    if state_dir.exists() {
        if !dimagine_write::fs::access_writable(&state_dir) {
            report.findings.push(Finding {
                severity: Severity::Error,
                code: wcodes::SERVICE_UNWRITABLE,
                path: ".dimagine/".to_string(),
                path_non_utf8: false,
                message: "the service user cannot write the derived folder, so no operation can \
                          take its lock or journal"
                    .to_string(),
                detail: None,
            });
        }
    } else if !dimagine_write::fs::access_writable(library_dir) {
        report.findings.push(Finding {
            severity: Severity::Error,
            code: wcodes::SERVICE_UNWRITABLE,
            path: ".".to_string(),
            path_non_utf8: false,
            message: "the service user cannot write the library root, so it cannot create the \
                      derived folder `.dimagine/`"
                .to_string(),
            detail: None,
        });
    }

    // Each directory is asked once: a note write is a temp-plus-rename inside
    // the directory, so an unwritable directory blocks every note in it and a
    // per-file check would only repeat that. A note whose own file is not
    // writable, in a directory that is, is reported against the note. Paths are
    // built from the root plus the entry's library-relative path, so a
    // root-level note sits in the root and never in an empty directory.
    let mut bad_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut checked_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut unwritable_notes: Vec<(String, bool)> = Vec::new();
    for entry in &library.files {
        if !matches!(
            entry.class,
            FileClass::ImageNote | FileClass::Note | FileClass::Canvas
        ) {
            continue;
        }
        let absolute = library_dir.join(&entry.rel);
        let lossy = absolute.to_str().is_none();
        let dir = absolute.parent().map(Path::to_path_buf);
        let dir_ok = match &dir {
            Some(dir) => {
                if !checked_dirs.contains(dir) {
                    checked_dirs.insert(dir.clone());
                    if !dimagine_write::fs::access_writable(dir) {
                        bad_dirs.insert(dir.clone());
                        report.findings.push(Finding {
                            severity: Severity::Error,
                            code: wcodes::SERVICE_UNWRITABLE,
                            path: format!("{}/", display_rel(library_dir, dir)),
                            path_non_utf8: dir.to_str().is_none(),
                            message: "the service user cannot write this folder, so it cannot \
                                      replace a note in it"
                                .to_string(),
                            detail: None,
                        });
                        false
                    } else {
                        true
                    }
                } else {
                    !bad_dirs.contains(dir)
                }
            }
            None => true,
        };
        if dir_ok && !dimagine_write::fs::access_writable(&absolute) {
            unwritable_notes.push((entry.rel.clone(), lossy));
        }
    }
    for (rel, lossy) in unwritable_notes {
        report.findings.push(Finding {
            severity: Severity::Error,
            code: wcodes::SERVICE_UNWRITABLE,
            path: rel,
            path_non_utf8: lossy,
            message: "the service user cannot write this note".to_string(),
            detail: None,
        });
    }
}

/// The old trash location, `.dimagine/trash/`, which ADR-017 §7 keeps readable
/// but never writes to again. A library can hold one only if some tool wrote
/// there before the location changed; dimagine does not create it.
const LEGACY_TRASH_DIR: &str = ".dimagine/trash";

/// Append write-state findings to a read `check` report. Read-only: it
/// inspects the filesystem but never opens the library through the write core.
pub fn append_write_state(report: &mut CheckReport, library_dir: &Path) {
    let root = library_dir;
    match std::fs::symlink_metadata(root.join(TRASH_DIR)) {
        Ok(meta) if meta.file_type().is_symlink() => report.findings.push(Finding {
            severity: Severity::Error,
            code: wcodes::TRASH_SYMLINK,
            path: format!("{TRASH_DIR}/"),
            path_non_utf8: false,
            message: ".trash is a symlink; the write core refuses to follow it".to_string(),
            detail: None,
        }),
        _ => {}
    }

    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Which record claims which trash file: a file claimed twice cannot be
    // restored (the resolver answers "ambiguous") and is reported.
    let mut claims: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let trash_root = root.join(TRASH_DIR);
    if trash_root.is_dir() {
        let mut trashed_files: Vec<PathBuf> = Vec::new();
        collect_trash_files(&trash_root, &mut trashed_files);
        for path in &trashed_files {
            if path
                .file_name()
                .map(|n| n.to_string_lossy().ends_with(".trash.json"))
                .unwrap_or(false)
            {
                check_trash_record(root, path, report, &mut referenced, &mut claims);
            }
        }
        for path in &trashed_files {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name.ends_with(".trash.json") {
                continue;
            }
            let rel = display_rel(root, path);
            if referenced.contains(&rel) {
                continue;
            }
            // A lone note may be another tool's trash. An image, its note or a
            // raw sidecar is the shape dimagine's own group move leaves behind,
            // and with no record neither restore nor purge can reach it — a
            // warning, not info (ADR-017 §7 only ever manages recorded groups).
            let (severity, message) = match classify(&name) {
                FileClass::Image | FileClass::ImageNote | FileClass::Raw => (
                    Severity::Warning,
                    "a file under .trash has no dimagine record; it is the shape of a \
                     dimagine group, and neither restore nor purge can reach it",
                ),
                _ => (
                    Severity::Info,
                    "a file under .trash has no dimagine record (an Obsidian file, or a \
                     group whose record is gone)",
                ),
            };
            report.findings.push(Finding {
                severity,
                code: wcodes::TRASH_FILE_NO_RECORD,
                path: rel,
                path_non_utf8: false,
                message: message.to_string(),
                detail: None,
            });
        }
    }

    let journal_root = root.join(".dimagine").join("journal");
    if journal_root.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&journal_root) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let rel = display_rel(root, &entry.path());
                    report.findings.push(Finding {
                        severity: Severity::Error,
                        code: wcodes::JOURNAL_LEFTOVER,
                        path: rel,
                        path_non_utf8: false,
                        message: "an operation journal was left unrecovered; a live writer may \
                                  still be running, or a crash needs review"
                            .to_string(),
                        detail: None,
                    });
                }
            }
        }
    }

    let mut temps: Vec<PathBuf> = Vec::new();
    collect_temp_files(root, &mut temps);
    for path in temps {
        let rel = display_rel(root, &path);
        report.findings.push(Finding {
            severity: Severity::Warning,
            code: wcodes::ORPHAN_TEMP,
            path: rel,
            path_non_utf8: false,
            message: "a write temporary file was left behind".to_string(),
            detail: None,
        });
    }

    append_legacy_trash_findings(report, root);
}

/// Report whatever is left in the old `.dimagine/trash/` (ADR-017 §7 keeps it
/// readable, never written). Nothing here deletes or moves: the location is
/// read-only compatibility, so the finding tells the user what is there and
/// that dimagine will not manage it again.
fn append_legacy_trash_findings(report: &mut CheckReport, root: &Path) {
    let legacy_root = root.join(".dimagine").join("trash");
    if !legacy_root.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    collect_trash_files(&legacy_root, &mut files);
    if files.is_empty() {
        return;
    }
    let mut relatives: Vec<String> = files.iter().map(|path| display_rel(root, path)).collect();
    relatives.sort();
    let count = relatives.len();
    report.findings.push(Finding {
        severity: Severity::Info,
        code: wcodes::LEGACY_TRASH,
        path: format!("{LEGACY_TRASH_DIR}/"),
        path_non_utf8: false,
        message: format!(
            "{count} file(s) are in the old trash location; dimagine no longer writes there, \
             and neither restores nor purges them — move them back by hand"
        ),
        detail: Some(serde_json::json!({ "files": relatives })),
    });
}

fn check_trash_record(
    root: &Path,
    record_path: &Path,
    report: &mut CheckReport,
    referenced: &mut std::collections::HashSet<String>,
    claims: &mut std::collections::HashMap<String, String>,
) {
    let rel_path = display_rel(root, record_path);
    let Ok(bytes) = std::fs::read(record_path) else {
        return;
    };
    let Ok(record) = TrashRecord::parse(&rel_path, &bytes) else {
        report.findings.push(Finding {
            severity: Severity::Error,
            code: wcodes::UNPARSABLE_RECORD,
            path: rel_path,
            path_non_utf8: false,
            message: "a trash record does not parse; its group is neither restored nor purged"
                .to_string(),
            detail: None,
        });
        return;
    };
    for file in &record.files {
        referenced.insert(file.trash.clone());
        match claims.get(&file.trash) {
            Some(owner) if owner != &rel_path => report.findings.push(Finding {
                severity: Severity::Error,
                code: wcodes::TRASH_DOUBLE_CLAIM,
                path: file.trash.clone(),
                path_non_utf8: false,
                message: format!(
                    "{owner} and {rel_path} both claim {}; restore cannot tell which group it belongs to",
                    file.trash
                ),
                detail: None,
            }),
            _ => {
                claims.insert(file.trash.clone(), rel_path.clone());
            }
        }
        if !root.join(&file.trash).exists() {
            report.findings.push(Finding {
                severity: Severity::Error,
                code: wcodes::TRASH_RECORD_MISSING_FILE,
                path: file.trash.clone(),
                path_non_utf8: false,
                message: format!("trash record {rel_path} names a file that is not in the trash"),
                detail: None,
            });
        }
    }
}

fn collect_trash_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_trash_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

fn collect_temp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if name == ".git" || name == ".obsidian" {
            continue;
        }
        if meta.is_dir() {
            collect_temp_files(&path, out);
        } else if name.starts_with(dimagine_write::fs::TMP_PREFIX) {
            out.push(path);
        }
    }
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// A write failure a person can read.
///
/// A partial batch names each step that did not land, one per line, and an
/// undo refusal lists the files that block it — rather than dumping the Rust
/// structure of the error (`Vec<BatchStepError>` prints its `Debug`).
fn error_message(error: &dimagine_write::Error) -> String {
    match error {
        dimagine_write::Error::BatchPartial { op, errors } => {
            let mut message = format!("operation {op} partially applied:");
            for step in errors {
                message.push_str(&format!("\n  {}: {}", step.path.display(), step.message));
            }
            message
        }
        dimagine_write::Error::UndoRefused { op, mismatches } => {
            let mut message = format!("undo of {op} refused:");
            for mismatch in mismatches {
                message.push_str(&format!(
                    "\n  {}: {}",
                    mismatch.path.display(),
                    undo_reason_text(mismatch.reason)
                ));
            }
            message
        }
        other => other.to_string(),
    }
}

/// Why one file blocks an undo, in words (ADR-016 §8 lists the paths).
fn undo_reason_text(reason: dimagine_write::UndoReason) -> &'static str {
    use dimagine_write::UndoReason::*;
    match reason {
        Changed => "changed since the operation",
        Reverted => "already back at its earlier bytes",
        Missing => "missing",
        SourceBack => "the move's source is back",
        NoRecord => "no recorded bytes to restore",
        Permanent => "permanently deleted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrite a journal's pid to a dead one, so recovery is allowed to act on
    /// it: the journal was written by this live test process.
    fn kill_journal_writer(root: &Path, op_id: &str) {
        let meta_file = root.join(".dimagine/journal").join(op_id).join("meta.json");
        let mut meta: dimagine_write::journal::JournalMeta =
            serde_json::from_str(&std::fs::read_to_string(&meta_file).unwrap()).unwrap();
        meta.pid = 0;
        std::fs::write(&meta_file, serde_json::to_string(&meta).unwrap()).unwrap();
    }

    /// A multi-image `trash` is one operation of many steps, and the journal
    /// names each step file from `step_idx`. Numbering per group let a later
    /// group's step files rename-replace an earlier group's, so a crash left
    /// only the last group visible to recovery and the earlier ones
    /// half-applied with nothing journaled about them.
    #[test]
    fn trash_numbers_every_step_of_the_operation_in_one_sequence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), b"aaa").unwrap();
        std::fs::write(dir.path().join("b.png"), b"bbb").unwrap();
        let writer = Writer::open(dir.path()).unwrap();
        let library = Library::open(dir.path()).unwrap();
        let resolver = Resolver::new(&library.files);
        let op_id = new_op_id();
        let images = vec!["a.png".to_string(), "b.png".to_string()];
        let (steps, trashed) = plan_trash_steps(
            &writer,
            &library.files,
            &resolver,
            &images,
            &op_id,
            "2026-10-13T00:00:00Z",
        )
        .unwrap();
        assert_eq!(trashed, vec!["a.png", "b.png"]);
        assert_eq!(steps.len(), 4, "a record and a move per group");
        let mut indices: Vec<usize> = steps.iter().map(|step| step.step_idx).collect();
        indices.sort_unstable();
        assert_eq!(
            indices,
            vec![0, 1, 2, 3],
            "one sequence across the whole operation"
        );

        // Crash after the second step: the journal must describe all four
        // steps, not just the group whose numbers were written last.
        let crash = writer.execute_op_with_crash_injection(
            &op_id,
            "test",
            None,
            serde_json::json!({"action": "trash"}),
            steps,
            Some(1),
        );
        assert!(crash.is_err());
        let op_dir = dir.path().join(".dimagine/journal").join(&op_id);
        let mut names: Vec<String> = std::fs::read_dir(&op_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "meta.json",
                "step_0000.json",
                "step_0001.json",
                "step_0002.json",
                "step_0003.json",
            ],
            "every step of the operation has its own journal file"
        );

        // Recovery sees both groups: the landed steps roll back, so both
        // images are back in the library and neither is left in the trash.
        kill_journal_writer(dir.path(), &op_id);
        let report = writer.startup_sweep().unwrap();
        assert!(
            matches!(
                &report.recovery[0],
                dimagine_write::RecoveryOutcome::RolledBack { steps: 2, .. }
            ),
            "{:?}",
            report.recovery
        );
        assert_eq!(std::fs::read(dir.path().join("a.png")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dir.path().join("b.png")).unwrap(), b"bbb");
        assert!(!op_dir.exists(), "recovery closed the journal");
    }

    /// A partial batch reaches the user as text: the Rust structure of
    /// `Vec<BatchStepError>` must not be printed at them.
    #[test]
    fn a_partial_batch_is_rendered_as_text() {
        let error = dimagine_write::Error::BatchPartial {
            op: "01H".to_string(),
            errors: vec![dimagine_write::BatchStepError {
                step_idx: 1,
                path: PathBuf::from(".trash/one.png"),
                message: "already exists".to_string(),
                landed_unaudited: false,
            }],
        };
        let text = error_message(&error);
        assert!(text.contains("partially applied"), "{text}");
        assert!(text.contains(".trash/one.png: already exists"), "{text}");
        assert!(!text.contains("BatchStepError"), "{text}");
    }

    /// One created file that cannot be handed back must not stop the others
    /// from reaching the trash.
    #[test]
    fn every_created_file_is_handed_back_even_when_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"a").unwrap();
        std::fs::write(dir.path().join("c.md"), b"c").unwrap();
        let writer = Writer::open(dir.path()).unwrap();
        let outcome = UndoOutcome {
            op_id: "01H".to_string(),
            undone: vec![
                UndoneItem::TrashThis(RelPath::parse("a.md").unwrap()),
                // Not on disk: its hand-back cannot read it.
                UndoneItem::TrashThis(RelPath::parse("b.md").unwrap()),
                UndoneItem::TrashThis(RelPath::parse("c.md").unwrap()),
            ],
            skipped: Vec::new(),
        };
        let failures = hand_back_created(&writer, &outcome);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("b.md:"), "{failures:?}");
        // The files that could be handed back were, in spite of the failure.
        assert!(!dir.path().join("a.md").exists());
        assert!(!dir.path().join("c.md").exists());
        assert!(dir.path().join(".trash/a.md").exists());
        assert!(dir.path().join(".trash/a.md.trash.json").exists());
        assert!(dir.path().join(".trash/c.md").exists());
        assert!(dir.path().join(".trash/c.md.trash.json").exists());
    }
}
