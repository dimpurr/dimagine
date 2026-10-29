//! Edit the YAML front matter of a note by splicing raw bytes (FORMAT §3.1).
//!
//! Everything in this module works on a note's raw bytes and returns new
//! bytes; it never touches a file. The write core (a separate crate) is the
//! only caller allowed to put these results on disk.
//!
//! The design goal of every operation is byte exactness: the edit splices
//! exactly the byte range it intends — the value token of one key, the lines
//! of one entry, or a block inserted at one point — and every other byte of
//! the note, front matter and body alike, stays byte-identical.
//!
//! Two invariants hold for every operation:
//!
//! - *Closed subset.* Only a plain (optionally double-quoted) top-level
//!   key that occurs exactly once can be edited, and its value must be
//!   one of: a single-line plain or double-quoted scalar, a single-line
//!   flow list of plain/double-quoted strings, or a block list of
//!   single-line plain/double-quoted strings. Refusals come in two
//!   scopes, and the difference is deliberate: *document hazards* —
//!   duplicate top-level keys, merge keys (`<<`), anchors and aliases at
//!   a key, value or item position, and constructs the scanner cannot
//!   attribute — refuse every operation on the region; *property
//!   shapes* — a single-quoted key, a nested structure, a block or
//!   multiline scalar, a `tags` value that is not a list of strings —
//!   refuse only the operation that targets that key, and an unrelated
//!   key holding such a value keeps it (ADR-017 item 1: only the
//!   target key changes). An anchor or merge key nested inside another
//!   key's value is not scanned (no splice can reach it), so it does
//!   not refuse; the value carrying it is itself outside the subset and
//!   refuses when targeted. A refusal never carries note content.
//! - *Re-parsed before committing.* The editor never trusts its own scanning:
//!   before the splice the parsed value of the key must equal what the
//!   scanner saw, and after the splice the edited front matter is parsed
//!   again and the key must read exactly the intended value, with every
//!   other key unchanged. If that fails the edit is refused.
//!
//! The front matter delimiters are delimited by one shared rule
//! ([`front_matter_range`]), used by both this module and the reader in
//! [`crate::note`]: a fence line is a line whose content is `---` with
//! optional trailing whitespace.

use std::fmt;
use std::ops::Range;

use saphyr::LoadableYamlNode;
use saphyr::Scalar;
use saphyr::Yaml;

use crate::note::is_valid_ulid;

/// Where the YAML front matter block sits in a note's raw bytes.
///
/// All offsets index the whole raw note, including a leading UTF-8 BOM.
/// The [`Closed`] variant's YAML region is `yaml_start..close_start`.
///
/// [`Closed`]: FmRange::Closed
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FmRange {
    /// The note does not start with a `---` fence; every byte after a
    /// possible BOM is body. `bom_len` is 0 or 3.
    NoFrontMatter {
        /// Length in bytes of the leading UTF-8 BOM, 0 when there is none.
        bom_len: usize,
    },
    /// The note starts with a `---` fence that a later `---` line closes.
    Closed {
        /// Just past the opening fence's line break; the YAML region
        /// starts here.
        yaml_start: usize,
        /// First byte of the closing `---` line; the YAML region ends here.
        /// New key lines are inserted at this offset.
        close_start: usize,
        /// Just past the closing fence's line break; the body starts here.
        body_start: usize,
        /// 1-based line number of the closing fence within the note; the
        /// body begins on line `close_line_no + 1`.
        close_line_no: usize,
    },
    /// The note starts with a `---` fence that is never closed. The body is
    /// everything from `body_start` (line 2).
    Unterminated {
        /// Just past the opening fence's line break; the body starts here.
        body_start: usize,
    },
}

/// Split a note into its front matter block and body, by bytes.
///
/// One rule for both fences: a fence line's content is `---` followed only
/// by whitespace. A leading UTF-8 BOM is recognized and reported; CRLF and
/// LF line endings are both accepted. The reader ([`crate::note::parse_note`])
/// and the editor share this function, so the two can never disagree about
/// where front matter ends.
pub fn front_matter_range(raw: &[u8]) -> FmRange {
    let bom_len = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let s = &raw[bom_len..];
    let (first_content, first_break_end) = line_at(s, 0);
    if !is_fence_line(&s[first_content]) {
        return FmRange::NoFrontMatter { bom_len };
    }
    // For a block that never closes, the body is everything from just past
    // the opening fence's line break (the note's line 2).
    let yaml_start = first_break_end.unwrap_or(s.len());
    let mut cursor = yaml_start;
    let mut line_no = 2usize;
    loop {
        let (content, break_end) = line_at(s, cursor);
        if is_fence_line(&s[content.clone()]) {
            // The closing fence may be the last line without a line break.
            let body_start = break_end.unwrap_or(content.end);
            return FmRange::Closed {
                yaml_start: bom_len + yaml_start,
                close_start: bom_len + content.start,
                body_start: bom_len + body_start,
                close_line_no: line_no,
            };
        }
        let Some(end) = break_end else {
            // Reached the end of the note without a closing fence.
            return FmRange::Unterminated {
                body_start: bom_len + yaml_start,
            };
        };
        cursor = end;
        line_no += 1;
    }
}

/// One line starting at `at`: `(content_range, Some(just_past_the_break))`.
/// The content excludes the line break (`\n` and a preceding `\r` stay with
/// the break); at end of input the content runs to the end and there is no
/// break. Offsets index the `s` slice passed in.
fn line_at(s: &[u8], at: usize) -> (Range<usize>, Option<usize>) {
    let end = match s[at..].iter().position(|&b| b == b'\n') {
        Some(offset) => at + offset,
        None => s.len(),
    };
    let break_end = (end < s.len()).then_some(end + 1);
    (at..end, break_end)
}

/// A front-matter fence line: `---` followed only by whitespace, matching the
/// reader's historical tolerance for the closing fence, now applied to both.
fn is_fence_line(line: &[u8]) -> bool {
    match std::str::from_utf8(line) {
        Ok(text) => text.trim_end() == "---",
        Err(_) => trim_ascii_ws_end(line) == b"---",
    }
}

fn trim_ascii_ws_end(mut line: &[u8]) -> &[u8] {
    while matches!(line.last(), Some(b' ' | b'\t' | b'\r' | b'\x0b' | b'\x0c')) {
        line = &line[..line.len() - 1];
    }
    line
}

/// Why a front-matter edit was refused. A refusal never carries bytes of the
/// note; strings in variants come from the caller's own arguments or from
/// property names, never from property values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FmEditError {
    /// The note opens front matter that no `---` line ever closes.
    UnterminatedFrontMatter,
    /// The front matter does not parse or is not a mapping of properties.
    /// The reader reports these notes too.
    FrontMatterInvalid {
        /// 1-based line within the front matter region if the parser gave one.
        line: Option<usize>,
        /// 1-based column within the line if the parser gave one.
        column: Option<usize>,
    },
    /// The front matter holds more than one YAML document (a `...` marker).
    MultipleDocuments,
    /// A top-level key appears twice anywhere in the front matter. Which line
    /// is "the" one is ambiguous, so no edit is attempted.
    DuplicateTopLevelKey,
    /// A `<<` merge key is present.
    MergeKey,
    /// An `&anchor` or `*alias` token appears in the front matter.
    AnchorOrAlias,
    /// A top-level construct the editor cannot attribute to a key, such as a
    /// complex (`?` …) key or a tagged key.
    Unsupported,
    /// The key to edit is present but written in a form outside the subset,
    /// such as single-quoted.
    KeyNotPlain { key: String },
    /// The key to edit holds a value outside the subset: a nested structure,
    /// a block scalar, a multiline scalar or list, or a list element that is
    /// not a single-line plain/double-quoted string.
    ValueNotEditable,
    /// The `tags` key holds a scalar instead of a list (use the tag edits).
    TagsNotAList,
    /// The key is owned by a typed operation: `rating` via [`set_rating`],
    /// `tags` via [`add_tags`] / [`remove_tags`].
    ReservedKey { key: String },
    /// An existing `id` is immutable (FORMAT §3.3); only
    /// [`ensure_id`], which writes one only when absent, may touch it.
    IdImmutable,
    /// The given key cannot be written as a plain YAML key.
    InvalidKey,
    /// The given tag is not a legal tag value (FORMAT §3.1 / tag rules:
    /// no whitespace, comma, `[`, `]`, `{`, `}`, `#`, `|` or empty).
    InvalidTag { tag: String },
    /// A rating outside 0–5.
    RatingOutOfRange(u8),
    /// The given `id` value is not a ULID.
    NotAUlid,
    /// The note's bytes are not valid UTF-8; the reader works on `&str` and
    /// dimagine never rewrites a note it cannot read.
    NotUtf8,
    /// The re-parse check failed: the edited front matter did not read back
    /// as exactly the intended value, or the scan disagreed with the parser.
    /// This never escapes with changed bytes.
    VerificationFailed,
}

impl fmt::Display for FmEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedFrontMatter => {
                write!(f, "front matter is never closed by a `---` line")
            }
            Self::FrontMatterInvalid { line, column } => {
                write!(f, "front matter does not parse (the reader reports it too)")?;
                if let Some(line) = line {
                    write!(f, " at line {line}")?;
                }
                if let Some(column) = column {
                    write!(f, " column {column}")?;
                }
                Ok(())
            }
            Self::MultipleDocuments => {
                write!(f, "the front matter holds more than one YAML document")
            }
            Self::DuplicateTopLevelKey => {
                write!(f, "a top-level key appears twice in the front matter")
            }
            Self::MergeKey => write!(f, "a `<<` merge key is present in the front matter"),
            Self::AnchorOrAlias => {
                write!(f, "an anchor or alias appears in the front matter")
            }
            Self::Unsupported => write!(
                f,
                "the front matter contains a construct the editor does not understand"
            ),
            Self::KeyNotPlain { key } => {
                write!(f, "the key `{key}` is not a plain key")
            }
            Self::TagsNotAList => write!(f, "the `tags` value is a scalar, not a list"),
            Self::ValueNotEditable => write!(
                f,
                "the value is outside the editable subset of front matter"
            ),
            Self::ReservedKey { key } => {
                write!(f, "the key `{key}` is owned by its typed operation")
            }
            Self::IdImmutable => {
                write!(f, "an existing `id` is never rewritten (FORMAT §3.3)")
            }
            Self::InvalidKey => {
                write!(f, "the key is not writable as a plain YAML key")
            }
            Self::InvalidTag { tag } => {
                write!(f, "the tag `{tag}` is not a legal tag value")
            }
            Self::RatingOutOfRange(value) => {
                write!(f, "rating {value} is outside 0-5")
            }
            Self::NotAUlid => write!(f, "the id value is not a ULID"),
            Self::NotUtf8 => write!(f, "the note's bytes are not valid UTF-8"),
            Self::VerificationFailed => write!(
                f,
                "the edited front matter did not read back as the intended value"
            ),
        }
    }
}

impl std::error::Error for FmEditError {}

/// The value a `set_scalar` restyle of the `rating` key writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RatingEdit {
    /// Set the rating, replacing the whole value token with an unquoted 0–5
    /// (`rating: "4"` becomes `rating: 5`).
    Set(u8),
    /// Remove the `rating` key; clearing an absent rating does nothing.
    Clear,
}

// ---------------------------------------------------------------------------
// Spelling: how new key lines, scalar tokens and tags are written.
// ---------------------------------------------------------------------------

/// The note's line-ending style, used for every line the editor
/// inserts: the style of the first line break in the file, or LF when
/// there is none. Only `\n` and `\r\n` are styles the reader and the
/// scanner can parse back; a CR-only file keeps LF here (a front
/// matter block must stay parseable), and the one place a CR-only
/// file keeps its own style is the self-embed's appended line.
fn newline_style(raw: &[u8]) -> &'static [u8] {
    match raw.iter().position(|&b| b == b'\n') {
        Some(pos) if pos > 0 && raw[pos - 1] == b'\r' => b"\r\n",
        _ => b"\n",
    }
}

/// True when `value` must be written in double quotes. FORMAT §3.1 names the
/// strings YAML could misread (`no`, `yes`, `on`, `off`, values starting with
/// an indicator, values containing `: `); this adds every other form a plain
/// scalar cannot keep semantically identical: empty values, embedded
/// control whitespace, leading/trailing space, number-like values (including
/// radix prefixes a YAML parser might resolve), boolean and null words, and
/// comment leaks.
fn scalar_needs_quotes(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let first = value.as_bytes()[0];
    if matches!(
        first,
        b' ' | b'\t'
            | b'-'
            | b'?'
            | b':'
            | b','
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'#'
            | b'&'
            | b'*'
            | b'!'
            | b'|'
            | b'>'
            | b'\''
            | b'"'
            | b'%'
            | b'@'
            | b'`'
    ) {
        return true;
    }
    if value
        .bytes()
        .any(|b| matches!(b, b'\n' | b'\r' | b'\t' | 0x00..=0x08 | 0x0B..=0x1F | 0x7F))
    {
        return true;
    }
    if value.starts_with(' ') || value.ends_with(' ') || value.ends_with('\t') {
        return true;
    }
    if value.contains(": ")
        || value.ends_with(':')
        || value.contains(" #")
        || value.starts_with('#')
    {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~" | "y" | "n"
    ) {
        return true;
    }
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
        return true;
    }
    // Byte comparisons only: multi-byte values must not be cut mid-character
    // just to look at a two-byte prefix.
    if let Some(head) = lower.as_bytes().get(0..2) {
        if matches!(head, b"0x" | b"0o" | b"0b")
            && lower.as_bytes()[2..]
                .iter()
                .all(|b| b.is_ascii_alphanumeric())
        {
            return true;
        }
    }

    false
}

/// Serialize `value` as a scalar token: plain when plain keeps it a string,
/// double-quoted with YAML escapes otherwise (FORMAT §3.1).
fn write_scalar(value: &str) -> String {
    if !scalar_needs_quotes(value) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A caller-supplied key that the editor may write as a new key line.
fn validate_new_key(key: &str) -> Result<(), FmEditError> {
    if key.is_empty()
        || key.contains(['\n', '\r'])
        || key == "<<"
        || scalar_needs_quotes(key)
        || key.starts_with(' ')
        || key.ends_with(' ')
    {
        return Err(FmEditError::InvalidKey);
    }
    Ok(())
}

/// A tag value per the tag rules: strip one leading `#`, keep case, allow
/// hierarchies (`style/ink`), reject whitespace, comma, `[`, `]`, `{`, `}`,
/// `#`, `|` and emptiness (tags are stored without `#`, FORMAT §3.1).
fn normalize_tag(tag: &str) -> Result<String, FmEditError> {
    let stripped = tag.strip_prefix('#').unwrap_or(tag);
    if stripped.is_empty()
        || stripped
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '{' | '}' | '#' | '|'))
    {
        return Err(FmEditError::InvalidTag {
            tag: tag.to_string(),
        });
    }
    Ok(stripped.to_string())
}

