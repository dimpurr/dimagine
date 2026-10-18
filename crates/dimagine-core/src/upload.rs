//! The pure half of an upload (ADR-017 §11): where a file lands, what
//! it is named, whether its bytes are the format its name claims, the
//! note a new image gets, and the collection-join edit.
//!
//! Everything here is pure over bytes and the files of one library
//! walk, like [`crate::body_edit`]: the write core (a separate crate)
//! is the only caller allowed to put the results on disk, and it does
//! so through one journaled operation.

use crate::format::{self, FormatFamily};
use crate::links;
use crate::sniff::{self, DetectedFormat};

/// Why an upload was refused before anything was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadRefusal {
    /// The file name carries no image extension (FORMAT §2.1).
    NotAnImageExtension,
    /// The bytes are not the format the extension claims.
    Mismatch {
        /// The family the extension names.
        extension_family: FormatFamily,
        /// What the leading bytes actually are.
        sniffed: DetectedFormat,
    },
    /// SVG, HTML and XML are refused on both sides: by the final
    /// extension and by the magic bytes (ADR-018 §5).
    MarkupRefused {
        /// True when the extension named one of them, false when the
        /// bytes did.
        by_extension: bool,
    },
}

impl UploadRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::NotAnImageExtension => "the file name has no image extension".to_string(),
            Self::Mismatch {
                extension_family,
                sniffed,
            } => format!(
                "the extension claims {} but the bytes are {}",
                extension_family.as_str(),
                sniffed.as_str()
            ),
            Self::MarkupRefused { by_extension } => format!(
                "SVG, HTML and XML are not uploadable (refused by {})",
                if *by_extension {
                    "the file extension"
                } else {
                    "the file content"
                }
            ),
        }
    }
}

/// Refuse an upload whose bytes are not the format its file name
/// claims, and SVG, HTML and XML on both the magic bytes and the final
/// extension (ADR-018 §5, ADR-017 §11.4).
///
/// `head` is the file's leading bytes ([`sniff::SNIFF_LEN`] is
/// enough); `file_name` is the name the upload arrived with. Only
/// leading bytes are inspected — the file is never decoded.
pub fn check_upload(head: &[u8], file_name: &str) -> Result<(), UploadRefusal> {
    let ext = format::file_extension(file_name).map(|ext| ext.to_ascii_lowercase());
    if matches!(
        ext.as_deref(),
        Some("svg" | "html" | "htm" | "xml" | "xhtml")
    ) {
        return Err(UploadRefusal::MarkupRefused { by_extension: true });
    }
    let Some(ext) = ext else {
        return Err(UploadRefusal::NotAnImageExtension);
    };
    let Some(claimed) = FormatFamily::from_extension(&ext) else {
        return Err(UploadRefusal::NotAnImageExtension);
    };
    if markup_head(head) {
        return Err(UploadRefusal::MarkupRefused {
            by_extension: false,
        });
    }
    let sniffed = sniff::detect(head);
    if sniffed.family() != Some(claimed) {
        return Err(UploadRefusal::Mismatch {
            extension_family: claimed,
            sniffed,
        });
    }
    Ok(())
}

/// Whether the leading bytes look like markup: a BOM or whitespace,
/// then `<`, which is how `<?xml`, `<svg` and `<html` all begin (and
/// no image format begins).
fn markup_head(head: &[u8]) -> bool {
    let rest = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let mut trimmed = rest.iter().skip_while(|b| b.is_ascii_whitespace());
    trimmed.next() == Some(&b'<')
}

