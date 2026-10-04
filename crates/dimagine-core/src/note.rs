//! Parse Markdown notes: split YAML front matter and read the properties that
//! other modules need (FORMAT §3).
//!
//! Parsing never fails hard: a note with broken front matter still yields its
//! body (so links can be extracted from it) alongside a front-matter error the
//! caller reports as a finding. Unknown properties are preserved by simply
//! keeping the text and the parsed mapping around — dimagine 0.1 reads more
//! than it writes, and never rewrites a note on its own.

use saphyr::{LoadableYamlNode, Yaml};

/// An error in a note's front matter, with the location if the parser could
/// report one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontMatterError {
    /// 1-based line within the note file, when known.
    pub line: Option<usize>,
    /// 1-based column within the line, when known.
    pub column: Option<usize>,
    pub message: String,
}

/// The `id` property of a note, or why it is unusable (FORMAT §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdProperty {
    /// A string that may or may not be a valid ULID; validation is separate.
    Text(String),
    /// Present but not a string (a number, list, ...).
    NotAString,
}

/// The parsed pieces of a note.
#[derive(Clone, Debug)]
pub struct ParsedNote {
    /// The file had a `---` block that ended with a closing `---`.
    pub has_front_matter: bool,
    /// The front matter exists but could not be parsed.
    pub error: Option<FrontMatterError>,
    pub id: Option<IdProperty>,
    /// The `kind` property if it is a string; `kind: collection` marks a
    /// collection note (FORMAT §5).
    pub kind: Option<String>,
    /// The text after the front matter; link extraction works on this.
    pub body: String,
    /// 1-based line number of the first body line; link line numbers are
    /// reported relative to the whole file.
    pub body_line: usize,
}

/// Parse a note file's text. `text` should be the whole file.
pub fn parse_note(text: &str) -> ParsedNote {
    let split = split_front_matter(text);
    let mut note = ParsedNote {
        has_front_matter: false,
        error: None,
        id: None,
        kind: None,
        body: String::new(),
        body_line: 1,
    };
    match split {
        FmSplit::NoFrontMatter { body } => {
            note.body = body.to_string();
        }
        FmSplit::FrontMatter {
            yaml,
            body,
            end_line,
        } => {
            note.has_front_matter = true;
            note.body = body.to_string();
            note.body_line = end_line + 1;
            read_properties(yaml, &mut note);
        }
        FmSplit::Unterminated { body } => {
            note.has_front_matter = false;
            note.body = body.to_string();
            note.body_line = 2;
            note.error = Some(FrontMatterError {
                line: None,
                column: None,
                message: "front matter block is never closed by a `---` line".to_string(),
            });
        }
    }
    note
}

fn read_properties(yaml: &str, note: &mut ParsedNote) {
    match Yaml::load_from_str(yaml) {
        Ok(docs) => {
            let Some(doc) = docs.first() else {
                // Empty ```---\n---\n``` block: no properties, no error.
                return;
            };
            if !doc.is_mapping() {
                note.error = Some(FrontMatterError {
                    line: None,
                    column: None,
                    message: "front matter is not a mapping of properties".to_string(),
                });
                return;
            }
            if let Some(id) = doc.as_mapping_get("id") {
                note.id = match id {
                    // `id:` set but empty stands for "no id" (FORMAT §3.3,
                    // a person may leave the id out).
                    Yaml::Value(saphyr::Scalar::String(s)) if s.is_empty() => None,
                    Yaml::Value(saphyr::Scalar::String(s)) => Some(IdProperty::Text(s.to_string())),
                    Yaml::Value(saphyr::Scalar::Null) => None,
                    Yaml::Representation(rep, _, _) if rep.is_empty() => None,
                    Yaml::Representation(rep, _, _) => Some(IdProperty::Text(rep.to_string())),
                    _ => Some(IdProperty::NotAString),
                };
            }
            note.kind = doc
                .as_mapping_get("kind")
                .and_then(Yaml::as_str)
                .map(str::to_string);
        }
        Err(err) => {
            // The parser reports positions inside `yaml`, which starts on the
            // note's second line (the first is the opening `---`).
            let marker = err.marker();
            note.error = Some(FrontMatterError {
                line: Some(marker.line() + 1),
                column: Some(marker.col() + 1),
                message: err.info().to_string(),
            });
        }
    }
}

enum FmSplit<'a> {
    NoFrontMatter {
        body: &'a str,
    },
    FrontMatter {
        yaml: &'a str,
        body: &'a str,
        end_line: usize,
    },
    Unterminated {
        body: &'a str,
    },
}

/// Split a document into front matter and body. Handles a leading BOM,
/// CRLF line endings and whitespace around the `---` delimiters, following
/// Obsidian conventions.
fn split_front_matter(text: &str) -> FmSplit<'_> {
    let s = text.strip_prefix('\u{feff}').unwrap_or(text);
    let first_line_end = s.find('\n').unwrap_or(s.len());
    if s[..first_line_end].trim_end_matches('\r') != "---" {
        return FmSplit::NoFrontMatter { body: s };
    }
    // `first_line_end + 1` is past EOF for a three-byte `---` file. Keep the
    // cursor inside the source before attempting to slice it below.
    let mut body_start = (first_line_end + 1).min(s.len());
    let mut line_no = 2usize;
    let mut cursor = body_start;
    loop {
        let line_end = match s[cursor..].find('\n') {
            Some(off_end) => cursor + off_end,
            None => s.len(),
        };
        let line = s[cursor..line_end].trim_end_matches('\r');
        if line.trim_end() == "---" {
            let yaml = &s[body_start..cursor];
            body_start = (line_end + 1).min(s.len());
            return FmSplit::FrontMatter {
                yaml,
                body: &s[body_start..],
                end_line: line_no,
            };
        }
        if line_end == s.len() {
            return FmSplit::Unterminated {
                body: &s[body_start..],
            };
        }
        line_no += 1;
        cursor = line_end + 1;
    }
}

/// The ULID rules from FORMAT §3.3: 26 characters of Crockford base32.
/// Lowercase spellings are accepted on read; canonical form is uppercase.
pub const ULID_LEN: usize = 26;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn is_valid_ulid(s: &str) -> bool {
    if s.len() != ULID_LEN {
        return false;
    }
    s.bytes()
        .all(|b| CROCKFORD.contains(&(b.to_ascii_uppercase())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ulids() {
        assert!(is_valid_ulid("01JA8X3Q7K2M9V4T6R1B5N0C3Q"));
        let lower = "01JA8X3Q7K2M9V4T6R1B5N0C3Q".to_ascii_lowercase();
        assert!(is_valid_ulid(&lower));
        assert!(!is_valid_ulid("01JA8X3Q7K2M9V4T6R1B5N0C3")); // 25 chars
        assert!(!is_valid_ulid("01JA8X3Q7K2M9V4T6R1B5N0C3QL")); // 27 chars
        assert!(!is_valid_ulid("01JA8X3Q7K2M9V4T6R1B5N0C3I")); // I is not in the alphabet
        assert!(!is_valid_ulid("01JA8X3Q7K2M9V4T6R1B5N0C3O")); // neither is O
        assert!(!is_valid_ulid("123")); // not 26 chars
        assert!(is_valid_ulid("001zzzzzzzzzzzzzzzzzzzzzzz")); // U and Z are the end of the alphabet
    }
}
