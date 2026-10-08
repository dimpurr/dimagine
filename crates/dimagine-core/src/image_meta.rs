//! Per-image metadata read from the bytes' header: the stored pixel size and
//! the "taken" time (HLD `index`).
//!
//! These functions are called by [`crate::index::sync_index`] for image files
//! whose content was re-hashed anyway (the digest requires the bytes; the
//! header does not), so the extraction costs no more file access. Only the
//! header and the EXIF block are parsed: pictures are never decoded here —
//! `image`'s `into_dimensions` reads a stored header and `exif::Reader` an
//! EXIF block, the same framing dimagine-preview reads when it applies
//! orientation. AVIF and HEIF have no header reader in this build, matching
//! the renditions previews consider supported, so their dimensions stay an
//! unknown (`None`), recorded with a reason, never guessed. Their taken time
//! is still read, because EXIF is read separately: one fact never borrows the
//! other's unknown.
//!
//! The "taken" time is the first date the file's own metadata names, in the
//! order `DateTimeOriginal` (when the picture was taken), `DateTimeDigitized`
//! (when it was digitised), the TIFF `DateTime` (when the file was last
//! written) — each refined by its own `SubSecTime*` and moved into UTC by its
//! own `OffsetTime*` — and, when EXIF names none, the XMP date an exporter
//! moved it to. A date is read tolerantly (the EXIF `YYYY:MM:DD HH:MM:SS` and
//! the ISO `YYYY-MM-DDThh:mm:ssZ` alike, a date with no time of day) but never
//! invented: `None` means no date named a moment, which is different from any
//! recorded time (FORMAT §0).
//!
//! When it is `None`, [`ImageFacts::taken_reason`] says which kind of unknown
//! this is — no EXIF block, EXIF with no date, a block or a date that could not
//! be read, or a file that is not readable at all — because those situations
//! look identical from the missing number and need different answers.

use std::{
    fs,
    io::{self, Cursor},
    path::Path,
};

use dimagine_index::TakenReason;
use image::{ImageFormat, ImageReader};
use sha2::{Digest, Sha256};

use crate::sniff::{self, DetectedFormat};

/// The XMP date properties that answer "when was this taken", in the order
/// they are trusted. The first two mirror the EXIF tags of the same name;
/// `photoshop:DateCreated` and `xmp:CreateDate` are where writers that drop
/// EXIF leave the date, and `xmp:ModifyDate` is last because it says when the
/// file was written, not when the picture was taken.
const XMP_DATES: [&[u8]; 5] = [
    b"exif:DateTimeOriginal".as_slice(),
    b"exif:DateTimeDigitized".as_slice(),
    b"photoshop:DateCreated".as_slice(),
    b"xmp:CreateDate".as_slice(),
    b"xmp:ModifyDate".as_slice(),
];

/// The XMP tag of a TIFF or EXIF block: an XMP packet stored as a field rather
/// than as a container segment (TIFF 6.0 SP1, Exif 2.3 §4.6.4).
const XMP_TAG: u16 = 700;

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
    /// The image's taken time in ns since the Unix epoch, as the module
    /// describes it, or `None` when nothing in the file names one.
    pub taken_ns: Option<i64>,
    /// Why that time is missing, when it is ([`TakenReason`]). The two fields
    /// are complementary: exactly one of `taken_ns` and `taken_reason` is
    /// `Some`, so an image either has an instant or names its kind of unknown.
    pub taken_reason: Option<TakenReason>,
    /// Why the dimensions could not be read, when they could not. A counted
    /// warning for the refresh, never a reason to abort it.
    pub problem: Option<String>,
}

/// Where one file's EXIF block got to. The distinction between the first two
/// and the last is what `taken_reason` reports: "the file has no EXIF" is a
/// fact about the file, "we could not read its EXIF" is a fact about the reader.
enum ExifBlock {
    /// A block was parsed, apart from any entry inside it that could not be
    /// read: an entry with an out-of-range offset costs that entry, not the
    /// date beside it.
    Read(exif::Exif),
    /// The container holds no EXIF block, or is a format with nowhere to put
    /// one (BMP, GIF).
    Absent,
    /// An EXIF block is there and did not parse.
    Unreadable,
}

impl ExifBlock {
    /// What a missing taken time means when the block looked like this.
    fn reason(&self) -> TakenReason {
        match self {
            Self::Read(_) => TakenReason::ExifWithoutDate,
            Self::Absent => TakenReason::NoExif,
            Self::Unreadable => TakenReason::UnreadableExif,
        }
    }
}

