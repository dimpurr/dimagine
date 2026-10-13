//! Writing links and editing collection bodies (ADR-017 §4, §5, §6).
//!
//! Every function here is pure: it takes bytes, the files of one library walk
//! and the [`Resolver`] built from them, and returns new bytes or a typed
//! refusal. Nothing opens a file, so the same semantics can be reused by the
//! CLI, by HTTP and by an agent without a second implementation.
//!
//! The rules these functions implement:
//!
//! - a link a tool writes names the image unambiguously: a full library path
//!   for an image in a folder, a bare name for one at the library root, which
//!   is also its full path. When a root image's bare name is shared by more
//!   than one file there is no such text, so adding it is refused rather than
//!   guessed (FORMAT §5.1 rule 3 stands unchanged);
//! - membership is decided by re-resolving the links of a note, never by
//!   comparing strings: aliases, headings, block references, Markdown images,
//!   percent-encoding and `<>`-wrapped targets all count, and links inside
//!   fenced code or inline code never do;
//! - removing a member takes the member line, and only that line: captions,
//!   headings and every other line stay.

use std::ops::Range;
use std::path::Component;

use crate::format;
use crate::library::{FileClass, FileEntry};
use crate::links::{extract_link_spans, Outcome, Resolver};

/// The library an edit runs against.
///
/// Files are addressed by index into [`BodyCtx::files`] — the same indexes
/// [`Resolver`] reports — because two entries can display the same lossy path
/// while being different files on disk.
#[derive(Clone, Copy, Debug)]
pub struct BodyCtx<'a> {
    pub files: &'a [FileEntry],
    pub resolver: &'a Resolver,
}

impl<'a> BodyCtx<'a> {
    /// Build the context for one library walk.
    pub fn new(files: &'a [FileEntry], resolver: &'a Resolver) -> Self {
        Self { files, resolver }
    }

    /// Index of the entry with this exact library-relative path.
    pub fn index_of(&self, rel: &str) -> Option<usize> {
        self.files.iter().position(|entry| entry.rel == rel)
    }
}

/// Why an edit cannot be performed. Every variant means: leave the file
/// untouched and report this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// The note bytes are not valid UTF-8.
    NotUtf8,
    /// The target file is not a Markdown note.
    NotANote,
    /// A path or index does not name a file of the expected class.
    NotInLibrary,
    /// An unclosed fenced code block starts at this byte offset, so an embed
    /// appended at the end would land inside code (ADR-017 §4).
    UnclosedFence(usize),
    /// The image cannot be named by any link text: its path holds characters
    /// that break wikilinks (FORMAT §2.1), which tools report and never rename.
    UnlinkableName { path: String, characters: Vec<char> },
    /// A component of the image's path is not valid UTF-8 on disk, so no text
    /// link can name it without risking a different file.
    NonUtf8Path,
    /// The note is the image's own note and the hit is its self-embed, which
    /// body edits must never delete (ADR-017 §2).
    SelfEmbed,
    /// A root image whose bare name matches more than one file: the caller
    /// must rename it or move it into a folder before it can be collected.
    AmbiguousBareName { name: String, matches: Vec<String> },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NotUtf8 => write!(f, "note is not valid UTF-8"),
            EditError::NotANote => write!(f, "target is not a Markdown note"),
            EditError::NotInLibrary => write!(f, "target is not a library file of the right class"),
            EditError::UnclosedFence(off) => {
                write!(f, "unclosed code block starts at byte {off}")
            }
            EditError::UnlinkableName { path, characters } => write!(
                f,
                "name {path} contains {} that breaks links",
                characters
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            EditError::NonUtf8Path => {
                write!(f, "path has a non-UTF-8 component and cannot be linked")
            }
            EditError::SelfEmbed => {
                write!(f, "an image note's self-embed cannot be removed")
            }
            EditError::AmbiguousBareName { name, matches } => write!(
                f,
                "bare name {name} matches {} files: rename it or move it into a folder",
                matches.len()
            ),
        }
    }
}

