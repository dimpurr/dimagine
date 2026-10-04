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

/// Extract image-relevant links from Markdown text. `first_line` is the
/// 1-based line number of the first line of `body` in the enclosing file.
///
/// Fenced code blocks and inline code spans are skipped, like Obsidian's
/// renderer does.
pub fn extract_markdown_links(body: &str, first_line: usize) -> Vec<Link> {
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

    let mut links = Vec::new();
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
                        push_wiki(&mut links, &body[i + 3..close], line_at(i), true);
                        i = close + 2;
                    }
                    None => i += 1,
                }
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                match find_byte(bytes, b']', i + 2) {
                    Some(alt_end) if alt_end + 1 < bytes.len() && bytes[alt_end + 1] == b'(' => {
                        match markdown_destination_end(bytes, alt_end + 2) {
                            Some(close) => {
                                push_markdown(&mut links, &body[alt_end + 2..close], line_at(i));
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
                        push_wiki(&mut links, &body[i + 2..close], line_at(i), false);
                        i = close + 2;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    links
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

fn push_wiki(links: &mut Vec<Link>, content: &str, line: usize, embed: bool) {
    links.push(Link {
        syntax: if embed {
            LinkSyntax::WikiEmbed
        } else {
            LinkSyntax::WikiLink
        },
        raw: content.to_string(),
        target: strip_display_parts(content),
        line,
    });
}

fn push_markdown(links: &mut Vec<Link>, path_raw: &str, line: usize) {
    let path_raw = path_raw.trim();
    let inner = if let Some(rest) = path_raw.strip_prefix('<') {
        match rest.find('>') {
            Some(end) => &rest[..end],
            None => path_raw,
        }
    } else {
        // A bare path is cut at the first whitespace; anything after is a
        // title. Paths with spaces must be percent-encoded or angle-quoted.
        match path_raw.find(char::is_whitespace) {
            Some(end) => &path_raw[..end],
            None => path_raw,
        }
    };
    let outer = strip_display_parts(inner)
        .replace("\\(", "(")
        .replace("\\)", ")");
    let decoded = percent_decode_str(&outer)
        .decode_utf8()
        .map(|cow| cow.into_owned().trim().to_string())
        .unwrap_or(outer.trim().to_string());
    links.push(Link {
        syntax: LinkSyntax::MarkdownImage,
        raw: inner.to_string(),
        target: decoded,
        line,
    });
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

fn markdown_destination_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut angle = false;
    let mut escaped = false;
    for (offset, &byte) in bytes.iter().enumerate().skip(from) {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        if byte == b'<' {
            angle = true;
            continue;
        }
        if byte == b'>' && angle {
            angle = false;
            continue;
        }
        if angle {
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(offset),
            b')' => depth -= 1,
            b'\n' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Byte ranges of fenced code blocks (``` and ~~~), including their fences.
/// An unclosed fence swallows the rest of the file, like a renderer would.
fn fenced_ranges(text: &str) -> Vec<(usize, usize)> {
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
    if fence_char.is_some() {
        ranges.push((start, text.len()));
    }
    ranges
}

/// The comparison key for names and paths: NFC-normalised, lowercased.
pub fn key(s: &str) -> String {
    s.nfc().collect::<String>().to_lowercase()
}

/// Resolve link targets against the files of one library walk.
#[derive(Debug)]
pub struct Resolver {
    /// Full file name → file indexes, by [`key`].
    name_index: HashMap<String, Vec<usize>>,
    /// Image file name without extension → file indexes, by [`key`].
    image_stem_index: HashMap<String, Vec<usize>>,
    /// Library-relative path → file indexes, by [`key`].
    path_index: HashMap<String, Vec<usize>>,
}

impl Resolver {
    pub fn new(files: &[FileEntry]) -> Resolver {
        let mut name_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut image_stem_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut path_index: HashMap<String, Vec<usize>> = HashMap::new();
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
            if entry.path.to_str().is_some() {
                path_index.entry(key(&entry.rel)).or_default().push(idx);
            }
        }
        Resolver {
            name_index,
            image_stem_index,
            path_index,
        }
    }

    /// Resolve one link target (FORMAT §5.1).
    ///
    /// `note_dir` is the library-relative folder of the linking note (`""`
    /// for a note at the root), used for the note-relative leg of path links.
    pub fn resolve(&self, target: &str, note_dir: &str, syntax: LinkSyntax) -> Outcome {
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
            if let Some(out) = self.lookup_path(&target, None) {
                return out;
            }
            if let Some(out) = self.lookup_path(&target, Some(note_dir)) {
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
                        if let Some(out) = self.lookup_path(&with_ext, Some(note_dir)) {
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
    pub fn resolve_note_reference(&self, target: &str, note_dir: &str) -> Option<Outcome> {
        let target = prepare_target(target)?;
        if !target.to_ascii_lowercase().ends_with(".md") {
            return None;
        }
        if target.contains('/') {
            if let Some(out) = self.lookup_path(&target, None) {
                return Some(out);
            }
            if let Some(out) = self.lookup_path(&target, Some(note_dir)) {
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

    fn lookup_path(&self, target: &str, base: Option<&str>) -> Option<Outcome> {
        let normalized = normalize_rel(target, base)?;
        let hits = self.path_index.get(&key(&normalized))?;
        Some(collapse(hits.iter().copied()))
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

/// Join `target` onto `base` and resolve `.` and `..` segments. Returns
/// `None` when the path escapes the library root.
fn normalize_rel(target: &str, base: Option<&str>) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(base) = base {
        if !base.is_empty() {
            parts.extend(base.split('/'));
        }
    }
    for segment in target.split('/').filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(segment),
        }
    }
    Some(parts.join("/"))
}