/// Why the stored pixel size could not be had, keeping the two cases apart
/// because only one of them says the file is unreadable.
enum HeaderGap {
    /// The format has no header reader in this build (AVIF, HEIF, unknown).
    NoReader,
    /// A reader exists and the header did not parse: truncated or corrupt. The
    /// reader's own words come along, as they always have for the operator.
    Unreadable(String),
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
    let detected = sniff::detect(&bytes[..bytes.len().min(sniff::SNIFF_LEN)]);
    let (width, height, gap) = stored_dimensions(bytes, detected);
    let block = read_exif(bytes, detected);
    // Orientations 5-8 transpose the stored pixels, which is the rule
    // dimagine-preview rotates by when it looks at the same block.
    let transposing = block
        .read()
        .and_then(orientation)
        .is_some_and(|value| (5..=8).contains(&value));
    let (taken_ns, taken_reason) = taken(bytes, detected, &block);
    // Two cases make the file itself the reason its taken time is missing: bytes
    // no header reader could parse, and bytes that are not an identified image at
    // all. Neither says anything about the EXIF inside them.
    let unreadable_file =
        detected == DetectedFormat::Unknown || matches!(gap, Some(HeaderGap::Unreadable(_)));
    let taken_reason = match taken_ns {
        Some(_) => None,
        None if unreadable_file => Some(TakenReason::UnreadableFile),
        None => taken_reason,
    };
    ImageFacts {
        sha256,
        width: if transposing { height } else { width },
        height: if transposing { width } else { height },
        taken_ns,
        taken_reason,
        problem: gap.as_ref().map(|gap| match gap {
            HeaderGap::NoReader => {
                format!("no header reader for image format {}", detected.as_str())
            }
            HeaderGap::Unreadable(detail) => format!("unreadable image header: {detail}"),
        }),
    }
}

/// The EXIF block of one file, read as far as it will go.
fn read_exif(bytes: &[u8], detected: DetectedFormat) -> ExifBlock {
    if !carries_exif(detected) {
        return ExifBlock::Absent;
    }
    // `continue_on_error` keeps the entries that did parse: real cameras write
    // maker notes whose offsets run past the block, and one such entry is no
    // reason to lose the date written next to it.
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    match reader.read_from_container(&mut Cursor::new(bytes)) {
        Ok(data) => ExifBlock::Read(data),
        Err(error) => match error.distill_partial_result(|_skipped| {}) {
            // A block that yielded no entry at all never got past its directory:
            // that is the reader failing, not a file with nothing in it, and
            // reporting `exif-without-date` would blame the file.
            Ok(data) if data.fields().count() == 0 => ExifBlock::Unreadable,
            Ok(data) => ExifBlock::Read(data),
            // The containers answer `NotFound` when they hold no EXIF
            // attribute; every other error is a block that failed to parse.
            Err(exif::Error::NotFound(_)) => ExifBlock::Absent,
            Err(_) => ExifBlock::Unreadable,
        },
    }
}

/// Whether this container family has anywhere to put an EXIF block. JPEG,
/// PNG (an `eXIf` chunk), WebP (an `EXIF` chunk), TIFF (itself) and the
/// ISO-BMFF family all do; BMP and GIF have no such place.
fn carries_exif(detected: DetectedFormat) -> bool {
    !matches!(
        detected,
        DetectedFormat::Bmp | DetectedFormat::Gif | DetectedFormat::Unknown
    )
}