impl std::error::Error for EditError {}

/// The text a tool writes to embed an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Embed {
    /// Embed with exactly this link text: `![[refs/a/x.jpg]]` for an image in
    /// a folder, `![[x.jpg]]` for one at the library root.
    Text(String),
    /// Refusal: an image at the library root whose bare name matches more than
    /// one file. No link text names it unambiguously, so the caller must
    /// rename it or move it into a folder first (ADR-017 §4).
    RenameOrMove { name: String, matches: Vec<String> },
}

/// The link text to embed image `image` (ADR-017 §4).
///
/// A nested image always gets its full library path, which cannot become
/// ambiguous later. A root image's full path is its bare name, so the name
/// must match exactly one file in the library; if it matches more, this
/// returns [`Embed::RenameOrMove`] and the caller must not add the image
/// anywhere.
pub fn link_text_for(image: usize, ctx: &BodyCtx) -> Result<Embed, EditError> {
    let entry = ctx.files.get(image).ok_or(EditError::NotInLibrary)?;
    if entry.class != FileClass::Image {
        return Err(EditError::NotInLibrary);
    }
    let path = exact_rel(entry).ok_or(EditError::NonUtf8Path)?;
    let banned = format::banned_characters(&path);
    if !banned.is_empty() {
        return Err(EditError::UnlinkableName {
            path,
            characters: banned,
        });
    }
    if !path.contains('/') {
        let matches = ctx.resolver.bare_name_hits(&entry.name);
        if matches.len() > 1 {
            return Ok(Embed::RenameOrMove {
                name: entry.name.clone(),
                matches: matches
                    .iter()
                    .filter_map(|&idx| ctx.files.get(idx))
                    .map(|hit| hit.rel.clone())
                    .collect(),
            });
        }
    }
    Ok(Embed::Text(format!("![[{path}]]")))
}

/// The library-relative path of an entry spelled exactly as on disk: every
/// component must be valid UTF-8, or no link text can name it safely. A lossy
/// display path is never used, because the replacement character could alias a
/// different real name.
fn exact_rel(entry: &FileEntry) -> Option<String> {
    let mut out = String::new();
    for component in entry.path.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let text = part.to_str()?;
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(text);
    }
    (!out.is_empty()).then_some(out)
}

/// One embed of the target image in a note, and how an editor may take it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberHit {
    /// 0-based line index within the note.
    pub line: usize,
    /// Byte range of the link text itself.
    pub range: Range<usize>,
    /// Byte range of the whole line, terminator included.
    pub line_range: Range<usize>,
    /// True when the link is all the line holds, so the line is a member line
    /// and deleting it deletes nothing of the user's (ADR-017 §5). Another
    /// embed, another link or any prose on the line makes this false, and the
    /// editor then cuts out only the link text.
    pub whole_line: bool,
}

/// Every embed of image `image` in note `note`, resolved rather than compared
/// as text (ADR-017 §4, §5). Aliases, headings, block references, Markdown
/// images, percent-encoded and `<>`-wrapped targets all count; links inside
/// fenced code blocks or inline code spans do not.
///
/// A hit whose line holds the link and nothing else is a member line
/// ([`MemberHit::whole_line`]); on a line shared with another embed, another
/// link or prose, only the link text may be cut out.
pub fn member_spans(
    body: &[u8],
    note: usize,
    image: usize,
    ctx: &BodyCtx<'_>,
) -> Result<Vec<MemberHit>, EditError> {
    let (text, lines) = parsed(body, note, ctx)?;
    let entry = ctx.files.get(note).ok_or(EditError::NotInLibrary)?;
    let spans = extract_link_spans(text, &entry.rel, 1);
    let mut candidates: Vec<(usize, Range<usize>, bool)> = Vec::new();
    for span in &spans {
        if !span.syntax.is_strong_image() || span.range.end > text.len() {
            continue;
        }
        let Some(line) = line_of(&lines, span.range.start) else {
            continue;
        };
        if ctx.resolver.resolve(&span.target, entry, span.syntax) == Outcome::Resolved(image) {
            let alone = link_is_all_of(text, &lines[line], &span.range);
            candidates.push((line, span.range.clone(), alone));
        }
    }
    Ok(candidates
        .into_iter()
        .map(|(line, range, whole_line)| MemberHit {
            line,
            range,
            line_range: lines[line].start..lines[line].end,
            whole_line,
        })
        .collect())
}

