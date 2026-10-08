//! Per-image metadata read from the bytes' header: the stored pixel size and
//! the EXIF "taken" time (HLD `index`).
//!
//! These functions are called by [`crate::index::sync_index`] for image files
//! whose content was re-hashed anyway (the digest requires the bytes; the
//! header does not), so the extraction costs no more file access. Only the
//! header and the EXIF block are parsed: pictures are never decoded here —
//! `image`'s `into_dimensions` reads a stored header and `exif::Reader` an
//! EXIF block, the same framing dimagine-preview reads when it applies
//! orientation. AVIF and HEIF have no header reader in this build, matching
//! the renditions previews consider supported, so their dimensions stay an
//! unknown (`None`), recorded with a reason, never guessed.
//!
//! The "taken" time is EXIF `DateTimeOriginal` plus `OffsetTimeOriginal`
//! (`SubSecTimeOriginal` refines it below the second). When the offset is
//! absent the wall-clock time is converted as if UTC: the true instant is
//! unknowable, but recording it under one fixed rule keeps a library's
//! relative order intact — one consistent per-camera shift — where an
//! "unknown" instant would have thrown the date away. `None` means the date
//! itself was absent or unreadable, which is different from any recorded
//! time: an unknown is never a zero (FORMAT §0).

use std::{
    fs,
    io::{self, Cursor},
    path::Path,
};

use image::{ImageFormat, ImageReader};
use sha2::{Digest, Sha256};

use crate::sniff::{self, DetectedFormat};

/// What reading one image file told the refresh.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageFacts {
    /// Lowercase hexadecimal SHA-256 digest of the file bytes, the key the
    /// index cache is addressed by (HLD `index`).
    pub sha256: String,
    /// Pixel width after EXIF orientation, or `None` with [`ImageFacts::problem`]
    /// set when the header could not be read.
    pub width: Option<u32>,
    /// Pixel height after EXIF orientation; `None` under the terms of
    /// [`ImageFacts::width`].
    pub height: Option<u32>,
    /// EXIF `DateTimeOriginal` (with `OffsetTimeOriginal` when present) in
    /// ns since the Unix epoch, or `None` when the image carries no
    /// readable one.
    pub taken_ns: Option<i64>,
    /// Why the dimensions could not be read, when they could not. A counted
    /// warning for the refresh, never a reason to abort it.
    pub problem: Option<String>,
}

/// Hash one image file and read its header block.
///
/// `Err` is only the I/O kind — the file could not be read at all. A file
/// whose bytes arrive but hold no readable header is `Ok` with `width`/`height`
/// `None` and [`ImageFacts::problem`] saying why.
pub fn probe(path: &Path) -> io::Result<ImageFacts> {
    let bytes = fs::read(path)?;
    Ok(probe_bytes(&bytes))
}

/// The same probe over bytes the caller already holds (the digest step of a
/// refresh reads the file once and hands the bytes here).
pub fn probe_bytes(bytes: &[u8]) -> ImageFacts {
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let (width, height, problem) = stored_dimensions(bytes);
    let exif_data = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok();
    // Orientations 5-8 transpose the stored pixels, which is the rule
    // dimagine-preview rotates by when it looks at the same block.
    let transposing = exif_data
        .as_ref()
        .and_then(orientation)
        .is_some_and(|value| (5..=8).contains(&value));
    let taken = exif_data.as_ref().and_then(taken_ns);
    ImageFacts {
        sha256,
        width: if transposing { height } else { width },
        height: if transposing { width } else { height },
        taken_ns: taken,
        problem,
    }
}

/// The stored pixel dimensions: the mapped [`sniff::DetectedFormat`] family
/// plus `image`'s header reader. `None` and a reason when the family has no
/// reader (AVIF, HEIF) or the header does not parse (truncated, corrupt).
fn stored_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>, Option<String>) {
    let head = &bytes[..bytes.len().min(sniff::SNIFF_LEN)];
    let detected = sniff::detect(head);
    let Some(format) = image_format(detected) else {
        return (
            None,
            None,
            Some(format!(
                "no header reader for image format {}",
                detected.as_str()
            )),
        );
    };
    match ImageReader::with_format(Cursor::new(bytes), format).into_dimensions() {
        Ok((width, height)) => (Some(width), Some(height), None),
        Err(error) => (
            None,
            None,
            Some(format!("unreadable image header: {error}")),
        ),
    }
}