/// The stored pixel dimensions: the mapped [`sniff::DetectedFormat`] family
/// plus `image`'s header reader. `None` and a gap when the family has no
/// reader (AVIF, HEIF) or the header does not parse (truncated, corrupt).
fn stored_dimensions(
    bytes: &[u8],
    detected: DetectedFormat,
) -> (Option<u32>, Option<u32>, Option<HeaderGap>) {
    let Some(format) = image_format(detected) else {
        return (None, None, Some(HeaderGap::NoReader));
    };
    match ImageReader::with_format(Cursor::new(bytes), format).into_dimensions() {
        Ok((width, height)) => (Some(width), Some(height), None),
        Err(error) => (None, None, Some(HeaderGap::Unreadable(error.to_string()))),
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
/// a readable one. The primary image's own, which is what the stored pixel
/// size is being oriented.
fn orientation(data: &exif::Exif) -> Option<u32> {
    data.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
}

impl ExifBlock {
    /// The parsed block, if there is one.
    fn read(&self) -> Option<&exif::Exif> {
        match self {
            Self::Read(data) => Some(data),
            Self::Absent | Self::Unreadable => None,
        }
    }
}

/// The image's taken time and, when there is none, the kind of unknown.
///
/// EXIF first — see the module's order — then the XMP packet for a file whose
/// exporter moved the date out of EXIF. A date that is present but names no
/// moment is [`TakenReason::UnreadableDate`], never a silent fall through to
/// "there was no date": the operator's question is exactly which of the two
/// happened.
fn taken(
    bytes: &[u8],
    detected: DetectedFormat,
    block: &ExifBlock,
) -> (Option<i64>, Option<TakenReason>) {
    let data = block.read();
    let (ns, exif_named_a_date) = data.map_or((None, false), exif_taken);
    if let Some(ns) = ns {
        return (Some(ns), None);
    }
    let (ns, xmp_named_a_date) = xmp_taken(bytes, detected, data);
    if let Some(ns) = ns {
        return (Some(ns), None);
    }
    let reason = if exif_named_a_date || xmp_named_a_date {
        TakenReason::UnreadableDate
    } else {
        block.reason()
    };
    (None, Some(reason))
}

/// The EXIF date tags, each with the subsecond and offset tags that belong to
/// it, in the order they answer "when was this taken".
const EXIF_DATES: [(exif::Tag, exif::Tag, exif::Tag); 3] = [
    (
        exif::Tag::DateTimeOriginal,
        exif::Tag::SubSecTimeOriginal,
        exif::Tag::OffsetTimeOriginal,
    ),
    (
        exif::Tag::DateTimeDigitized,
        exif::Tag::SubSecTimeDigitized,
        exif::Tag::OffsetTimeDigitized,
    ),
    (
        exif::Tag::DateTime,
        exif::Tag::SubSecTime,
        exif::Tag::OffsetTime,
    ),
];

/// The EXIF taken time: `(the instant, whether any date tag was present)`.
/// The second answer is what separates "no date was written" from "a date was
/// written and could not be read".
fn exif_taken(data: &exif::Exif) -> (Option<i64>, bool) {
    let mut named_a_date = false;
    for (date_tag, subsec_tag, offset_tag) in EXIF_DATES {
        let Some(date) = field(data, date_tag) else {
            continue;
        };
        named_a_date = true;
        let subsec = field(data, subsec_tag).and_then(text);
        let offset = field(data, offset_tag).and_then(text);
        if let Some(ns) = text(date).and_then(|raw| moment(raw, subsec, offset)) {
            return (Some(ns), named_a_date);
        }
    }
    (None, named_a_date)
}

/// The first field carrying `tag`, in any IFD, in parse order.
///
/// The Exif and GPS sub-IFDs are read under the number of the IFD that points
/// at them, so `In::PRIMARY` covers the common single-image file but not a file
/// whose metadata hangs off a second IFD (a thumbnail first, the main image
/// second, is how scanners write TIFF). Parse order is the main image's
/// directories first, so a date there wins over one a thumbnail repeats.
fn field(data: &exif::Exif, tag: exif::Tag) -> Option<&exif::Field> {
    data.fields().find(|candidate| candidate.tag == tag)
}

/// The bytes of a field's first value, when it holds text. A date belongs in
/// an ASCII field; some writers store it as UNDEFINED, which is the same bytes
/// held flat rather than chunked.
fn text(field: &exif::Field) -> Option<&[u8]> {
    match &field.value {
        exif::Value::Ascii(chunks) => chunks.first().map(Vec::as_slice),
        exif::Value::Undefined(bytes, _) => Some(bytes),
        _ => None,
    }
}

/// The XMP taken time: `(the instant, whether a date property was present)`.
/// The packet is looked for in the EXIF's own XMP field and in the container:
/// a JPEG APP1 `xap/1.0` segment or a WebP `XMP ` chunk — the second is read
/// even when there is no EXIF to read, because an exporter that strips EXIF
/// leaves the packet where it is. PNG is left out: its XMP has three competing
/// carriers (a raw `XML:com.adobe.xmp` chunk libpng rejects, a `zTXt`/`iTXt`
/// keyword, ImageMagick's raw-profile profile) and reading them needs a
/// decompressor this build does not carry.
fn xmp_taken(
    bytes: &[u8],
    detected: DetectedFormat,
    data: Option<&exif::Exif>,
) -> (Option<i64>, bool) {
    let in_exif = data
        .and_then(|data| field_number(data, XMP_TAG))
        .and_then(text);
    let in_container = match detected {
        DetectedFormat::Jpeg => jpeg_xmp(bytes),
        DetectedFormat::Webp => riff_chunk(bytes, b"XMP "),
        _ => None,
    };
    let mut named_a_date = false;
    for packet in [in_exif, in_container].into_iter().flatten() {
        for name in XMP_DATES {
            let Some(value) = xmp_value(packet, name) else {
                continue;
            };
            named_a_date = true;
            if let Some(ns) = moment(value, None, None) {
                return (Some(ns), named_a_date);
            }
        }
    }
    (None, named_a_date)
}

/// The first field whose tag number is `number`, in any IFD. Tag 700 (XMP) is
/// not a tag `exif` knows, so it is reached by number rather than by name.
fn field_number(data: &exif::Exif, number: u16) -> Option<&exif::Field> {
    data.fields()
        .find(|candidate| candidate.tag.number() == number)
}

/// The XMP packet of a JPEG: the first APP1 segment that is one. Segment
/// walking stops where the scan data starts, which is as far as any metadata
/// can be.
fn jpeg_xmp(bytes: &[u8]) -> Option<&[u8]> {
    // The NUL is part of the identifier: what follows it is the packet.
    const NAMESPACE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    if !bytes.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            return None;
        }
        let marker = bytes[at + 1];
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        if length < 2 || at + 2 + length > bytes.len() {
            return None;
        }
        let payload = &bytes[at + 4..at + 2 + length];
        if marker == 0xe1 && payload.starts_with(NAMESPACE) {
            let packet = &payload[NAMESPACE.len()..];
            // The packet ends with a NUL terminator or the segment's padding,
            // neither of which is XML.
            let end = packet
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(packet.len());
            return Some(&packet[..end]);
        }
        at += 2 + length;
    }
    None
}