/// True when nothing but whitespace surrounds the link on its line: the line
/// is a member line, and taking it out takes nothing else with it.
///
/// A line that carries another embed, another link or a piece of prose is not
/// one, however many links resolve to the target (ADR-017 §5).
fn link_is_all_of(text: &str, line: &Line, range: &Range<usize>) -> bool {
    let start = range.start.clamp(line.start, line.content_end);
    let end = range.end.clamp(start, line.content_end);
    text[line.start..start]
        .bytes()
        .chain(text[end..line.content_end].bytes())
        .all(|byte| byte.is_ascii_whitespace())
}

/// True when the image is already embedded in the note, outside code.
pub fn is_member(
    body: &[u8],
    note: usize,
    image: usize,
    ctx: &BodyCtx<'_>,
) -> Result<bool, EditError> {
    Ok(!member_spans(body, note, image, ctx)?.is_empty())
}

/// Re-resolve written bytes and confirm they embed `image` (ADR-017 §4: after
/// writing, parse again and check the new link really points at the target
/// image, otherwise roll the step back).
pub fn verify_points_to(
    new_bytes: &[u8],
    note: usize,
    image: usize,
    ctx: &BodyCtx<'_>,
) -> Result<bool, EditError> {
    is_member(new_bytes, note, image, ctx)
}

/// True when `note` is the image's own note (its `<image>.<ext>.md`).
fn is_own_note(entry: &FileEntry, image: usize, ctx: &BodyCtx<'_>) -> bool {
    let Some(image_entry) = ctx.files.get(image) else {
        return false;
    };
    entry.class == FileClass::ImageNote
        && entry.paired_image_path().as_deref() == Some(image_entry.path.as_path())
}

/// Parse note bytes into text plus its line map.
fn parsed<'a>(
    body: &'a [u8],
    note: usize,
    ctx: &BodyCtx<'_>,
) -> Result<(&'a str, Vec<Line>), EditError> {
    let entry = ctx.files.get(note).ok_or(EditError::NotInLibrary)?;
    if !matches!(entry.class, FileClass::Note | FileClass::ImageNote) {
        return Err(EditError::NotANote);
    }
    let text = std::str::from_utf8(body).map_err(|_| EditError::NotUtf8)?;
    Ok((text, split_lines(text)))
}

/// A line of the note: `[start, end)` covers the terminator, `content_end`
/// excludes it.
#[derive(Clone, Copy, Debug)]
struct Line {
    start: usize,
    content_end: usize,
    end: usize,
}

impl Line {
    /// True for the last line of a file that does not end in a newline.
    fn unterminated(&self) -> bool {
        self.content_end == self.end
    }
}

fn split_lines(text: &str) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            lines.push(Line {
                start,
                content_end,
                end: i + 1,
            });
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        lines.push(Line {
            start,
            content_end: bytes.len(),
            end: bytes.len(),
        });
    }
    lines
}

fn line_of(lines: &[Line], offset: usize) -> Option<usize> {
    let idx = lines.partition_point(|line| line.start <= offset);
    idx.checked_sub(1)
}

/// The note's own line terminator, taken from its first terminator so a CRLF
/// note keeps CRLF (ADR-017 §4, §5).
fn newline_style(text: &str) -> &'static str {
    let bytes = text.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            return if i > 0 && bytes[i - 1] == b'\r' {
                "\r\n"
            } else {
                "\n"
            };
        }
    }
    "\n"
}

