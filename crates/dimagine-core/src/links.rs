//! Extract links from notes and canvases, and resolve them against the
//! library (FORMAT §5.1, §6).
//!
//! Link kinds understood in 0.1:
//!
//! - Obsidian embeds `![[target]]` and wikilinks `[[target]]` (alias and
//!   `#heading` / `#^block` parts are dropped before resolving, like
//!   obsidian-export's `references.rs` does);
//! - standard Markdown images `![alt](path)`, including `<...>`-quoted
//!   and percent-encoded paths;
//! - JSON Canvas `file` nodes.
//!
//! Resolution follows FORMAT §5.1 exactly: a target with a folder part is a
//! path, first relative to the library root, then relative to the note's
//! folder; a bare name resolves only when exactly one file in the library has
//! that name, and is ambiguous otherwise. Comparison is NFC-normalised and
//! case-insensitive, which is also how duplicate-id detection compares.
//! Extensionless targets fall back to image stems, the way Obsidian resolves
//! `![[photo]]` to `photo.jpg`: one stem hit resolves, several are ambiguous,
//! none means the link does not point at an image (and is not reported).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::path::{Component, Path};

use percent_encoding::percent_decode_str;
use unicode_normalization::UnicodeNormalization;

use crate::format;
use crate::library::{FileClass, FileEntry};

/// The syntax a link was written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSyntax {
    /// `![[target]]`
    WikiEmbed,
    /// `[[target]]`
    WikiLink,
    /// `![alt](target)`
    MarkdownImage,
    /// A JSON Canvas `file` node.
    CanvasFileNode,
}

impl LinkSyntax {
    /// True for syntaxes that unambiguously mean "show an image": embeds,
    /// Markdown images and canvas file nodes.
    pub fn is_strong_image(&self) -> bool {
        matches!(
            self,
            LinkSyntax::WikiEmbed | LinkSyntax::MarkdownImage | LinkSyntax::CanvasFileNode
        )
    }
}

/// One extracted link.
#[derive(Clone, Debug)]
pub struct Link {
    pub syntax: LinkSyntax,
    /// The target as written between the brackets (alias and heading
    /// included), for messages.
    pub raw: String,
    /// Just the file reference: alias, heading and block parts stripped.
    pub target: String,
    /// 1-based line of the link in the note file.
    pub line: usize,
}

/// A canvas `file` node.
#[derive(Clone, Debug)]
pub struct CanvasRef {
    /// The `file` property as written in the canvas.
    pub file: String,
    pub node_id: Option<String>,
}

/// Result of resolving one link target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exactly one library file matches (index into `Library.files`).
    Resolved(usize),
    /// Several files match; the caller must not guess (FORMAT §5.1 rule 3).
    Ambiguous(Vec<usize>),
    /// Nothing matched, and the target claims to be an image.
    NotFound,
    /// The target cannot be an image link (targets a note, an external URL,
    /// or an extensionless name with no image stem match); never reported.
    NotImageTarget,
}

/// What a target's extension says about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetClass {
    /// An image extension (jpg, png, ...).
    Image,
    /// An extension whose files are not images but are still linked often
    /// (`.md`, `.canvas`, `.json`).
    Text,
    /// Some other recognized extension but claimed by image-syntax.
    Other,
    /// No extension at all.
    None,
}

pub fn target_class(target: &str) -> TargetClass {
    match format::file_extension(target) {
        Some(ext) if format::is_image_extension(ext) => TargetClass::Image,
        Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "md" | "canvas" | "json") => {
            TargetClass::Text
        }
        Some(_) => TargetClass::Other,
        None => TargetClass::None,
    }
}

/// One link as it is written in the text, before it is turned into a
/// [`Link`] or a [`SpanLink`].
enum RawHit<'a> {
    Wiki {
        /// The text between `[[` and `]]` (alias, heading and block included).
        content: &'a str,
        /// Byte range of the whole link, brackets included.
        range: Range<usize>,
        embed: bool,
        line: usize,
    },
    Markdown {
        /// The `alt` text between `![` and `]`.
        alt: &'a str,
        /// The destination, before angle brackets, titles, escapes and
        /// percent-decoding are handled.
        dest: &'a str,
        /// Everything after the destination up to the closing paren: the
        /// optional title.
        trailing: &'a str,
        range: Range<usize>,
        line: usize,
    },
}

