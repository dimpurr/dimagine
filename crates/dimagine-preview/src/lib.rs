//! Content-addressed image previews for dimagine libraries.
//!
//! Preview files are derived cache entries; originals are opened read-only and
//! are never changed. Colour profiles are detected, but no colour transform is
//! applied, so each rendition reports ColourStatus::NotConverted when an
//! embedded ICC profile is present.

use std::{
    fs::{self, File},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
};

use image::{imageops, DynamicImage, GenericImageView, ImageEncoder, ImageFormat, ImageReader};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

const THUMB_EDGE: u32 = 400;
const VIEW_EDGE: u32 = 1568;
const VIEW_AREA: u64 = 1_150_000;
const DEFAULT_PIXEL_LIMIT: u64 = 200_000_000;

/// A lowercase hexadecimal SHA-256 digest of the original file bytes.
pub type Hash = String;

/// The requested preview size class.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Grid and phone preview, long edge at most 400 pixels.
    Thumb,
    /// Detail and vision-model preview, size limited by edge and area.
    View,
}

/// The encoded output format.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RenditionFormat {
    /// JPEG, encoded at quality 88.
    Jpeg,
    /// PNG, used when the decoded first frame contains transparency.
    Png,
}

/// Colour management outcome for the source image.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColourStatus {
    /// No embedded ICC marker was found in the source bytes.
    NoProfile,
    /// ICC profile was detected but pixels were kept unchanged.
    NotConverted,
}

/// A generated or already cached preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Rendition {
    /// Preview size class.
    pub kind: Kind,
    /// Output pixel width after orientation is applied.
    pub width: u32,
    /// Output pixel height after orientation is applied.
    pub height: u32,
    /// Encoded format.
    pub format: RenditionFormat,
    /// Full path to the cache file.
    pub path: PathBuf,
    /// Colour management status for this source.
    pub colour_status: ColourStatus,
}

/// Options for preview generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Maximum allowed source pixels. Defaults to 200 million.
    pub pixel_limit: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pixel_limit: DEFAULT_PIXEL_LIMIT,
        }
    }
}

/// Errors encountered while hashing, decoding, or writing a preview.
#[derive(Debug, Error)]
pub enum PreviewError {
    /// Source could not be read.
    #[error("cannot read image: {0}")]
    Io(#[from] io::Error),
    /// The file format is unsupported by enabled decoders.
    #[error("unsupported image format: {0}")]
    Unsupported(String),
    /// The file was recognized as an image but could not be decoded.
    #[error("corrupt or undecodable image: {0}")]
    Decode(String),
    /// Source exceeds the configured pixel limit.
    #[error("image dimensions {width}x{height} exceed pixel limit {limit}")]
    PixelLimit { width: u32, height: u32, limit: u64 },
}

/// Hash a file's bytes with SHA-256.
pub fn sha256_file(path: impl AsRef<Path>) -> Result<Hash, PreviewError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Plan output dimensions without upscaling, preserving the source aspect ratio.
/// Integer dimensions are rounded down to guarantee every size limit holds.
pub fn plan(width: u32, height: u32, kind: Kind) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let (max_edge, max_area) = match kind {
        Kind::Thumb => (THUMB_EDGE, None),
        Kind::View => (VIEW_EDGE, Some(VIEW_AREA)),
    };
    let mut scale = 1.0_f64.min(f64::from(max_edge) / f64::from(width.max(height)));
    if let Some(area) = max_area {
        scale = scale.min((area as f64 / (u64::from(width) * u64::from(height)) as f64).sqrt());
    }
    let mut out_w = (f64::from(width) * scale).floor().max(1.0) as u32;
    let mut out_h = (f64::from(height) * scale).floor().max(1.0) as u32;
    while out_w.max(out_h) > max_edge
        || max_area.is_some_and(|a| u64::from(out_w) * u64::from(out_h) > a)
    {
        if f64::from(out_w) / f64::from(width) >= f64::from(out_h) / f64::from(height) {
            out_w -= 1;
        } else {
            out_h -= 1;
        }
    }
    (out_w, out_h)
}

/// Create missing requested previews with the default 200 MP decode limit.
pub fn ensure(
    library_root: impl AsRef<Path>,
    image_path: impl AsRef<Path>,
    kinds: &[Kind],
) -> Result<Vec<Rendition>, PreviewError> {
    ensure_with_options(library_root, image_path, kinds, Options::default())
}