/// The result of [`append_member`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appended {
    /// The new bytes of the note.
    pub bytes: Vec<u8>,
    /// Byte range of the added embed line in `bytes`, terminator included.
    /// `None` when the image was already a member, so nothing changed.
    pub added: Option<Range<usize>>,
}

/// Add one embed line at the end of a collection note (ADR-017 §4).
///
/// A no-op when the image is already a member, so the same intent retried does
/// not add a second embed. A non-empty note that does not end in a newline
/// gets one first, in the note's own terminator style. An unclosed fenced code
/// block is refused, because the append would land inside code.
pub fn append_member(
    body: &[u8],
    note: usize,
    image: usize,
    ctx: &BodyCtx<'_>,
) -> Result<Appended, EditError> {
    let (text, _) = parsed(body, note, ctx)?;
    if !member_spans(body, note, image, ctx)?.is_empty() {
        return Ok(Appended {
            bytes: body.to_vec(),
            added: None,
        });
    }
    if let Some(offset) = crate::links::unclosed_fence(text) {
        return Err(EditError::UnclosedFence(offset));
    }
    let embed = match link_text_for(image, ctx)? {
        Embed::Text(text) => text,
        Embed::RenameOrMove { name, matches } => {
            return Err(EditError::AmbiguousBareName { name, matches })
        }
    };
    let term = newline_style(text);
    let mut out = String::with_capacity(text.len() + embed.len() + term.len() * 2);
    out.push_str(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(term);
    }
    let added = out.len()..out.len() + embed.len() + term.len();
    out.push_str(&embed);
    out.push_str(term);
    Ok(Appended {
        bytes: out.into_bytes(),
        added: Some(added),
    })
}

/// The result of [`remove_member`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    /// The new bytes of the note.
    pub bytes: Vec<u8>,
    /// Byte ranges taken out of the original, ascending: member lines with
    /// their terminator, plus the spliced link text of an inline embed.
    pub removed: Vec<Range<usize>>,
}

