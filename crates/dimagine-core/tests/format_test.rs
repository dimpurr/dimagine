//! FORMAT.md constants and classification rules.

mod support;

use dimagine_core::format::{
    banned_characters, file_extension, file_stem, ignore_reason, is_image_extension, FormatFamily,
    IgnoreReason,
};
use dimagine_core::library::classify;
use dimagine_core::library::FileClass;

#[test]
fn image_extensions_case_insensitive() {
    for ext in [
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp",
    ] {
        assert!(is_image_extension(ext), "{ext}");
        assert!(is_image_extension(&ext.to_ascii_uppercase()), "{ext} upper");
        let upper = ext.to_ascii_uppercase();
        assert_eq!(
            FormatFamily::from_extension(&upper),
            FormatFamily::from_extension(ext)
        );
    }
    assert!(!is_image_extension("md"));
    assert!(!is_image_extension("json"));
    assert!(!is_image_extension("txt"));
    assert!(!is_image_extension(""));
    assert!(!is_image_extension("mp4"));
}

#[test]
fn jpg_and_jpeg_are_one_family() {
    assert_eq!(
        FormatFamily::from_extension("jpg"),
        FormatFamily::from_extension("jpeg")
    );
    assert_eq!(
        FormatFamily::from_extension("tif"),
        FormatFamily::from_extension("tiff")
    );
    assert_eq!(
        FormatFamily::from_extension("heic"),
        FormatFamily::from_extension("heif")
    );
    assert_eq!(
        FormatFamily::from_extension("heic"),
        Some(FormatFamily::Heif)
    );
    assert_eq!(FormatFamily::from_extension("cr2"), None);
}

#[test]
fn extension_extraction() {
    assert_eq!(file_extension("girl.jpg"), Some("jpg"));
    assert_eq!(file_extension("girl.tar.gz"), Some("gz"));
    assert_eq!(file_extension("noext"), None);
    assert_eq!(file_extension(".bashrc"), None);
    assert_eq!(file_extension("endswith."), None);
    assert_eq!(file_stem("girl.jpg"), "girl");
    assert_eq!(file_stem("a.b.png"), "a.b");
    assert_eq!(file_stem("noext"), "noext");
}

#[test]
fn banned_characters_in_file_names() {
    assert!(banned_characters("girl.jpg").is_empty());
    assert_eq!(banned_characters("shot[1].jpg"), vec!['[', ']']);
    assert_eq!(banned_characters("a#b^c|d.png"), vec!['#', '^', '|']);
}

#[test]
fn ignore_rules_match_format_2_2() {
    assert_eq!(ignore_reason(".hidden"), Some(IgnoreReason::Hidden));
    assert_eq!(ignore_reason(".dimagine"), Some(IgnoreReason::Hidden));
    assert_eq!(
        ignore_reason("._girl.jpg"),
        Some(IgnoreReason::ResourceFork)
    );
    assert_eq!(ignore_reason("thumbs.db"), Some(IgnoreReason::OsMetadata));
    assert_eq!(ignore_reason("desktop.ini"), Some(IgnoreReason::OsMetadata));
    assert_eq!(ignore_reason("Thumbs.db"), Some(IgnoreReason::OsMetadata));
    assert_eq!(ignore_reason("Desktop.INI"), Some(IgnoreReason::OsMetadata));
    assert_eq!(ignore_reason("girl.jpg"), None);
    assert_eq!(ignore_reason("folder"), None);
    assert_eq!(ignore_reason(".env.local"), Some(IgnoreReason::Hidden));
}

#[test]
fn file_classification() {
    assert_eq!(classify("girl.jpg"), FileClass::Image);
    assert_eq!(classify("GIRL.JPG"), FileClass::Image);
    assert_eq!(classify("girl.jpg.md"), FileClass::ImageNote);
    assert_eq!(classify("girl.JPG.MD"), FileClass::ImageNote);
    assert_eq!(classify("ideas.md"), FileClass::Note);
    assert_eq!(classify("board.canvas"), FileClass::Canvas);
    assert_eq!(classify("girl.jpg.eagle.json"), FileClass::Raw);
    assert_eq!(classify("catalog.json"), FileClass::Other);
    // `girl.jpg.json` lacks the `<source>` segment, so it is not raw pattern.
    assert_eq!(classify("girl.jpg.json"), FileClass::Other);
    assert_eq!(classify("girl.jpg.eagle"), FileClass::Other);
    assert_eq!(classify("movie.mp4"), FileClass::Other);
    assert_eq!(classify("README"), FileClass::Other);
    // A note about an image note-styled name must stay a plain note.
    assert_eq!(classify("delta.canvas.md"), FileClass::Note);
}