/// Create missing requested previews with an explicit source pixel limit.
pub fn ensure_with_options(
    library_root: impl AsRef<Path>,
    image_path: impl AsRef<Path>,
    kinds: &[Kind],
    options: Options,
) -> Result<Vec<Rendition>, PreviewError> {
    let image_path = image_path.as_ref();
    let bytes = fs::read(image_path)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let format = format_from_bytes(&bytes)
        .ok_or_else(|| PreviewError::Unsupported(extension_or_unknown(image_path)))?;
    if !supported(format) {
        return Err(PreviewError::Unsupported(format!("{format:?}")));
    }
    let reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let dimensions = reader
        .into_dimensions()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    check_limit(dimensions.0, dimensions.1, options.pixel_limit)?;
    let mut image = ImageReader::with_format(Cursor::new(&bytes), format)
        .decode()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    apply_orientation(&mut image, &bytes);
    let (width, height) = image.dimensions();
    let transparent = has_transparency(&image);
    let rendition_format = if transparent {
        RenditionFormat::Png
    } else {
        RenditionFormat::Jpeg
    };
    let colour_status = if has_icc_marker(&bytes) {
        ColourStatus::NotConverted
    } else {
        ColourStatus::NoProfile
    };
    let mut output = Vec::new();
    for &kind in kinds {
        let (out_w, out_h) = plan(width, height, kind);
        let path = cache_path(library_root.as_ref(), &hash, kind, rendition_format);
        if !path.exists() {
            let rendered = if (out_w, out_h) == (width, height) {
                image.clone()
            } else {
                image.resize_exact(out_w, out_h, imageops::FilterType::Lanczos3)
            };
            write_atomic(&path, &rendered, rendition_format)?;
        }
        output.push(Rendition {
            kind,
            width: out_w,
            height: out_h,
            format: rendition_format,
            path,
            colour_status,
        });
    }
    Ok(output)
}

fn check_limit(width: u32, height: u32, limit: u64) -> Result<(), PreviewError> {
    if u64::from(width) * u64::from(height) > limit {
        return Err(PreviewError::PixelLimit {
            width,
            height,
            limit,
        });
    }
    Ok(())
}

fn format_from_bytes(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok()
}

fn extension_or_unknown(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("unknown")
        .to_ascii_lowercase()
}

fn supported(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp
            | ImageFormat::Tiff
    )
}

fn cache_path(root: &Path, hash: &str, kind: Kind, format: RenditionFormat) -> PathBuf {
    let ext = match format {
        RenditionFormat::Jpeg => "jpg",
        RenditionFormat::Png => "png",
    };
    let kind = match kind {
        Kind::Thumb => "thumb",
        Kind::View => "view",
    };
    root.join(".dimagine/cache/previews")
        .join(&hash[..2])
        .join(format!("{hash}-{kind}.{ext}"))
}

fn write_atomic(
    path: &Path,
    image: &DynamicImage,
    format: RenditionFormat,
) -> Result<(), PreviewError> {
    let parent = path.parent().expect("cache path has a parent");
    fs::create_dir_all(parent)?;
    let mut temp = Builder::new().prefix(".preview-").tempfile_in(parent)?;
    match format {
        RenditionFormat::Jpeg => {
            let rgb = image.to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut temp, 88)
                .encode_image(&DynamicImage::ImageRgb8(rgb))
                .map_err(|e| PreviewError::Decode(e.to_string()))?;
        }
        RenditionFormat::Png => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut temp)
                .write_image(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| PreviewError::Decode(e.to_string()))?;
        }
    }
    temp.flush()?;
    temp.as_file().sync_all()?;
    fs::rename(temp.path(), path)?;
    Ok(())
}

fn has_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageLumaA8(img) => img.pixels().any(|p| p.0[1] < 255),
        DynamicImage::ImageLumaA16(img) => img.pixels().any(|p| p.0[1] < u16::MAX),
        DynamicImage::ImageRgba8(img) => img.pixels().any(|p| p.0[3] < 255),
        DynamicImage::ImageRgba16(img) => img.pixels().any(|p| p.0[3] < u16::MAX),
        DynamicImage::ImageRgba32F(img) => img.pixels().any(|p| p.0[3] < 1.0),
        _ => false,
    }
}

fn has_icc_marker(bytes: &[u8]) -> bool {
    [b"ICC_PROFILE".as_slice(), b"iCCP", b"ICCP"]
        .iter()
        .any(|marker| bytes.windows(marker.len()).any(|w| w == *marker))
}

