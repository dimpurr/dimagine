//! Sniff the real image format from the first bytes of a file (FORMAT §2.1).
//!
//! Only leading bytes are inspected; images are never decoded or validated
//! beyond their magic signatures.

use crate::format::FormatFamily;

/// How many bytes of a file are read for sniffing. Enough for the ISO-BMFF
/// `ftyp` brand, the largest header we look at (12 bytes) plus headroom.
pub const SNIFF_LEN: usize = 32;

/// A format recognised from leading bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectedFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
    Avif,
    Heif,
    Tiff,
    Bmp,
    /// Bytes that are no image format dimagine recognises.
    Unknown,
}

impl DetectedFormat {
    /// The family this format belongs to; `None` when unknown.
    pub fn family(self) -> Option<FormatFamily> {
        match self {
            DetectedFormat::Jpeg => Some(FormatFamily::Jpeg),
            DetectedFormat::Png => Some(FormatFamily::Png),
            DetectedFormat::Gif => Some(FormatFamily::Gif),
            DetectedFormat::Webp => Some(FormatFamily::Webp),
            DetectedFormat::Avif => Some(FormatFamily::Avif),
            DetectedFormat::Heif => Some(FormatFamily::Heif),
            DetectedFormat::Tiff => Some(FormatFamily::Tiff),
            DetectedFormat::Bmp => Some(FormatFamily::Bmp),
            DetectedFormat::Unknown => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DetectedFormat::Jpeg => "jpeg",
            DetectedFormat::Png => "png",
            DetectedFormat::Gif => "gif",
            DetectedFormat::Webp => "webp",
            DetectedFormat::Avif => "avif",
            DetectedFormat::Heif => "heif",
            DetectedFormat::Tiff => "tiff",
            DetectedFormat::Bmp => "bmp",
            DetectedFormat::Unknown => "unknown",
        }
    }
}

/// Identify the format from up to [`SNIFF_LEN`] leading bytes.
///
/// Short input is fine: files are only ever claimed to be images by their
/// extension, and a header that does not match any signature is `Unknown`
/// (which counts as a content mismatch against every image extension).
pub fn detect(head: &[u8]) -> DetectedFormat {
    let starts = |sig: &[u8]| head.len() >= sig.len() && &head[..sig.len()] == sig;
    if starts(b"\xff\xd8\xff") {
        return DetectedFormat::Jpeg;
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return DetectedFormat::Png;
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return DetectedFormat::Gif;
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return DetectedFormat::Webp;
    }
    if starts(b"II*\x00") || starts(b"MM\x00*") {
        return DetectedFormat::Tiff;
    }
    if starts(b"BM") {
        return DetectedFormat::Bmp;
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return match &head[8..12] {
            b"avif" | b"avis" => DetectedFormat::Avif,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs"
            | b"mif1" | b"msf1" | b"miaf" => DetectedFormat::Heif,
            _ => DetectedFormat::Unknown,
        };
    }
    DetectedFormat::Unknown
}