/// The name an upload lands as: `(stem, extension, renamed)`, where
/// `renamed` says the original name was meaningless and the
/// `Upload <timestamp>` name was used instead, so the original belongs
/// in the note's `title` (ADR-017 §11.2).
///
/// The original name's last path component is taken (a hostile
/// `../../x.jpg` cannot climb); `[ ] # ^ |`, control characters and
/// NUL are replaced with `-` (FORMAT §2.1); a stem that comes out
/// empty is meaningless.
pub fn upload_file_name(original: &str, timestamp: &str) -> (String, String, bool) {
    let name = original
        .rsplit_once('/')
        .map(|(_, file)| file)
        .unwrap_or(original);
    let name = name.rsplit_once('\\').map(|(_, file)| file).unwrap_or(name);
    let (stem, ext) = match name.rsplit_once('.') {
        // A leading dot alone (`.jpg`) is a hidden file with no
        // stem: the name is meaningless, whatever the extension.
        Some((stem, ext)) if !ext.is_empty() => (stem.to_string(), ext.to_ascii_lowercase()),
        // No usable extension: the sniff check has already refused the
        // upload by now, so this is only a safe spelling.
        _ => (name.to_string(), String::new()),
    };
    let clean = sanitize_stem(&stem);
    if clean.is_empty() || is_meaningless_stem(&clean) {
        return (format!("Upload {timestamp}"), ext, true);
    }
    (clean, ext, false)
}

/// Replace the characters that break wikilinks (FORMAT §2.1), control
/// characters and NUL with `-`.
fn sanitize_stem(stem: &str) -> String {
    stem.chars()
        .map(|ch| match ch {
            '[' | ']' | '#' | '^' | '|' | '\0' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect()
}

/// The meaningless camera, screenshot and share-panel names (ADR-017
/// §11.2). The list lives here so it is one place to extend; it is
/// matched case-insensitively against the sanitized stem without its
/// extension.
fn is_meaningless_stem(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();
    let base = strip_copy_suffix(&lower);
    base == "image"
        || base == "photo"
        || base == "picture"
        || base == "download"
        || base == "paste"
        || base == "pasted"
        || base == "share"
        || base == "shared"
        || base == "attachment"
        || lower.starts_with("img_")
            && lower["img_".len()..].chars().all(|c| c.is_ascii_digit())
            && lower.len() > "img_".len()
        || lower.starts_with("pxl_")
        || lower.starts_with("screenshot")
        || lower.starts_with("screen shot")
        || base.chars().all(|c| c.is_ascii_digit())
}

/// Drop a trailing ` (<n>)` — the shape macOS and Windows give a
/// second download of the same generic name.
fn strip_copy_suffix(stem: &str) -> &str {
    let Some((base, suffix)) = stem.rsplit_once(" (") else {
        return stem;
    };
    let digits = suffix.strip_suffix(')').unwrap_or("");
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        base
    } else {
        stem
    }
}

/// What one new image's note is made of (ADR-017 §11.5).
pub struct UploadNote<'a> {
    /// The ULID assigned on upload (FORMAT §3.3: upload is one of the
    /// events that assigns an `id`).
    pub id: &'a str,
    /// The upload time, RFC 3339 with a numeric offset (`+00:00`).
    pub time: &'a str,
    /// The importer spelling: `dimagine <version>`.
    pub importer: &'a str,
    /// The name the upload arrived with.
    pub original_name: &'a str,
    /// The original name, when the file was renamed from a meaningless
    /// name: it becomes the note's `title`.
    pub title: Option<&'a str>,
}

/// The bytes of the note a newly uploaded image gets: front matter
/// (`id`, `title` when renamed, `added` and `imported` at the upload
/// time, the `sources` entry of ADR-017 §11.5), a blank line, and the
/// self-embed the caller spells with [`upload_embed_text`] (ADR-017
/// §2: front matter, one blank line, self-embed).
pub fn upload_note_bytes(note: &UploadNote<'_>, embed_text: &str) -> Vec<u8> {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", note.id));
    if let Some(title) = note.title {
        out.push_str(&format!("title: {}\n", yaml_string(title)));
    }
    out.push_str(&format!("added: {}\n", note.time));
    out.push_str(&format!("imported: {}\n", note.time));
    out.push_str("sources:\n");
    out.push_str("  - type: upload\n");
    out.push_str(&format!("    imported: {}\n", note.time));
    out.push_str(&format!("    importer: {}\n", yaml_string(note.importer)));
    out.push_str(&format!(
        "    original_name: {}\n",
        yaml_string(note.original_name)
    ));
    out.push_str("---\n\n");
    out.push_str(embed_text);
    out.push('\n');
    out.into_bytes()
}