// ---------------------------------------------------------------------------
// Line-level byte helpers.
// ---------------------------------------------------------------------------

fn trim_ascii_ws_start(line: &[u8]) -> &[u8] {
    let offset = line
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\x0b' | b'\x0c'))
        .unwrap_or(line.len());
    &line[offset..]
}

/// End offset of a plain scalar token starting at `start` in `bytes`: plain
/// scalars run to the end of the line, cut before a ` #` comment, before a
/// `:` that would make the context a mapping (`type: eagle` inside an item
/// is a nested map, not a string), and never include trailing whitespace.
fn plain_token_end(bytes: &[u8], start: usize) -> usize {
    let mut end = bytes.len();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                end = i;
                break;
            }
            b':' if matches!(
                bytes.get(i + 1),
                None | Some(b' ') | Some(b'\t') | Some(b'\r')
            ) =>
            {
                end = i;
                break;
            }
            b' ' if bytes.get(i + 1) == Some(&b'#') => {
                end = i;
                break;
            }
            _ => i += 1,
        }
    }
    trim_ascii_end(bytes, start, end)
}

fn trim_ascii_end(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut end = end;
    while end > start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r' | b'\x0b' | b'\x0c') {
        end -= 1;
    }
    end
}

/// Offset of the closing `"` of a double-quoted scalar, honouring `\`
/// escapes; `None` when the text ends before it closes.
fn find_dquote_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Offset of the closing `'` of a single-quoted scalar, honouring the `''`
/// escape.
fn find_squote_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Decode a double-quoted scalar's contents (the common escapes; anything
/// exotic is taken literally, which only affects matching keys no writer in
/// this repository ever produces).
fn decode_double_quoted(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The scanner: one pass over the YAML region, indentation aware, skipping
// comments and block scalars. What it says about the target key is then
// confirmed against the YAML parser before any edit splices bytes.
// ---------------------------------------------------------------------------

/// How a scanned top-level key is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyForm {
    Plain,
    DoubleQuoted,
    SingleQuoted,
}

/// One flow-list element.
#[derive(Clone, Debug)]
enum FlowItem {
    /// A plain or double-quoted scalar token: a byte range in the raw file,
    /// quotes included when double-quoted.
    Token { token: Range<usize> },
    /// Anything else: single-quoted elements, nested collections, tags.
    Other,
}

impl FlowItem {
    fn token(&self) -> Option<&Range<usize>> {
        match self {
            Self::Token { token, .. } => Some(token),
            Self::Other => None,
        }
    }
}

/// One block-list item line: `[ws]- [ws]<token>`.
#[derive(Clone, Debug)]
struct BlockItem {
    /// Start of the `-` line, at its indentation.
    line_start: usize,
    /// Just past the item line's line break.
    end: usize,
    /// Column of the `-`.
    dash_col: usize,
    /// The item's scalar token, quotes included when double-quoted.
    token: Range<usize>,
}

/// What the scanner believes a top-level key's value looks like. Only the
/// first four variants are inside the editable subset.
#[derive(Clone, Debug)]
enum ScannedValue {
    /// The key line has no value (only spaces or a comment follow the
    /// colon). Treated as writable: `insert_at` is where a token goes,
    /// within the key line.
    Empty {
        insert_at: usize,
        prefix_space: bool,
        suffix_space: bool,
    },
    /// A single-line scalar on the key line.
    Scalar { token: Range<usize> },
    /// A single-line flow list whose elements are all plain or double-quoted
    /// scalar tokens.
    FlowSeq {
        items: Vec<FlowItem>,
        close_pos: usize,
    },
    /// A block list of single-line scalar items at one indentation.
    BlockSeq { items: Vec<BlockItem> },
    /// Anything outside the subset: nested structures, block scalars,
    /// multiline scalars or lists, single-quoted values, tagged values.
    Uneditable,
}

/// The value token of a scanned scalar.
fn scalar_span(value: &ScannedValue) -> Range<usize> {
    match value {
        ScannedValue::Scalar { token } => token.clone(),
        _ => 0..0,
    }
}

/// The splice spot of a scanned empty value.
fn empty_spot(value: &ScannedValue) -> (usize, bool, bool) {
    match value {
        ScannedValue::Empty {
            insert_at,
            prefix_space,
            suffix_space,
        } => (*insert_at, *prefix_space, *suffix_space),
        _ => (0, false, false),
    }
}

/// One top-level entry: a key line plus every line attributed to it.
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    key_form: KeyForm,
    /// Start of the key line.
    line_start: usize,
    /// Just past the entry's last attributed line's line break (or its
    /// content when the region ends without one).
    end: usize,
    value: ScannedValue,
}

/// The whole scan of one front matter region. `refusal`, when set, applies to
/// every operation: it marks structures that make the region unsafe to touch
/// (duplicates, merge keys, anchors, unknown top-level lines).
#[derive(Clone, Debug, Default)]
struct Scan {
    entries: Vec<Entry>,
    refusal: Option<FmEditError>,
}

impl Scan {
    fn find(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.name == key)
    }
}

/// Multi-line value consumption for the entry currently open: values that
/// can span lines are consumed here so no later line is misattributed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Consume {
    /// Inside an unclosed quoted scalar (double-quoted when `double`).
    Quote { double: bool },
    /// Inside a flow collection opened but not closed this line.
    Flow { depth: usize, quote: Option<bool> },
    /// Inside a block scalar: consume indented and blank lines only.
    BlockScalar,
    /// Inside a nested structure: consume indented lines, watching for
    /// quotes or flow collections left open at the end of a line.
    Nested,
}

/// What a key line's or item line's value part looks like (the part after
/// `key:` or after the `-`).
#[derive(Clone, Debug)]
enum RestClass {
    /// Nothing — or only a comment — follows the separator. `insert_at` is
    /// the offset where a value token would be spliced in.
    Nothing {
        insert_at: usize,
        prefix_space: bool,
        suffix_space: bool,
    },
    /// A single-line scalar token. `junk_after` marks content after it that
    /// is neither whitespace nor a comment.
    Scalar {
        token: Range<usize>,
        junk_after: bool,
    },
    /// A single-quoted scalar: readable YAML, outside the editable subset.
    SingleQuoted,
    /// A quoted scalar (double when `double`) that never closes on the line.
    QuoteOpen {
        double: bool,
    },
    /// A flow collection closed on the line, or still open.
    FlowClosed {
        items: Vec<FlowItem>,
        close_pos: usize,
    },
    FlowOpen {
        depth: usize,
        quote: Option<bool>,
    },
    /// A block scalar indicator (`|`, `>`).
    BlockScalar,
    /// An `&anchor` or `*alias` token: refused for the whole region.
    AnchorOrAlias,
    /// A YAML tag (`!!str`, `!foo`).
    Tagged,
}

/// Classify the value part after a separator. `rest` starts at the first
/// non-space character after the separator; `base` is its absolute offset;
/// `prefix_space` tells whether the separator was not followed by a space
/// (an inserted token then needs one). Returns the class and whether an
/// anchor or alias appeared anywhere in it.
fn classify_rest(rest: &[u8], base: usize, prefix_space: bool) -> (RestClass, bool) {
    if rest.is_empty() {
        return (
            RestClass::Nothing {
                insert_at: base,
                prefix_space,
                suffix_space: false,
            },
            false,
        );
    }
    match rest[0] {
        b'#' => (
            RestClass::Nothing {
                insert_at: base,
                prefix_space,
                suffix_space: true,
            },
            false,
        ),
        b'"' => match find_dquote_close(rest, 1) {
            Some(close) => {
                let token = base..base + close + 1;
                let junk_after = has_junk_after(rest, close + 1);
                (RestClass::Scalar { token, junk_after }, false)
            }
            None => (RestClass::QuoteOpen { double: true }, false),
        },
        b'\'' => match find_squote_close(rest, 1) {
            Some(_) => (RestClass::SingleQuoted, false),
            None => (RestClass::QuoteOpen { double: false }, false),
        },
        b'&' | b'*' => (RestClass::AnchorOrAlias, true),
        b'!' => (RestClass::Tagged, false),
        b'|' | b'>' => (RestClass::BlockScalar, false),
        b'[' | b'{' => match scan_flow(rest, base) {
            FlowScan::Closed { items, close_pos } => {
                (RestClass::FlowClosed { items, close_pos }, false)
            }
            FlowScan::Open { depth, quote } => (RestClass::FlowOpen { depth, quote }, false),
            FlowScan::Anchor => (RestClass::AnchorOrAlias, true),
        },
        _ => {
            let token_end = plain_token_end(rest, 0);
            let token = base..base + token_end;
            let junk_after = has_junk_after(rest, token_end);
            (RestClass::Scalar { token, junk_after }, false)
        }
    }
}

/// Is there content after `from` that is neither whitespace nor a comment?
fn has_junk_after(rest: &[u8], from: usize) -> bool {
    let tail = trim_ascii_ws_start(&rest[from.min(rest.len())..]);
    !tail.is_empty() && tail[0] != b'#'
}

/// The scan of one flow collection that starts at `bytes[0]` (`[` or `{`).
enum FlowScan {
    /// Closed on this line, with the top-level element token spans and the
    /// absolute offset of the closing bracket.
    Closed {
        items: Vec<FlowItem>,
        close_pos: usize,
    },
    /// Still open at the end of the line (counting an open quote): continue
    /// consuming lines.
    Open { depth: usize, quote: Option<bool> },
    /// An `&anchor`/`*alias` element: refused for the whole region.
    Anchor,
}

fn scan_flow(bytes: &[u8], base: usize) -> FlowScan {
    let mut items: Vec<FlowItem> = Vec::new();
    let mut depth = 1usize;
    let mut quote: Option<bool> = None;
    let mut i = 1usize;
    let mut at_elem_start = true;
    let mut plain_from: Option<usize> = None;
    let mut plain_other = false;
    loop {
        let Some(b) = bytes.get(i) else {
            close_plain(bytes, &mut items, base, &mut plain_from, plain_other, i);
            return FlowScan::Open { depth, quote };
        };
        if let Some(double) = quote {
            if double {
                match b {
                    b'\\' => i += 2,
                    b'"' => {
                        quote = None;
                        i += 1;
                    }
                    _ => i += 1,
                }
            } else if *b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    quote = None;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        match b {
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' if matches!(bytes[i - 1], b' ' | b'\t') => {
                // A comment eats the rest of the line; an unclosed flow
                // continues on the next line.
                close_plain(bytes, &mut items, base, &mut plain_from, plain_other, i);
                return FlowScan::Open { depth, quote };
            }
            b',' if depth == 1 => {
                close_plain(bytes, &mut items, base, &mut plain_from, plain_other, i);
                at_elem_start = true;
                plain_other = false;
                i += 1;
            }
            b']' | b'}' => {
                close_plain(bytes, &mut items, base, &mut plain_from, plain_other, i);
                depth -= 1;
                if depth == 0 {
                    return FlowScan::Closed {
                        items,
                        close_pos: base + i,
                    };
                }
                i += 1;
            }
            b'[' | b'{' => {
                if at_elem_start {
                    items.push(FlowItem::Other);
                    at_elem_start = false;
                } else {
                    close_plain(bytes, &mut items, base, &mut plain_from, plain_other, i);
                }
                depth += 1;
                i += 1;
            }
            // A `:` inside a plain element makes it a flow mapping pair
            // (`a: 1`), which the subset does not carry.
            b':' if matches!(
                bytes.get(i + 1),
                None | Some(b' ')
                    | Some(b'\t')
                    | Some(b'\r')
                    | Some(b',')
                    | Some(b']')
                    | Some(b'}')
            ) =>
            {
                plain_other = true;
                i += 1;
            }
            b'"' => match find_dquote_close(bytes, i + 1) {
                Some(close) => {
                    items.push(FlowItem::Token {
                        token: base + i..base + close + 1,
                    });
                    i = close + 1;
                    at_elem_start = false;
                }
                None => {
                    return FlowScan::Open {
                        depth,
                        quote: Some(true),
                    }
                }
            },
            b'\'' => match find_squote_close(bytes, i + 1) {
                Some(close) => {
                    // A single-quoted element: inside YAML, outside the
                    // subset.
                    items.push(FlowItem::Other);
                    i = close + 1;
                    at_elem_start = false;
                }
                None => {
                    return FlowScan::Open {
                        depth,
                        quote: Some(false),
                    }
                }
            },
            b'&' | b'*' if at_elem_start => return FlowScan::Anchor,
            b'!' if at_elem_start => {
                items.push(FlowItem::Other);
                at_elem_start = false;
                i = skip_plain_word(bytes, i);
            }
            _ => {
                if at_elem_start {
                    at_elem_start = false;
                    plain_from.get_or_insert(i);
                }
                i += 1;
            }
        }
    }
}

fn close_plain(
    bytes: &[u8],
    items: &mut Vec<FlowItem>,
    base: usize,
    plain_from: &mut Option<usize>,
    plain_other: bool,
    end: usize,
) {
    if let Some(from) = plain_from.take() {
        if plain_other {
            items.push(FlowItem::Other);
            return;
        }
        let end = trim_ascii_end(bytes, from, end);
        if end > from {
            items.push(FlowItem::Token {
                token: base + from..base + end,
            });
        }
    }
}

/// Skip a non-quoted word: up to whitespace, comma or a closing bracket.
fn skip_plain_word(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b',' | b']' | b'}' => break,
            _ => i += 1,
        }
    }
    i
}

/// A parsed top-level key line, or why the line is not one.
enum KeyLineKind {
    Key {
        name: String,
        form: KeyForm,
        /// Absolute offset just past the `:`.
        colon_end: usize,
    },
    Comment,
    /// A `...` document end marker: the region holds more than one document.
    DotDoc,
    /// Not a key line in any form the editor understands.
    Unknown,
}

