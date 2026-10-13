//! Library-relative paths: validation, the hidden-component rule, and the
//! fold used wherever two names must be seen as equal.

use std::path::{Path, PathBuf};

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, PathReject};

/// The note extensions this crate may rewrite. Anything else is an image or a
/// raw metadata sidecar, and those are never opened for writing (FORMAT §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    Md,
    Canvas,
}

/// A path validated to be inside the library and free of hidden components.
///
/// Construction is the only gate: once a [`RelPath`] exists, its components
/// contain no NUL, no absolute prefix, no `.` or `..`, and nothing
/// dot-prefixed (ADR-018 §4). Symlinks are a separate matter — they are
/// refused at resolution, because a name can be perfectly legal and still be
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelPath {
    components: Vec<String>,
}

impl RelPath {
    /// Validate a caller-supplied `/`-separated path.
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.is_empty() {
            return Err(bad(input, PathReject::Empty));
        }
        if input.starts_with('/') {
            return Err(bad(input, PathReject::Absolute));
        }
        let mut components = Vec::new();
        for part in input.split('/') {
            components.push(check_component(input, part)?);
        }
        if components.is_empty() {
            return Err(bad(input, PathReject::Empty));
        }
        Ok(Self { components })
    }

    /// Validate a native path, which must be UTF-8 to become a lookup key.
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let text = path
            .to_str()
            .ok_or_else(|| bad(path.to_string_lossy().to_string(), PathReject::NonUtf8))?;
        Self::parse(text)
    }

    /// The components, in order, library root first.
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The final component.
    pub fn name(&self) -> &str {
        self.components
            .last()
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// The directory holding this path, as `/`-separated text (`""` for the
    /// library root).
    pub fn parent_text(&self) -> String {
        self.components[..self.components.len() - 1].join("/")
    }

    /// Components of this path's parent directory, or empty if in the library root.
    pub fn parent_components(&self) -> &[String] {
        if self.components.len() <= 1 {
            &[]
        } else {
            &self.components[..self.components.len() - 1]
        }
    }

    /// This path's parent directory as a validated path, or the library root.
    pub fn parent(&self) -> Option<Self> {
        if self.components.len() <= 1 {
            return None;
        }
        Some(Self {
            components: self.components[..self.components.len() - 1].to_vec(),
        })
    }

    /// `/`-separated text.
    pub fn text(&self) -> String {
        self.components.join("/")
    }

    /// The same directory with a different file name: used to build the
    /// ` 2` siblings and a note's image, so the result is validated by the
    /// same rule rather than by trust.
    pub fn sibling(&self, name: &str) -> Result<Self, Error> {
        let mut components = self.components[..self.components.len() - 1].to_vec();
        components.push(check_component(name, name)?);
        Ok(Self { components })
    }

    /// A path under a directory, e.g. a note beside an image.
    pub fn join(&self, name: &str) -> Result<Self, Error> {
        let mut components = self.components.clone();
        components.push(check_component(&self.text(), name)?);
        Ok(Self { components })
    }

    /// Native path, relative to the library root.
    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(self.text())
    }

    /// The note kind, if this is a note at all.
    pub fn note_kind(&self) -> Option<NoteKind> {
        note_kind(self.name())
    }

    /// A hidden-free path is always safe to display.
    pub fn display(&self) -> String {
        self.text()
    }
}

/// A validated path under `.trash/`, reachable only through the trash API.
///
/// The first component is forced to be the trash directory itself; the rest
/// may be dot-prefixed, because a trashed file keeps its original name
/// (ADR-017 §7 preserves the relative path). `..` is still refused, so the
/// path cannot climb out of the trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashPath {
    components: Vec<String>,
}

/// The one trash directory: the library root's `.trash/` (ADR-017 §7).
pub const TRASH_DIR: &str = ".trash";

impl TrashPath {
    /// Mirror an existing library path into the trash, keeping its relative
    /// position.
    pub fn for_original(original: &RelPath) -> Self {
        let mut components = vec![TRASH_DIR.to_string()];
        components.extend(original.components.iter().cloned());
        Self { components }
    }

    /// Validate a path whose first component is `.trash`. This is the only
    /// public door into a dot-prefixed directory (ADR-018 §4).
    pub fn parse(input: &str) -> Result<Self, Error> {
        let mut parts = input.split('/');
        let first = parts.next().unwrap_or_default();
        if first != TRASH_DIR {
            return Err(bad(input, PathReject::Hidden));
        }
        let mut components = vec![TRASH_DIR.to_string()];
        for part in parts {
            let part = reject_nul_and_dots(input, part)?;
            components.push(part);
        }
        if components.len() < 2 {
            return Err(bad(input, PathReject::Empty));
        }
        Ok(Self { components })
    }

    pub fn components(&self) -> &[String] {
        &self.components
    }

    pub fn name(&self) -> &str {
        self.components
            .last()
            .map(String::as_str)
            .unwrap_or_default()
    }