/// One chunk of a RIFF container (a WebP's `EXIF` or `XMP ` chunk), by fourcc.
fn riff_chunk<'a>(bytes: &'a [u8], fourcc: &[u8]) -> Option<&'a [u8]> {
    if !bytes.starts_with(b"RIFF") || bytes.get(8..12) != Some(b"WEBP".as_slice()) {
        return None;
    }
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let length =
            u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
                as usize;
        let body_start = at + 8;
        if bytes[at..at + 4] != *fourcc {
            at = body_start + length + (length % 2);
            continue;
        }
        let body_end = (body_start + length).min(bytes.len());
        return Some(&bytes[body_start..body_end]);
    }
    None
}

/// The value the XMP property `name` carries, as an element
/// (`<name>value</name>`) or as an attribute of a tag (`<tag name="value">`):
/// XMP writers use both forms. A name counts only at its own boundaries — so
/// `exif:DateTime` never matches inside `exif:DateTimeDigitized` — and only in
/// one of those two positions, never as a *mention* inside another property's
/// text or quoted value: a caption that happens to spell
/// `exif:DateTimeOriginal='2019-…'` is not this file's date (RW50 M1).
fn xmp_value<'a>(packet: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut rest = packet;
    while let Some(offset) = find(rest, name) {
        let after = &rest[offset + name.len()..];
        if property_position(rest, offset) {
            if let Some(value) = quoted_or_element(after) {
                return Some(value);
            }
        }
        rest = after;
    }
    None
}

/// Whether the occurrence of a property name at `offset` is one of the two
/// positions a property is written in: the head of an element (`<name>`) or a
/// top-level attribute of a tag (`<tag name="…">`).
///
/// Text content and quoted attribute values are where a *mention* lives. A
/// `dc:description` caption that spells `exif:DateTimeOriginal='2019-…'` puts
/// the name inside a tag — but inside a quoted value, and this is what tells
/// the two apart.
fn property_position(packet: &[u8], offset: usize) -> bool {
    // The `<` that opens the tag this name sits in: the last one before it,
    // with no `>` between. An occurrence with no such `<`, or one that follows
    // a closed tag, is text content.
    let before = &packet[..offset];
    let Some(open) = before.iter().rposition(|byte| *byte == b'<') else {
        return false;
    };
    let head = &before[open + 1..];
    if head.contains(&b'>') {
        return false;
    }
    // A name byte immediately before means this is part of a longer name.
    if offset > 0 && is_name_byte(packet[offset - 1]) {
        return false;
    }
    // `<name…>`: the element's own name.
    if head.is_empty() {
        return true;
    }
    // `<tag name…>`: an attribute, so the tag name must have ended and no
    // quote may be open between the two — an open quote makes this a value.
    head.last().is_some_and(|byte| byte.is_ascii_whitespace()) && !inside_quotes(head)
}

/// Whether a quote opened in `text` is still open at its end.
fn inside_quotes(text: &[u8]) -> bool {
    let mut open = None;
    for byte in text {
        match (open, byte) {
            (None, b'"' | b'\'') => open = Some(*byte),
            (Some(quote), _) if *byte == quote => open = None,
            _ => {}
        }
    }
    open.is_some()
}