/// The `image` crate decoder family for a sniffed format. AVIF and HEIC stay
/// outside, on the same line previews drew: a format with no renditions has
/// no dimension reader here either.
fn image_format(detected: DetectedFormat) -> Option<ImageFormat> {
    match detected {
        DetectedFormat::Jpeg => Some(ImageFormat::Jpeg),
        DetectedFormat::Png => Some(ImageFormat::Png),
        DetectedFormat::Gif => Some(ImageFormat::Gif),
        DetectedFormat::Webp => Some(ImageFormat::WebP),
        DetectedFormat::Tiff => Some(ImageFormat::Tiff),
        DetectedFormat::Bmp => Some(ImageFormat::Bmp),
        DetectedFormat::Avif | DetectedFormat::Heif | DetectedFormat::Unknown => None,
    }
}

/// The EXIF orientation of the stored pixels, 1..8, when the image carries
/// a readable one.
fn orientation(data: &exif::Exif) -> Option<u32> {
    data.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
}

/// The EXIF "taken" time of an image, in ns since the Unix epoch: the
/// `DateTimeOriginal` ASCII value refined by `SubSecTimeOriginal` and moved
/// into UTC by `OffsetTimeOriginal` when it is present. `None` when the date
/// is absent, blank, or outside a range an epoch nanosecond can hold.
fn taken_ns(data: &exif::Exif) -> Option<i64> {
    let mut timestamp = exif::DateTime::from_ascii(ascii(data)?).ok()?;
    if let Some(subsec) = ascii_field(data, exif::Tag::SubSecTimeOriginal) {
        // A junk fraction loses nothing but its own precision: the date
        // still stands at the whole second.
        let _ = timestamp.parse_subsec(subsec);
    }
    if let Some(offset) = ascii_field(data, exif::Tag::OffsetTimeOriginal) {
        // Same reasoning as subseconds: a junk offset degrades to the
        // no-offset rule instead of discarding the date.
        let _ = timestamp.parse_offset(offset);
    }
    let date = chrono::NaiveDate::from_ymd_opt(
        i32::from(timestamp.year),
        u32::from(timestamp.month),
        u32::from(timestamp.day),
    )?;
    let time = chrono::NaiveTime::from_hms_nano_opt(
        u32::from(timestamp.hour),
        u32::from(timestamp.minute),
        u32::from(timestamp.second),
        timestamp.nanosecond.unwrap_or(0),
    )?;
    let naive_ns = date.and_time(time).and_utc().timestamp_nanos_opt()?;
    let offset_ns = i64::from(timestamp.offset.unwrap_or(0)) * 60_i64 * 1_000_000_000;
    naive_ns.checked_sub(offset_ns)
}

/// The ASCII value of `Tag::DateTimeOriginal`.
fn ascii(data: &exif::Exif) -> Option<&[u8]> {
    ascii_field(data, exif::Tag::DateTimeOriginal)
}