fn apply_orientation(image: &mut DynamicImage, bytes: &[u8]) {
    let orientation = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|exif| {
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1);
    *image = match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate90().flipv(),
        8 => image.rotate270(),
        _ => image.clone(),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    #[test]
    fn plan_table_obeys_limits_and_never_upscales() {
        let cases = [
            (1500, 1000, Kind::Thumb, (400, 266)),
            (1920, 1080, Kind::Thumb, (400, 225)),
            (1920, 1080, Kind::View, (1429, 804)),
            (1000, 1000, Kind::View, (1000, 1000)),
            (100, 400, Kind::Thumb, (100, 400)),
            (100, 1200, Kind::Thumb, (33, 400)),
            (100, 1200, Kind::View, (100, 1200)),
            (20, 10, Kind::Thumb, (20, 10)),
        ];
        for (w, h, kind, expected) in cases {
            assert_eq!(plan(w, h, kind), expected, "{w}x{h} {kind:?}");
        }
        let (w, h) = plan(2000, 2000, Kind::View);
        assert!(w.max(h) <= VIEW_EDGE && u64::from(w) * u64::from(h) <= VIEW_AREA);
    }

    fn temp_library() -> tempfile::TempDir {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        Builder::new()
            .prefix("preview-test-")
            .tempdir_in(target)
            .expect("target temp dir")
    }

    #[test]
    fn transparent_png_produces_png_and_cache_hit_preserves_mtime() {
        let dir = temp_library();
        let source = dir.path().join("source.png");
        let pixels = ImageBuffer::from_fn(20, 10, |x, _| {
            if x == 0 {
                Rgba([1_u8, 2, 3, 100])
            } else {
                Rgba([1_u8, 2, 3, 255])
            }
        });
        pixels.save(&source).expect("save source");
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).expect("preview");
        assert_eq!(first[0].format, RenditionFormat::Png);
        let modified = fs::metadata(&first[0].path).unwrap().modified().unwrap();
        let second = ensure(dir.path(), &source, &[Kind::Thumb]).expect("cache hit");
        assert_eq!(
            fs::metadata(&second[0].path).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn identical_bytes_share_cache_entry() {
        let dir = temp_library();
        let image = ImageBuffer::from_pixel(8, 8, Rgb([10_u8, 20, 30]));
        let one = dir.path().join("one.bmp");
        let two = dir.path().join("two.bmp");
        image.save(&one).unwrap();
        fs::copy(&one, &two).unwrap();
        let a = ensure(dir.path(), one, &[Kind::View]).unwrap();
        let b = ensure(dir.path(), two, &[Kind::View]).unwrap();
        assert_eq!(a[0].path, b[0].path);
    }

    #[test]
    fn corrupt_image_is_a_typed_error() {
        let dir = temp_library();
        let path = dir.path().join("bad.png");
        fs::write(&path, b"\x89PNG\r\n\x1a\ncorrupt").unwrap();
        assert!(matches!(
            ensure(dir.path(), path, &[Kind::Thumb]),
            Err(PreviewError::Decode(_))
        ));
    }

    #[test]
    fn exif_rotated_jpeg_dimensions_are_oriented_before_planning() {
        let dir = temp_library();
        let source = dir.path().join("rotated.jpg");
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&DynamicImage::ImageRgb8(ImageBuffer::from_pixel(
                20,
                10,
                Rgb([20_u8, 40, 60]),
            )))
            .unwrap();
        let exif_payload = [
            b"Exif\0\0".as_slice(),
            b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0",
        ]
        .concat();
        let segment_len = (exif_payload.len() + 2) as u16;
        let mut with_exif = Vec::new();
        with_exif.extend_from_slice(&jpeg[..2]);
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&segment_len.to_be_bytes());
        with_exif.extend_from_slice(&exif_payload);
        with_exif.extend_from_slice(&jpeg[2..]);
        fs::write(&source, with_exif).unwrap();
        let renditions = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert_eq!((renditions[0].width, renditions[0].height), (10, 20));
    }

    #[test]
    fn pixel_limit_is_typed() {
        let dir = temp_library();
        let source = dir.path().join("small.bmp");
        ImageBuffer::from_pixel(4, 4, Rgb([0_u8, 0, 0]))
            .save(&source)
            .unwrap();
        let result = ensure_with_options(
            dir.path(),
            source,
            &[Kind::Thumb],
            Options { pixel_limit: 4 },
        );
        assert!(matches!(result, Err(PreviewError::PixelLimit { .. })));
    }
}