    pub fn text(&self) -> String {
        self.components.join("/")
    }

    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(self.text())
    }

    /// The trash directory with a different file name, keeping the folder
    /// structure (the ` 2` group rename of ADR-017 §7).
    pub fn sibling(&self, name: &str) -> Result<Self, Error> {
        let mut components = self.components[..self.components.len() - 1].to_vec();
        components.push(reject_nul_and_dots(name, name)?);
        Ok(Self { components })
    }

    /// The library-relative path this mirrors back to on restore.
    pub fn original(&self) -> Result<RelPath, Error> {
        RelPath::parse(&self.components[1..].join("/"))
    }
}

/// The note file that goes with an image: `<image name>.md`, beside it
/// (FORMAT §3).
pub fn note_name_for(image_name: &str) -> String {
    format!("{image_name}.md")
}

/// Case-folded then NFC: the one identity used when two names must not
/// collide (ADR-016 §10, ADR-017 §4). It matches the index's own fold, so the
/// search layer and the write layer agree on what "same name" means.
pub fn fold_key(text: &str) -> String {
    text.case_fold().nfc().collect()
}

/// NFC of a caller-supplied name, for storing.
pub fn to_nfc(text: &str) -> String {
    text.nfc().collect()
}

fn note_kind(name: &str) -> Option<NoteKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") {
        Some(NoteKind::Md)
    } else if lower.ends_with(".canvas") {
        Some(NoteKind::Canvas)
    } else {
        None
    }
}

fn check_component(whole: &str, part: &str) -> Result<String, Error> {
    if part.is_empty() {
        return Err(bad(whole, PathReject::Traversal));
    }
    if part == "." || part == ".." {
        return Err(bad(whole, PathReject::Traversal));
    }
    let checked = reject_nul(whole, part)?;
    if checked.starts_with('.') {
        return Err(bad(whole, PathReject::Hidden));
    }
    Ok(checked)
}

fn reject_nul_and_dots(whole: &str, part: &str) -> Result<String, Error> {
    if part.is_empty() || part == "." || part == ".." {
        return Err(bad(whole, PathReject::Traversal));
    }
    reject_nul(whole, part)
}

fn reject_nul(whole: &str, part: &str) -> Result<String, Error> {
    if part.contains('\0') {
        return Err(bad(whole, PathReject::Traversal));
    }
    Ok(part.to_string())
}

fn bad(path: impl Into<String>, reject: PathReject) -> Error {
    Error::BadPath {
        path: path.into(),
        reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_parse() {
        let p = RelPath::parse("refs/deep-sea/girl.jpg").unwrap();
        assert_eq!(p.name(), "girl.jpg");
        assert_eq!(p.parent_text(), "refs/deep-sea");
        assert_eq!(p.note_kind(), None);
        assert_eq!(
            RelPath::parse("a.md").unwrap().note_kind(),
            Some(NoteKind::Md)
        );
        assert_eq!(
            RelPath::parse("a.canvas").unwrap().note_kind(),
            Some(NoteKind::Canvas)
        );
    }

    #[test]
    fn refusals() {
        assert!(matches!(
            RelPath::parse("/abs/x.md"),
            Err(Error::BadPath {
                reject: PathReject::Absolute,
                ..
            })
        ));
        for input in ["x/../y.md", "..", "a/./b", "a//b", ""] {
            assert!(
                matches!(
                    RelPath::parse(input),
                    Err(Error::BadPath {
                        reject: PathReject::Traversal | PathReject::Empty,
                        ..
                    })
                ),
                "{input}"
            );
        }
        for input in [".trash/x.md", ".git/config", "a/.dimagine/b", ".hidden"] {
            assert!(matches!(
                RelPath::parse(input),
                Err(Error::BadPath {
                    reject: PathReject::Hidden,
                    ..
                })
            ));
        }
        assert!(RelPath::parse("a\0.md").is_err());
    }

    #[test]
    fn trash_paths_are_the_only_hidden_door() {
        let t = TrashPath::parse(".trash/refs/x.jpg").unwrap();
        assert_eq!(t.original().unwrap().text(), "refs/x.jpg");
        assert_eq!(
            TrashPath::for_original(&RelPath::parse("refs/x.jpg").unwrap()),
            t
        );
        assert!(TrashPath::parse("refs/x.jpg").is_err());
        assert!(TrashPath::parse(".trash/../x").is_err());
        assert!(TrashPath::parse(".trash").is_err());
    }

    #[test]
    fn fold_is_case_insensitive_and_nfc() {
        assert_eq!(fold_key("Style/Ink"), fold_key("style/ink"));
        assert_eq!(fold_key("Girl.JPG"), fold_key("girl.jpg"));
        // "é" precomposed and "e" + combining acute fold to one key.
        assert_eq!(fold_key("\u{e9}"), fold_key("e\u{301}"));
        assert_eq!(fold_key("A"), "a");
    }

    #[test]
    fn sibling_and_note_name() {
        let p = RelPath::parse("refs/x.jpg.md").unwrap();
        assert_eq!(p.sibling("x 2.jpg.md").unwrap().text(), "refs/x 2.jpg.md");
        assert_eq!(note_name_for("x.jpg"), "x.jpg.md");
        assert!(p.sibling("..").is_err());
    }
}