/// Parse the (leading-whitespace-stripped) content of a column-0 line.
fn parse_key_line(trim: &[u8], base: usize) -> KeyLineKind {
    let Some(&first) = trim.first() else {
        return KeyLineKind::Unknown;
    };
    if trim == b"..." || trim.starts_with(b"... ") {
        return KeyLineKind::DotDoc;
    }
    match first {
        b'#' => KeyLineKind::Comment,
        b'"' => {
            let Some(close) = find_dquote_close(trim, 1) else {
                return KeyLineKind::Unknown;
            };
            let name = decode_double_quoted(&trim[1..close]);
            let after = trim_ascii_ws_start(&trim[close + 1..]);
            if after.first() == Some(&b':') {
                KeyLineKind::Key {
                    name,
                    form: KeyForm::DoubleQuoted,
                    colon_end: base + (trim.len() - after.len()) + 1,
                }
            } else {
                KeyLineKind::Unknown
            }
        }
        b'\'' => {
            let Some(close) = find_squote_close(trim, 1) else {
                return KeyLineKind::Unknown;
            };
            let name = String::from_utf8_lossy(&trim[1..close]).into_owned();
            let after = trim_ascii_ws_start(&trim[close + 1..]);
            if after.first() == Some(&b':') {
                KeyLineKind::Key {
                    name,
                    form: KeyForm::SingleQuoted,
                    colon_end: base + (trim.len() - after.len()) + 1,
                }
            } else {
                KeyLineKind::Unknown
            }
        }
        b'&' | b'*' | b'?' | b'%' | b'!' | b'[' | b'{' | b',' | b']' | b'}' | b'|' | b'>' => {
            KeyLineKind::Unknown
        }
        _ => {
            // A plain key runs to the first `:` that ends the token.
            let mut colon = None;
            for i in 0..trim.len() {
                if trim[i] == b':' {
                    let next = trim.get(i + 1);
                    if next.is_none() || matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r')) {
                        colon = Some(i);
                        break;
                    }
                }
            }
            let Some(colon) = colon else {
                return KeyLineKind::Unknown;
            };
            let key_end = trim_ascii_end(trim, 0, colon);
            if key_end == 0 {
                return KeyLineKind::Unknown;
            }
            let Ok(name) = std::str::from_utf8(&trim[..key_end]) else {
                return KeyLineKind::Unknown;
            };
            KeyLineKind::Key {
                name: name.to_string(),
                form: KeyForm::Plain,
                colon_end: base + colon + 1,
            }
        }
    }
}

/// What `RestClass` turns into as a key line's value, plus the multi-line
/// consumption mode it opens, plus a file-wide refusal (anchors).
fn value_from_rest(class: RestClass) -> (ScannedValue, Option<Consume>, Option<FmEditError>) {
    match class {
        RestClass::Nothing {
            insert_at,
            prefix_space,
            suffix_space,
        } => (
            ScannedValue::Empty {
                insert_at,
                prefix_space,
                suffix_space,
            },
            None,
            None,
        ),
        RestClass::Scalar {
            token,
            junk_after: false,
        } => (ScannedValue::Scalar { token }, None, None),
        RestClass::Scalar { .. } => (ScannedValue::Uneditable, None, None),
        RestClass::SingleQuoted => (ScannedValue::Uneditable, None, None),
        RestClass::QuoteOpen { double } => (
            ScannedValue::Uneditable,
            Some(Consume::Quote { double }),
            None,
        ),
        RestClass::FlowClosed { items, close_pos } => {
            if items.iter().all(|item| item.token().is_some()) {
                (ScannedValue::FlowSeq { items, close_pos }, None, None)
            } else {
                (ScannedValue::Uneditable, None, None)
            }
        }
        RestClass::FlowOpen { depth, quote } => (
            ScannedValue::Uneditable,
            Some(Consume::Flow { depth, quote }),
            None,
        ),
        RestClass::BlockScalar => (ScannedValue::Uneditable, Some(Consume::BlockScalar), None),
        RestClass::AnchorOrAlias => (
            ScannedValue::Uneditable,
            None,
            Some(FmEditError::AnchorOrAlias),
        ),
        // A tagged value is outside the subset; other keys may keep theirs.
        RestClass::Tagged => (ScannedValue::Uneditable, None, None),
    }
}

fn consume_refusal(scan: &mut Scan, refusal: FmEditError) {
    if scan.refusal.is_none() {
        scan.refusal = Some(refusal);
    }
}

/// The parse of one `-` item line: a clean item is `- <plain or
/// double-quoted scalar>` and nothing else on the line.
enum ItemScan {
    Clean(BlockItem),
    Unclean,
}

/// Parse one block item line. `line` is the item's content (without its
/// line break), `line_start` its absolute offset, `dash_col` the `-`'s
/// column.
fn parse_block_item(
    line: &[u8],
    line_start: usize,
    content_end: usize,
    break_end: Option<usize>,
    dash_col: usize,
) -> (ItemScan, Option<Consume>, Option<FmEditError>) {
    if line.get(dash_col) != Some(&b'-') {
        return (ItemScan::Unclean, None, None);
    }
    let after_dash = &line[dash_col + 1..];
    if after_dash.is_empty() || !matches!(after_dash[0], b' ' | b'\t') {
        // `-` alone is a null item; `-x` is not a block item.
        return (ItemScan::Unclean, None, None);
    }
    let ws = after_dash
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r'))
        .unwrap_or(after_dash.len());
    let rest = &after_dash[ws..];
    let rest_base = line_start + dash_col + 1 + ws;
    let (class, anchor) = classify_rest(rest, rest_base, ws == 0);
    if anchor {
        return (ItemScan::Unclean, None, Some(FmEditError::AnchorOrAlias));
    }
    let consume = match &class {
        RestClass::QuoteOpen { double } => Some(Consume::Quote { double: *double }),
        RestClass::FlowOpen { depth, quote } => Some(Consume::Flow {
            depth: *depth,
            quote: *quote,
        }),
        _ => None,
    };
    match class {
        RestClass::Scalar {
            token,
            junk_after: false,
        } => (
            ItemScan::Clean(BlockItem {
                line_start,
                end: break_end.unwrap_or(content_end),
                dash_col,
                token,
            }),
            consume,
            None,
        ),
        _ => (ItemScan::Unclean, consume, None),
    }
}

/// One line as the scanner walks the region.
struct RawLine<'a> {
    raw: &'a [u8],
    /// Line content without its break; the absolute line start.
    content: Range<usize>,
    trimmed: &'a [u8],
    break_end: Option<usize>,
    indent: usize,
}

impl RawLine<'_> {
    fn line(&self) -> &[u8] {
        &self.raw[self.content.clone()]
    }

    fn is_blank_or_comment(&self) -> bool {
        self.trimmed.is_empty() || self.trimmed[0] == b'#'
    }
}

/// Scan one front matter region: `raw[region]` covers the YAML between the
/// fences; every offset stored is absolute into `raw`.
fn scan_region(raw: &[u8], region: Range<usize>) -> Scan {
    let mut scan = Scan::default();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut pos = region.start;
    let mut consume: Option<Consume> = None;
    let mut block_indent: Option<usize> = None;

    while pos < region.end {
        let content_start = pos;
        let content_end = match raw[pos..region.end].iter().position(|&b| b == b'\n') {
            Some(offset) => pos + offset,
            None => region.end,
        };
        let break_end = (content_end < region.end).then_some(content_end + 1);
        let line = &raw[content_start..content_end];
        let trimmed = trim_ascii_ws_start(line);
        let current = RawLine {
            raw,
            content: content_start..content_end,
            trimmed,
            break_end,
            indent: line.len() - trimmed.len(),
        };

        // 1. A multi-line value in progress consumes the line wholesale,
        //    except column-0 content, which ends block scalars and nested
        //    structures and is reprocessed below.
        if let Some(mode) = consume {
            if current.is_blank_or_comment() {
                // The mode stays and re-enters on the next
                // content line; attribution rides on the
                // entry's end, which only extends here.
            } else {
                let mut reprocess = false;
                match mode {
                    Consume::Quote { double } => {
                        let closed = if double {
                            find_dquote_close(current.line(), 0).is_some()
                        } else {
                            find_squote_close(current.line(), 0).is_some()
                        };
                        if closed {
                            consume = None;
                        }
                    }
                    Consume::Flow { depth, quote } => {
                        let (closed, depth, quote) = scan_flow_line(current.line(), depth, quote);
                        if closed {
                            consume = None;
                        } else {
                            consume = Some(Consume::Flow { depth, quote });
                        }
                    }
                    Consume::BlockScalar | Consume::Nested => {
                        if current.indent == 0 {
                            consume = None;
                            reprocess = true;
                        } else if mode == Consume::Nested {
                            // One line of a nested structure may itself
                            // leave a quote or flow open.
                            let (open_depth, open_quote) = line_open_state(current.line());
                            consume = match (open_quote, open_depth) {
                                (Some(double), _) => Some(Consume::Quote { double }),
                                (None, 1..) => Some(Consume::Flow {
                                    depth: open_depth,
                                    quote: None,
                                }),
                                _ => Some(Consume::Nested),
                            };
                        }
                    }
                }
                if !reprocess {
                    if let Some(entry) = scan.entries.last_mut() {
                        entry.end = break_end.unwrap_or(content_end);
                    }
                    pos = break_end.unwrap_or(region.end);
                    continue;
                }
            }
        }

        // 2. Blank lines and comment lines never close anything;
        //    attribution of them to an entry rides on the entry's
        //    end, extended only when content continues after them.
        if current.is_blank_or_comment() {
            pos = break_end.unwrap_or(region.end);
            continue;
        }

        // 3. Indented lines continue the entry that is currently open —
        //    including plain scalars continued on the next line, which moves
        //    the value out of the subset.
        if current.indent > 0 && scan.entries.last().is_some() {
            continue_entry(&mut scan, &mut consume, &mut block_indent, &current);
            pos = break_end.unwrap_or(region.end);
            continue;
        }

        // 4. A column-0 `-` continues a block list (YAML allows block
        //    sequences at their parent key's indentation), or continues a
        //    nested value the editor is only attributing.
        if current.indent == 0 && trimmed[0] == b'-' && scan.entries.last().is_some() {
            let open_for_items = scan.entries.last().is_some_and(|entry| {
                matches!(
                    entry.value,
                    ScannedValue::Empty { .. } | ScannedValue::BlockSeq { .. }
                )
            });
            if open_for_items || consume == Some(Consume::Nested) {
                if consume == Some(Consume::Nested) {
                    if let Some(entry) = scan.entries.last_mut() {
                        entry.end = break_end.unwrap_or(content_end);
                    }
                } else {
                    continue_entry(&mut scan, &mut consume, &mut block_indent, &current);
                }
                pos = break_end.unwrap_or(region.end);
                continue;
            }
            // A stray item with nothing to belong to: not editable.
            consume_refusal(&mut scan, FmEditError::Unsupported);
            pos = break_end.unwrap_or(region.end);
            continue;
        }

        // 5. Column-0 content: a new key line, or something the editor
        //    refuses to interpret.
        block_indent = None;
        match parse_key_line(trimmed, content_start) {
            KeyLineKind::Key {
                name,
                form,
                colon_end,
            } => {
                let value_rest = &raw[colon_end..content_end];
                let ws = value_rest
                    .iter()
                    .position(|b| !matches!(b, b' ' | b'\t' | b'\r'))
                    .unwrap_or(value_rest.len());
                let (class, _anchor) = classify_rest(&value_rest[ws..], colon_end + ws, ws == 0);
                let (value, mode, refusal) = value_from_rest(class);
                if name == "<<" {
                    consume_refusal(&mut scan, FmEditError::MergeKey);
                }
                if let Some(refusal) = refusal {
                    consume_refusal(&mut scan, refusal);
                }
                if !names.insert(name.clone()) {
                    consume_refusal(&mut scan, FmEditError::DuplicateTopLevelKey);
                }
                scan.entries.push(Entry {
                    name,
                    key_form: form,
                    line_start: content_start,
                    end: break_end.unwrap_or(content_end),
                    value,
                });
                consume = mode;
            }
            KeyLineKind::Comment => {}
            KeyLineKind::DotDoc => {
                consume_refusal(&mut scan, FmEditError::MultipleDocuments);
            }
            KeyLineKind::Unknown => {
                if trimmed[0] == b'&' || trimmed[0] == b'*' {
                    consume_refusal(&mut scan, FmEditError::AnchorOrAlias);
                } else {
                    consume_refusal(&mut scan, FmEditError::Unsupported);
                }
            }
        }
        pos = break_end.unwrap_or(region.end);
    }
    scan
}

/// Attribute one continuation line to the entry that is open, keeping its
/// classification honest: only indented dash lines at one indentation extend
/// a clean block list; anything else moves the value out of the subset.
fn continue_entry(
    scan: &mut Scan,
    consume: &mut Option<Consume>,
    block_indent: &mut Option<usize>,
    current: &RawLine<'_>,
) {
    let is_dash_item =
        current.trimmed[0] == b'-' && (current.trimmed.len() == 1 || current.trimmed[1] == b' ');
    let (item_scan, item_consume, item_refusal) = if is_dash_item {
        parse_block_item(
            current.line(),
            current.content.start,
            current.content.end,
            current.break_end,
            current.indent,
        )
    } else {
        // A nested line (map entry, plain continuation) may still open a
        // quote or a flow collection that spans further lines.
        let (open_depth, open_quote) = line_open_state(current.line());
        let mode = match (open_quote, open_depth) {
            (Some(double), _) => Some(Consume::Quote { double }),
            (None, 1..) => Some(Consume::Flow {
                depth: open_depth,
                quote: None,
            }),
            _ => Some(Consume::Nested),
        };
        (ItemScan::Unclean, mode, None)
    };
    if let Some(refusal) = item_refusal {
        consume_refusal(scan, refusal);
    }
    let entry = scan.entries.last_mut().expect("continuation without entry");
    entry.end = current.break_end.unwrap_or(current.content.end);
    let extendable_block = match (&entry.value, is_dash_item) {
        (ScannedValue::Empty { .. }, true) => true,
        (ScannedValue::BlockSeq { .. }, true) => *block_indent == Some(current.indent),
        _ => false,
    };
    match item_scan {
        ItemScan::Clean(item) if extendable_block => {
            if let ScannedValue::BlockSeq { items } = &mut entry.value {
                items.push(item);
            } else {
                entry.value = ScannedValue::BlockSeq { items: vec![item] };
            }
            *block_indent = Some(current.indent);
        }
        ItemScan::Clean(_) | ItemScan::Unclean => {
            entry.value = ScannedValue::Uneditable;
            *consume = item_consume.or(Some(Consume::Nested));
        }
    }
}

/// One line of a flow collection being consumed: returns whether the
/// collection closed, and the depth and quote state it carries past the line.
fn scan_flow_line(
    line: &[u8],
    mut depth: usize,
    mut quote: Option<bool>,
) -> (bool, usize, Option<bool>) {
    let mut i = 0usize;
    while i < line.len() {
        let b = line[i];
        if let Some(double) = quote {
            if double {
                match b {
                    b'\\' => i += 2,
                    b'"' => {
                        quote = None;
                        i += 1;
                    }
                    _ => i += 1,
                }
            } else if b == b'\'' {
                if line.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    quote = None;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        match b {
            b'"' => {
                quote = Some(true);
                i += 1;
            }
            b'\'' => {
                quote = Some(false);
                i += 1;
            }
            b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (true, 0, None);
                }
                i += 1;
            }
            b'#' if i > 0 && matches!(line[i - 1], b' ' | b'\t') => break,
            _ => i += 1,
        }
    }
    (false, depth, quote)
}

