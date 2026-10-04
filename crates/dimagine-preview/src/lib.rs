//! Content-addressed image previews for dimagine libraries.
//!
//! Preview files are derived cache entries; originals are opened read-only and
//! are never changed. Embedded colour profiles are transformed to sRGB.

use std::{
    fs::{self, File},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
};

use image::{
    imageops, AnimationDecoder, DynamicImage, GenericImageView, ImageDecoder, ImageEncoder,
    ImageError, ImageFormat, ImageReader, Limits,
};
use lcms2::{ColorSpaceSignature, Intent, PixelFormat, Profile, Transform};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

const THUMB_EDGE: u32 = 400;
const VIEW_EDGE: u32 = 1568;
const VIEW_AREA: u64 = 1_150_000;
const DEFAULT_PIXEL_LIMIT: u64 = 200_000_000;
const MAX_ENCODED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;

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
    /// No embedded ICC profile was exposed by the image decoder.
    NoProfile,
    /// An embedded ICC profile was converted to sRGB.
    Srgb,
    /// Embedded ICC profile could not be interpreted by Little CMS.
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
    /// Decoder refused the image because it exceeded a resource limit.
    #[error("image resource limit exceeded: {0}")]
    ResourceLimit(String),
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
    let metadata = fs::metadata(image_path)?;
    if metadata.len() > MAX_ENCODED_BYTES {
        return Err(PreviewError::ResourceLimit(format!(
            "encoded image exceeds {MAX_ENCODED_BYTES} bytes"
        )));
    }
    let bytes = fs::read(image_path)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let format = format_from_bytes(&bytes)
        .ok_or_else(|| PreviewError::Unsupported(extension_or_unknown(image_path)))?;
    if !supported(format) {
        return Err(PreviewError::Unsupported(format!("{format:?}")));
    }
    if format == ImageFormat::WebP {
        check_webp_frame_limits(&bytes, options.pixel_limit)?;
    }
    let reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let dimensions = reader
        .into_dimensions()
        .map_err(|e| PreviewError::Decode(e.to_string()))?;
    check_limit(dimensions.0, dimensions.1, options.pixel_limit)?;
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut decode_limits = Limits::default();
    decode_limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(decode_limits);
    let mut decoder = reader.into_decoder().map_err(map_image_error)?;
    let icc = decoder.icc_profile().map_err(map_image_error)?;
    let profiled_cmyk = format == ImageFormat::Jpeg
        && icc.as_deref().is_some_and(|profile| {
            Profile::new_icc(profile)
                .is_ok_and(|p| p.color_space() == ColorSpaceSignature::CmykData)
        });
    let (mut image, converted) = if profiled_cmyk {
        match decode_profiled_cmyk_jpeg(&bytes, icc.as_deref().expect("profile checked")) {
            Ok(cmyk_srgb) => (cmyk_srgb, true),
            Err(_) => (
                DynamicImage::from_decoder(decoder).map_err(map_image_error)?,
                false,
            ),
        }
    } else {
        let mut image = DynamicImage::from_decoder(decoder).map_err(map_image_error)?;
        let converted = icc
            .as_deref()
            .is_some_and(|profile| convert_to_srgb(&mut image, profile).is_ok());
        (image, converted)
    };
    if format == ImageFormat::Png {
        let mut png_reader =
            image::codecs::png::PngDecoder::new(Cursor::new(&bytes)).map_err(map_image_error)?;
        let mut png_limits = Limits::default();
        png_limits.max_alloc = Some(MAX_DECODE_BYTES);
        png_reader.set_limits(png_limits).map_err(map_image_error)?;
        if png_reader.is_apng().map_err(map_image_error)? {
            let mut frames = png_reader.apng().map_err(map_image_error)?.into_frames();
            let frame = frames
                .next()
                .ok_or_else(|| PreviewError::Decode("APNG has no animation frames".into()))?
                .map_err(map_image_error)?;
            image = DynamicImage::ImageRgba8(frame.into_buffer());
        }
    }
    apply_orientation(&mut image, &bytes);
    let (width, height) = image.dimensions();
    let transparent = has_transparency(&image);
    let rendition_format = if transparent {
        RenditionFormat::Png
    } else {
        RenditionFormat::Jpeg
    };
    let colour_status = if converted {
        ColourStatus::Srgb
    } else if icc.is_some() {
        ColourStatus::NotConverted
    } else {
        ColourStatus::NoProfile
    };
    let mut output = Vec::new();
    for &kind in kinds {
        let (out_w, out_h) = plan(width, height, kind);
        let path = cache_path(library_root.as_ref(), &hash, kind, rendition_format);
        ensure_cache_dirs(library_root.as_ref(), &path)?;
        if !valid_cache_hit(&path, (out_w, out_h), rendition_format) {
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

fn map_image_error(error: ImageError) -> PreviewError {
    match error {
        ImageError::Unsupported(e) => PreviewError::Unsupported(e.to_string()),
        ImageError::Limits(e) => PreviewError::ResourceLimit(e.to_string()),
        ImageError::IoError(e) => PreviewError::Io(e),
        other => PreviewError::Decode(other.to_string()),
    }
}

fn convert_to_srgb(image: &mut DynamicImage, icc: &[u8]) -> Result<(), String> {
    let input = Profile::new_icc(icc).map_err(|e| e.to_string())?;
    let output = Profile::new_srgb();
    let rgb = image.to_rgb8();
    let source = rgb.as_raw();
    let mut converted = vec![0; source.len()];
    let transform = Transform::new(
        &input,
        PixelFormat::RGB_8,
        &output,
        PixelFormat::RGB_8,
        Intent::Perceptual,
    )
    .map_err(|e| e.to_string())?;
    transform.transform_pixels(source, &mut converted);
    let result = image::RgbImage::from_raw(rgb.width(), rgb.height(), converted)
        .ok_or("invalid transformed pixels")?;
    *image = DynamicImage::ImageRgb8(result);
    Ok(())
}

fn decode_profiled_cmyk_jpeg(bytes: &[u8], icc: &[u8]) -> Result<DynamicImage, String> {
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        ZCursor::new(bytes),
        DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::CMYK),
    );
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("JPEG header missing")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let output_size = decoder
        .output_buffer_size()
        .ok_or("JPEG decoded size overflow")?;
    if output_size as u64 > MAX_DECODE_BYTES
        || u64::from(width) * u64::from(height) * 7 > MAX_DECODE_BYTES
    {
        return Err("JPEG decoded buffer exceeds memory limit".into());
    }
    let cmyk = decoder.decode().map_err(|e| e.to_string())?;
    let input = Profile::new_icc(icc).map_err(|e| e.to_string())?;
    let output = Profile::new_srgb();
    let mut rgb = vec![0; (u64::from(width) * u64::from(height) * 3) as usize];
    let transform = Transform::new(
        &input,
        PixelFormat::CMYK_8,
        &output,
        PixelFormat::RGB_8,
        Intent::Perceptual,
    )
    .map_err(|e| e.to_string())?;
    transform.transform_pixels(&cmyk, &mut rgb);
    image::RgbImage::from_raw(width, height, rgb)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| "invalid transformed CMYK pixels".into())
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