/// The self-embed text for an uploaded image (ADR-017 §2, §4): a full
/// library path for an image in a folder, the bare name for one at the
/// library root. The name the write core picks is unique library-wide
/// (NFC + case-insensitive), so a root upload's bare name is
/// unambiguous by construction — the same answer [`crate::body_edit::
/// link_text_for`] gives, without needing the not-yet-created image in
/// the walk.
pub fn upload_embed_text(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        format!("![[{name}]]")
    } else {
        format!("![[{folder}/{name}]]")
    }
}

/// Why appending an upload to a collection note was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinError {
    /// The note's text is not UTF-8.
    NotUtf8,
    /// An unclosed fenced code block starts at this byte offset, so an
    /// appended embed would land inside code (ADR-017 §4).
    UnclosedFence(usize),
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8 => write!(f, "the collection note is not valid UTF-8"),
            Self::UnclosedFence(offset) => write!(
                f,
                "the collection note has an unclosed code block at byte {offset}"
            ),
        }
    }
}

/// Append one embed line to a collection note for an image this
/// operation is about to create (ADR-017 §4: a join appends one embed
/// line; a non-empty note that does not end in a newline gets one
/// first, in the note's own style; an unclosed code block is
/// refused).
///
/// Membership is judged by the embed text's presence: the image did
/// not exist before this operation, so an embed naming it was a
/// dangling link to exactly this upload, and the note that already
/// carries it is a member the moment the image lands. (A join for an
/// image that already exists — a dedupe hit — uses the resolution-based
/// [`crate::body_edit::append_member`] instead.)
pub fn join_collection_note(body: &[u8], embed_text: &str) -> Result<Vec<u8>, JoinError> {
    let text = std::str::from_utf8(body).map_err(|_| JoinError::NotUtf8)?;
    if text.contains(embed_text) {
        return Ok(body.to_vec());
    }
    if let Some(offset) = links::unclosed_fence(text) {
        return Err(JoinError::UnclosedFence(offset));
    }
    let term = newline_style(text);
    let mut out = String::with_capacity(text.len() + embed_text.len() + term.len() * 2);
    out.push_str(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(term);
    }
    out.push_str(embed_text);
    out.push_str(term);
    Ok(out.into_bytes())
}