/// The open-quote and bracket-depth state a line leaves behind, for
/// consumption of values that span lines.
fn line_open_state(line: &[u8]) -> (usize, Option<bool>) {
    let (_, depth, quote) = scan_flow_line(line, 0, None);
    (depth, quote)
}

// ---------------------------------------------------------------------------
// Parsing and verification: the scanner's word against saphyr's, before and
// after every splice.
// ---------------------------------------------------------------------------

/// A value the editor can compare a parsed value against, independent of
/// lifetimes or scalar representation.
#[derive(Clone, Debug, PartialEq)]
enum Expect {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    List(Vec<Expect>),
}

fn expect_of(y: &Yaml) -> Option<Expect> {
    match y {
        Yaml::Value(scalar) => match scalar {
            Scalar::String(text) => Some(Expect::Str(text.to_string())),
            Scalar::Integer(n) => Some(Expect::Int(*n)),
            Scalar::Boolean(b) => Some(Expect::Bool(*b)),
            Scalar::FloatingPoint(f) => Some(Expect::Float(f.0)),
            Scalar::Null => Some(Expect::Null),
        },
        Yaml::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(expect_of(item)?);
            }
            Some(Expect::List(out))
        }
        // Mappings, tags and bad values have no word the editor trusts; a
        // target carrying one is outside the subset.
        _ => None,
    }
}

/// Equality of two parsed nodes the editor can reason about: scalars,
/// sequences and mappings, compared in order. Tagged nodes compare by tag
/// and inner value.
fn yaml_eq(a: &Yaml, b: &Yaml) -> bool {
    match (a, b) {
        (Yaml::Value(x), Yaml::Value(y)) => scalar_eq(x, y),
        (Yaml::Sequence(x), Yaml::Sequence(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(u, v)| yaml_eq(u, v))
        }
        (Yaml::Mapping(x), Yaml::Mapping(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((kx, vx), (ky, vy))| yaml_eq(kx, ky) && yaml_eq(vx, vy))
        }
        (Yaml::Tagged(tx, vx), Yaml::Tagged(ty, vy)) => {
            tx.handle == ty.handle && tx.suffix == ty.suffix && yaml_eq(vx, vy)
        }
        (Yaml::BadValue, Yaml::BadValue) => true,
        (Yaml::Alias(x), Yaml::Alias(y)) => x == y,
        (Yaml::Representation(rx, _, _), Yaml::Representation(ry, _, _)) => rx == ry,
        _ => false,
    }
}

fn scalar_eq(a: &Scalar<'_>, b: &Scalar<'_>) -> bool {
    match (a, b) {
        (Scalar::String(x), Scalar::String(y)) => x == y,
        (Scalar::Integer(x), Scalar::Integer(y)) => x == y,
        (Scalar::Boolean(x), Scalar::Boolean(y)) => x == y,
        (Scalar::FloatingPoint(x), Scalar::FloatingPoint(y)) => x.0 == y.0,
        (Scalar::Null, Scalar::Null) => true,
        _ => false,
    }
}

/// Parse a front matter region into its single document, which must be a
/// mapping; an empty region stands in as an empty mapping. Parse errors keep
/// the parser's position plus one line: the region starts on the note's
/// second line.
fn parse_region(yaml: &str) -> Result<Yaml<'_>, FmEditError> {
    match Yaml::load_from_str(yaml) {
        Err(err) => {
            let marker = err.marker();
            Err(FmEditError::FrontMatterInvalid {
                line: Some(marker.line() + 1),
                column: Some(marker.col() + 1),
            })
        }
        Ok(docs) => {
            if docs.len() > 1 {
                return Err(FmEditError::MultipleDocuments);
            }
            let Some(doc) = docs.into_iter().next() else {
                return Ok(Yaml::Mapping(saphyr::Mapping::new()));
            };
            if !doc.is_mapping() {
                return Err(FmEditError::FrontMatterInvalid {
                    line: None,
                    column: None,
                });
            }
            Ok(doc)
        }
    }
}

fn mapping_get<'y>(doc: &'y Yaml<'y>, key: &str) -> Option<&'y Yaml<'y>> {
    let mapping = doc.as_mapping()?;
    mapping
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Whether a parsed scalar spells "no value": the reader
/// ([`crate::note`]) reads `id: ""` and `id: null` as no `id`,
/// just like a bare `id:`. The writer must agree, or it would
/// refuse to fill or delete a spelling the reader already treats
/// as absent (RW61 L1).
fn scalar_is_empty(doc: &Yaml, key: &str) -> bool {
    match mapping_get(doc, key) {
        Some(Yaml::Value(Scalar::Null)) => true,
        Some(Yaml::Value(Scalar::String(text))) => text.is_empty(),
        _ => false,
    }
}

/// What the target key must read after the splice.
enum TargetAfter {
    /// The key must be gone.
    Absent,
    /// The key must read exactly this value.
    Value(Expect),
}

/// How the scanner's tokens rebuild the value, for the pre-splice check.
enum MiniForm {
    /// `x: <token>`
    Scalar,
    /// `x: [t1, t2, …]`
    Flow,
    /// `x:` followed by `- token` lines.
    Block,
}

/// Before any splice: the scanner and the parser must agree that the target
/// key exists, and on what its value is. `Uneditable` values need only the
/// presence agreed; the operations refuse them on their own.
fn pre_verify(raw: &[u8], scan: &Scan, doc: &Yaml, key: &str) -> Result<(), FmEditError> {
    let parsed_value = mapping_get(doc, key);
    let Some(entry) = scan.find(key) else {
        if parsed_value.is_some() {
            // The parser sees a key the scanner missed: refuse everything.
            return Err(FmEditError::VerificationFailed);
        }
        return Ok(());
    };
    let Some(actual) = parsed_value else {
        return Err(FmEditError::VerificationFailed);
    };
    let agrees = match &entry.value {
        ScannedValue::Empty { .. } => {
            // An empty `key:` reads as an empty string under the
            // locked saphyr 0.1.0 and as null under 0.1.2; both
            // spellings are the same "no value yet" state, so the
            // check must not depend on which saphyr version `0.1`
            // resolves to (RW61 M2).
            matches!(expect_of(actual), Some(Expect::Null))
                || matches!(expect_of(actual), Some(Expect::Str(ref text)) if text.is_empty())
        }
        ScannedValue::Scalar { token } => {
            mini_doc_match(raw, std::slice::from_ref(token), actual, MiniForm::Scalar)
        }
        ScannedValue::FlowSeq { items, .. } => {
            let tokens: Vec<Range<usize>> = items
                .iter()
                .filter_map(|item| item.token().cloned())
                .collect();
            mini_doc_match(raw, &tokens, actual, MiniForm::Flow)
        }
        ScannedValue::BlockSeq { items } => {
            let tokens: Vec<Range<usize>> = items.iter().map(|item| item.token.clone()).collect();
            mini_doc_match(raw, &tokens, actual, MiniForm::Block)
        }
        ScannedValue::Uneditable => true,
    };
    if agrees {
        Ok(())
    } else {
        Err(FmEditError::VerificationFailed)
    }
}

/// Rebuild the value the scanner sees from the raw token bytes, parse it as
/// its own little document, and require it to equal the parser's value.
fn mini_doc_match(raw: &[u8], tokens: &[Range<usize>], actual: &Yaml, form: MiniForm) -> bool {
    let single = tokens.first().cloned().unwrap_or(0..0);
    let body = match form {
        MiniForm::Scalar => format!("x: {}", String::from_utf8_lossy(&raw[single])),
        MiniForm::Flow => {
            let parts: Vec<String> = tokens
                .iter()
                .map(|token| String::from_utf8_lossy(&raw[token.clone()]).into_owned())
                .collect();
            format!("x: [{}]", parts.join(", "))
        }
        MiniForm::Block => {
            let parts: Vec<String> = tokens
                .iter()
                .map(|token| format!("- {}", String::from_utf8_lossy(&raw[token.clone()])))
                .collect();
            format!("x:\n{}", parts.join("\n"))
        }
    };
    let Ok(docs) = Yaml::load_from_str(&body) else {
        return false;
    };
    let Some(doc) = docs.into_iter().next() else {
        return false;
    };
    let Some(mini_value) = mapping_get(&doc, "x") else {
        return false;
    };
    match (expect_of(actual), expect_of(mini_value)) {
        (Some(a), Some(m)) => a == m,
        _ => false,
    }
}

/// After the splice: the new front matter must parse, the target must read
/// exactly the intended value, and every other key must be unchanged.
fn verify_after(
    new_bytes: &[u8],
    key: &str,
    after: &TargetAfter,
    old_doc: &Yaml,
) -> Result<(), FmEditError> {
    let Ok(new_text) = std::str::from_utf8(new_bytes) else {
        return Err(FmEditError::VerificationFailed);
    };
    let FmRange::Closed {
        yaml_start,
        close_start,
        ..
    } = front_matter_range(new_bytes)
    else {
        return Err(FmEditError::VerificationFailed);
    };
    let new_doc = parse_region(&new_text[yaml_start..close_start])
        .map_err(|_| FmEditError::VerificationFailed)?;
    let target_ok = match after {
        TargetAfter::Absent => mapping_get(&new_doc, key).is_none(),
        TargetAfter::Value(expect) => match mapping_get(&new_doc, key) {
            Some(value) => expect_of(value).is_some_and(|found| found == *expect),
            None => false,
        },
    };
    if !target_ok || !mappings_equal_except(old_doc, &new_doc, key) {
        return Err(FmEditError::VerificationFailed);
    }
    Ok(())
}