/// Take the image out of a collection note (ADR-017 §5).
///
/// A member line — a line whose only image link resolves to the target — is
/// deleted whole, together with one blank line directly below it when deleting
/// would otherwise leave two consecutive blanks. Nothing else is ever deleted:
/// a caption, a heading or any other line stays, including when it sits
/// directly under the member line. When the embed shares its line with another
/// embed, only that link's own text is cut out.
pub fn remove_member(
    body: &[u8],
    note: usize,
    image: usize,
    ctx: &BodyCtx<'_>,
) -> Result<Removed, EditError> {
    let entry = ctx.files.get(note).ok_or(EditError::NotInLibrary)?;
    let (text, lines) = parsed(body, note, ctx)?;
    let hits = member_spans(body, note, image, ctx)?;
    if hits.is_empty() {
        return Ok(Removed {
            bytes: body.to_vec(),
            removed: Vec::new(),
        });
    }
    if is_own_note(entry, image, ctx) {
        // FORMAT §3.2: the self-embed of an image note is a preview of its own
        // picture, and no body edit may remove it (ADR-017 §2).
        return Err(EditError::SelfEmbed);
    }
    let blank = |i: usize| text[lines[i].start..lines[i].content_end].trim().is_empty();

    let mut deleted = vec![false; lines.len()];
    let mut splices: Vec<Vec<Range<usize>>> = vec![Vec::new(); lines.len()];
    for hit in &hits {
        if hit.whole_line {
            deleted[hit.line] = true;
        } else {
            splices[hit.line].push(hit.range.clone());
        }
    }
    for (i, ranges) in splices.iter_mut().enumerate() {
        if ranges.is_empty() {
            continue;
        }
        ranges.sort_by_key(|range| range.start);
        let left = cut(text, &lines[i], ranges);
        if left.trim().is_empty() {
            // The line existed for its embeds alone; a whitespace-only remnant
            // is not content, so the line goes.
            deleted[i] = true;
            ranges.clear();
        }
    }
    // The blank below a member line goes with it, but only one: the rule is
    // judged from the lines that were marked as member lines, so a blank
    // removed this way does not trigger the rule again for its own successor.
    let member_lines = deleted.clone();
    for (i, was_member) in member_lines.iter().enumerate() {
        if !was_member {
            continue;
        }
        let next = i + 1;
        if next >= lines.len() || deleted[next] || !blank(next) {
            continue;
        }
        let previous_survivor = (0..i).rev().find(|&p| !deleted[p]);
        if previous_survivor.is_some_and(blank) {
            deleted[next] = true;
        }
    }

    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let mut removed: Vec<Range<usize>> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if deleted[i] {
            removed.push(line.start..line.end);
            continue;
        }
        if splices[i].is_empty() {
            out.extend_from_slice(&body[line.start..line.end]);
            continue;
        }
        let mut cursor = line.start;
        for range in &splices[i] {
            out.extend_from_slice(&body[cursor..range.start]);
            removed.push(range.clone());
            cursor = range.end;
        }
        out.extend_from_slice(&body[cursor..line.end]);
    }
    // A note whose last line had no terminator keeps that shape when that line
    // is what got removed: the previous line's terminator goes with it.
    if let Some(last) = lines.len().checked_sub(1) {
        if deleted[last] && lines[last].unterminated() && out.ends_with(b"\n") {
            if let Some(previous) = (0..last).rev().find(|&p| !deleted[p]) {
                let tail = &lines[previous];
                if tail.content_end < tail.end {
                    removed.push(tail.content_end..tail.end);
                    out.truncate(out.len() - (tail.end - tail.content_end));
                }
            }
        }
    }
    removed.sort_by_key(|range| range.start);
    Ok(Removed {
        bytes: out,
        removed,
    })
}

/// The text of one line with the given absolute ranges cut out.
fn cut(text: &str, line: &Line, ranges: &[Range<usize>]) -> String {
    let mut out = String::new();
    let mut cursor = line.start;
    for range in ranges {
        out.push_str(&text[cursor..range.start]);
        cursor = range.end;
    }
    out.push_str(&text[cursor..line.content_end]);
    out
}

/// The content of a new collection note (ADR-017 §6): front matter with
/// `kind: collection` and the title, a blank line, then one embed per image in
/// the order given.
///
/// The image list is refused whole when any image cannot be named
/// unambiguously, and which one is reported, so a half-written collection is
/// never created.
pub fn new_collection_body(
    title: &str,
    images: &[usize],
    ctx: &BodyCtx<'_>,
) -> Result<Vec<u8>, EditError> {
    let mut out = String::from("---\nkind: collection\n");
    let title = single_line(title);
    if !title.is_empty() {
        out.push_str("title: ");
        out.push_str(&yaml_scalar(&title));
        out.push('\n');
    }
    out.push_str("---\n\n");
    for &image in images {
        let embed = match link_text_for(image, ctx)? {
            Embed::Text(text) => text,
            Embed::RenameOrMove { name, matches } => {
                return Err(EditError::AmbiguousBareName { name, matches })
            }
        };
        out.push_str(&embed);
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// The file name for a new collection note titled `title` (ADR-017 §6): the
/// characters that break wikilinks, path separators, leading dots and control
/// characters are stripped; what is left empty is named `untitled`. The result
/// is a Markdown file name, because a collection note is one.
pub fn safe_file_name(title: &str) -> String {
    let stripped: String = title
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '[' | ']' | '#' | '^' | '|' | '/' | '\\'))
        .collect();
    let stem = stripped
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim()
        .to_string();
    let stem = if stem.is_empty() {
        "untitled".to_string()
    } else {
        stem
    };
    format!("{stem}.md")
}

