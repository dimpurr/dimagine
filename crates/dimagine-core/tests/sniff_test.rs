//! Magic-byte sniffing against the committed fixtures (FORMAT §2.1).

mod support;

use dimagine_core::format::FormatFamily;
use dimagine_core::sniff::{self, DetectedFormat};
use support::*;

#[test]
fn detects_every_fixture_format() {
    let cases = [
        (jpg(), DetectedFormat::Jpeg),
        (png(), DetectedFormat::Png),
        (gif(), DetectedFormat::Gif),
        (bmp(), DetectedFormat::Bmp),
        (tiff(), DetectedFormat::Tiff),
        (webp(), DetectedFormat::Webp),
        (avif(), DetectedFormat::Avif),
        (heic(), DetectedFormat::Heif),
    ];
    for (bytes, expected) in cases {
        assert_eq!(sniff::detect(&bytes), expected);
        assert_eq!(
            expected.family().map(FormatFamily::as_str),
            Some(expected.as_str())
        );
    }
}

#[test]
fn unknown_for_text_and_empty() {
    assert_eq!(
        sniff::detect(b"just some text, not an image"),
        DetectedFormat::Unknown
    );
    assert_eq!(sniff::detect(b""), DetectedFormat::Unknown);
    // A few bytes of a jpeg are not enough.
    assert_eq!(sniff::detect(&jpg()[..2]), DetectedFormat::Unknown);
}

#[test]
fn ftyp_boxes_of_non_image_brands_are_unknown() {
    // An `ftyp` box that is not an image brand (e.g. a video) is unknown.
    let mut mov = vec![0, 0, 0, 24];
    mov.extend_from_slice(b"ftypqt  \0\0\0\0qt  ");
    assert_eq!(sniff::detect(&mov), DetectedFormat::Unknown);
}

#[test]
fn heif_family_covers_heic_brand() {
    // jpeg expects the jpeg family; heic content reported sniffed heif.
    assert_eq!(sniff::detect(&heic()).family(), Some(FormatFamily::Heif));
    assert_eq!(
        sniff::detect(&jpg()).family().unwrap(),
        FormatFamily::Jpeg,
        "jpeg content must be the jpeg family"
    );
}