/// The two mappings must agree key for key (in order) except `key` itself.
fn mappings_equal_except(old: &Yaml, new: &Yaml, key: &str) -> bool {
    let (Some(old_mapping), Some(new_mapping)) = (old.as_mapping(), new.as_mapping()) else {
        return false;
    };
    let mut old_pairs = old_mapping.iter().filter(|(k, _)| k.as_str() != Some(key));
    let mut new_pairs = new_mapping.iter().filter(|(k, _)| k.as_str() != Some(key));
    loop {
        match (old_pairs.next(), new_pairs.next()) {
            (Some((old_key, old_value)), Some((new_key, new_value))) => {
                if !yaml_eq(old_key, new_key) || !yaml_eq(old_value, new_value) {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Splicing bytes.
// ---------------------------------------------------------------------------

/// Apply non-overlapping replacements in one pass. Ranges are absolute into
/// `raw`; an empty range inserts. Overlap or disorder is an internal bug and
/// yields `None`.
fn apply_splices(raw: &[u8], mut edits: Vec<(Range<usize>, Vec<u8>)>) -> Option<Vec<u8>> {
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = Vec::with_capacity(raw.len() + 32);
    let mut cursor = 0usize;
    for (range, replacement) in edits {
        if range.start < cursor {
            return None;
        }
        out.extend_from_slice(&raw[cursor..range.start]);
        out.extend_from_slice(&replacement);
        cursor = range.end;
    }
    out.extend_from_slice(&raw[cursor.min(raw.len())..]);
    Some(out)
}

/// The line-break bytes that end the line whose content ends at `pos`.
fn line_break_at(raw: &[u8], pos: usize) -> &'static [u8] {
    if pos >= 2 && raw[pos - 1] == b'\n' && raw[pos - 2] == b'\r' {
        b"\r\n"
    } else {
        b"\n"
    }
}

/// The cleaned-up closed range, in one place.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FmClosed {
    yaml_start: usize,
    close_start: usize,
    close_line_no: usize,
}

/// Every operation's prepared context on a note with closed front matter.
struct EditContext<'a> {
    raw: &'a [u8],
    fm: FmClosed,
    scan: Scan,
    doc: Yaml<'a>,
}

impl<'a> EditContext<'a> {
    /// The entry for `key`, refusing a key written outside the editable
    /// subset (single-quoted keys).
    fn find(&'a self, key: &str) -> Result<Option<&'a Entry>, FmEditError> {
        match self.scan.find(key) {
            None => Ok(None),
            Some(entry) if entry.key_form == KeyForm::SingleQuoted => {
                Err(FmEditError::KeyNotPlain {
                    key: key.to_string(),
                })
            }
            Some(entry) => Ok(Some(entry)),
        }
    }
}

/// Open the shared gates of an edit on a note whose front matter is closed:
/// the scanner with its file-wide refusals, then the parser with its errors,
/// then the agreement between the two on the target key.
fn open_edit<'a>(
    raw: &'a [u8],
    text: &'a str,
    fm: FmClosed,
    key: &str,
) -> Result<EditContext<'a>, FmEditError> {
    // Parse first, so a note the reader already reports broken refuses with
    // the reader-consistent reason; the scanner then refuses what the parser
    // silently accepts (duplicates, anchors, merge keys).
    let doc = parse_region(&text[fm.yaml_start..fm.close_start])?;
    let scan = scan_region(raw, fm.yaml_start..fm.close_start);
    if let Some(refusal) = scan.refusal.clone() {
        return Err(refusal);
    }
    let ctx = EditContext { raw, fm, scan, doc };
    pre_verify(raw, &ctx.scan, &ctx.doc, key)?;
    Ok(ctx)
}

fn closed_of(fm: FmRange) -> Option<FmClosed> {
    match fm {
        FmRange::Closed {
            yaml_start,
            close_start,
            close_line_no,
            ..
        } => Some(FmClosed {
            yaml_start,
            close_start,
            close_line_no,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Operations. Each takes the note's raw bytes and returns new bytes, or a
// typed refusal with no bytes. Untouched bytes stay byte-identical.
// ---------------------------------------------------------------------------

/// The note must be readable text before it is ever rewritten: the reader
/// works on `&str`, so a note that is not UTF-8 is never touched.
fn ensure_utf8(raw: &[u8]) -> Result<&str, FmEditError> {
    std::str::from_utf8(raw).map_err(|_| FmEditError::NotUtf8)
}

/// A new front matter block for a note without one: fences in the file's
/// newline style, the key line inside, the BOM kept, the body untouched.
fn created_block(
    raw: &[u8],
    bom_len: usize,
    key: &str,
    yaml_line: &str,
    expect: Expect,
) -> Result<Vec<u8>, FmEditError> {
    let newline = newline_style(raw);
    let block = [
        &b"---"[..],
        newline,
        yaml_line.as_bytes(),
        newline,
        b"---",
        newline,
    ]
    .concat();
    let new = [raw[..bom_len].to_vec(), block, raw[bom_len..].to_vec()].concat();
    verify_created(&new, key, &TargetAfter::Value(expect))?;
    Ok(new)
}

/// Insert a new key line just before the closing fence and verify it reads
/// back; the line uses the line break the front matter already ends with.
fn insert_key_line(
    ctx: &EditContext<'_>,
    key: &str,
    yaml_line: String,
    expect: Expect,
) -> Result<Vec<u8>, FmEditError> {
    validate_new_key(key)?;
    let break_style = line_break_at(ctx.raw, ctx.fm.close_start);
    let mut line_bytes: Vec<u8> = yaml_line.into_bytes();
    line_bytes.extend_from_slice(break_style);
    finish_edit(
        ctx,
        key,
        vec![(ctx.fm.close_start..ctx.fm.close_start, line_bytes)],
        TargetAfter::Value(expect),
    )
}

/// Apply the splices and verify the result; the only path from a plan to
/// new bytes. The verification re-parses the new front matter with saphyr
/// and requires the target key to read exactly the intended value.
fn finish_edit(
    ctx: &EditContext<'_>,
    key: &str,
    edits: Vec<(Range<usize>, Vec<u8>)>,
    after: TargetAfter,
) -> Result<Vec<u8>, FmEditError> {
    let Some(new) = apply_splices(ctx.raw, edits) else {
        return Err(FmEditError::VerificationFailed);
    };
    verify_after(&new, key, &after, &ctx.doc)?;
    Ok(new)
}

/// Verify a front matter block the editor itself created.
fn verify_created(new: &[u8], key: &str, after: &TargetAfter) -> Result<(), FmEditError> {
    let text = std::str::from_utf8(new).map_err(|_| FmEditError::VerificationFailed)?;
    let FmRange::Closed {
        yaml_start,
        close_start,
        ..
    } = front_matter_range(new)
    else {
        return Err(FmEditError::VerificationFailed);
    };
    let doc = parse_region(&text[yaml_start..close_start])
        .map_err(|_| FmEditError::VerificationFailed)?;
    let ok = match after {
        TargetAfter::Absent => mapping_get(&doc, key).is_none(),
        TargetAfter::Value(expect) => match mapping_get(&doc, key) {
            Some(value) => expect_of(value).is_some_and(|found| found == *expect),
            None => false,
        },
    };
    if ok {
        Ok(())
    } else {
        Err(FmEditError::VerificationFailed)
    }
}

/// Set `key` to the single-line scalar `value` (keys owned by typed
/// operations are refused: `rating` via [`set_rating`], `tags` via
/// [`add_tags`] and [`remove_tags`]).
///
/// * `key` absent — appended as the last line inside the fences as
///   `key: value` (or as the only key line of a new block when the note has
///   no front matter, using the file's newline style, before the body, after
///   any BOM).
/// * `key` present — its whole value token is replaced; quoting follows this
///   writer, which quotes what YAML could misread (FORMAT §3.1). Setting an
///   existing `id` refuses (they are immutable, FORMAT §3.3); writing a new
///   `id` requires a ULID.
pub fn set_scalar(raw: &[u8], key: &str, value: &str) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    validate_new_key(key)?;
    if key == "rating" || key == "tags" {
        return Err(FmEditError::ReservedKey {
            key: key.to_string(),
        });
    }
    if key == "id" && !is_valid_ulid(value) {
        return Err(FmEditError::NotAUlid);
    }
    let token = write_scalar(value);
    let expect = Expect::Str(value.to_string());
    match front_matter_range(raw) {
        FmRange::NoFrontMatter { bom_len } => {
            created_block(raw, bom_len, key, &format!("{key}: {token}"), expect)
        }
        FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
        FmRange::Closed { .. } => {
            let Some(fm) = closed_of(front_matter_range(raw)) else {
                unreachable!("the match arm already established a closed block");
            };
            let ctx = open_edit(raw, text, fm, key)?;
            match ctx.find(key)? {
                Some(entry) => {
                    if key == "id"
                        && !matches!(entry.value, ScannedValue::Empty { .. })
                        && !scalar_is_empty(&ctx.doc, key)
                    {
                        return Err(FmEditError::IdImmutable);
                    }
                    match &entry.value {
                        ScannedValue::Scalar { .. } => finish_edit(
                            &ctx,
                            key,
                            vec![(scalar_span(&entry.value), token.clone().into_bytes())],
                            TargetAfter::Value(expect),
                        ),
                        ScannedValue::Empty { .. } => {
                            let (insert_at, prefix_space, suffix_space) = empty_spot(&entry.value);
                            finish_edit(
                                &ctx,
                                key,
                                vec![(
                                    insert_at..insert_at,
                                    empty_insert_bytes(prefix_space, &token, suffix_space),
                                )],
                                TargetAfter::Value(expect),
                            )
                        }
                        _ => Err(FmEditError::ValueNotEditable),
                    }
                }
                None => insert_key_line(&ctx, key, format!("{key}: {token}"), expect),
            }
        }
    }
}

/// The bytes spliced into an empty value: a separating space when the colon
/// had none after it, the token, and one before a trailing comment.
fn empty_insert_bytes(prefix_space: bool, token: &str, suffix_space: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(token.len() + 2);
    if prefix_space {
        bytes.push(b' ');
    }
    bytes.extend_from_slice(token.as_bytes());
    if suffix_space {
        bytes.push(b' ');
    }
    bytes
}

/// Delete `key`'s entry. Deleting an absent key changes nothing. Deleting an
/// existing `id` refuses (FORMAT §3.3). Only entries inside the subset can
/// be deleted as a whole; nested structures stay untouched.
pub fn delete_key(raw: &[u8], key: &str) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    if key.is_empty() || key.contains(['\n', '\r']) {
        return Err(FmEditError::InvalidKey);
    }
    let fm = closed_of(front_matter_range(raw));
    let Some(fm) = fm else {
        return match front_matter_range(raw) {
            FmRange::NoFrontMatter { .. } => Ok(raw.to_vec()),
            FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
            FmRange::Closed { .. } => unreachable!(),
        };
    };
    let ctx = open_edit(raw, text, fm, key)?;
    let Some(entry) = ctx.find(key)? else {
        return Ok(raw.to_vec());
    };
    if key == "id"
        && !matches!(entry.value, ScannedValue::Empty { .. })
        && !scalar_is_empty(&ctx.doc, key)
    {
        return Err(FmEditError::IdImmutable);
    }
    match &entry.value {
        ScannedValue::Scalar { .. }
        | ScannedValue::Empty { .. }
        | ScannedValue::FlowSeq { .. }
        | ScannedValue::BlockSeq { .. } => {
            let span = entry.line_start..entry.end;
            finish_edit(&ctx, key, vec![(span, Vec::new())], TargetAfter::Absent)
        }
        ScannedValue::Uneditable => Err(FmEditError::ValueNotEditable),
    }
}

/// Set or clear the rating. Setting replaces the whole value token with an
/// unquoted 0–5 (`rating: "4"` becomes `rating: 5`); clearing deletes the
/// line; the rest of the note stays byte-identical.
pub fn set_rating(raw: &[u8], edit: RatingEdit) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    const KEY: &str = "rating";
    match edit {
        RatingEdit::Set(rating) if rating > 5 => Err(FmEditError::RatingOutOfRange(rating)),
        RatingEdit::Set(rating) => {
            let line = format!("{KEY}: {rating}");
            let expect = Expect::Int(i64::from(rating));
            let fm = closed_of(front_matter_range(raw));
            let Some(fm) = fm else {
                return match front_matter_range(raw) {
                    FmRange::NoFrontMatter { bom_len } => {
                        created_block(raw, bom_len, KEY, &line, expect)
                    }
                    FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
                    FmRange::Closed { .. } => unreachable!(),
                };
            };
            let ctx = open_edit(raw, text, fm, KEY)?;
            match ctx.find(KEY)? {
                Some(entry) => match &entry.value {
                    ScannedValue::Scalar { token } => finish_edit(
                        &ctx,
                        KEY,
                        vec![(token.clone(), rating.to_string().into_bytes())],
                        TargetAfter::Value(expect),
                    ),
                    ScannedValue::Empty { .. } => {
                        let (insert_at, prefix_space, suffix_space) = empty_spot(&entry.value);
                        finish_edit(
                            &ctx,
                            KEY,
                            vec![(
                                insert_at..insert_at,
                                empty_insert_bytes(prefix_space, &rating.to_string(), suffix_space),
                            )],
                            TargetAfter::Value(expect),
                        )
                    }
                    _ => Err(FmEditError::ValueNotEditable),
                },
                None => insert_key_line(&ctx, KEY, line, expect),
            }
        }
        RatingEdit::Clear => {
            let fm = closed_of(front_matter_range(raw));
            let Some(fm) = fm else {
                return match front_matter_range(raw) {
                    FmRange::NoFrontMatter { .. } => Ok(raw.to_vec()),
                    FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
                    FmRange::Closed { .. } => unreachable!(),
                };
            };
            let ctx = open_edit(raw, text, fm, KEY)?;
            match ctx.find(KEY)? {
                None => Ok(raw.to_vec()),
                Some(entry) => match &entry.value {
                    ScannedValue::Scalar { .. } | ScannedValue::Empty { .. } => {
                        let span = entry.line_start..entry.end;
                        finish_edit(&ctx, KEY, vec![(span, Vec::new())], TargetAfter::Absent)
                    }
                    _ => Err(FmEditError::ValueNotEditable),
                },
            }
        }
    }
}

/// The guard rails for tag lists, before any bytes are touched: strip one
/// leading `#`, reject forbidden characters and empties, and keep the first
/// spelling case-insensitively.
fn normalize_tag_list(tags: &[&str]) -> Result<Vec<String>, FmEditError> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let normalized = normalize_tag(tag)?;
        if seen.insert(normalized.to_lowercase()) {
            out.push(normalized);
        }
    }
    Ok(out)
}

/// The tag texts an entry already holds, in order, from the parsed
/// values. A `tags` list holding an element that is not a plain or
/// double-quoted *string* — a number, boolean or null — is outside
/// the closed subset (ADR-016 §5: flow and block lists carry string
/// elements), so it refuses as `ValueNotEditable` rather than as an
/// internal verification failure (RW61 M1). `Ok(None)` means the
/// parsed `tags` value is not a sequence, which the callers treat
/// as a scanner/parser disagreement.
fn existing_tag_texts(doc: &Yaml) -> Result<Option<Vec<String>>, FmEditError> {
    let Some(value) = mapping_get(doc, "tags") else {
        return Ok(None);
    };
    let Some(sequence) = value.as_vec() else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(sequence.len());
    for item in sequence {
        match item {
            Yaml::Value(Scalar::String(text)) => out.push(text.to_string()),
            _ => return Err(FmEditError::ValueNotEditable),
        }
    }
    Ok(Some(out))
}

/// The tags that survive, as expected values: the parsed existing ones kept
/// (and filtered), plus the additions as strings.
fn expected_tags(kept: &[String], additions: &[String]) -> Expect {
    let mut items: Vec<Expect> = kept.iter().map(|t| Expect::Str(t.clone())).collect();
    items.extend(additions.iter().map(|t| Expect::Str(t.clone())));
    Expect::List(items)
}

/// The flow-list token spellings for new tags.
fn flow_spelling(tags: &[String]) -> String {
    tags.iter()
        .map(|tag| write_scalar(tag))
        .collect::<Vec<String>>()
        .join(", ")
}

/// Read editable tag values without changing the note. Unsupported or ambiguous
/// front matter is refused using the same rules as tag edits.
pub fn editable_tags(raw: &[u8]) -> Result<Vec<String>, FmEditError> {
    let text = ensure_utf8(raw)?;
    let Some(fm) = closed_of(front_matter_range(raw)) else {
        return match front_matter_range(raw) {
            FmRange::NoFrontMatter { .. } => Ok(Vec::new()),
            FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
            FmRange::Closed { .. } => unreachable!(),
        };
    };
    let ctx = open_edit(raw, text, fm, "tags")?;
    let Some(entry) = ctx.find("tags")? else {
        return Ok(Vec::new());
    };
    match &entry.value {
        ScannedValue::Empty { .. } => Ok(Vec::new()),
        ScannedValue::Scalar { .. } => Err(FmEditError::TagsNotAList),
        ScannedValue::Uneditable => Err(FmEditError::ValueNotEditable),
        ScannedValue::FlowSeq { .. } | ScannedValue::BlockSeq { .. } => {
            existing_tag_texts(&ctx.doc)?.ok_or(FmEditError::VerificationFailed)
        }
    }
}