/// The ASCII value of any EXIF tag, when the image carries one.
fn ascii_field(data: &exif::Exif, tag: exif::Tag) -> Option<&[u8]> {
    match &data.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(chunks) => chunks.first().map(Vec::as_slice),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIXEL_PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");
    const PIXEL_JPG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.jpg");
    const PIXEL_GIF: &[u8] = include_bytes!("../../../tests/fixtures/pixel.gif");
    const PIXEL_BMP: &[u8] = include_bytes!("../../../tests/fixtures/pixel.bmp");
    const PIXEL_TIFF: &[u8] = include_bytes!("../../../tests/fixtures/pixel.tiff");
    const PIXEL_AVIF: &[u8] = include_bytes!("../../../tests/fixtures/pixel.avif");
    const PIXEL_HEIC: &[u8] = include_bytes!("../../../tests/fixtures/pixel.heic");
    /// A real 12x1 JPEG with a JFIF header and no EXIF block at all: the
    /// constant of `scripts/dev/gen-library.py` (the library the benchmark
    /// generates is made of these).
    const NO_EXIF_JPEG: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x02, 0x02,
        0x02, 0x03, 0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x06, 0x04, 0x04, 0x04, 0x04,
        0x04, 0x08, 0x06, 0x06, 0x05, 0x06, 0x09, 0x08, 0x0a, 0x0a, 0x09, 0x08, 0x09, 0x09, 0x0a,
        0x0c, 0x0f, 0x0c, 0x0a, 0x0b, 0x0e, 0x0b, 0x09, 0x09, 0x0d, 0x11, 0x0d, 0x0e, 0x0f, 0x10,
        0x10, 0x11, 0x10, 0x0a, 0x0c, 0x12, 0x13, 0x12, 0x10, 0x13, 0x0f, 0x10, 0x10, 0x10, 0xff,
        0xdb, 0x00, 0x43, 0x01, 0x03, 0x03, 0x03, 0x04, 0x03, 0x04, 0x08, 0x04, 0x04, 0x08, 0x10,
        0x0b, 0x09, 0x0b, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x0c, 0x03, 0x01, 0x11, 0x00, 0x02,
        0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00, 0x15, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xff, 0xc4,
        0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xc4, 0x00, 0x16, 0x01, 0x01, 0x01, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x07, 0x08, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xda, 0x00,
        0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00, 0xad, 0x58, 0x5d, 0x53,
        0x01, 0xff, 0xd9,
    ];
    /// A real 12x1 lossy WebP bitstream: the constant of
    /// `scripts/dev/gen-library.py` (the stub in tests/fixtures is a header
    /// for sniff tests and holds no frame header).
    const REAL_WEBP: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x38, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x2c, 0x00, 0x00, 0x00, 0x90, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x0c, 0x00, 0x01, 0x00,
        0x02, 0x00, 0x34, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x00, 0x03, 0x98, 0x00, 0xfe, 0xf3, 0x4b,
        0x97, 0xfe, 0xd6, 0x87, 0xfe, 0xbb, 0x3f, 0xfd, 0x76, 0x7f, 0xd1, 0x1f, 0xf5, 0x76, 0x27,
        0xc2, 0x87, 0x20, 0x00,
    ];

    fn probe_fixture(bytes: &[u8]) -> ImageFacts {
        probe_bytes(bytes)
    }

    fn sha256_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn probe_hashes_the_bytes_it_was_given() {
        let facts = probe_fixture(PIXEL_PNG);
        assert_eq!(facts.problem, None, "a healthy header is not a problem");
        assert_eq!(facts.sha256, sha256_of(PIXEL_PNG));
    }

    #[test]
    fn probe_reports_stored_dimensions_for_every_reader_family() {
        let cases = [
            (PIXEL_PNG, "png", (Some(1), Some(1))),
            (PIXEL_JPG, "jpeg", (Some(1), Some(1))),
            (PIXEL_GIF, "gif", (Some(1), Some(1))),
            (PIXEL_BMP, "bmp", (Some(1), Some(1))),
            (PIXEL_TIFF, "tiff", (Some(1), Some(1))),
            (REAL_WEBP, "webp", (Some(12), Some(1))),
        ];
        for (bytes, name, expected) in cases {
            let facts = probe_fixture(bytes);
            assert_eq!(
                (facts.width, facts.height),
                expected,
                "{name} needs its stored dimensions"
            );
            assert_eq!(facts.problem, None, "{name} read cleanly");
        }
    }

    /// EXIF with a one-entry directory (a real `sips` conversion) holds no
    /// date at all, so the taken time is an unknown, not an instant.
    #[test]
    fn a_jpeg_without_a_date_has_an_unknown_taken_time() {
        let facts = probe_fixture(PIXEL_JPG);
        assert_eq!(
            (facts.width, facts.height),
            (Some(1), Some(1)),
            "its dimensions still read"
        );
        assert_eq!(facts.taken_ns, None);
        assert_eq!(facts.problem, None);
    }

    /// The 12x1 JPEG the library generator writes: no EXIF block at all.
    #[test]
    fn a_jpeg_with_no_exif_block_still_carries_dimensions() {
        let facts = probe_fixture(NO_EXIF_JPEG);
        assert_eq!(
            (facts.width, facts.height),
            (Some(12), Some(1)),
            "scripts/dev gen-library.py's constant is 12x1"
        );
        assert_eq!(facts.taken_ns, None);
        assert_eq!(facts.problem, None);
    }

    #[test]
    fn a_jpeg_with_a_date_and_an_offset_records_the_true_instant() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/exif-date.jpg"));
        assert_eq!(
            (facts.width, facts.height, facts.problem),
            (Some(12), Some(1), None)
        );
        // 2023-07-12 20:54:07.123 +01:00: the same instant the `added`
        // fixture in this crate uses, plus the 123 ms `SubSecTimeOriginal`
        // refines it with.
        assert_eq!(facts.taken_ns, Some(1_689_191_647_123_000_000));
    }

    /// Without `OffsetTimeOriginal` the wall-clock is recorded as if UTC:
    /// one fixed rule, so the library's relative order survives.
    #[test]
    fn a_jpeg_without_an_offset_is_recorded_as_utc() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/exif-naive.jpg"));
        assert_eq!(
            (facts.width, facts.height),
            (Some(12), Some(1)),
            "its dimensions still read"
        );
        assert_eq!(facts.taken_ns, Some(1_689_195_247_000_000_000));
    }

    /// Orientation 6: the stored 12x1 is displayed 1x12 — the pair a justified
    /// row needs. The offset here is negative, to pin the direction of the
    /// conversion too.
    #[test]
    fn a_rotated_jpeg_swaps_its_dimension_pair() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/rotated-exif.jpg"));
        assert_eq!(
            (facts.width, facts.height),
            (Some(1), Some(12)),
            "orientation 6 swaps stored 12x1"
        );
        // 2018-03-25 11:22:33 -05:00.
        assert_eq!(facts.taken_ns, Some(1_521_994_953_000_000_000));
        assert_eq!(facts.problem, None);
    }

    /// The classic junk date: all zeros. It parses as EXIF-shaped bytes but
    /// names no real day, so the taken time is unknown — never an epoch
    /// instant masquerading as midnight 1970.
    #[test]
    fn a_zero_datetime_is_an_unknown_not_the_epoch() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/exif-zeros.jpg"));
        assert_eq!(
            (facts.width, facts.height),
            (Some(12), Some(1)),
            "the header still reads"
        );
        assert_eq!(facts.taken_ns, None);
        assert_eq!(facts.problem, None, "the date is unknown, not the header");
    }

    #[test]
    fn a_truncated_png_is_a_problem_not_a_crash() {
        // The signature plus four bytes of an IHDR length: the dimension
        // payload never arrives (the generator's `truncated.png` bad case).
        let truncated = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let facts = probe_fixture(truncated);
        assert_eq!((facts.width, facts.height), (None, None));
        assert_eq!(facts.taken_ns, None);
        let reason = facts.problem.unwrap();
        assert!(
            reason.contains("unreadable"),
            "the reason is recorded: {reason}"
        );
    }

    #[test]
    fn a_truncated_jpeg_is_a_problem_not_a_crash() {
        // SOI and the start of the JFIF APP0, cut before any frame marker.
        let truncated = &NO_EXIF_JPEG[..8];
        let facts = probe_fixture(truncated);
        assert_eq!((facts.width, facts.height), (None, None));
        assert_eq!(facts.taken_ns, None);
        assert!(facts
            .problem
            .is_some_and(|problem| problem.contains("unreadable")));
    }

    #[test]
    fn formats_without_a_reader_have_an_honest_unknown() {
        for (bytes, name) in [(PIXEL_AVIF, "avif"), (PIXEL_HEIC, "heif")] {
            let facts = probe_fixture(bytes);
            assert_eq!((facts.width, facts.height), (None, None), "{name}");
            assert_eq!(
                facts.problem,
                Some(format!("no header reader for image format {name}")),
                "{name} names its reason"
            );
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_an_io_error() {
        let error = probe(Path::new("no/such/dir/image.jpg")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// The four EXIF fixtures as live files, so the path-taking probe is
    /// exercised too.
    #[test]
    fn probe_on_a_path_reads_the_same_facts_as_probe_on_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures: [(&[u8], &str); 4] = [
            (
                include_bytes!("../../../tests/fixtures/exif-date.jpg"),
                "exif-date.jpg",
            ),
            (
                include_bytes!("../../../tests/fixtures/exif-naive.jpg"),
                "exif-naive.jpg",
            ),
            (
                include_bytes!("../../../tests/fixtures/rotated-exif.jpg"),
                "rotated-exif.jpg",
            ),
            (
                include_bytes!("../../../tests/fixtures/exif-zeros.jpg"),
                "exif-zeros.jpg",
            ),
        ];
        for (bytes, name) in fixtures {
            std::fs::write(dir.path().join(name), bytes).unwrap();
            assert_eq!(
                probe(&dir.path().join(name)).unwrap(),
                probe_bytes(bytes),
                "{name} reads the same through a path"
            );
        }
    }
}
