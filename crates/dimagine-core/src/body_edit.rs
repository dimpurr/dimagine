//! Writing links and editing note bodies (ADR-017 §4, §5, §6, §9, §10).
//!
//! Every function here is pure: it takes bytes, the files of one library walk
//! and the [`Resolver`] built from them, and returns new bytes or a typed
//! refusal. Nothing opens a file, so the same semantics can be reused by the
//! CLI, by HTTP and by an agent without a second implementation.
//!
//! The rules these functions implement:
//!
//! - a body edit (ADR-017 §10) replaces the bytes after the closing front
//!   matter fence and nothing before it: the front matter is carried over
//!   verbatim, never re-serialized;
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
//!   headings and every other line stay;
//! - an image note always carries the embed of its own picture, so a body edit
//!   that left it out gets one appended at the end (ADR-017 §2).

use std::ops::Range;
use std::path::Component;

use crate::fm_edit::{front_matter_range, FmRange};
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
    /// taking a member out must never delete (ADR-017 §2). A body edit, by
    /// contrast, restores the embed rather than refusing.
    SelfEmbed,
    /// A root image whose bare name matches more than one file: the caller
    /// must rename it or move it into a folder before it can be collected.
    AmbiguousBareName { name: String, matches: Vec<String> },
    /// The ordinals a reorder was given are not a permutation of the member
    /// blocks, so the reorder is refused whole (ADR-017 §9).
    NotAPermutation { given: Vec<usize>, blocks: usize },
    /// The block ordinal does not name a member block.
    NoSuchBlock { block: usize, blocks: usize },
    /// Line offset 0 is the member line itself: deleting it is what `--remove`
    /// does, so a caption edit must not (ADR-017 §9).
    MemberLineNotCaption { block: usize },
    /// The line offset names no line of the block, and is not one past its last
    /// line (the only place a new caption is added).
    NoSuchLine { block: usize, line: usize },
    /// A caption must be pure text (ADR-017 §9): not a heading, thematic
    /// break, setext underline, list, quote, table row (or delimiter row),
    /// code fence or embed.
    NotCaptionText { text: String },
    /// A caption line was given no text and there is nothing there to delete.
    NothingToDelete { block: usize, line: usize },
    /// A whole-body replacement was asked of a note whose opening `---` fence
    /// is never closed: there is no line that says where the front matter ends,
    /// so no byte range can be replaced without guessing (ADR-017 §10).
    UnterminatedFrontMatter,
    /// The new body is not valid UTF-8. A note is a text file and the reader
    /// decodes the whole thing, so bytes that are not text cannot become its
    /// body.
    BodyNotUtf8,
}

/// True when a line can be a caption line (ADR-017 §9): pure text, not a
/// heading, thematic break, setext underline, list item, blockquote, table
/// row (or delimiter row), code fence or embed.
fn is_caption_text(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    // An embed on the line — `![[..]]` or `![..](..)` — is never pure text,
    // wherever it sits (a link inside code does not count, as everywhere else).
    if extract_link_spans(content, "", 1)
        .iter()
        .any(|span| span.syntax.is_strong_image())
    {
        return false;
    }
    let left = content.trim_start();
    // ATX heading: one to six `#` followed by a space, a tab or the line end.
    let hashes = left.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = &left[hashes..];
        if rest.is_empty() || rest.starts_with([' ', '\t']) {
            return false;
        }
    }
    // Blockquote.
    if left.starts_with('>') {
        return false;
    }
    // Unordered list item.
    if left.starts_with("- ") || left.starts_with("* ") || left.starts_with("+ ") {
        return false;
    }
    // Ordered list item: digits then `.` or `)` then a space.
    let digits = left.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &left[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return false;
        }
    }
    // Table row.
    if left.starts_with('|') {
        return false;
    }
    // A thematic break (`---`, `***`, `___`, spaces between the marks allowed)
    // or a setext underline (`===`, `--`, `-`) restructures the note around the
    // caption: either one under the member line turns the member line and its
    // captions into a heading (ADR-017 §9).
    if is_rule_or_setext(left) {
        return false;
    }
    // A table delimiter row (`--- | ---`, `:-- | --:`) is a table row even
    // without a leading `|`.
    if is_table_delimiter(left) {
        return false;
    }
    // A fenced code block delimiter never captions a member.
    if left.starts_with("```") || left.starts_with("~~~") {
        return false;
    }
    true
}