/// Add tags to the `tags` list, deduplicating case-insensitively and keeping
/// the first spelling. The existing style is kept — a flow list gains
/// elements before its `]`, a block list gains `- item` lines — and a note
/// without a `tags` key gets a new flow-style one (`tags: [a, b]`) as the
/// last line inside the fences (or as a new block, style- and BOM-aware,
/// when the note has no front matter).
pub fn add_tags(raw: &[u8], tags: &[&str]) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    let additions = normalize_tag_list(tags)?;
    if additions.is_empty() {
        return Ok(raw.to_vec());
    }
    const KEY: &str = "tags";
    let fm = closed_of(front_matter_range(raw));
    let Some(fm) = fm else {
        return match front_matter_range(raw) {
            FmRange::NoFrontMatter { bom_len } => created_block(
                raw,
                bom_len,
                KEY,
                &format!("{KEY}: [{}]", flow_spelling(&additions)),
                expected_tags(&[], &additions),
            ),
            FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
            FmRange::Closed { .. } => unreachable!(),
        };
    };
    let ctx = open_edit(raw, text, fm, KEY)?;
    match ctx.find(KEY)? {
        Some(entry) => match &entry.value {
            ScannedValue::FlowSeq { items, close_pos } => {
                let existing =
                    existing_tag_texts(&ctx.doc)?.ok_or(FmEditError::VerificationFailed)?;
                let existing_ci: std::collections::BTreeSet<String> =
                    existing.iter().map(|t| t.to_lowercase()).collect();
                let to_add: Vec<String> = additions
                    .iter()
                    .filter(|tag| !existing_ci.contains(&tag.to_lowercase()))
                    .cloned()
                    .collect();
                if to_add.is_empty() {
                    return Ok(raw.to_vec());
                }
                let insert = if items.is_empty() {
                    flow_spelling(&to_add)
                } else {
                    format!(", {}", flow_spelling(&to_add))
                };
                finish_edit(
                    &ctx,
                    KEY,
                    vec![(*close_pos..*close_pos, insert.into_bytes())],
                    TargetAfter::Value(expected_tags(&existing, &to_add)),
                )
            }
            ScannedValue::BlockSeq { items } => {
                let existing =
                    existing_tag_texts(&ctx.doc)?.ok_or(FmEditError::VerificationFailed)?;
                let existing_ci: std::collections::BTreeSet<String> =
                    existing.iter().map(|t| t.to_lowercase()).collect();
                let to_add: Vec<String> = additions
                    .iter()
                    .filter(|tag| !existing_ci.contains(&tag.to_lowercase()))
                    .cloned()
                    .collect();
                if to_add.is_empty() {
                    return Ok(raw.to_vec());
                }
                let Some(last) = items.last() else {
                    return Err(FmEditError::VerificationFailed);
                };
                let indent = " ".repeat(last.dash_col);
                let break_style = line_break_at(ctx.raw, last.end);
                let mut insert = Vec::new();
                for tag in &to_add {
                    insert.extend_from_slice(indent.as_bytes());
                    insert.push(b'-');
                    insert.push(b' ');
                    insert.extend_from_slice(write_scalar(tag).into_bytes().as_slice());
                    insert.extend_from_slice(break_style);
                }
                finish_edit(
                    &ctx,
                    KEY,
                    vec![(last.end..last.end, insert)],
                    TargetAfter::Value(expected_tags(&existing, &to_add)),
                )
            }
            ScannedValue::Empty { .. } => {
                let (insert_at, prefix_space, suffix_space) = empty_spot(&entry.value);
                let bytes = empty_insert_bytes(
                    prefix_space,
                    &format!("[{}]", flow_spelling(&additions)),
                    suffix_space,
                );
                // Nothing to deduplicate against: an empty `tags:` reads as
                // no tags.
                finish_edit(
                    &ctx,
                    KEY,
                    vec![(insert_at..insert_at, bytes)],
                    TargetAfter::Value(expected_tags(&[], &additions)),
                )
            }
            ScannedValue::Scalar { .. } => Err(FmEditError::TagsNotAList),
            ScannedValue::Uneditable => Err(FmEditError::ValueNotEditable),
        },
        None => insert_key_line(
            &ctx,
            KEY,
            format!("{KEY}: [{}]", flow_spelling(&additions)),
            expected_tags(&[], &additions),
        ),
    }
}

/// Remove tags from the `tags` list, case-insensitively, keeping the byte
/// spelling of what stays. Removing the last tag deletes the `tags` key;
/// removing nothing changes nothing.
pub fn remove_tags(raw: &[u8], tags: &[&str]) -> Result<Vec<u8>, FmEditError> {
    // Preserve validation order and the no-op behaviour for empty selections.
    ensure_utf8(raw)?;
    let removals = normalize_tag_list(tags)?;
    if removals.is_empty() {
        return Ok(raw.to_vec());
    }
    let remove_ci: std::collections::BTreeSet<String> =
        removals.iter().map(|t| t.to_lowercase()).collect();
    remove_matching_tags(raw, |tag| remove_ci.contains(&tag.to_lowercase()))
}

/// Remove exact stored tag spellings, including historical names containing
/// spaces. This does not normalize inputs or resolve aliases.
pub fn remove_exact_tags(raw: &[u8], tags: &[&str]) -> Result<Vec<u8>, FmEditError> {
    ensure_utf8(raw)?;
    if tags.is_empty() {
        return Ok(raw.to_vec());
    }
    let removals: std::collections::BTreeSet<&str> = tags.iter().copied().collect();
    remove_matching_tags(raw, |tag| removals.contains(tag))
}

fn remove_matching_tags(
    raw: &[u8],
    matches: impl Fn(&str) -> bool,
) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    const KEY: &str = "tags";
    let fm = closed_of(front_matter_range(raw));
    let Some(fm) = fm else {
        return match front_matter_range(raw) {
            FmRange::NoFrontMatter { .. } => Ok(raw.to_vec()),
            FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
            FmRange::Closed { .. } => unreachable!(),
        };
    };
    let ctx = open_edit(raw, text, fm, KEY)?;
    let Some(entry) = ctx.find(KEY)? else {
        return Ok(raw.to_vec());
    };
    let existing_and_matched = |doc: &Yaml| {
        let Some(existing) = existing_tag_texts(doc)? else {
            return Ok(None);
        };
        let matched: Vec<bool> = existing.iter().map(|t| matches(t)).collect();
        Ok(Some((existing, matched)))
    };
    match &entry.value {
        // Nothing on the key line can hold tags to remove.
        ScannedValue::Empty { .. } => Ok(raw.to_vec()),
        ScannedValue::Scalar { .. } => Err(FmEditError::TagsNotAList),
        ScannedValue::Uneditable => Err(FmEditError::ValueNotEditable),
        ScannedValue::BlockSeq { .. } => {
            let (existing, matched) =
                existing_and_matched(&ctx.doc)?.ok_or(FmEditError::VerificationFailed)?;
            if !matched.iter().any(|m| *m) {
                return Ok(raw.to_vec());
            }
            if matched.iter().all(|m| *m) {
                let span = entry.line_start..entry.end;
                return finish_edit(&ctx, KEY, vec![(span, Vec::new())], TargetAfter::Absent);
            }
            let kept: Vec<String> = existing
                .iter()
                .zip(matched.iter())
                .filter(|(_, m)| !*m)
                .map(|(t, _)| t.clone())
                .collect();
            let edits: Vec<(Range<usize>, Vec<u8>)> = items_of(entry)
                .zip(matched.iter())
                .filter(|(_, m)| **m)
                .map(|(item, _)| (item.line_start..item.end, Vec::new()))
                .collect();
            finish_edit(
                &ctx,
                KEY,
                edits,
                TargetAfter::Value(expected_tags(&kept, &[])),
            )
        }
        ScannedValue::FlowSeq { items, .. } => {
            let (existing, matched) =
                existing_and_matched(&ctx.doc)?.ok_or(FmEditError::VerificationFailed)?;
            if !matched.iter().any(|m| *m) {
                return Ok(raw.to_vec());
            }
            if matched.iter().all(|m| *m) {
                // Removing the last tag deletes the key.
                let span = entry.line_start..entry.end;
                return finish_edit(&ctx, KEY, vec![(span, Vec::new())], TargetAfter::Absent);
            }
            let kept: Vec<String> = existing
                .iter()
                .zip(matched.iter())
                .filter(|(_, m)| !*m)
                .map(|(t, _)| t.clone())
                .collect();
            // Excise whole runs of matched elements: a run that begins with
            // the first element takes its trailing separator with it; any
            // other run takes the comma that leads it.
            let mut edits: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
            let mut idx = 0usize;
            while idx < items.len() {
                if !matched[idx] {
                    idx += 1;
                    continue;
                }
                let run_start = idx;
                while idx < items.len() && matched[idx] {
                    idx += 1;
                }
                let run_end = idx;
                let start_token = item_token(&items[run_start]);
                let end_token = item_token(&items[run_end - 1]);
                if run_start == 0 {
                    if run_end < items.len() {
                        let next_token = item_token(&items[run_end]);
                        edits.push((start_token.start..next_token.start, Vec::new()));
                    }
                } else {
                    let comma = comma_before(ctx.raw, start_token.start);
                    edits.push((comma..end_token.end, Vec::new()));
                }
            }
            finish_edit(
                &ctx,
                KEY,
                edits,
                TargetAfter::Value(expected_tags(&kept, &[])),
            )
        }
    }
}

fn items_of(entry: &Entry) -> impl Iterator<Item = &BlockItem> {
    match &entry.value {
        ScannedValue::BlockSeq { items } => items.iter(),
        _ => [].iter(),
    }
}

fn item_token(item: &FlowItem) -> &Range<usize> {
    item.token().expect("FlowSeq values hold only tokens")
}

/// The comma preceding the element token starting at `from`.
fn comma_before(raw: &[u8], from: usize) -> usize {
    let mut i = from;
    while i > 0 && matches!(raw[i - 1], b' ' | b'\t' | b'\r') {
        i -= 1;
    }
    if i > 0 && raw[i - 1] == b',' {
        i - 1
    } else {
        from
    }
}

/// Write the `id` when — and only when — it is absent: a ULID in canonical
/// uppercase. An existing `id` is left alone (FORMAT §3.3); an empty
/// spelling — a bare `id:`, `id: ""` or `id: null`, all of which the
/// reader reads as no id — is writable.
pub fn ensure_id(raw: &[u8], ulid: &str) -> Result<Vec<u8>, FmEditError> {
    let text = ensure_utf8(raw)?;
    if !is_valid_ulid(ulid) {
        return Err(FmEditError::NotAUlid);
    }
    let canonical = ulid.to_ascii_uppercase();
    const KEY: &str = "id";
    let fm = closed_of(front_matter_range(raw));
    let Some(fm) = fm else {
        return match front_matter_range(raw) {
            FmRange::NoFrontMatter { bom_len } => created_block(
                raw,
                bom_len,
                KEY,
                &format!("{KEY}: {canonical}"),
                Expect::Str(canonical),
            ),
            FmRange::Unterminated { .. } => Err(FmEditError::UnterminatedFrontMatter),
            FmRange::Closed { .. } => unreachable!(),
        };
    };
    let ctx = open_edit(raw, text, fm, KEY)?;
    match ctx.find(KEY)? {
        Some(entry) => match &entry.value {
            // `id: ""` / `id: null` read as no id (the reader's
            // spelling), so the writer fills them like a bare
            // `id:` rather than treating the key as present.
            ScannedValue::Scalar { .. } if scalar_is_empty(&ctx.doc, KEY) => {
                let token = scalar_span(&entry.value);
                finish_edit(
                    &ctx,
                    KEY,
                    vec![(token, canonical.clone().into_bytes())],
                    TargetAfter::Value(Expect::Str(canonical)),
                )
            }
            ScannedValue::Scalar { .. } => Ok(raw.to_vec()),
            ScannedValue::Empty { .. } => {
                let (insert_at, prefix_space, suffix_space) = empty_spot(&entry.value);
                let bytes = empty_insert_bytes(prefix_space, &canonical, suffix_space);
                finish_edit(
                    &ctx,
                    KEY,
                    vec![(insert_at..insert_at, bytes)],
                    TargetAfter::Value(Expect::Str(canonical)),
                )
            }
            _ => Err(FmEditError::ValueNotEditable),
        },
        None => insert_key_line(
            &ctx,
            KEY,
            format!("{KEY}: {canonical}"),
            Expect::Str(canonical),
        ),
    }
}