/// The bytes after a property name: element text up to the closing tag, or an
/// attribute value between its quotes.
fn quoted_or_element(after: &[u8]) -> Option<&[u8]> {
    match after.first()? {
        b'>' => {
            let body = &after[1..];
            let end = body.iter().position(|byte| *byte == b'<')?;
            Some(&body[..end])
        }
        b'=' => {
            let quote = *after.get(1)?;
            if quote != b'"' && quote != b'\'' {
                return None;
            }
            let body = &after[2..];
            let end = body.iter().position(|byte| *byte == quote)?;
            Some(&body[..end])
        }
        _ => None,
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_')
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The instant a date string names, tolerating the forms real files use: the
/// EXIF `2023:07:12 20:54:07` and the ISO `2023-07-12T20:54:07+01:00` alike, a
/// date with no seconds or no time of day at all, `Z` or `±hh:mm` zones.
/// `subsec` and `offset` are the companion tags of the EXIF field the value
/// came from, used when the value itself carries no fraction or zone.
///
/// `None` is honest: text that names no moment — the classic all-zero date, a
/// month 13, a year no epoch nanosecond can hold — stays an unknown and never
/// becomes an instant that sorts beside 1970.
fn moment(raw: &[u8], subsec: Option<&[u8]>, offset: Option<&[u8]>) -> Option<i64> {
    let mut rest = trim_outer(raw);
    let year = fixed(&mut rest, 4)?;
    date_separator(&mut rest)?;
    let month = digits(&mut rest)?;
    date_separator(&mut rest)?;
    let day = digits(&mut rest)?;
    let mut hour = 0;
    let mut minute = 0;
    let mut second = 0;
    let mut fraction = 0;
    match rest.first() {
        None => {}
        Some(b' ' | b'T' | b't') => {
            rest = &rest[1..];
            hour = digits(&mut rest)?;
            if rest.first() != Some(&b':') {
                return None;
            }
            rest = &rest[1..];
            minute = digits(&mut rest)?;
            if rest.first() == Some(&b':') {
                rest = &rest[1..];
                second = digits(&mut rest)?;
            }
            if matches!(rest.first(), Some(b'.' | b',')) {
                let after = &rest[1..];
                let decimals = take_digits(after)?;
                fraction = fraction_ns(decimals)?;
                rest = &after[decimals.len()..];
            }
        }
        _ => return None,
    }
    if fraction == 0 {
        // A companion tag that is junk loses nothing but its own precision, the
        // same rule as an unreadable offset: the date still stands at the whole
        // second.
        fraction = subsec.and_then(subsec_ns).unwrap_or(0);
    }
    let rest = trim_outer(rest);
    // Neither the value nor its companion names a zone: the wall clock is
    // recorded as if UTC, one fixed rule that keeps a library's relative order
    // intact where an unknown instant would have thrown the date away.
    let zone_minutes = match rest.first() {
        None => offset.and_then(offset_minutes).unwrap_or(0),
        Some(b'Z' | b'z') if rest.len() == 1 => 0,
        Some(b'+' | b'-') => offset_minutes(rest)?,
        _ => return None,
    };
    let date = chrono::NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    let time = chrono::NaiveTime::from_hms_nano_opt(hour, minute, second, fraction)?;
    let naive_ns = date.and_time(time).and_utc().timestamp_nanos_opt()?;
    naive_ns.checked_sub(i64::from(zone_minutes) * 60 * 1_000_000_000)
}

/// The text with its surrounding whitespace and the NUL an EXIF ASCII field is
/// terminated with — neither of which is part of the date.
fn trim_outer(bytes: &[u8]) -> &[u8] {
    let blank = |byte: &u8| byte.is_ascii_whitespace() || *byte == 0;
    let start = bytes.iter().position(|byte| !blank(byte)).unwrap_or(0);
    let end = bytes
        .iter()
        .rposition(|byte| !blank(byte))
        .map_or(0, |last| last + 1);
    &bytes[start.min(end)..end]
}

/// The run of decimal digits at the front, or `None` where there are none.
fn take_digits(bytes: &[u8]) -> Option<&[u8]> {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    (end > 0).then_some(&bytes[..end])
}

/// One or two decimal digits at the front, consumed. Time and calendar fields
/// are written zero-padded, and one writer that is not still names the same
/// moment, so a short field is read rather than rejected.
fn digits(rest: &mut &[u8]) -> Option<u32> {
    let taken = take_digits(&rest[..rest.len().min(2)])?;
    let value = std::str::from_utf8(taken).ok()?.parse().ok()?;
    *rest = &rest[taken.len()..];
    Some(value)
}

/// Exactly `len` decimal digits at the front, consumed. Unlike [`digits`] this
/// tolerates no short field: a two-digit year is not a sloppily written four-
/// digit one, and which century it meant would have to be invented. The field
/// may be followed by more digits, because `+0100` is two of them, not one.
fn fixed(rest: &mut &[u8], len: usize) -> Option<u32> {
    let taken = rest.get(..len)?;
    if !taken.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = std::str::from_utf8(taken).ok()?.parse().ok()?;
    *rest = &rest[len..];
    Some(value)
}

/// The character between date fields — `:` as EXIF writes it, `-` as ISO writes
/// it — consumed. Mixed within one value is not tolerated: it names no standard
/// the reader could follow.
fn date_separator(rest: &mut &[u8]) -> Option<()> {
    matches!(rest.first(), Some(b':' | b'-')).then(|| *rest = &rest[1..])
}

/// The digits after a decimal point as nanoseconds: `123` is `.123` and `1` is
/// `.1`; precision past a nanosecond is dropped, never rounded up.
fn fraction_ns(digits: &[u8]) -> Option<u32> {
    let mut nano = [b'0'; 9];
    let taken = &digits[..digits.len().min(9)];
    nano[..taken.len()].copy_from_slice(taken);
    std::str::from_utf8(&nano).ok()?.parse().ok()
}

/// The value of a `SubSecTime*` tag as nanoseconds: the digits after the point,
/// which some writers include in the tag itself.
fn subsec_ns(raw: &[u8]) -> Option<u32> {
    let bytes = trim_outer(raw);
    let bytes = bytes.strip_prefix(b".").unwrap_or(bytes);
    fraction_ns(take_digits(bytes)?)
}

/// An offset from UTC as minutes: the EXIF and ISO `+01:00`, the `+0100` some
/// writers prefer, and `Z`. Nothing else is guessed at — a value that is not one
/// of these leaves the date in the zone-less rule rather than a zone of the
/// reader's choosing.
///
/// A syntactically perfect offset no zone could name is refused the same way,
/// so a corrupt or hand-edited `OffsetTimeOriginal` cannot move the instant by
/// days: no zone stands further than ±14:00 from UTC, and a minute field past
/// 59 is not a time. `+25:00`, `+24:00` and `+99:99` all parse as shapes and
/// then fail this check, which is what puts the caller back on its documented
/// rule instead of applying six and a half days of error.
fn offset_minutes(raw: &[u8]) -> Option<i32> {
    let mut rest = trim_outer(raw);
    let sign = match rest.first()? {
        b'+' => 1,
        b'-' => -1,
        b'Z' | b'z' if rest.len() == 1 => return Some(0),
        _ => return None,
    };
    rest = &rest[1..];
    let hours = fixed(&mut rest, 2)?;
    if rest.first() == Some(&b':') {
        rest = &rest[1..];
    }
    let minutes = fixed(&mut rest, 2)?;
    if !rest.is_empty() || hours > 23 || minutes > 59 {
        return None;
    }
    let from_utc = i32::try_from(hours * 60 + minutes).ok()?;
    (from_utc <= 14 * 60).then_some(sign * from_utc)
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
        assert_eq!(facts.taken_reason, Some(TakenReason::ExifWithoutDate));
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
        assert_eq!(facts.taken_reason, Some(TakenReason::NoExif));
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
        assert_eq!(facts.taken_reason, Some(TakenReason::UnreadableDate));
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

    /// Files whose EXIF or XMP names a moment in a form the narrow reading of
    /// `DateTimeOriginal` alone misses, each with the instant its own bytes say.
    /// The camera stamp — `2023:07:12 20:54:07.123 +01:00` — is written
    /// identically in the JPEG, PNG and WebP fixtures, so the three containers
    /// are checked against one expected value.
    const DATED: [(&[u8], &str, i64); 13] = [
        (
            include_bytes!("../../../tests/fixtures/exif-digitized.jpg"),
            "exif-digitized.jpg",
            1_554_210_900_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-datetime-only.jpg"),
            "exif-datetime-only.jpg",
            1_448_258_828_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-no-seconds.jpg"),
            "exif-no-seconds.jpg",
            1_488_280_800_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-iso-date.jpg"),
            "exif-iso-date.jpg",
            1_625_067_912_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-undefined-date.jpg"),
            "exif-undefined-date.jpg",
            1_462_298_521_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-second-ifd.jpg"),
            "exif-second-ifd.jpg",
            1_312_884_672_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-broken-entry.jpg"),
            "exif-broken-entry.jpg",
            1_689_191_647_123_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/xmp-date.jpg"),
            "xmp-date.jpg",
            1_662_027_072_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/xmp-in-exif.jpg"),
            "xmp-in-exif.jpg",
            1_393_909_567_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/xmp-date.webp"),
            "xmp-date.webp",
            1_525_590_489_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/xmp-mention.jpg"),
            "xmp-mention.jpg",
            1_465_286_950_000_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif.png"),
            "exif.png",
            1_689_191_647_123_000_000,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif.webp"),
            "exif.webp",
            1_689_191_647_123_000_000,
        ),
    ];

    /// Files that name no moment, each with the kind of unknown it is — the
    /// five kinds the index column records, on a file that shows each.
    const UNDATED: [(&[u8], &str, TakenReason); 5] = [
        (
            include_bytes!("../../../tests/fixtures/exif-no-date.jpg"),
            "exif-no-date.jpg",
            TakenReason::ExifWithoutDate,
        ),
        (
            include_bytes!("../../../tests/fixtures/pixel.bmp"),
            "pixel.bmp",
            TakenReason::NoExif,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-broken-block.jpg"),
            "exif-broken-block.jpg",
            TakenReason::UnreadableExif,
        ),
        (
            include_bytes!("../../../tests/fixtures/exif-far-date.jpg"),
            "exif-far-date.jpg",
            TakenReason::UnreadableDate,
        ),
        (
            include_bytes!("../../../tests/fixtures/pixel.webp"),
            "pixel.webp",
            TakenReason::UnreadableFile,
        ),
    ];

    /// The dates real files carry that a reader looking only for
    /// `DateTimeOriginal` in the first IFD would walk past: a later date tag,
    /// the file's own `DateTime`, a date to the minute, an ISO spelling, an
    /// UNDEFINED type, a second IFD, an entry that fails beside the date, the
    /// XMP packet an exporter moved the date to, and a packet whose caption
    /// merely *mentions* a date property beside the one the file really sets.
    #[test]
    fn dates_outside_the_narrow_reading_all_come_back() {
        for (bytes, name, expected) in DATED {
            let facts = probe_fixture(bytes);
            assert_eq!(facts.taken_ns, Some(expected), "{name} names its moment");
            assert_eq!(facts.taken_reason, None, "{name} has no unknown left");
        }
    }

    /// The mention trap (RW50 M1): a caption that *spells* a date property
    /// must not speak for the file. `xmp-mention.jpg` carries
    /// `dc:description="scanned; exif:DateTimeOriginal='2019-01-02T03:04:05Z'
    /// noted"` beside a real `<photoshop:DateCreated>`; the property wins, and
    /// the sentence inside the caption stays a sentence. The reader searched
    /// the packet for the *name* anywhere and took the first match that was
    /// followed by `=` or `>`, so before this it recorded the mention — a date
    /// the file does not carry — and counted the image as having a taken time,
    /// where no "why missing" breakdown could ever flag it.
    #[test]
    fn a_date_property_mentioned_in_a_caption_does_not_speak_for_the_file() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/xmp-mention.jpg"));
        assert_eq!(
            facts.taken_ns,
            Some(1_465_286_950_000_000_000),
            "the file's own photoshop:DateCreated, not the caption's mention"
        );
        assert_ne!(
            facts.taken_ns,
            Some(1_546_398_245_000_000_000),
            "the date the caption mentions must not be recorded"
        );
        assert_eq!(facts.taken_reason, None);
    }

    /// The kinds of missing taken time, which look identical from the missing
    /// number and need different answers. The header is a separate fact: the
    /// files here all read their dimensions fine, except the one whose reason is
    /// that it did not.
    #[test]
    fn a_missing_taken_time_names_which_kind_of_unknown_it_is() {
        for (bytes, name, expected) in UNDATED {
            let facts = probe_fixture(bytes);
            assert_eq!(facts.taken_ns, None, "{name} invents no instant");
            assert_eq!(facts.taken_reason, Some(expected), "{name} names its kind");
        }
    }

    /// HEIC has no dimension reader in this build, and that changes nothing
    /// about the date it carries: one fact never borrows the other's unknown.
    #[test]
    fn an_unknown_size_does_not_make_an_unknown_date() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/exif.heic"));
        assert_eq!((facts.width, facts.height), (None, None));
        assert_eq!(
            facts.problem.as_deref(),
            Some("no header reader for image format heif")
        );
        assert_eq!(facts.taken_ns, Some(1_689_191_647_123_000_000));
        assert_eq!(facts.taken_reason, None);
    }

    /// The one case where the file, not its EXIF, is the honest reason: bytes no
    /// reader could parse say nothing about any date inside them.
    #[test]
    fn an_unreadable_file_is_its_own_reason() {
        let facts = probe_fixture(include_bytes!("../../../tests/fixtures/pixel.webp"));
        assert_eq!((facts.width, facts.height), (None, None));
        assert_eq!(facts.taken_ns, None);
        assert_eq!(facts.taken_reason, Some(TakenReason::UnreadableFile));
        assert!(facts
            .problem
            .as_deref()
            .is_some_and(|problem| problem.starts_with("unreadable image header: ")));
    }

    /// The forms the value itself takes. `2023:07:12 20:54:07` with no zone and
    /// `...T20:54:07Z` are one rule's two spellings, and `21:54:07+01:00` is the
    /// same instant written in a zone that was named.
    #[test]
    fn the_date_forms_files_use_all_name_the_same_moment() {
        let same_instant = 1_689_195_247_000_000_000;
        for raw in [
            b"2023:07:12 20:54:07".as_slice(),
            b"2023-07-12T20:54:07Z".as_slice(),
            b"2023-07-12T20:54:07z".as_slice(),
            b"2023-07-12T21:54:07+01:00".as_slice(),
            b"2023-07-12T21:54:07+0100".as_slice(),
            b"2023:07:12 20:54:07\x00".as_slice(),
            b" 2023:07:12 20:54:07 ".as_slice(),
        ] {
            let text = String::from_utf8_lossy(raw).into_owned();
            assert_eq!(moment(raw, None, None), Some(same_instant), "{text:?}");
        }
        // A date with no time of day is midnight under the same rule, and a
        // time to the minute needs no seconds field to be a moment.
        assert_eq!(
            moment(b"2023:07:12", None, None),
            Some(1_689_120_000_000_000_000)
        );
        assert_eq!(
            moment(b"2023-07-12 20:54", None, None),
            Some(1_689_195_240_000_000_000)
        );
    }

    /// The companions belong to the field they came with: a fraction the value
    /// itself carries wins, and a zone named by neither rule keeps the wall clock
    /// rather than discarding the date.
    #[test]
    fn companion_tags_refine_the_value_they_came_with() {
        assert_eq!(
            moment(b"2023:07:12 20:54:07", Some(b"123"), None),
            Some(1_689_195_247_123_000_000)
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07", Some(b".5"), None),
            Some(1_689_195_247_500_000_000)
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07", None, Some(b"-05:00")),
            Some(1_689_213_247_000_000_000)
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07", None, Some(b"junk")),
            Some(1_689_195_247_000_000_000),
            "a junk offset degrades to the zone-less rule"
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07", Some(b"junk"), None),
            Some(1_689_195_247_000_000_000),
            "a junk fraction does the same"
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07.25", Some(b"123"), None),
            Some(1_689_195_247_250_000_000),
            "the value's own fraction is the nearer answer"
        );
    }

    /// An offset that parses but names no zone is refused, not applied. The
    /// parser accepted the `±HH:MM` shape for any two digits and then moved
    /// the instant by up to ±6.7 days, so a corrupt or hand-edited
    /// `OffsetTimeOriginal` was a wrong date rather than an absent one
    /// (RW48 Medium-1). A *tag* refused this way leaves the date in the
    /// zone-less rule — the date still stands; a *value* that carries the bad
    /// zone names no moment, because its zone is part of the text.
    #[test]
    fn an_offset_no_zone_could_name_is_refused() {
        // ±14:00 is the furthest any zone stands from UTC, and both apply.
        assert_eq!(
            moment(b"2023:07:12 20:54:07", None, Some(b"+14:00")),
            Some(1_689_144_847_000_000_000)
        );
        assert_eq!(
            moment(b"2023:07:12 20:54:07", None, Some(b"-14:00")),
            Some(1_689_245_647_000_000_000)
        );
        let zone_less = Some(1_689_195_247_000_000_000);
        for offset in [
            b"+14:01".as_slice(),
            b"+23:59".as_slice(),
            b"+24:00".as_slice(),
            b"+25:00".as_slice(),
            b"-25:00".as_slice(),
            b"+60:00".as_slice(),
            b"+99:99".as_slice(),
            b"-99:99".as_slice(),
        ] {
            assert_eq!(
                moment(b"2023:07:12 20:54:07", None, Some(offset)),
                zone_less,
                "the tag {} is refused, so the date stands without it",
                String::from_utf8_lossy(offset)
            );
        }
        for raw in [
            b"2023:07:12 20:54:07+14:01".as_slice(),
            b"2023:07:12 20:54:07+99:99".as_slice(),
        ] {
            assert_eq!(
                moment(raw, None, None),
                None,
                "a value whose own zone is refused names no moment"
            );
        }
    }

    /// Nothing is invented: text that names no moment is `None`, whether it is
    /// the classic all-zero date, a calendar that has no such month, a year no
    /// epoch nanosecond can hold, or a zone spelled too short to trust.
    #[test]
    fn text_that_names_no_moment_stays_an_unknown() {
        for raw in [
            b"".as_slice(),
            b"junk".as_slice(),
            b"0000:00:00 00:00:00".as_slice(),
            b"2023:13:01 00:00:00".as_slice(),
            b"2023:07:32 00:00:00".as_slice(),
            b"23:07:12 20:54:07".as_slice(),
            b"9999:12:31 23:59:59".as_slice(),
            b"2023:07:12 20:54:07 UTC".as_slice(),
            b"2023:07:12 20:54:07+1".as_slice(),
            b"2023:07:12T20-54-07".as_slice(),
        ] {
            assert_eq!(moment(raw, None, None), None, "{raw:?} names nothing");
        }
        // A year four digits cannot be is not a date with a short spelling.
        assert_eq!(moment(b"20231:07:12 20:54:07", None, None), None);
    }
}