/// True when the line is a thematic break or a setext underline: one mark and
/// nothing else on the line but spaces and tabs. `=` and `-` close a setext
/// heading at any length and take no space inside the run; `-`, `*` and `_` are
/// a thematic break at three or more marks, where a space between the marks is
/// allowed (`- - -`) (CommonMark).
fn is_rule_or_setext(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_' | '=') {
        return false;
    }
    let mut count = 0;
    let mut spaced = false;
    for c in trimmed.chars() {
        if c == first {
            count += 1;
        } else if matches!(c, ' ' | '\t') {
            spaced = true;
        } else {
            return false;
        }
    }
    if spaced {
        // A space inside the run rules out an underline, so only the thematic
        // break is left: three or more of `-`, `*` or `_` (`- - -`). `= = =` is
        // neither, so it stays plain text.
        matches!(first, '-' | '*' | '_') && count >= 3
    } else {
        // A solid run: `=` and `-` are a setext underline even at one mark,
        // while `*` and `_` are block syntax only from three on.
        matches!(first, '-' | '=') || count >= 3
    }
}

/// True when the line is a table delimiter row (GFM): cells of one or more
/// `-` with an optional leading and trailing `:`, separated by `|`. A leading
/// or trailing `|` contributes an empty edge cell; an empty middle cell means
/// the row is not a delimiter row.
fn is_table_delimiter(line: &str) -> bool {
    if !line.contains('|') {
        return false;
    }
    let cells: Vec<&str> = line.split('|').collect();
    for (i, raw) in cells.iter().enumerate() {
        let cell = raw.trim();
        if cell.is_empty() {
            if i == 0 || i + 1 == cells.len() {
                continue;
            }
            return false;
        }
        let core = cell.strip_prefix(':').unwrap_or(cell);
        let core = core.strip_suffix(':').unwrap_or(core);
        if core.is_empty() || !core.bytes().all(|b| b == b'-') {
            return false;
        }
    }
    true
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
            EditError::NotAPermutation { given, blocks } => write!(
                f,
                "the order {given:?} is not a permutation of the {blocks} member blocks"
            ),
            EditError::NoSuchBlock { block, blocks } => {
                write!(f, "block {block} does not exist ({blocks} member blocks)")
            }
            EditError::MemberLineNotCaption { block } => write!(
                f,
                "line 0 of block {block} is the member line: use --remove to delete it"
            ),
            EditError::NoSuchLine { block, line } => {
                write!(f, "block {block} has no caption line {line}")
            }
            EditError::NotCaptionText { text } => write!(
                f,
                "a caption must be plain text, not a heading, thematic break, setext underline, list, quote, table row or embed: {text:?}"
            ),
            EditError::NothingToDelete { block, line } => {
                write!(f, "block {block} has no caption line {line} to delete")
            }
            EditError::UnterminatedFrontMatter => write!(
                f,
                "the note opens a `---` fence that is never closed, so its body has no start"
            ),
            EditError::BodyNotUtf8 => write!(f, "the new body is not valid UTF-8"),
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
///
/// An image note's own embed is refused here, because taking an image out of a
/// collection has no business deleting the note that belongs to the picture; a
/// whole-body replacement ([`replace_body`]) is where that embed is restored if
/// the new text left it out (ADR-017 §2).
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
        // picture, and taking a member out is not a way to remove it
        // (ADR-017 §2).
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

// ---------------------------------------------------------------------------
// Member blocks: reorder and caption editing (ADR-017 §9)
// ---------------------------------------------------------------------------

/// One member block: a member line and the caption lines that directly follow
/// it (ADR-017 §9).
///
/// A *member line* is a line whose only content is one strong-image link that
/// resolves to an image outside code ([`member_blocks`] and ADR-017 §5). A
/// *caption line* is a pure-text line right after it — a heading, list item,
/// blockquote, table row, code fence or embed ends the block, and so does a
/// blank line. Blocks are addressed by ordinal in document order, because the
/// same image may appear twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberBlock {
    /// 0-based block ordinal, in document order.
    pub ordinal: usize,
    /// 0-based line index of the member line within the note.
    pub member_line: usize,
    /// The image the member line resolves to, as a library-relative path.
    pub image: String,
    /// Byte range of the whole block, member line through last caption line,
    /// terminators included.
    pub range: Range<usize>,
    /// The byte range of each line of the block, member line first, then the
    /// caption lines. `line_ranges[0]` is the member line, so line offsets into
    /// a caption are 1-based.
    pub line_ranges: Vec<Range<usize>>,
}