/// Append the self-embed as the last block when the note does not already
/// contain it. The caller supplies the link text (W62 owns its spelling);
/// presence is a byte match against the body. The append uses the file's
/// newline style — except that a file whose only breaks are lone `\r`s
/// (a CR-only file) keeps that style for the embed's own line, instead
/// of the mixed `\r\n` break plus LF line it used to get (RW61 L4). A
/// trailing `\r` of a `\n`-using file is an incomplete CRLF break and is
/// completed rather than joined into the embed's own line. Front matter
/// is never re-serialized.
pub fn ensure_self_embed(raw: &[u8], embed_text: &str) -> Result<Vec<u8>, FmEditError> {
    ensure_utf8(raw)?;
    let body_start = match front_matter_range(raw) {
        FmRange::NoFrontMatter { bom_len } => bom_len,
        FmRange::Closed { body_start, .. } | FmRange::Unterminated { body_start } => body_start,
    };
    if embed_text.is_empty() || contains_slice(&raw[body_start..], embed_text.as_bytes()) {
        return Ok(raw.to_vec());
    }
    // The embed line's style: the file's own, except that a
    // CR-only file (no `\n` anywhere, breaks of lone `\r`) keeps
    // its CR breaks.
    let newline = if raw.contains(&b'\n') {
        newline_style(raw)
    } else if raw.contains(&b'\r') {
        b"\r"
    } else {
        b"\n"
    };
    let after_bom = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    if after_bom.is_empty() {
        // An empty (or BOM-only) note becomes just the embed.
        let mut out = raw[..raw.len() - after_bom.len()].to_vec();
        out.extend_from_slice(embed_text.as_bytes());
        out.extend_from_slice(newline);
        return Ok(out);
    }
    let mut out = raw.to_vec();
    match out.last() {
        Some(b'\n') => {}
        // A `\n`-using file's trailing `\r` is an incomplete
        // CRLF break (or a stray CR): complete it.
        Some(b'\r') if newline != b"\r" => out.push(b'\n'),
        // A CR-only file's trailing `\r` is a complete break
        // of the file's own style; the embed starts a new
        // line after it.
        Some(b'\r') => {}
        _ => out.extend_from_slice(newline),
    }
    out.extend_from_slice(embed_text.as_bytes());
    out.extend_from_slice(newline);
    Ok(out)
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    /// Byte-exact change: `expected_old` must occur exactly once in
    /// `before`; the splice must replace exactly that occurrence with
    /// `expected_new`, leaving every byte before and after it identical.
    fn assert_only_splice(before: &[u8], after: &[u8], expected_old: &[u8], expected_new: &[u8]) {
        let positions: Vec<usize> = (0..=before.len().saturating_sub(expected_old.len()))
            .filter(|start| before[*start..start + expected_old.len()] == *expected_old)
            .collect();
        assert_eq!(positions.len(), 1, "the intended old range must occur once");
        let at = positions[0];
        assert_eq!(&before[..at], &after[..at], "the prefix must be identical");
        assert_eq!(
            &after[at..at + expected_new.len()],
            expected_new,
            "the new bytes differ from the intended spelling"
        );
        assert_eq!(
            &before[at + expected_old.len()..],
            &after[at + expected_new.len()..],
            "the suffix must be identical"
        );
    }

    #[test]
    fn set_scalar_replaces_the_token_byte_exactly() {
        let before = b"---\ntitle: Old name\nrating: 4\n---\nbody\n".to_vec();
        let after = set_scalar(&before, "title", "New name").unwrap();
        assert_only_splice(&before, &after, b"Old name", b"New name");
    }

    #[test]
    fn set_scalar_quotes_values_yaml_could_misread() {
        let before = b"---\ntitle: plain\n---\n".to_vec();
        let after = set_scalar(&before, "title", "4").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("title: \"4\""));
        let after = set_scalar(&before, "title", "no").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("title: \"no\""));
        let after = set_scalar(&before, "title", "a: b").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("title: \"a: b\""));
        let after = set_scalar(&before, "title", " leading space").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("title: \" leading space\""));
        // Plain values stay plain.
        let after = set_scalar(&before, "title", "Girl sinking").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("title: Girl sinking"));
    }

    #[test]
    fn set_scalar_appends_new_keys_as_the_last_line_inside_the_fences() {
        let before = b"---\nk1: 1\nk2: 2\n---\nbody\n".to_vec();
        let after = set_scalar(&before, "title", "T").unwrap();
        assert_eq!(after, b"---\nk1: 1\nk2: 2\ntitle: T\n---\nbody\n");
    }

    #[test]
    fn set_scalar_on_an_empty_value_inserts_after_the_colon() {
        let before = b"---\ncopied_from:\n---\n".to_vec();
        let after = set_scalar(&before, "copied_from", "01JA8X3Q7K2M9V4T6R1B5N0C3Q").unwrap();
        assert_eq!(
            after,
            b"---\ncopied_from: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n"
        );

        // A comment follows the colon: the token splits it with a space.
        let before = b"---\ncopied_from: # none yet\n---\n".to_vec();
        let after = set_scalar(&before, "copied_from", "01JA8X3Q7K2M9V4T6R1B5N0C3Q").unwrap();
        assert_eq!(
            after,
            b"---\ncopied_from: 01JA8X3Q7K2M9V4T6R1B5N0C3Q # none yet\n---\n"
        );
    }

    #[test]
    fn set_scalar_fills_an_empty_value_with_a_trailing_space() {
        let before = b"---\ncopied_from: \n---\n".to_vec();
        let after = set_scalar(&before, "copied_from", "x").unwrap();
        assert_eq!(after, b"---\ncopied_from: x\n---\n");
    }

    #[test]
    fn set_scalar_without_front_matter_creates_a_block() {
        let before = b"Body text.\n".to_vec();
        let after = set_scalar(&before, "title", "T").unwrap();
        assert_eq!(after, b"---\ntitle: T\n---\nBody text.\n");

        // A BOM is kept and the body stays byte-identical after it.
        let before = b"\xEF\xBB\xBFBody\r\ntext.\r\n".to_vec();
        let after = set_scalar(&before, "title", "T").unwrap();
        assert_eq!(
            after,
            b"\xEF\xBB\xBF---\r\ntitle: T\r\n---\r\nBody\r\ntext.\r\n"
        );
    }

    #[test]
    fn set_scalar_keeps_a_comment_on_the_edited_line() {
        let before = b"---\ntitle: Old # why\n---\n".to_vec();
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_eq!(after, b"---\ntitle: New # why\n---\n");
    }

    #[test]
    fn set_scalar_keeps_neighbour_comments_and_order() {
        let before =
            b"---\n# about the title\ntitle: Old\n# about the rest\nrating: 4\n---\n".to_vec();
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_eq!(
            after,
            b"---\n# about the title\ntitle: New\n# about the rest\nrating: 4\n---\n"
        );
    }

    #[test]
    fn set_scalar_refuses_reserved_and_non_writable_keys() {
        let note = b"---\nrating: 4\ntags: [a]\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n".to_vec();
        assert_eq!(
            set_scalar(&note, "rating", "5"),
            Err(FmEditError::ReservedKey {
                key: "rating".to_string()
            })
        );
        assert_eq!(
            set_scalar(&note, "tags", "one"),
            Err(FmEditError::ReservedKey {
                key: "tags".to_string()
            })
        );
        assert_eq!(
            set_scalar(&note, "id", "01JA8X3Q7K2M9V4T6R1B5N0C3P"),
            Err(FmEditError::IdImmutable)
        );
        assert_eq!(
            set_scalar(&note, "id", "not a ulid"),
            Err(FmEditError::NotAUlid)
        );
        assert_eq!(set_scalar(&note, "", "x"), Err(FmEditError::InvalidKey));
        assert_eq!(set_scalar(&note, "a: b", "x"), Err(FmEditError::InvalidKey));
    }

    #[test]
    fn delete_key_removes_the_whole_line_and_nothing_else() {
        let before = b"---\nk1: 1\ntitle: T\nk2: 2\n---\nbody\n".to_vec();
        let after = delete_key(&before, "title").unwrap();
        assert_only_splice(&before, &after, b"title: T\n", b"");
        assert_eq!(after, b"---\nk1: 1\nk2: 2\n---\nbody\n");

        // Deleting a missing key changes nothing; deleting an id refuses.
        assert_eq!(delete_key(&before, "absent"), Ok(before.clone()));
        assert_eq!(
            delete_key(&b"---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n"[..], "id"),
            Err(FmEditError::IdImmutable)
        );
    }

    #[test]
    fn rating_set_rewrites_the_whole_token_unquoted_and_clear_deletes_the_line() {
        let before = b"---\nid: x\nrating: \"4\"\n---\nbody\n".to_vec();
        let after = set_rating(&before, RatingEdit::Set(5)).unwrap();
        assert_only_splice(&before, &after, b"\"4\"", b"5");

        let cleared = set_rating(&after, RatingEdit::Clear).unwrap();
        assert_only_splice(&after, &cleared, b"rating: 5\n", b"");

        // Plain ratings and ratings with a trailing comment work the same.
        let before = b"---\nrating: 4 # nice\n---\n".to_vec();
        let after = set_rating(&before, RatingEdit::Set(0)).unwrap();
        assert_eq!(after, b"---\nrating: 0 # nice\n---\n");

        // New key as the last line, clear of an absent rating no-ops.
        let before = b"---\ntitle: T\n---\n".to_vec();
        let after = set_rating(&before, RatingEdit::Set(3)).unwrap();
        assert_eq!(after, b"---\ntitle: T\nrating: 3\n---\n");
        assert_eq!(set_rating(&before, RatingEdit::Clear), Ok(before.clone()));
        assert_eq!(
            set_rating(&before, RatingEdit::Set(6)),
            Err(FmEditError::RatingOutOfRange(6))
        );
    }

    #[test]
    fn rating_in_a_note_without_front_matter_creates_a_block() {
        let before = b"![[a.png]]\n".to_vec();
        let after = set_rating(&before, RatingEdit::Set(4)).unwrap();
        assert_eq!(after, b"---\nrating: 4\n---\n![[a.png]]\n");
    }

    #[test]
    fn add_tags_to_a_flow_list_splices_before_the_closing_bracket() {
        let before = b"---\ntags: [a, \"b,c\"]\n---\n".to_vec();
        let after = add_tags(&before, &["d", "#e"]).unwrap();
        assert_only_splice(&before, &after, b"\"b,c\"]", b"\"b,c\", d, e]");
        assert_eq!(after, b"---\ntags: [a, \"b,c\", d, e]\n---\n");
    }

    #[test]
    fn add_tags_to_an_empty_flow_list_and_to_block_lists() {
        let before = b"---\ntags: []\n---\n".to_vec();
        let after = add_tags(&before, &["a", "b"]).unwrap();
        assert_eq!(after, b"---\ntags: [a, b]\n---\n");

        let before = b"---\ntags:\n  - a\n  - b\n---\n".to_vec();
        let after = add_tags(&before, &["c"]).unwrap();
        assert_only_splice(&before, &after, b"  - b\n", b"  - b\n  - c\n");
        assert_eq!(after, b"---\ntags:\n  - a\n  - b\n  - c\n---\n");

        // Items at their parent key's indentation, YAML's other shape.
        let before = b"---\ntags:\n- a\n---\n".to_vec();
        let after = add_tags(&before, &["b"]).unwrap();
        assert_eq!(after, b"---\ntags:\n- a\n- b\n---\n");

        // An empty `tags:` gets a flow list after the colon.
        let before = b"---\ntags:\n---\n".to_vec();
        let after = add_tags(&before, &["a"]).unwrap();
        assert_eq!(after, b"---\ntags: [a]\n---\n");
    }

    #[test]
    fn add_tags_dedupes_case_insensitively_keeping_the_first_spelling() {
        let before = b"---\ntags: [style/Ink, STYLE/ink, monochrome]\n---\n".to_vec();
        // The new spelling of both existing tags: nothing to add.
        assert_eq!(
            add_tags(&before, &["Style/ink", "STYLE/INK"]),
            Ok(before.clone())
        );
        let after = add_tags(&before, &["Style/ink", "new"]).unwrap();
        assert_eq!(
            after,
            b"---\ntags: [style/Ink, STYLE/ink, monochrome, new]\n---\n"
        );
        // Deduplication inside one call keeps the first spelling.
        let before = b"---\ntags: [a]\n---\n".to_vec();
        let after = add_tags(&before, &["D", "d", "D"]).unwrap();
        assert_eq!(after, b"---\ntags: [a, D]\n---\n");
    }

    #[test]
    fn add_tags_creates_a_flow_list_key_when_tags_is_absent() {
        let before = b"---\ntitle: T\n---\n".to_vec();
        let after = add_tags(&before, &["blue", "style/ink"]).unwrap();
        assert_eq!(after, b"---\ntitle: T\ntags: [blue, style/ink]\n---\n");

        let before = b"body only\n".to_vec();
        let after = add_tags(&before, &["blue"]).unwrap();
        assert_eq!(after, b"---\ntags: [blue]\n---\nbody only\n");
    }

    #[test]
    fn remove_tags_excises_runs_exactly_and_deletes_the_key_when_empty() {
        // Removing a run at both ends: two exact excisions in one pass.
        let before = b"---\ntags: [a, b, c]\n---\n".to_vec();
        let after = remove_tags(&before, &["a", "c"]).unwrap();
        assert_eq!(after, b"---\ntags: [b]\n---\n");

        let before = b"---\ntags: [a, b, c]\n---\n".to_vec();
        let after = remove_tags(&before, &["b"]).unwrap();
        assert_only_splice(&before, &after, b", b", b"");
        assert_eq!(after, b"---\ntags: [a, c]\n---\n");

        // Removing every tag deletes the key; a no-match removes nothing.
        let after = remove_tags(&before, &["A", "B", "c"]).unwrap();
        assert_only_splice(&before, &after, b"tags: [a, b, c]\n", b"");
        assert_eq!(after, b"---\n---\n");
        assert_eq!(remove_tags(&before, &["nope"]), Ok(before.clone()));

        // Block lists drop whole item lines.
        let before = b"---\nIdk: x\ntags:\n  - one\n  - two\nrating: 3\n---\n".to_vec();
        let after = remove_tags(&before, &["TWO"]).unwrap();
        assert_only_splice(&before, &after, b"  - two\n", b"");
        assert_eq!(after, b"---\nIdk: x\ntags:\n  - one\nrating: 3\n---\n");
    }

    #[test]
    fn add_then_remove_the_same_tag_returns_the_original_bytes() {
        for before in [
            b"---\ntags: [a]\n---\n".to_vec(),
            b"---\ntags: [ a , b ]\n---\n".to_vec(),
            b"---\ntags:\n  - a\n---\n".to_vec(),
            b"---\ntags: [a, \"b,c\"]\n---\n".to_vec(),
        ] {
            let after = add_tags(&before, &["b2"]).unwrap();
            assert_ne!(after, before);
            let restored = remove_tags(&after, &["b2"]).unwrap();
            assert_eq!(restored, before, "must restore original bytes");
        }
    }

    #[test]
    fn a_non_string_tag_element_is_outside_the_subset() {
        // A `tags` list holding a number, boolean or null
        // element is outside the closed subset: it refuses
        // as ValueNotEditable, not as an internal
        // verification failure (RW61 M1).
        for before in [
            b"---\ntags: [4]\n---\n".to_vec(),
            b"---\ntags: [a, true]\n---\n".to_vec(),
            b"---\ntags: [null]\n---\n".to_vec(),
            b"---\ntags: [0x10]\n---\n".to_vec(),
            b"---\ntags:\n  - 4\n---\n".to_vec(),
        ] {
            assert_eq!(
                add_tags(&before, &["new"]),
                Err(FmEditError::ValueNotEditable),
                "add_tags on {:?}",
                before
            );
            assert_eq!(
                remove_tags(&before, &["a"]),
                Err(FmEditError::ValueNotEditable),
                "remove_tags on {:?}",
                before
            );
        }
        // A quoted "4" is a string element and stays
        // editable, keeping its spelling.
        let before = b"---\ntags: [\"4\"]\n---\n".to_vec();
        assert_eq!(
            add_tags(&before, &["five"]).unwrap(),
            b"---\ntags: [\"4\", five]\n---\n"
        );
    }

    #[test]
    fn ensure_id_writes_only_when_absent() {
        let id = "01JA8X3Q7K2M9V4T6R1B5N0C3Q";
        let before = b"---\ntitle: T\n---\n".to_vec();
        let after = ensure_id(&before, id).unwrap();
        assert_eq!(after, format!("---\ntitle: T\nid: {id}\n---\n").as_bytes());

        // Present: unchanged, bytes and all.
        assert_eq!(
            ensure_id(&after, "01JA8X3Q7K2M9V4T6R1B5N0C3P"),
            Ok(after.clone())
        );
        // Empty: written after the colon.
        let before = b"---\nid:\n---\n".to_vec();
        let after = ensure_id(&before, id).unwrap();
        assert_eq!(after, format!("---\nid: {id}\n---\n").as_bytes());
        // `id: ""` and `id: null` read as no id to the reader,
        // so the writer fills them too (RW61 L1).
        for before in [
            b"---\nid: \"\"\n---\n".to_vec(),
            b"---\nid: null\n---\n".to_vec(),
        ] {
            let after = ensure_id(&before, id).unwrap();
            assert_eq!(after, format!("---\nid: {id}\n---\n").as_bytes());
            // Deleting the empty spelling is allowed as well.
            let before = b"---\nid: \"\"\n---\n".to_vec();
            assert_eq!(delete_key(&before, "id").unwrap(), b"---\n---\n");
            let before = b"---\nid: null\n---\n".to_vec();
            assert_eq!(delete_key(&before, "id").unwrap(), b"---\n---\n");
        }
        // Invalid input refuses.
        assert_eq!(ensure_id(&before, "nope"), Err(FmEditError::NotAUlid));
        assert_eq!(
            ensure_id(&before, "01JA8X3Q7K2M9V4T6R1B5N0C3"),
            Err(FmEditError::NotAUlid)
        );
        // A block of front matter appears when the note had none.
        let before = b"".to_vec();
        let after = ensure_id(&before, id).unwrap();
        assert_eq!(after, format!("---\nid: {id}\n---\n").as_bytes());
    }

    #[test]
    fn self_embed_appends_once_in_the_files_style() {
        let embed = "![[girl.jpg]]";
        let before = b"---\ntitle: T\n---\n\nText.\n".to_vec();
        let after = ensure_self_embed(&before, embed).unwrap();
        assert_eq!(after, b"---\ntitle: T\n---\n\nText.\n![[girl.jpg]]\n");
        assert_eq!(ensure_self_embed(&after, embed), Ok(after.clone()));

        // Files not ending in a break first get one, in their own style.
        let before = b"---\r\ntitle: T\r\n---\r\nText.".to_vec();
        let after = ensure_self_embed(&before, embed).unwrap();
        assert_eq!(
            after,
            b"---\r\ntitle: T\r\n---\r\nText.\r\n![[girl.jpg]]\r\n"
        );

        // A CR-only file keeps its own style: the trailing `\r`
        // is a complete break, and the embed gets its own CR
        // line, not an LF line glued onto a mixed break (RW61 L4).
        let before = b"Text.\r".to_vec();
        let after = ensure_self_embed(&before, embed).unwrap();
        assert_eq!(after, b"Text.\r![[girl.jpg]]\r");
        // The same for a file with several CR-only lines.
        let before = b"Text.\rMore.\r".to_vec();
        let after = ensure_self_embed(&before, embed).unwrap();
        assert_eq!(after, b"Text.\rMore.\r![[girl.jpg]]\r");
        // A `\n`-using file's stray trailing `\r` is still
        // completed, not glued to the embed.
        let before = b"Text\n.\r".to_vec();
        let after = ensure_self_embed(&before, embed).unwrap();
        assert_eq!(after, b"Text\n.\r\n![[girl.jpg]]\n");

        // Empty and BOM-only notes become just the embed.
        let after = ensure_self_embed(b"", embed).unwrap();
        assert_eq!(after, b"![[girl.jpg]]\n");
        let after = ensure_self_embed(b"\xEF\xBB\xBF", embed).unwrap();
        assert_eq!(after, b"\xEF\xBB\xBF![[girl.jpg]]\n");
    }

    #[test]
    fn refusals_report_reasons_without_touching_bytes() {
        let quoted_key = b"---\n'title': T\n---\n".to_vec();

        let mut expected = FmEditError::KeyNotPlain {
            key: "title".to_string(),
        };
        assert_eq!(
            set_scalar(&quoted_key, "title", "New"),
            Err(expected.clone())
        );
        expected = FmEditError::KeyNotPlain {
            key: "title".into(),
        };
        assert_eq!(delete_key(&quoted_key, "title"), Err(expected));

        // Duplicate key anywhere: refuse everything. (Keys with a typed
        // operation of their own refuse first, so these use plain keys.)
        let duplicate = b"---\nauthor: A\nauthor: B\nrating: 4\n---\n".to_vec();
        assert_eq!(
            set_scalar(&duplicate, "title", "T"),
            Err(FmEditError::DuplicateTopLevelKey)
        );
        assert_eq!(
            delete_key(&duplicate, "author"),
            Err(FmEditError::DuplicateTopLevelKey)
        );
        assert_eq!(
            set_rating(&duplicate, RatingEdit::Set(3)),
            Err(FmEditError::DuplicateTopLevelKey)
        );

        // Merge keys, anchors, aliases.
        assert_eq!(
            set_scalar(&b"---\n<<: {a: 1}\ntitle: T\n---\n"[..], "title", "N"),
            Err(FmEditError::MergeKey)
        );
        assert_eq!(
            set_scalar(&b"---\nbase: &a v\ntitle: T\n---\n"[..], "title", "N"),
            Err(FmEditError::AnchorOrAlias)
        );
        // An alias to an anchor that does not exist does not even parse,
        // and refuses with the reader's own reason.
        assert!(matches!(
            set_scalar(&b"---\nother: *a\ntitle: T\n---\n"[..], "title", "N"),
            Err(FmEditError::FrontMatterInvalid { .. })
        ));

        // Unterminated and reader-error front matter.
        assert_eq!(
            set_scalar(&b"---\ntitle: T\n"[..], "title", "N"),
            Err(FmEditError::UnterminatedFrontMatter)
        );
        assert!(matches!(
            set_scalar(&b"---\nbroken: [\ntitle: T\n---\n"[..], "title", "N"),
            Err(FmEditError::FrontMatterInvalid { .. })
        ));
        assert!(matches!(
            set_scalar(&b"---\njust a scalar\n---\n"[..], "title", "N"),
            Err(FmEditError::FrontMatterInvalid { .. })
        ));

        // Values outside the subset refuse: block scalars, nested maps,
        // multiline quoted scalars, single-quoted scalars.
        assert_eq!(
            set_scalar(&b"---\ndesc: |\n  text\ntitle: T\n---\n"[..], "desc", "x"),
            Err(FmEditError::ValueNotEditable)
        );
        assert_eq!(
            set_scalar(
                &b"---\nsources:\n  - type: eagle\ntitle: T\n---\n"[..],
                "sources",
                "x"
            ),
            Err(FmEditError::ValueNotEditable)
        );
        assert_eq!(
            set_scalar(&b"---\ndesc: 'sq'\n---\n"[..], "desc", "x"),
            Err(FmEditError::ValueNotEditable)
        );
        // A correctly continued multiline scalar parses, and then its value
        // is outside the subset; the badly indented one is a reader error.
        assert_eq!(
            set_scalar(
                &b"---\ndesc: \"one\n  two\"\ntitle: T\n---\n"[..],
                "desc",
                "x"
            ),
            Err(FmEditError::ValueNotEditable)
        );
        assert!(matches!(
            set_scalar(
                &b"---\ndesc: \"one\ntwo\"\ntitle: T\n---\n"[..],
                "desc",
                "x"
            ),
            Err(FmEditError::FrontMatterInvalid { .. })
        ));

        // Tags as a scalar, and tags as anything but a list of strings.
        assert_eq!(
            add_tags(&b"---\ntags: one two\n---\n"[..], &["x"]),
            Err(FmEditError::TagsNotAList)
        );
        assert_eq!(
            add_tags(&b"---\ntags: [a, 'b']\n---\n"[..], &["x"]),
            Err(FmEditError::ValueNotEditable)
        );
        assert_eq!(
            add_tags(&b"---\ntags:\n  - a: 1\n---\n"[..], &["x"]),
            Err(FmEditError::ValueNotEditable)
        );

        // Invalid tag inputs refuse before anything happens.
        assert_eq!(
            add_tags(&b"---\ntags: [a]\n---\n"[..], &["a,b"]),
            Err(FmEditError::InvalidTag {
                tag: "a,b".to_string()
            })
        );
        assert_eq!(
            add_tags(&b"---\ntags: [a]\n---\n"[..], &["#"]),
            Err(FmEditError::InvalidTag {
                tag: "#".to_string()
            })
        );

        // Not UTF-8: never rewritten.
        let bytes = b"---\n\xff\xfe\n---\n".to_vec();
        assert_eq!(set_scalar(&bytes, "title", "T"), Err(FmEditError::NotUtf8));
    }

    #[test]
    fn complex_other_keys_do_not_block_edits() {
        // The canonical FORMAT §3.1 example shape: unknown keys, nested
        // maps, block scalars and blank lines coexist with edits.
        let before = b"---\ntitle: Girl sinking into deep water\n\
tags: [underwater, sketch]\n\
rating: 4\n\
sources:\n  - type: eagle\n    library: References\n  - type: other\n\
notes: |\n  a block\n  scalar here\n\
copied_from:\n\
created: 2021-10-15\n\
---\n\nFree text.\n"
            .to_vec();
        let after = set_rating(&before, RatingEdit::Set(5)).unwrap();
        assert_only_splice(&before, &after, b"4", b"5");

        let after = set_scalar(&before, "title", "New").unwrap();
        assert_only_splice(&before, &after, b"Girl sinking into deep water", b"New");

        let after = add_tags(&before, &["colour"]).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after),
            "---\ntitle: Girl sinking into deep water\n\
tags: [underwater, sketch, colour]\n\
rating: 4\n\
sources:\n  - type: eagle\n    library: References\n  - type: other\n\
notes: |\n  a block\n  scalar here\n\
copied_from:\n\
created: 2021-10-15\n\
---\n\nFree text.\n"
        );

        // (The minimal byte diff shares the `c` that starts the next key,
        // so this one pins the whole file instead of the range.)
        let after = delete_key(&before, "copied_from").unwrap();
        let expected: Vec<u8> = [
            &b"---\ntitle: Girl sinking into deep water\n"[..],
            &b"tags: [underwater, sketch]\n"[..],
            &b"rating: 4\n"[..],
            &b"sources:\n  - type: eagle\n    library: References\n  - type: other\n"[..],
            &b"notes: |\n  a block\n  scalar here\n"[..],
            &b"created: 2021-10-15\n"[..],
            &b"---\n\nFree text.\n"[..],
        ]
        .concat();
        assert_eq!(after, expected);

        // Deleting a nested-structure key refuses instead of shredding it.
        assert_eq!(
            delete_key(&before, "sources"),
            Err(FmEditError::ValueNotEditable)
        );
        assert_eq!(
            delete_key(&before, "notes"),
            Err(FmEditError::ValueNotEditable)
        );
    }

    #[test]
    fn crlf_and_bom_edits_stay_in_the_files_style() {
        let before =
            b"\xEF\xBB\xBF---\r\ntitle: Old\r\nrating: 4\r\n---\r\n![[a.png]]\r\n".to_vec();
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_only_splice(&before, &after, b"Old", b"New");
        let after = set_rating(&before, RatingEdit::Set(5)).unwrap();
        assert_only_splice(&before, &after, b"4", b"5");
        let after = add_tags(&before, &["x"]).unwrap();
        assert_eq!(
            after,
            b"\xEF\xBB\xBF---\r\ntitle: Old\r\nrating: 4\r\ntags: [x]\r\n---\r\n![[a.png]]\r\n"
        );
    }

    #[test]
    fn uneditable_or_anchors_do_not_panic_the_scanner() {
        // A rambling but valid region: the scanner attributes or refuses
        // without panicking; only the target's editability decides.
        let tricky =
            b"---\na: [1, {x: y}, \"q\"]\nb: &anch v\nc: *anch\n? [k]\n: v\n---\n".to_vec();
        assert!(matches!(
            set_scalar(&tricky, "a", "x"),
            Err(FmEditError::AnchorOrAlias)
        ));
        let deep = b"---\ntags:\n  - a\n   - b\n---\n".to_vec();
        assert_eq!(add_tags(&deep, &["c"]), Err(FmEditError::ValueNotEditable));
    }

    #[test]
    fn body_starting_with_dashes_survives_edits_byte_exactly() {
        let before = b"---\ntitle: T\n---\n---\na thematic break\n".to_vec();
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_eq!(after, b"---\ntitle: New\n---\n---\na thematic break\n");
    }
}