fn check_webp_frame_limits(bytes: &[u8], limit: u64) -> Result<(), PreviewError> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Ok(());
    }
    let mut offset = 12_usize;
    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let tag = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let data_start = offset + 8;
        let Some(data_end) = data_start.checked_add(length) else {
            break;
        };
        if data_end > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_end];
        let dimensions = if tag == b"VP8 " && data.len() >= 10 && data[3..6] == [0x9d, 0x01, 0x2a] {
            Some((
                u16::from_le_bytes([data[6], data[7]]) & 0x3fff,
                u16::from_le_bytes([data[8], data[9]]) & 0x3fff,
            ))
        } else if tag == b"VP8L" && data.len() >= 5 && data[0] == 0x2f {
            let w = 1 + u32::from(data[1]) + (u32::from(data[2] & 0x3f) << 8);
            let h = 1
                + (u32::from(data[2] >> 6))
                + (u32::from(data[3]) << 2)
                + (u32::from(data[4] & 0x0f) << 10);
            Some((w as u16, h as u16))
        } else {
            None
        };
        if let Some((width, height)) = dimensions {
            check_limit(u32::from(width), u32::from(height), limit)?;
        }
        offset = data_end + (length & 1);
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

fn ensure_cache_dirs(root: &Path, file: &Path) -> Result<(), PreviewError> {
    let root = fs::canonicalize(root)?;
    let shard = file
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned();
    let mut current = root.clone();
    for component in [".dimagine", "cache", "previews", shard.as_str()] {
        if component.is_empty() {
            continue;
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(PreviewError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cache path contains a symlink or non-directory",
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
        let resolved = fs::canonicalize(&current)?;
        if !resolved.starts_with(&root) {
            return Err(PreviewError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache path resolves outside the library",
            )));
        }
        current = resolved;
    }
    Ok(())
}

fn valid_cache_hit(path: &Path, expected: (u32, u32), format: RenditionFormat) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    let Ok(reader) = ImageReader::open(path) else {
        return false;
    };
    let expected_format = match format {
        RenditionFormat::Jpeg => ImageFormat::Jpeg,
        RenditionFormat::Png => ImageFormat::Png,
    };
    let mut reader = reader;
    reader.set_format(expected_format);
    let Ok(reader) = reader.into_dimensions() else {
        return false;
    };
    reader == expected
}