/// Every member block of a collection note, in document order (ADR-017 §9).
///
/// Equality and membership are resolved through the [`Resolver`], never
/// compared as text: aliases, headings, block references, Markdown images,
/// percent-encoding and `<>`-wrapped targets all count, and links inside
/// fenced code or inline code never do.
pub fn member_blocks(
    body: &[u8],
    note: usize,
    ctx: &BodyCtx<'_>,
) -> Result<Vec<MemberBlock>, EditError> {
    let (text, lines) = parsed(body, note, ctx)?;
    let entry = ctx.files.get(note).ok_or(EditError::NotInLibrary)?;
    let spans = extract_link_spans(text, &entry.rel, 1);

    // The member line of each strong-image link: a line holding exactly one
    // strong-image link that is all of the line and resolves to an image.
    let mut strong_per_line: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for span in &spans {
        if !span.syntax.is_strong_image() || span.range.end > text.len() {
            continue;
        }
        if let Some(line) = line_of(&lines, span.range.start) {
            *strong_per_line.entry(line).or_insert(0) += 1;
        }
    }
    let mut member_of: Vec<Option<usize>> = vec![None; lines.len()];
    for span in &spans {
        if !span.syntax.is_strong_image() || span.range.end > text.len() {
            continue;
        }
        let Some(line) = line_of(&lines, span.range.start) else {
            continue;
        };
        if strong_per_line.get(&line).copied() != Some(1) {
            continue;
        }
        if !link_is_all_of(text, &lines[line], &span.range) {
            continue;
        }
        if let Outcome::Resolved(idx) = ctx.resolver.resolve(&span.target, entry, span.syntax) {
            if ctx
                .files
                .get(idx)
                .is_some_and(|hit| hit.class == FileClass::Image)
            {
                member_of[line] = Some(idx);
            }
        }
    }
    // A line that carries any strong-image link is never pure text, however the
    // link resolves: it has an embed on it (ADR-017 §9).
    let embed_lines: std::collections::HashSet<usize> = strong_per_line.keys().copied().collect();

    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(image) = member_of[i] else {
            i += 1;
            continue;
        };
        let mut last = i;
        let mut j = i + 1;
        while j < lines.len()
            && !embed_lines.contains(&j)
            && is_caption_text(&text[lines[j].start..lines[j].content_end])
        {
            last = j;
            j += 1;
        }
        let range = lines[i].start..lines[last].end;
        let line_ranges: Vec<Range<usize>> =
            (i..=last).map(|k| lines[k].start..lines[k].end).collect();
        blocks.push(MemberBlock {
            ordinal: blocks.len(),
            member_line: i,
            image: ctx.files[image].rel.clone(),
            range,
            line_ranges,
        });
        i = last + 1;
    }
    Ok(blocks)
}

/// The length of the line terminator closing a byte range, 0 when it has none.
/// Only the note's final line can lack one, so this is how a range's shape is
/// read: `\n`, `\r\n` or nothing.
fn terminator_of(range: &[u8]) -> usize {
    let n = range.len();
    if n == 0 || range[n - 1] != b'\n' {
        return 0;
    }
    if n >= 2 && range[n - 2] == b'\r' {
        2
    } else {
        1
    }
}