/// The title as it can sit on one front matter line.
fn single_line(value: &str) -> String {
    let collapsed: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A single-line YAML scalar: plain when nothing could be misread in it,
/// double-quoted otherwise. Those are the two shapes the writable front matter
/// subset accepts (ADR-016 §5).
fn yaml_scalar(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value != value.trim()
        || value.contains([
            ':', '#', '"', '\\', '[', ']', '{', '}', ',', '&', '*', '!', '|', '>', '@', '%', '`',
        ])
        || value.starts_with(['-', '?', '.', ' '])
        || value.chars().any(char::is_control)
        || value.parse::<f64>().is_ok()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
        );
    if !needs_quotes {
        return value.to_string();
    }
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            c if char::is_control(c) => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Why a folder was chosen for a new file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderSource {
    /// The configured folder, accepted, possibly re-spelled to match a folder
    /// that already exists in the library.
    Setting,
    /// The caller's current folder, because the setting asked for it.
    Current,
    /// Library root: nothing usable was configured, or the file is missing
    /// (Obsidian's default, and the fallback ADR-017 §6 prescribes).
    RootFallback,
    /// `inbox/`: the fallback landing place for uploads (ADR-017 §11).
    InboxFallback,
}

/// A resolved folder choice: a library-relative path with no trailing
/// separator, where `""` is the library root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderChoice {
    pub folder: String,
    pub source: FolderSource,
}

impl FolderChoice {
    fn new(folder: String, source: FolderSource) -> Self {
        Self { folder, source }
    }

    /// Join the folder with a file name.
    pub fn join(&self, name: &str) -> String {
        if self.folder.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", self.folder, name)
        }
    }
}

/// Where a new collection note goes, following `.obsidian/app.json`
/// (ADR-017 §6).
///
/// `app_json` is the file's text, `None` when there is no such file;
/// `current_folder` is the caller's current folder as the interface defines it
/// (folder view, the collection's own folder, or the library root). Anything
/// unusable falls back to the library root. dimagine only ever reads
/// `.obsidian/`.
pub fn collection_folder(
    app_json: Option<&str>,
    current_folder: &str,
    ctx: &BodyCtx<'_>,
) -> FolderChoice {
    let root = FolderChoice::new(String::new(), FolderSource::RootFallback);
    let Some(settings) = settings(app_json) else {
        return root;
    };
    let current = normalize_folder(current_folder, ctx);
    match settings
        .get("newFileLocation")
        .and_then(serde_json::Value::as_str)
    {
        Some("root") => FolderChoice::new(String::new(), FolderSource::Setting),
        Some("current") => match current {
            Some(folder) => FolderChoice::new(folder, FolderSource::Current),
            None => root,
        },
        Some("folder") => {
            let raw = settings
                .get("newFileFolderPath")
                .and_then(serde_json::Value::as_str);
            match raw.and_then(|raw| resolve_folder(raw, current.as_deref(), ctx)) {
                Some(folder) => {
                    let source = source_for(raw.unwrap_or_default(), &folder, current.as_deref());
                    FolderChoice::new(folder, source)
                }
                None => root,
            }
        }
        // No file, no setting, or a spelling dimagine does not know.
        _ => root,
    }
}

/// Where an upload lands, following `attachmentFolderPath` in
/// `.obsidian/app.json` (ADR-017 §11).
///
/// The spellings Obsidian writes are all supported: `/` for the library root,
/// `./` or `.` for the current folder, `./sub` for a subfolder of the current
/// folder, and a plain library-relative path. Anything outside the library,
/// absolute, escaping through `..`, pointing at a dot folder or unparsable
/// falls back to `inbox/`.
pub fn upload_folder(
    app_json: Option<&str>,
    current_folder: &str,
    ctx: &BodyCtx<'_>,
) -> FolderChoice {
    let inbox = FolderChoice::new("inbox".to_string(), FolderSource::InboxFallback);
    let Some(settings) = settings(app_json) else {
        return inbox;
    };
    let current = normalize_folder(current_folder, ctx);
    let Some(raw) = settings
        .get("attachmentFolderPath")
        .and_then(serde_json::Value::as_str)
    else {
        return inbox;
    };
    match resolve_folder(raw, current.as_deref(), ctx) {
        Some(folder) => {
            let source = source_for(raw, &folder, current.as_deref());
            FolderChoice::new(folder, source)
        }
        None => inbox,
    }
}