fn write_atomic(
    path: &Path,
    image: &DynamicImage,
    format: RenditionFormat,
) -> Result<(), PreviewError> {
    let parent = path.parent().expect("cache path has a parent");
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
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || meta.is_file() {
            #[cfg(windows)]
            fs::remove_file(path)?;
        } else if meta.is_dir() {
            fs::remove_dir_all(path)?;
        }
    }
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

    #[test]
    fn oversized_encoded_image_is_rejected_before_reading_payload() {
        let dir = temp_library();
        let source = dir.path().join("large.bmp");
        fs::write(&source, b"BM").unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(MAX_ENCODED_BYTES + 1)
            .unwrap();
        assert!(matches!(
            ensure(dir.path(), source, &[Kind::Thumb]),
            Err(PreviewError::ResourceLimit(_))
        ));
    }

    #[test]
    fn webp_embedded_frame_dimensions_are_checked_before_decode() {
        let mut data = b"RIFF\0\0\0\0WEBP".to_vec();
        data.extend_from_slice(b"VP8X");
        data.extend_from_slice(&10_u32.to_le_bytes());
        data.extend_from_slice(&[0; 10]);
        data.extend_from_slice(b"VP8 ");
        data.extend_from_slice(&10_u32.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0x9d, 1, 0x2a, 0xff, 0x3f, 0xff, 0x3f]);
        let riff_len = (data.len() - 8) as u32;
        data[4..8].copy_from_slice(&riff_len.to_le_bytes());
        assert!(matches!(
            check_webp_frame_limits(&data, 1),
            Err(PreviewError::PixelLimit {
                width: 16_383,
                height: 16_383,
                ..
            })
        ));
    }

    #[test]
    fn decoder_limit_errors_keep_their_resource_category() {
        let error = ImageError::Limits(image::error::LimitError::from_kind(
            image::error::LimitErrorKind::InsufficientMemory,
        ));
        assert!(matches!(
            map_image_error(error),
            PreviewError::ResourceLimit(_)
        ));
        let unsupported =
            ImageError::Unsupported(image::error::UnsupportedError::from_format_and_kind(
                ImageFormat::Png.into(),
                image::error::UnsupportedErrorKind::GenericFeature("fixture".into()),
            ));
        assert!(matches!(
            map_image_error(unsupported),
            PreviewError::Unsupported(_)
        ));
    }

    #[test]
    fn apng_uses_the_first_animation_frame_instead_of_default_image() {
        let dir = temp_library();
        let source = dir.path().join("poster-apng.png");
        let file = File::create(&source).unwrap();
        let mut encoder = png::Encoder::new(file, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(1, 0).unwrap();
        encoder.set_sep_def_img(true).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[255, 0, 0, 255].repeat(4))
            .unwrap();
        writer
            .write_image_data(&[0, 0, 255, 255].repeat(4))
            .unwrap();
        drop(writer);
        let result = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        let decoded = image::open(&result[0].path).unwrap().to_rgba8();
        assert!(
            decoded.pixels().all(|p| p.0[2] > p.0[0]),
            "first animation frame should be blue"
        );
    }

    #[test]
    fn embedded_icc_profile_is_converted_to_srgb() {
        let dir = temp_library();
        let source = dir.path().join("profiled.png");
        let profile = Profile::new_srgb();
        let icc = profile.icc().unwrap();
        let file = File::create(&source).unwrap();
        let image = ImageBuffer::from_pixel(1, 1, Rgb([30_u8, 80, 160]));
        let mut encoder = image::codecs::png::PngEncoder::new(file);
        encoder.set_icc_profile(icc).unwrap();
        encoder
            .write_image(&image, 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let rendition = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert_eq!(rendition[0].colour_status, ColourStatus::Srgb);
    }

    #[cfg(unix)]
    #[test]
    fn cache_directory_symlink_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let outside = temp_library();
        fs::create_dir_all(dir.path().join(".dimagine/cache")).unwrap();
        symlink(outside.path(), dir.path().join(".dimagine/cache/previews")).unwrap();
        assert!(matches!(
            ensure(dir.path(), source, &[Kind::Thumb]),
            Err(PreviewError::Io(_))
        ));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn invalid_cache_directory_entry_is_replaced_with_valid_rendition() {
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        fs::remove_file(&first[0].path).unwrap();
        fs::create_dir(&first[0].path).unwrap();
        fs::write(first[0].path.join("stale"), b"cache debris").unwrap();
        let repaired = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert!(valid_cache_hit(
            &repaired[0].path,
            (2, 2),
            RenditionFormat::Jpeg
        ));
    }
}