/// Extract image-relevant links from Markdown text. `first_line` is the
/// 1-based line number of the first line of `body` in the enclosing file.
///
/// Fenced code blocks and inline code spans are skipped, like Obsidian's
/// renderer does.
pub fn extract_markdown_links(body: &str, first_line: usize) -> Vec<Link> {
    scan_raw(body, first_line)
        .into_iter()
        .map(|hit| match hit {
            RawHit::Wiki {
                content,
                embed,
                line,
                ..
            } => Link {
                syntax: if embed {
                    LinkSyntax::WikiEmbed
                } else {
                    LinkSyntax::WikiLink
                },
                raw: content.to_string(),
                target: strip_display_parts(content),
                line,
            },
            RawHit::Markdown { dest, line, .. } => {
                let mut link = Link {
                    syntax: LinkSyntax::MarkdownImage,
                    raw: dest.trim().to_string(),
                    target: String::new(),
                    line,
                };
                fill_markdown_dest(&mut link, dest);
                link
            }
        })
        .collect()
}

/// A link with the byte range it occupies in the note. This is what in-place
/// edits (moving a file, editing collection membership) need: the range says
/// exactly which bytes to replace, and the split-out display parts say what
/// must be preserved when they are replaced (ADR-017 §8).
///
/// Links inside fenced code blocks or inline code spans are never reported,
/// exactly as in [`extract_markdown_links`].
#[derive(Clone, Debug)]
pub struct SpanLink {
    /// Library-relative path of the note the link was found in, as given by
    /// the caller.
    pub note: String,
    /// Byte range of the whole link text in the note, brackets and `!`
    /// included: `![[a/x.jpg|alt]]`, `![alt](a/x.jpg "t")`.
    pub range: Range<usize>,
    pub syntax: LinkSyntax,
    /// `true` for `![[..]]` and `![..](..)`, `false` for a plain `[[..]]`.
    pub embed: bool,
    /// The file reference with alias, heading, block, title and percent
    /// encoding removed — the string to hand to [`Resolver::resolve`].
    pub target: String,
    /// The `|label` part of a wikilink, or the `alt` text of a Markdown
    /// image.
    pub alias: Option<String>,
    /// The `#heading` part, without the `#`.
    pub heading: Option<String>,
    /// The `#^block` part, without the `#^`.
    pub block: Option<String>,
    /// The quoted title of a Markdown image, without the quotes.
    pub markdown_title: Option<String>,
    /// 1-based line of the link in the note file.
    pub line: usize,
}

/// Extract links with byte ranges. `note` is the library-relative path of the
/// file `body` came from, recorded on every span for the caller's report.
pub fn extract_link_spans(body: &str, note: &str, first_line: usize) -> Vec<SpanLink> {
    scan_raw(body, first_line)
        .into_iter()
        .map(|hit| match hit {
            RawHit::Wiki {
                content,
                range,
                embed,
                line,
            } => {
                let (heading, block) =
                    split_heading_block(content.split('|').next().unwrap_or("").trim());
                SpanLink {
                    note: note.to_string(),
                    range,
                    syntax: if embed {
                        LinkSyntax::WikiEmbed
                    } else {
                        LinkSyntax::WikiLink
                    },
                    embed,
                    target: strip_display_parts(content),
                    alias: wiki_alias(content),
                    heading,
                    block,
                    markdown_title: None,
                    line,
                }
            }
            RawHit::Markdown {
                alt,
                dest,
                trailing,
                range,
                line,
            } => {
                let mut link = Link {
                    syntax: LinkSyntax::MarkdownImage,
                    raw: dest.trim().to_string(),
                    target: String::new(),
                    line,
                };
                fill_markdown_dest(&mut link, dest);
                let (heading, block) = split_heading_block(markdown_inner(dest));
                SpanLink {
                    note: note.to_string(),
                    range,
                    syntax: LinkSyntax::MarkdownImage,
                    embed: true,
                    target: link.target,
                    alias: (!alt.is_empty()).then(|| alt.to_string()),
                    heading,
                    block,
                    markdown_title: markdown_title(trailing),
                    line,
                }
            }
        })
        .collect()
}