/// `./`-relative spellings are answered by the current folder; anything else
/// the setting names is the setting's own answer.
fn source_for(raw: &str, folder: &str, current: Option<&str>) -> FolderSource {
    if written_relative_to_current(raw) && current == Some(folder) {
        FolderSource::Current
    } else {
        FolderSource::Setting
    }
}

fn written_relative_to_current(raw: &str) -> bool {
    raw.split(['/', '\\']).next() == Some(".")
}

/// Parse the setting text; anything that is not a JSON object is no setting.
fn settings(app_json: Option<&str>) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = app_json?;
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .as_object()
        .cloned()
}

/// The caller's current folder, if it is a usable library folder.
fn normalize_folder(current: &str, ctx: &BodyCtx<'_>) -> Option<String> {
    resolve_folder(current, None, ctx)
}

/// One configured folder spelled the way Obsidian spells it: `None` when it is
/// unusable — absolute, escaping the library with `..`, pointing into a dot
/// folder such as `.git`, `.dimagine`, `.obsidian` or `.trash`, naming a file
/// instead of a folder, or holding a NUL (ADR-018 §4).
///
/// `current` grounds a `./`-relative spelling; without one, `./sub` is not
/// usable. An accepted path is re-spelled to match a folder that already
/// exists, compared NFC + case-insensitively, so a case difference cannot
/// create a second folder on a case-insensitive filesystem.
fn resolve_folder(raw: &str, current: Option<&str>, ctx: &BodyCtx<'_>) -> Option<String> {
    if raw.contains('\0') {
        return None;
    }
    let bytes = raw.as_bytes();
    let absolute = bytes.first().is_some_and(|b| *b == b'/' || *b == b'\\');
    if absolute && raw != "/" {
        return None;
    }
    if raw == "/" {
        return Some(String::new());
    }
    if raw.contains(':') {
        return None; // a URL scheme or a Windows drive letter
    }
    let mut written = raw.split(['/', '\\']).peekable();
    let mut segments: Vec<String> = Vec::new();
    // A leading `.` means "relative to the folder we were told about".
    if written.peek() == Some(&".") {
        let current = current?;
        segments = current
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(str::to_string)
            .collect();
        written.next();
    }
    for segment in written.filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            // `..` escapes the library and a dot folder belongs to another
            // tool, so neither is ever a target (ADR-018 §4).
            ".." => return None,
            other if other.starts_with('.') => return None,
            other => segments.push(other.to_string()),
        }
    }
    let spellings = folder_spellings(ctx.files);
    let file_keys = ctx
        .files
        .iter()
        .filter_map(exact_rel)
        .map(|rel| crate::links::key(&rel))
        .collect::<std::collections::HashSet<_>>();
    let mut out = String::new();
    for segment in &segments {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(segment);
        let lookup = crate::links::key(&out);
        if file_keys.contains(&lookup) {
            return None; // a file cannot be a folder
        }
        if let Some(existing) = spellings.get(&lookup) {
            out = existing.clone();
        }
    }
    Some(out)
}

/// Every folder in the library, keyed by [`crate::links::key`], with the
/// spelling it already has on disk.
fn folder_spellings(files: &[FileEntry]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for entry in files {
        let Some(rel) = exact_rel(entry) else {
            continue;
        };
        let mut prefix = String::new();
        for part in rel.split('/').take(rel.matches('/').count()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            map.entry(crate::links::key(&prefix))
                .or_insert_with(|| prefix.clone());
        }
    }
    map
}