/// Permute the member blocks within the byte ranges they already occupy
/// (ADR-017 §9).
///
/// `order` is a permutation: `order[slot]` is the block ordinal that lands in
/// the `slot`-th block range. Every byte outside the blocks — blank lines,
/// headings, prose, diagnostics — is reproduced unchanged and in order; only
/// the block contents change places. Anything that is not a permutation of the
/// blocks is refused whole.
///
/// A range's terminator stays with the range rather than travelling with its
/// block: only the note's last line may lack one, so an unterminated block
/// moved into the middle would otherwise glue its last line onto the text that
/// follows — a caption swallowing the next embed and the two blocks becoming
/// one. Each slot is emitted with its own terminator, which keeps every line
/// boundary and the note's trailing-newline shape as they were.
pub fn reorder_members(
    body: &[u8],
    note: usize,
    ctx: &BodyCtx<'_>,
    order: &[usize],
) -> Result<Vec<u8>, EditError> {
    let blocks = member_blocks(body, note, ctx)?;
    let n = blocks.len();
    let mut seen = vec![false; n];
    let valid = order.len() == n
        && order.iter().all(|&slot| {
            if slot >= n || seen[slot] {
                return false;
            }
            seen[slot] = true;
            true
        });
    if !valid {
        return Err(EditError::NotAPermutation {
            given: order.to_vec(),
            blocks: n,
        });
    }
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0;
    for (slot, block) in blocks.iter().enumerate() {
        out.extend_from_slice(&body[cursor..block.range.start]);
        let source = blocks[order[slot]].range.clone();
        let content_end = source.end - terminator_of(&body[source.clone()]);
        out.extend_from_slice(&body[source.start..content_end]);
        let slot_range = &body[block.range.clone()];
        let terminator = terminator_of(slot_range);
        if terminator > 0 {
            out.extend_from_slice(&slot_range[slot_range.len() - terminator..]);
        }
        cursor = block.range.end;
    }
    out.extend_from_slice(&body[cursor..]);
    Ok(out)
}

/// What a caption edit did, so the caller can report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionChange {
    /// An existing caption line's text was replaced.
    Edited,
    /// A new caption line was appended to the block.
    Added,
    /// An existing caption line was deleted.
    Deleted,
}

/// Add, edit or delete one caption line of a member block (ADR-017 §9).
///
/// `block` is the block ordinal and `line_offset` is 1-based within the block
/// (`0` is the member line, whose deletion is what `--remove` does, so it is
/// refused). `text` is the new caption; `None` deletes the line at that offset.
/// `line_offset == block line count` appends a new caption after the last line.
/// A caption that is not pure text — a heading, thematic break, setext
/// underline, list item, blockquote, table row (or delimiter row) or embed —
/// is refused, as is an out-of-range offset.
pub fn edit_caption(
    body: &[u8],
    note: usize,
    ctx: &BodyCtx<'_>,
    block: usize,
    line_offset: usize,
    text: Option<&str>,
) -> Result<(Vec<u8>, CaptionChange), EditError> {
    let (src, lines) = parsed(body, note, ctx)?;
    let blocks = member_blocks(body, note, ctx)?;
    let target = blocks.get(block).ok_or(EditError::NoSuchBlock {
        block,
        blocks: blocks.len(),
    })?;
    if line_offset == 0 {
        return Err(EditError::MemberLineNotCaption { block });
    }
    let caption_count = target.line_ranges.len() - 1;
    if line_offset > caption_count + 1 {
        return Err(EditError::NoSuchLine {
            block,
            line: line_offset,
        });
    }

    let newline = newline_style(src);
    let member_line = target.member_line;
    let last_line = member_line + caption_count;
    let mut out = Vec::with_capacity(body.len() + 64);
    out.extend_from_slice(&body[..lines[member_line].start]);
    // The member line, terminator included.
    out.extend_from_slice(&body[lines[member_line].start..lines[member_line].end]);

    let mut change = CaptionChange::Added;
    for offset in 1..=caption_count {
        let line = &lines[member_line + offset];
        if offset == line_offset {
            match text {
                Some(value) => {
                    if !is_caption_text(value) {
                        return Err(EditError::NotCaptionText {
                            text: value.to_string(),
                        });
                    }
                    change = CaptionChange::Edited;
                    out.extend_from_slice(value.as_bytes());
                    out.extend_from_slice(&body[line.content_end..line.end]);
                }
                None => {
                    change = CaptionChange::Deleted;
                }
            }
            continue;
        }
        out.extend_from_slice(&body[line.start..line.end]);
    }

    if line_offset == caption_count + 1 {
        let value = match text {
            Some(value) => value,
            None => {
                return Err(EditError::NothingToDelete {
                    block,
                    line: line_offset,
                })
            }
        };
        if !is_caption_text(value) {
            return Err(EditError::NotCaptionText {
                text: value.to_string(),
            });
        }
        // A block whose last line has no terminator (the note's final line)
        // needs one before a caption can follow it.
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.extend_from_slice(newline.as_bytes());
        }
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(newline.as_bytes());
        change = CaptionChange::Added;
    }

    out.extend_from_slice(&body[lines[last_line].end..]);
    Ok((out, change))
}

