//! Constants of the dimagine library format (docs/FORMAT.md 0.1).
//!
//! The format is the contract: every rule implemented elsewhere in this crate
//! that comes from FORMAT.md refers back to a constant or function here.

/// Image file extensions, lowercase (FORMAT §2.1).
pub const IMAGE_EXTENSIONS: [&str; 11] = [
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp",
];

/// Characters that break wikilinks and must not appear in file names (FORMAT §2.1).
pub const BANNED_FILENAME_CHARS: [char; 5] = ['[', ']', '#', '^', '|'];

/// OS bookkeeping files that tools must ignore (FORMAT §2.2), lowercase.
pub const IGNORED_OS_FILES: [&str; 2] = ["thumbs.db", "desktop.ini"];

/// Why a path is ignored while scanning (FORMAT §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Any file or folder whose name starts with `.` (including `.dimagine/`).
    Hidden,
    /// macOS resource-fork files (`._*`).
    ResourceFork,
    /// `Thumbs.db` or `desktop.ini`.
    OsMetadata,
}

/// The format "family" an extension or a sniffed header belongs to. Exchanges
/// within a family (`jpg`/`jpeg`, `tif`/`tiff`, `heic`/`heif`) are the same
/// format and never reported as a mismatch (FORMAT §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormatFamily {
    Jpeg,
    Png,
    Gif,
    Webp,
    Avif,
    Heif,
    Tiff,
    Bmp,
}

impl FormatFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            FormatFamily::Jpeg => "jpeg",
            FormatFamily::Png => "png",
            FormatFamily::Gif => "gif",
            FormatFamily::Webp => "webp",
            FormatFamily::Avif => "avif",
            FormatFamily::Heif => "heif",
            FormatFamily::Tiff => "tiff",
            FormatFamily::Bmp => "bmp",
        }
    }

    /// The family a file extension claims, if it is an image extension.
    pub fn from_extension(ext: &str) -> Option<FormatFamily> {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(FormatFamily::Jpeg),
            "png" => Some(FormatFamily::Png),
            "gif" => Some(FormatFamily::Gif),
            "webp" => Some(FormatFamily::Webp),
            "avif" => Some(FormatFamily::Avif),
            "heic" | "heif" => Some(FormatFamily::Heif),
            "tif" | "tiff" => Some(FormatFamily::Tiff),
            "bmp" => Some(FormatFamily::Bmp),
            _ => None,
        }
    }
}

/// True if `ext` (any case) is one of the image extensions (FORMAT §2.1).
pub fn is_image_extension(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// The lowercased extension of a file name: the part after the last `.`.
/// Returns `None` for names without an extension (including dotfiles, which
/// are ignored before classification anyway).
pub fn file_extension(name: &str) -> Option<&str> {
    let last = name.rsplit_once('.')?;
    if last.0.is_empty() || last.1.is_empty() {
        return None;
    }
    Some(last.1)
}

/// The file name without its final extension, if any.
pub fn file_stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() && stem != name => stem,
        _ => name,
    }
}

/// True if the file name contains a character that breaks wikilinks (§2.1).
pub fn banned_characters(name: &str) -> Vec<char> {
    name.chars()
        .filter(|c| BANNED_FILENAME_CHARS.contains(c))
        .collect()
}

/// Why a name must be ignored while scanning, if it must be (FORMAT §2.2).
/// Applies to files and folders alike.
pub fn ignore_reason(name: &str) -> Option<IgnoreReason> {
    if name.starts_with("._") {
        Some(IgnoreReason::ResourceFork)
    } else if IGNORED_OS_FILES.contains(&name.to_ascii_lowercase().as_str()) {
        Some(IgnoreReason::OsMetadata)
    } else if name.starts_with('.') {
        Some(IgnoreReason::Hidden)
    } else {
        None
    }
}