fn scan_raw(body: &str, first_line: usize) -> Vec<RawHit<'_>> {
    let bytes = body.as_bytes();
    let newline_at: Vec<usize> = body
        .bytes()
        .enumerate()
        .filter(|(_, b)| *b == b'\n')
        .map(|(i, _)| i)
        .collect();
    let line_at = |pos: usize| -> usize {
        let before = newline_at.partition_point(|&n| n < pos);
        first_line + before
    };
    let fences = fenced_ranges(body);
    let mut fence_i = 0usize;

    let mut hits = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // Jump over fenced code blocks.
        while fence_i < fences.len() && fences[fence_i].1 <= i {
            fence_i += 1;
        }
        if fence_i < fences.len() && fences[fence_i].0 <= i {
            i = fences[fence_i].1;
            continue;
        }
        match bytes[i] {
            b'\n' => i += 1,
            b'`' => {
                // Skip an inline code span on this line.
                let line_end = newline_at.partition_point(|&n| n < i).min(newline_at.len());
                let line_end = newline_at.get(line_end).map_or(body.len(), |&n| n);
                i = bytes[i + 1..line_end]
                    .iter()
                    .position(|&b| b == b'`')
                    .map(|off| i + 1 + off + 1)
                    .unwrap_or(line_end);
            }
            b'!' if i + 2 < bytes.len() && bytes[i + 1] == b'[' && bytes[i + 2] == b'[' => {
                match find_sub(bytes, b"]]", i + 3) {
                    Some(close) => {
                        hits.push(RawHit::Wiki {
                            content: &body[i + 3..close],
                            range: i..close + 2,
                            embed: true,
                            line: line_at(i),
                        });
                        i = close + 2;
                    }
                    None => i += 1,
                }
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                match find_byte(bytes, b']', i + 2) {
                    Some(alt_end) if alt_end + 1 < bytes.len() && bytes[alt_end + 1] == b'(' => {
                        match markdown_link_tail(bytes, alt_end + 2) {
                            Some((dest_start, dest_end, close)) => {
                                hits.push(RawHit::Markdown {
                                    alt: &body[i + 2..alt_end],
                                    dest: &body[dest_start..dest_end],
                                    trailing: &body[dest_end..close],
                                    range: i..close + 1,
                                    line: line_at(i),
                                });
                                i = close + 1;
                            }
                            None => i = alt_end + 1,
                        }
                    }
                    _ => i += 1,
                }
            }
            b'[' if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                match find_sub(bytes, b"]]", i + 2) {
                    Some(close) => {
                        hits.push(RawHit::Wiki {
                            content: &body[i + 2..close],
                            range: i..close + 2,
                            embed: false,
                            line: line_at(i),
                        });
                        i = close + 2;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    hits
}

/// Extract `file` targets from a parsed JSON Canvas document (FORMAT §6).
pub fn extract_canvas_refs(canvas: &serde_json::Value) -> Vec<CanvasRef> {
    let Some(nodes) = canvas.get("nodes").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    nodes
        .iter()
        .filter(|n| n.get("type").and_then(serde_json::Value::as_str) == Some("file"))
        .filter_map(|n| {
            let file = n.get("file")?.as_str()?.to_string();
            Some(CanvasRef {
                file,
                node_id: n
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Fill `raw` and `target` of a Markdown image link from the destination text
/// between the parentheses.
fn fill_markdown_dest(link: &mut Link, path_raw: &str) {
    let inner = markdown_inner(path_raw);
    link.raw = inner.to_string();
    let outer = strip_display_parts(inner)
        .replace("\\(", "(")
        .replace("\\)", ")");
    link.target = percent_decode_str(&outer)
        .decode_utf8()
        .map(|cow| cow.into_owned().trim().to_string())
        .unwrap_or_else(|_| outer.trim().to_string());
}

/// The file reference inside a Markdown image destination: angle brackets
/// unwrapped, otherwise cut at the first whitespace, because what follows is
/// a title. Paths with spaces must be percent-encoded or angle-quoted.
fn markdown_inner(path_raw: &str) -> &str {
    let path_raw = path_raw.trim();
    if let Some(rest) = path_raw.strip_prefix('<') {
        match rest.find('>') {
            Some(end) => &rest[..end],
            None => path_raw,
        }
    } else {
        match path_raw.find(char::is_whitespace) {
            Some(end) => &path_raw[..end],
            None => path_raw,
        }
    }
}

/// The `|label` part of a wikilink.
fn wiki_alias(content: &str) -> Option<String> {
    content
        .split_once('|')
        .map(|(_, alias)| alias.trim())
        .filter(|alias| !alias.is_empty())
        .map(str::to_string)
}

/// Split a file reference at its `#`: no heading or block when there is none,
/// the block id when the part starts with `^`, the heading otherwise.
fn split_heading_block(file_ref: &str) -> (Option<String>, Option<String>) {
    let Some((_, rest)) = file_ref
        .split('|')
        .next()
        .and_then(|file| file.split_once('#'))
    else {
        return (None, None);
    };
    if let Some(block) = rest.strip_prefix('^') {
        return (None, (!block.is_empty()).then(|| block.to_string()));
    }
    (!rest.is_empty())
        .then(|| rest.to_string())
        .map_or((None, None), |heading| (Some(heading), None))
}

/// The quoted title of a Markdown image: everything after the destination,
/// with the surrounding quotes removed.
fn markdown_title(trailing: &str) -> Option<String> {
    let t = trailing.trim();
    let bytes = t.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let close_pair = match bytes[0] {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    if bytes[bytes.len() - 1] != close_pair {
        return None;
    }
    let inner = t[1..t.len() - 1].trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

/// Drop the alias (`|label`) and the heading / block part (`#target`) of a
/// link, leaving the file reference. Both parts exist only after a `|` or `#`
/// that cannot legally appear inside a file name (FORMAT §2.1), so splitting
/// on the first occurrence is exact.
fn strip_display_parts(content: &str) -> String {
    let file = content
        .split('|')
        .next()
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    file
}

fn find_sub(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn find_byte(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack[from..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| p + from)
}

/// Parse the tail of an inline Markdown image, starting right after the
/// opening `(`: optional spaces or tabs, a destination, an optional quoted
/// title, and the closing paren. The whole tail must sit on one line and
/// follow the CommonMark inline-info shape, so the destination and the title
/// are handled separately. Brackets inside a quoted title are title text,
/// not nesting. Returns `(destination start, destination end, index of the
/// closing paren)`.
fn markdown_link_tail(bytes: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let mut i = from;
    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    let dest_start = i;
    let (dest_end, closed_here) = if bytes.get(i) == Some(&b'<') {
        // Angle-quoted destination: no line endings, no unescaped `<`/`>`.
        let mut j = i + 1;
        let dest_end = loop {
            match bytes.get(j) {
                None | Some(b'\n') | Some(b'<') => return None,
                Some(b'\\') => j = j.checked_add(2)?,
                Some(b'>') => break j + 1,
                Some(_) => j += 1,
            }
        };
        (dest_end, false)
    } else {
        // Bare destination: stops at ASCII whitespace or the unbalanced
        // closing paren; parentheses inside must stay balanced or escaped.
        let mut j = i;
        let mut depth = 0usize;
        loop {
            match bytes.get(j) {
                None | Some(b'\n') => return None,
                Some(b'\\') => {
                    j = j.checked_add(2)?;
                }
                Some(b' ' | b'\t') => break (j, false),
                Some(b'(') => {
                    depth += 1;
                    j += 1;
                }
                Some(b')') => {
                    if depth == 0 {
                        break (j, true);
                    }
                    depth -= 1;
                    j += 1;
                }
                Some(_) => j += 1,
            }
        }
    };
    if closed_here {
        // The destination ended at the paren that also closes the image.
        return Some((dest_start, dest_end, dest_end));
    }
    title_and_close(bytes, dest_start, dest_end)
}

/// After a destination that ended at whitespace (or after an angle-quoted
/// destination): an optional title in double quotes, single quotes or
/// parentheses, then the closing paren.
fn title_and_close(
    bytes: &[u8],
    dest_start: usize,
    dest_end: usize,
) -> Option<(usize, usize, usize)> {
    let mut k = dest_end;
    while matches!(bytes.get(k), Some(b' ' | b'\t')) {
        k += 1;
    }
    match bytes.get(k) {
        Some(b'"') | Some(b'\'') => {
            let quote = bytes[k];
            let mut t = k + 1;
            loop {
                match bytes.get(t) {
                    None | Some(b'\n') => return None,
                    Some(b'\\') => t = t.checked_add(2)?,
                    Some(byte) if *byte == quote => {
                        t += 1;
                        break;
                    }
                    Some(_) => t += 1,
                }
            }
            title_ends_at(bytes, dest_start, dest_end, t)
        }
        Some(b'(') => {
            let mut t = k + 1;
            let mut depth = 1usize;
            loop {
                match bytes.get(t) {
                    None | Some(b'\n') => return None,
                    Some(b'\\') => t = t.checked_add(2)?,
                    Some(b'(') => {
                        depth += 1;
                        t += 1;
                    }
                    Some(b')') => {
                        depth -= 1; // opening paren guaranteed depth >= 1
                        t += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Some(_) => t += 1,
                }
            }
            title_ends_at(bytes, dest_start, dest_end, t)
        }
        Some(b')') => Some((dest_start, dest_end, k)),
        _ => None,
    }
}

/// The title just ended at `from`; optional whitespace, then `)` must follow.
fn title_ends_at(
    bytes: &[u8],
    dest_start: usize,
    dest_end: usize,
    from: usize,
) -> Option<(usize, usize, usize)> {
    let mut k = from;
    while matches!(bytes.get(k), Some(b' ' | b'\t')) {
        k += 1;
    }
    match bytes.get(k) {
        Some(b')') => Some((dest_start, dest_end, k)),
        _ => None,
    }
}

/// Byte ranges of fenced code blocks (``` and ~~~), including their fences.
/// An unclosed fence swallows the rest of the file, like a renderer would.
fn fenced_ranges(text: &str) -> Vec<(usize, usize)> {
    fence_scan(text).0
}

/// The byte offset of the opening fence of an unclosed fenced code block, if
/// the text has one. Everything after it renders as code, so a writer that
/// appends an embed would put it inside a code block (ADR-017 §4: refuse).
pub fn unclosed_fence(text: &str) -> Option<usize> {
    fence_scan(text).1
}

fn fence_scan(text: &str) -> (Vec<(usize, usize)>, Option<usize>) {
    let opens_fence = |line: &str, c: u8| {
        let trimmed = line.trim_start_matches(' ').trim_end_matches(['\r', '\n']);
        trimmed.len() >= 3 && trimmed.as_bytes()[..3] == [c; 3]
    };
    let mut ranges = Vec::new();
    let mut fence_char: Option<u8> = None;
    let mut start = 0usize;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        match fence_char {
            None => {
                if opens_fence(line, b'`') {
                    fence_char = Some(b'`');
                    start = offset;
                } else if opens_fence(line, b'~') {
                    fence_char = Some(b'~');
                    start = offset;
                }
            }
            Some(c) => {
                if opens_fence(line, c) {
                    let end = offset + line.len();
                    ranges.push((start, end));
                    fence_char = None;
                }
            }
        }
        offset += line.len();
    }
    let unclosed = fence_char.map(|_| start);
    if fence_char.is_some() {
        ranges.push((start, text.len()));
    }
    (ranges, unclosed)
}

/// The comparison key for names and paths: NFC-normalised, lowercased.
pub fn key(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// One component of a native library-relative path, as a resolver key.
/// Components that are valid UTF-8 normalize like every other name
/// (NFC + lowercase); components that are not valid UTF-8 keep their native
/// bytes and can only be reached through a note-relative base, never by
/// link text, so a lossy display spelling can never alias them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PartKey {
    Text(String),
    Opaque(OsString),
}

fn part_key(component: &OsStr) -> PartKey {
    match component.to_str() {
        Some(text) => PartKey::Text(key(text)),
        None => PartKey::Opaque(component.to_os_string()),
    }
}

/// The resolver keys of every path component of `path`, skipping any
/// non-normal components (walked library paths only contain normal ones).
fn path_parts(path: &Path) -> Vec<PartKey> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part_key(part)),
            _ => None,
        })
        .collect()
}

/// Join a link target onto an optional native base and resolve `.` and `..`
/// segments. Returns `None` when the path escapes the library root.
fn join_parts(base: Option<&[PartKey]>, target: &str) -> Option<Vec<PartKey>> {
    let mut parts: Vec<PartKey> = base.map(|b| b.to_vec()).unwrap_or_default();
    for segment in target.split('/').filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            segment => parts.push(PartKey::Text(key(segment))),
        }
    }
    Some(parts)
}

/// Resolve link targets against the files of one library walk.
#[derive(Debug)]
pub struct Resolver {
    /// Full file name → file indexes, by [`key`].
    name_index: HashMap<String, Vec<usize>>,
    /// Image file name without extension → file indexes, by [`key`].
    image_stem_index: HashMap<String, Vec<usize>>,
    /// Native library-relative path components → file indexes. Keys are
    /// built from [`std::path::Path`] components, never from lossy display
    /// text, so different native folders (or a folder with a `\` in its
    /// name and the same path spelled with separators) never collide.
    path_index: HashMap<Vec<PartKey>, Vec<usize>>,
}

impl Resolver {
    pub fn new(files: &[FileEntry]) -> Resolver {
        let mut name_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut image_stem_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut path_index: HashMap<Vec<PartKey>, Vec<usize>> = HashMap::new();
        for (idx, entry) in files.iter().enumerate() {
            // A lossy display spelling is never a lookup key: it could alias
            // a different valid UTF-8 name. Such entries remain reportable
            // by their native path, but text links cannot name them safely.
            if entry.native_name.to_str().is_some() {
                name_index.entry(key(&entry.name)).or_default().push(idx);
                if entry.class == FileClass::Image {
                    let stem = format::file_stem(&entry.name);
                    image_stem_index.entry(key(stem)).or_default().push(idx);
                }
            }
            // Path keys come from native components; paths with non-UTF-8
            // folders stay reachable through note-relative resolution.
            path_index
                .entry(path_parts(&entry.path))
                .or_default()
                .push(idx);
        }
        Resolver {
            name_index,
            image_stem_index,
            path_index,
        }
    }

    /// Resolve one link target (FORMAT §5.1).
    ///
    /// `note` is the linking note; its native folder ([`FileEntry::dir_path`])
    /// grounds the note-relative leg of path links, so a note under a
    /// non-UTF-8 directory resolves against that directory, not against an
    /// unrelated folder with the same lossy display spelling.
    pub fn resolve(&self, target: &str, note: &FileEntry, syntax: LinkSyntax) -> Outcome {
        let Some(target) = prepare_target(target) else {
            return Outcome::NotImageTarget;
        };
        let class = target_class(&target);
        if !target.contains('/') {
            // Bare name: exactly one file may have it (§5.1 rule 2).
            if let Some(hits) = self.name_index.get(&key(&target)) {
                return collapse(hits.iter().copied());
            }
            match class {
                TargetClass::None => {
                    // Obsidian-style extensionless reference: look at image
                    // stems only; notes and other files keep the link out of
                    // scope. One stem hit resolves, several are ambiguous.
                    let hits = self
                        .image_stem_index
                        .get(&key(&target))
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    if hits.is_empty() {
                        extless_miss(syntax)
                    } else {
                        collapse(hits.iter().copied())
                    }
                }
                TargetClass::Image => Outcome::NotFound,
                TargetClass::Text | TargetClass::Other => Outcome::NotImageTarget,
            }
        } else {
            // Path: library root first, then the note's folder (§5.1 rule 1).
            let note_base = path_parts(note.dir_path());
            if let Some(out) = self.lookup_path(&target, None) {
                return out;
            }
            if let Some(out) = self.lookup_path(&target, Some(&note_base)) {
                return out;
            }
            match class {
                TargetClass::None => {
                    let mut hits: Vec<usize> = Vec::new();
                    for ext in format::IMAGE_EXTENSIONS {
                        let with_ext = format!("{target}.{ext}");
                        if let Some(out) = self.lookup_path(&with_ext, None) {
                            collect(&mut hits, out);
                        }
                        if let Some(out) = self.lookup_path(&with_ext, Some(&note_base)) {
                            collect(&mut hits, out);
                        }
                    }
                    hits.sort_unstable();
                    hits.dedup();
                    if hits.is_empty() {
                        extless_miss(syntax)
                    } else {
                        collapse(hits.into_iter())
                    }
                }
                TargetClass::Image => Outcome::NotFound,
                TargetClass::Text | TargetClass::Other => Outcome::NotImageTarget,
            }
        }
    }

    /// Resolve a Canvas file node as a library-root path (FORMAT §6).
    pub fn resolve_canvas(&self, target: &str) -> Outcome {
        let Some(target) = prepare_target(target) else {
            return Outcome::NotImageTarget;
        };
        if target_class(&target) != TargetClass::Image {
            return Outcome::NotImageTarget;
        }
        self.lookup_path(&target, None).unwrap_or(Outcome::NotFound)
    }

    /// Resolve explicit local Markdown note references so missing and
    /// ambiguous note links are not hidden by image-only classification.
    /// `note` grounds the note-relative leg, like in [`Resolver::resolve`].
    pub fn resolve_note_reference(&self, target: &str, note: &FileEntry) -> Option<Outcome> {
        let target = prepare_target(target)?;
        if !target.to_ascii_lowercase().ends_with(".md") {
            return None;
        }
        if target.contains('/') {
            let note_base = path_parts(note.dir_path());
            if let Some(out) = self.lookup_path(&target, None) {
                return Some(out);
            }
            if let Some(out) = self.lookup_path(&target, Some(&note_base)) {
                return Some(out);
            }
            Some(Outcome::NotFound)
        } else {
            Some(
                self.name_index
                    .get(&key(&target))
                    .map(|hits| collapse(hits.iter().copied()))
                    .unwrap_or(Outcome::NotFound),
            )
        }
    }

    fn lookup_path(&self, target: &str, base: Option<&[PartKey]>) -> Option<Outcome> {
        let parts = join_parts(base, target)?;
        let hits = self.path_index.get(&parts)?;
        Some(collapse(hits.iter().copied()))
    }

    /// Indexes of the files whose full name equals `name`, compared by
    /// [`key`] (NFC + casefold, exactly as bare-name resolution does). More
    /// than one hit means the bare name is ambiguous (FORMAT §5.1 rule 3), so
    /// a writer must not use it.
    pub fn bare_name_hits(&self, name: &str) -> &[usize] {
        self.name_index
            .get(&key(name))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

fn collapse(hits: impl Iterator<Item = usize>) -> Outcome {
    let hits: Vec<usize> = hits.collect();
    match hits.len() {
        0 => Outcome::NotFound,
        1 => Outcome::Resolved(hits[0]),
        _ => Outcome::Ambiguous(hits),
    }
}

/// An extensionless target matched nothing: report it only when the syntax
/// can only ever mean an image link (`![](name)`). Embeds may point at notes
/// and canvas `file` nodes at text notes, so those are left alone.
fn extless_miss(syntax: LinkSyntax) -> Outcome {
    if syntax == LinkSyntax::MarkdownImage {
        Outcome::NotFound
    } else {
        Outcome::NotImageTarget
    }
}

fn collect(hits: &mut Vec<usize>, outcome: Outcome) {
    match outcome {
        Outcome::Resolved(one) => hits.push(one),
        Outcome::Ambiguous(many) => hits.extend(many),
        Outcome::NotFound | Outcome::NotImageTarget => {}
    }
}

/// Trim, reject empty targets and things that are URLs, not library paths.
fn prepare_target(target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(colon) = t.find(':') {
        let scheme = &t[..colon];
        let looks_like_scheme = scheme.len() > 1
            && !scheme.contains('/')
            && scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic());
        if looks_like_scheme {
            return None; // http:, mailto:, data:, obsidian:, ...
        }
    }
    Some(t.to_string())
}