// ---------------------------------------------------------------------------
// Whole-body replacement (ADR-017 §10)
// ---------------------------------------------------------------------------

/// The result of [`replace_body`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyReplaced {
    /// The note's new bytes.
    pub bytes: Vec<u8>,
    /// True when `bytes` differ from the bytes as read.
    pub changed: bool,
    /// True when the new body held no embed of this image note's own picture,
    /// so one was appended at the end (ADR-017 §2).
    pub self_embed_restored: bool,
}

/// Replace everything after the front matter's closing fence, and nothing
/// before it (ADR-017 §10).
///
/// The split point is the end of that fence's line, as [`front_matter_range`]
/// reports it — the one delimiter rule the reader uses too — so the front matter
/// is carried into the result byte for byte and is never re-serialized. Line
/// endings, a BOM, and an absent trailing newline are the new body's own business:
/// these bytes are spliced, not normalized.
///
/// A note with no front matter is body from just past its BOM, so its body is
/// the whole file. A note whose opening fence is never closed is refused: nothing
/// says where its front matter ends, and guessing would rewrite user bytes. A
/// note that ends *at* its closing fence has an empty body and no line break
/// after the fence; the break is added, because a body spliced onto the fence
/// line would leave the reader with a note whose front matter never closes.
///
/// The result still satisfies the self-embed rule (ADR-017 §2): an image note
/// whose new body holds no embed resolving to its own picture gets the standard
/// embed line appended at the end. `note` and `ctx` exist for that check alone;
/// a plain note, or an image note whose picture is not in the library, is
/// spliced as given.
pub fn replace_body(
    note_bytes: &[u8],
    note: usize,
    new_body: &[u8],
    ctx: &BodyCtx<'_>,
) -> Result<BodyReplaced, EditError> {
    let entry = ctx.files.get(note).ok_or(EditError::NotInLibrary)?;
    if !matches!(entry.class, FileClass::Note | FileClass::ImageNote) {
        return Err(EditError::NotANote);
    }
    let text = std::str::from_utf8(note_bytes).map_err(|_| EditError::NotUtf8)?;
    std::str::from_utf8(new_body).map_err(|_| EditError::BodyNotUtf8)?;
    let (head_end, fence_closed_the_line) = match front_matter_range(note_bytes) {
        FmRange::NoFrontMatter { bom_len } => (bom_len, true),
        FmRange::Closed { body_start, .. } => {
            (body_start, note_bytes[..body_start].ends_with(b"\n"))
        }
        FmRange::Unterminated { .. } => return Err(EditError::UnterminatedFrontMatter),
    };
    let mut bytes = Vec::with_capacity(head_end + new_body.len() + 2);
    bytes.extend_from_slice(&note_bytes[..head_end]);
    if !fence_closed_the_line {
        // The closing fence was the file's last line and held no line break, so
        // the body had no line to start on. Splicing onto it directly would make
        // that line `---<new text>`, which is no longer a fence, and the reader
        // would then report the front matter as never closed. So the fence gets
        // the break it lacked, in the note's own style — the treatment an append
        // already gives a file whose last line has no terminator (ADR-017 §4).
        bytes.extend_from_slice(newline_style(text).as_bytes());
    }
    bytes.extend_from_slice(new_body);

    let mut self_embed_restored = false;
    if let Some(image) = own_image(note, ctx) {
        // The same append the collection rules use: no-op while an embed of the
        // picture resolves from the body, an unclosed fence refused rather than
        // written into.
        let appended = append_member(&bytes, note, image, ctx)?;
        if appended.added.is_some() {
            bytes = appended.bytes;
            self_embed_restored = true;
        }
    }
    let changed = bytes != note_bytes;
    Ok(BodyReplaced {
        bytes,
        changed,
        self_embed_restored,
    })
}

/// The library index of the picture paired with image note `note`, or `None`
/// when the note is not an image note or its picture is not in the library.
fn own_image(note: usize, ctx: &BodyCtx<'_>) -> Option<usize> {
    let entry = ctx.files.get(note)?;
    let paired = entry.paired_image_path()?;
    ctx.files
        .iter()
        .position(|candidate| candidate.class == FileClass::Image && candidate.path == paired)
}