/// The note's own line terminator: CRLF when any break is CRLF, else
/// LF (the same rule [`crate::fm_edit`] applies to a splice).
fn newline_style(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// A YAML double-quoted scalar: always valid, whatever the string
/// holds (a name from a camera or a browser is not to be trusted
/// plain).
fn yaml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use saphyr::LoadableYamlNode;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff];

    #[test]
    fn sniff_check_accepts_matching_bytes() {
        assert_eq!(check_upload(PNG, "girl.png"), Ok(()));
        assert_eq!(check_upload(JPEG, "girl.jpg"), Ok(()));
        assert_eq!(check_upload(JPEG, "girl.JPEG"), Ok(()));
        // tif/tiff and heic/heif are the same family either way.
        assert_eq!(check_upload(&[b'I', b'I', 0x2a, 0], "girl.tif"), Ok(()));
    }

    #[test]
    fn sniff_check_refuses_a_mismatch() {
        // A PNG's bytes behind a JPEG name.
        let refusal = check_upload(PNG, "girl.jpg").unwrap_err();
        assert_eq!(
            refusal,
            UploadRefusal::Mismatch {
                extension_family: FormatFamily::Jpeg,
                sniffed: DetectedFormat::Png,
            }
        );
        // Unknown bytes are a mismatch against every image extension.
        let refusal = check_upload(b"not an image at all", "girl.png").unwrap_err();
        assert!(matches!(refusal, UploadRefusal::Mismatch { .. }));
        // No image extension at all.
        assert_eq!(
            check_upload(PNG, "girl.txt"),
            Err(UploadRefusal::NotAnImageExtension)
        );
        assert_eq!(
            check_upload(PNG, "girl"),
            Err(UploadRefusal::NotAnImageExtension)
        );
    }

    #[test]
    fn markup_is_refused_on_both_sides() {
        // By extension.
        for name in [
            "girl.svg",
            "girl.html",
            "girl.htm",
            "girl.xml",
            "girl.xhtml",
        ] {
            assert_eq!(
                check_upload(b"<svg></svg>", name),
                Err(UploadRefusal::MarkupRefused { by_extension: true }),
                "{name}"
            );
        }
        // By magic, whatever the extension claims.
        for head in [
            b"<?xml version=\"1.0\"?><x/>".as_slice(),
            b"<svg xmlns=\"urn:x\"></svg>".as_slice(),
            b"<!DOCTYPE html><html></html>".as_slice(),
            b"<html><body></body></html>".as_slice(),
        ] {
            let refusal = check_upload(head, "girl.png").unwrap_err();
            assert_eq!(
                refusal,
                UploadRefusal::MarkupRefused {
                    by_extension: false
                },
                "{head:?}"
            );
        }
        // A BOM before the markup does not hide it.
        let bom = b"\xEF\xBB\xBF<svg/>";
        assert_eq!(
            check_upload(bom, "girl.png"),
            Err(UploadRefusal::MarkupRefused {
                by_extension: false
            })
        );
    }

    #[test]
    fn meaningless_names_are_detected() {
        for name in [
            "IMG_1234.jpg",
            "img_0001.jpg",
            "PXL_20261017.jpg",
            "Screenshot 2026-10-17 at 09.30.00.png",
            "screenshot.png",
            "image.png",
            "IMAGE.jpg",
            "photo.jpeg",
            "photo (2).png",
            "picture.webp",
            "download.jpg",
            "download (3).png",
            "paste.png",
            "shared.gif",
            "attachment.bmp",
            "12345678.jpg",
        ] {
            let (stem, _ext, renamed) = upload_file_name(name, "20261017093000");
            assert!(renamed, "{name} should be meaningless");
            assert_eq!(stem, "Upload 20261017093000", "{name}");
        }
    }

    #[test]
    fn meaningful_names_are_kept_and_sanitized() {
        let (stem, ext, renamed) = upload_file_name("girl-underwater.jpg", "20261017093000");
        assert!(!renamed);
        assert_eq!(stem, "girl-underwater");
        assert_eq!(ext, "jpg");
        // Banned characters are replaced, not kept (FORMAT §2.1).
        let (stem, _, _) = upload_file_name("girl [1] #2 ^x |y.png", "t");
        assert_eq!(stem, "girl -1- -2 -x -y");
        // A hostile path cannot climb: only the last component is used.
        let (stem, ext, _) = upload_file_name("../../etc/girl.png", "t");
        assert_eq!(stem, "girl");
        assert_eq!(ext, "png");
        // A name that is only an extension is meaningless.
        let (stem, _, renamed) = upload_file_name(".jpg", "20261017093000");
        assert!(renamed);
        assert_eq!(stem, "Upload 20261017093000");
    }

    #[test]
    fn note_bytes_carry_provenance() {
        let note = UploadNote {
            id: "01JA8X3Q7K2M9V4T6R1B5N0C3Q",
            time: "2026-10-17T09:30:00+00:00",
            importer: "dimagine 0.1.0",
            original_name: "IMG_1234.jpg",
            title: Some("IMG_1234.jpg"),
        };
        let bytes = upload_note_bytes(&note, "![[inbox/Upload 20261017093000.jpg]]");
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.starts_with("---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n"),
            "{text}"
        );
        assert!(text.contains("title: \"IMG_1234.jpg\"\n"), "{text}");
        assert!(
            text.contains("added: 2026-10-17T09:30:00+00:00\n"),
            "{text}"
        );
        assert!(
            text.contains("imported: 2026-10-17T09:30:00+00:00\n"),
            "{text}"
        );
        assert!(text.contains("  - type: upload\n"), "{text}");
        assert!(
            text.contains("    importer: \"dimagine 0.1.0\"\n"),
            "{text}"
        );
        assert!(
            text.contains("    original_name: \"IMG_1234.jpg\"\n"),
            "{text}"
        );
        assert!(
            text.ends_with("---\n\n![[inbox/Upload 20261017093000.jpg]]\n"),
            "{text}"
        );
        // The front matter parses, and the strings read back as written.
        let fm = text
            .strip_suffix("\n")
            .and_then(|rest| rest.split_once("\n\n"))
            .map(|(front, _)| front)
            .unwrap();
        let docs = saphyr::Yaml::load_from_str(fm).unwrap();
        let key = |name: &str| {
            saphyr::Yaml::Value(saphyr::Scalar::String(std::borrow::Cow::Owned(
                name.to_string(),
            )))
        };
        let top = match &docs[0] {
            saphyr::Yaml::Mapping(map) => map,
            other => panic!("expected a mapping, got {other:?}"),
        };
        let sources = match top.get(&key("sources")) {
            Some(saphyr::Yaml::Sequence(seq)) => seq,
            other => panic!("expected a sequence, got {other:?}"),
        };
        let first = match &sources[0] {
            saphyr::Yaml::Mapping(map) => map,
            other => panic!("expected a mapping, got {other:?}"),
        };
        let scalar = |node: &saphyr::Yaml| match node {
            saphyr::Yaml::Value(saphyr::Scalar::String(text)) => text.to_string(),
            other => panic!("expected a string scalar, got {other:?}"),
        };
        assert_eq!(
            scalar(first.get(&key("original_name")).unwrap()),
            "IMG_1234.jpg"
        );
        assert_eq!(scalar(top.get(&key("title")).unwrap()), "IMG_1234.jpg");
    }

    #[test]
    fn note_bytes_without_a_title_when_the_name_was_meaningful() {
        let note = UploadNote {
            id: "01JA8X3Q7K2M9V4T6R1B5N0C3Q",
            time: "2026-10-17T09:30:00+00:00",
            importer: "dimagine 0.1.0",
            original_name: "girl.jpg",
            title: None,
        };
        let text = String::from_utf8(upload_note_bytes(&note, "![[girl.jpg]]")).unwrap();
        assert!(!text.contains("title:"), "{text}");
    }

    #[test]
    fn embed_text_is_a_path_for_a_folder_and_a_bare_name_at_the_root() {
        assert_eq!(
            upload_embed_text("refs/deep-sea", "girl.jpg"),
            "![[refs/deep-sea/girl.jpg]]"
        );
        assert_eq!(upload_embed_text("", "girl.jpg"), "![[girl.jpg]]");
    }

    #[test]
    fn join_appends_one_embed_line_in_the_notes_own_style() {
        let body = b"---\nkind: collection\ntitle: C\n---\n\nShot one.".as_slice();
        let joined = join_collection_note(body, "![[inbox/girl.jpg]]").unwrap();
        let text = String::from_utf8(joined).unwrap();
        assert!(text.ends_with("Shot one.\n![[inbox/girl.jpg]]\n"), "{text}");
        // CRLF notes keep their style.
        let crlf = "---\nkind: collection\n---\r\n\r\nShot one.".as_bytes();
        let joined = join_collection_note(crlf, "![[inbox/girl.jpg]]").unwrap();
        assert!(
            joined.ends_with(b"Shot one.\r\n![[inbox/girl.jpg]]\r\n"),
            "{joined:?}"
        );
        // A note that does not end in a newline gets one first.
        let no_break = b"---\nkind: collection\n---\n\nShot one".as_slice();
        let joined = join_collection_note(no_break, "![[inbox/girl.jpg]]").unwrap();
        assert!(joined.ends_with(b"Shot one\n![[inbox/girl.jpg]]\n"));
        // An empty note is just the embed.
        let joined = join_collection_note(b"", "![[inbox/girl.jpg]]").unwrap();
        assert_eq!(joined, b"![[inbox/girl.jpg]]\n");
    }

    #[test]
    fn join_refuses_an_unclosed_fence_and_a_double_join() {
        let fenced = b"---\nkind: collection\n---\n\n```\n![[x]]".as_slice();
        assert!(matches!(
            join_collection_note(fenced, "![[inbox/girl.jpg]]"),
            Err(JoinError::UnclosedFence(_))
        ));
        // A note that already names the image is left alone.
        let member = b"---\nkind: collection\n---\n\n![[inbox/girl.jpg]]\n".as_slice();
        let joined = join_collection_note(member, "![[inbox/girl.jpg]]").unwrap();
        assert_eq!(joined, member);
        // Not UTF-8 is refused, not guessed at.
        assert_eq!(
            join_collection_note(&[0xff, 0xfe], "![[inbox/girl.jpg]]"),
            Err(JoinError::NotUtf8)
        );
    }
}