#[cfg(test)]
mod subset_edge_tests {
    use super::*;

    #[test]
    fn double_quoted_keys_are_inside_the_subset() {
        let before = b"---\n\"title\": Old\n---\n".to_vec();
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_eq!(after, b"---\n\"title\": New\n---\n");
        // Single-quoted keys are not.
        assert_eq!(
            set_scalar(&b"---\n'title': Old\n---\n"[..], "title", "New"),
            Err(FmEditError::KeyNotPlain {
                key: "title".to_string()
            })
        );
    }

    #[test]
    fn unicode_keys_and_values_work() {
        let before = b"---\nkind: collection\n---\n".to_vec();
        let after = set_scalar(&before, "标题", "深い水").unwrap();
        // Value needs no quotes; key is plain UTF-8.
        assert_eq!(
            String::from_utf8_lossy(&after),
            "---\nkind: collection\n标题: 深い水\n---\n"
        );
        // The re-parse check reads it back regardless.
        let after = set_scalar(&after.clone(), "标题", "4").unwrap();
        assert!(String::from_utf8_lossy(&after).contains("标题: \"4\""));
    }

    #[test]
    fn values_with_line_breaks_write_escaped_and_read_back() {
        let before = b"---\ntitle: Old\n---\n".to_vec();
        let after = set_scalar(&before, "title", "a\nb \"q\" \\ backslash").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after),
            "---\ntitle: \"a\\nb \\\"q\\\" \\\\ backslash\"\n---\n"
        );
        // And the value reads back exactly as given.
        let text = String::from_utf8_lossy(&after).into_owned();
        let yaml = crate::note::front_matter_text(&text).unwrap_or_default();
        let docs = Yaml::load_from_str(yaml).unwrap();
        assert_eq!(
            docs.first()
                .and_then(|d| d.as_mapping_get("title"))
                .map(|v| v.as_str().map(str::to_string)),
            Some(Some("a\nb \"q\" \\ backslash".to_string()))
        );
    }

    #[test]
    fn removing_the_single_tag_deletes_the_flow_key_line() {
        let before = b"---\ntitle: T\ntags: [only]\nrating: 1\n---\n".to_vec();
        let after = remove_tags(&before, &["only"]).unwrap();
        assert_eq!(after, b"---\ntitle: T\nrating: 1\n---\n");
        assert_eq!(remove_tags(&before, &["else"]), Ok(before.clone()));
    }

    #[test]
    fn multiline_flow_lists_and_maps_in_other_keys_refuse_the_target() {
        // A flow list spanning lines is not the single-line subset.
        let before = b"---\ntags: [a,\n  b]\ntitle: T\n---\n".to_vec();
        assert_eq!(
            add_tags(&before, &["c"]),
            Err(FmEditError::ValueNotEditable)
        );
        // Flow maps as other keys coexist with edits.
        let after = set_scalar(&before, "title", "New").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after),
            "---\ntags: [a,\n  b]\ntitle: New\n---\n"
        );

        let before = b"---\nmeta: {a: 1}\ntitle: T\n---\n".to_vec();
        assert_eq!(
            delete_key(&before, "meta"),
            Err(FmEditError::ValueNotEditable)
        );
        assert_eq!(
            String::from_utf8_lossy(&set_scalar(&before, "title", "New").unwrap()),
            "---\nmeta: {a: 1}\ntitle: New\n---\n"
        );
    }

    #[test]
    fn empty_and_bom_only_notes_grow_a_full_block_on_demand() {
        for before in [b"".to_vec(), b"\xEF\xBB\xBF".to_vec()] {
            let expect_bom = !before.is_empty();
            let after = set_rating(&before.clone(), RatingEdit::Set(5)).unwrap();
            let body: &[u8] = if expect_bom {
                b"\xEF\xBB\xBF---\nrating: 5\n---\n"
            } else {
                b"---\nrating: 5\n---\n"
            };
            assert_eq!(after, body.to_vec());
            let tags = add_tags(&before, &["x", "y"]).unwrap();
            // `y` is one of YAML 1.1's one-letter booleans, so this writer
            // keeps it quoted (FORMAT §3.1: quote what YAML could misread).
            assert!(tags.ends_with(b"tags: [x, \"y\"]\n---\n"));
        }
    }
}
