//! Content-addressed image previews for dimagine libraries.
//!
//! Preview files are derived cache entries; originals are opened read-only and
//! are never changed. Embedded colour profiles are transformed to sRGB.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use image::{
    imageops, AnimationDecoder, DynamicImage, GenericImageView, ImageDecoder, ImageEncoder,
    ImageError, ImageFormat, ImageReader, Limits,
};
use lcms2::{ColorSpaceSignature, Intent, PixelFormat, Profile, Transform};
use serde::Serialize;
use sha2::{Digest, Sha256};
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
    let dimensions = reader.into_dimensions().map_err(map_image_error)?;
    check_limit(dimensions.0, dimensions.1, options.pixel_limit)?;
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut decode_limits = Limits::default();
    decode_limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(decode_limits);
    let mut decoder = reader.into_decoder().map_err(map_image_error)?;
    // `DynamicImage::from_decoder` allocates the output buffer without
    // consulting the decoder limits, so reserve it explicitly first.
    reserve_decode_buffer(&decoder)?;
    // image's TIFF decoder reads the ICC tag through a buffer limit scaled by
    // the size of its internal value enum, which drops the profile of small
    // TIFFs entirely; read the tag through the raw decoder instead.
    let icc = if format == ImageFormat::Tiff {
        tiff_icc_profile(&bytes)
    } else {
        decoder.icc_profile().map_err(map_image_error)?
    };
    let profiled_cmyk = matches!(format, ImageFormat::Jpeg | ImageFormat::Tiff)
        && icc.as_deref().is_some_and(|profile| {
            Profile::new_icc(profile)
                .is_ok_and(|p| p.color_space() == ColorSpaceSignature::CmykData)
        });
    let (mut image, mut converted) = if profiled_cmyk {
        let raw = match format {
            ImageFormat::Jpeg => {
                decode_profiled_cmyk_jpeg(&bytes, icc.as_deref().expect("profile checked"))
            }
            _ => decode_profiled_cmyk_tiff(&bytes, icc.as_deref().expect("profile checked")),
        };
        match raw {
            Ok(converted_image) => (converted_image, true),
            Err(_) => (
                DynamicImage::from_decoder(decoder).map_err(map_image_error)?,
                false,
            ),
        }
    } else {
        (
            DynamicImage::from_decoder(decoder).map_err(map_image_error)?,
            false,
        )
    };
    // Animated PNGs publish the first composited animation frame, so that
    // frame must be selected before the colour transform is applied.
    complete_selected_frame(&mut image, format, &bytes)?;
    apply_orientation(&mut image, &bytes);
    converted = finish_colour_conversion(&mut image, icc.as_deref(), converted);
    publish_renditions(
        library_root.as_ref(),
        &hash,
        image,
        converted,
        icc.is_some(),
        kinds,
    )
}

/// Replace `image` with the frame that will actually be published: for
/// animated PNGs this is the first composited animation frame rather than the
/// poster (default) image. The composited frame buffer is bounded too.
fn complete_selected_frame(
    image: &mut DynamicImage,
    format: ImageFormat,
    bytes: &[u8],
) -> Result<(), PreviewError> {
    if format != ImageFormat::Png {
        return Ok(());
    }
    let mut png_reader =
        image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(map_image_error)?;
    let mut png_limits = Limits::default();
    png_limits.max_alloc = Some(MAX_DECODE_BYTES);
    png_reader.set_limits(png_limits).map_err(map_image_error)?;
    if !png_reader.is_apng().map_err(map_image_error)? {
        return Ok(());
    }
    let (width, height) = png_reader.dimensions();
    if u64::from(width) * u64::from(height) * 4 > MAX_DECODE_BYTES {
        return Err(PreviewError::ResourceLimit(format!(
            "composited animation frame exceeds {MAX_DECODE_BYTES} bytes"
        )));
    }
    let mut frames = png_reader.apng().map_err(map_image_error)?.into_frames();
    let frame = frames
        .next()
        .ok_or_else(|| PreviewError::Decode("APNG has no animation frames".into()))?
        .map_err(map_image_error)?;
    *image = DynamicImage::ImageRgba8(frame.into_buffer());
    Ok(())
}

/// Extract the alpha plane of `image` when it carries alpha.
fn alpha_plane(image: &DynamicImage) -> Option<Vec<u8>> {
    let has_alpha = matches!(
        image,
        DynamicImage::ImageLumaA8(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgba8(_)
            | DynamicImage::ImageRgba16(_)
            | DynamicImage::ImageRgba32F(_)
    );
    has_alpha.then(|| {
        image
            .to_rgba8()
            .pixels()
            .map(|p| p.0[3])
            .collect::<Vec<u8>>()
    })
}

/// Convert the selected image through its embedded profile when present.
/// Returns whether the published pixels are in sRGB.
fn finish_colour_conversion(
    image: &mut DynamicImage,
    icc: Option<&[u8]>,
    already_converted: bool,
) -> bool {
    if already_converted {
        return true;
    }
    icc.is_some_and(|profile| convert_to_srgb(image, profile).is_ok())
}

fn publish_renditions(
    library_root: &Path,
    hash: &str,
    image: DynamicImage,
    converted: bool,
    has_profile: bool,
    kinds: &[Kind],
) -> Result<Vec<Rendition>, PreviewError> {
    let (width, height) = image.dimensions();
    let transparent = has_transparency(&image);
    let rendition_format = if transparent {
        RenditionFormat::Png
    } else {
        RenditionFormat::Jpeg
    };
    let colour_status = if converted {
        ColourStatus::Srgb
    } else if has_profile {
        ColourStatus::NotConverted
    } else {
        ColourStatus::NoProfile
    };
    let mut output = Vec::new();
    for &kind in kinds {
        let (out_w, out_h) = plan(width, height, kind);
        let path = cache_path(library_root, hash, kind, rendition_format);
        let target = prepare_cache_dir(library_root, &path)?;
        if !valid_cache_hit(&target, (out_w, out_h), rendition_format) {
            let rendered = if (out_w, out_h) == (width, height) {
                image.clone()
            } else {
                image.resize_exact(out_w, out_h, imageops::FilterType::Lanczos3)
            };
            write_atomic(&target, &rendered, rendition_format)?;
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
        // The ensure pipeline decodes from an in-memory copy of the source, so
        // an unexpected EOF inside a decoder is truncation, not a system I/O
        // failure.
        ImageError::IoError(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            PreviewError::Decode(format!("truncated image data: {e}"))
        }
        ImageError::IoError(e) => PreviewError::Io(e),
        other => PreviewError::Decode(other.to_string()),
    }
}

/// `ImageReader::decode` reserves `decoder.total_bytes()` against the alloc
/// limit before decoding; `DynamicImage::from_decoder` allocates that buffer
/// directly. Keep the explicit reservation so images that pass the pixel
/// limit cannot bypass the byte ceiling (e.g. a 14000x14000 RGB BMP would
/// allocate 588 MB).
fn reserve_decode_buffer(decoder: &impl ImageDecoder) -> Result<(), PreviewError> {
    let total = decoder.total_bytes();
    if total > MAX_DECODE_BYTES {
        return Err(PreviewError::ResourceLimit(format!(
            "decoded image would allocate {total} bytes, exceeding {MAX_DECODE_BYTES}"
        )));
    }
    Ok(())
}

/// Transform an image to sRGB through its embedded ICC profile. The sample
/// layout follows the profile colour space; profiles that need samples the
/// decoder did not produce stay unconverted. Alpha is preserved by splitting
/// it out before the transform and splicing it back afterwards, so colour
/// conversion never changes transparency.
fn convert_to_srgb(image: &mut DynamicImage, icc: &[u8]) -> Result<(), String> {
    let input = Profile::new_icc(icc).map_err(|e| e.to_string())?;
    match input.color_space() {
        ColorSpaceSignature::GrayData => convert_gray_to_srgb(image, &input),
        ColorSpaceSignature::RgbData => convert_rgb_to_srgb(image, &input),
        _ => Err("profile colour space is incompatible with the decoded pixels".into()),
    }
}

fn convert_gray_to_srgb(image: &mut DynamicImage, profile: &Profile) -> Result<(), String> {
    let (width, height) = image.dimensions();
    let alpha = alpha_plane(image);
    let luma = image.to_luma8();
    let source = luma.as_raw();
    let output = Profile::new_srgb();
    let transform = Transform::new(
        profile,
        PixelFormat::GRAY_8,
        &output,
        PixelFormat::RGB_8,
        Intent::Perceptual,
    )
    .map_err(|e| e.to_string())?;
    let mut converted = vec![0; source.len() * 3];
    transform.transform_pixels(source, &mut converted);
    finish_rgb_or_rgba(image, width, height, alpha, converted)
}

fn convert_rgb_to_srgb(image: &mut DynamicImage, profile: &Profile) -> Result<(), String> {
    let (width, height) = image.dimensions();
    let output = Profile::new_srgb();
    let transform = Transform::new(
        profile,
        PixelFormat::RGB_8,
        &output,
        PixelFormat::RGB_8,
        Intent::Perceptual,
    )
    .map_err(|e| e.to_string())?;
    let alpha = alpha_plane(image);
    let source = if alpha.is_some() {
        image
            .to_rgba8()
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|px| [px[0], px[1], px[2]])
            .collect::<Vec<u8>>()
    } else {
        image.to_rgb8().into_raw()
    };
    let mut converted = vec![0; source.len()];
    transform.transform_pixels(&source, &mut converted);
    finish_rgb_or_rgba(image, width, height, alpha, converted)
}

/// Rebuild the image as RGB8 (or RGBA8 when splicing `alpha`) from converted
/// colour triples.
fn finish_rgb_or_rgba(
    image: &mut DynamicImage,
    width: u32,
    height: u32,
    alpha: Option<Vec<u8>>,
    converted: Vec<u8>,
) -> Result<(), String> {
    if let Some(alpha) = alpha {
        let mut rgba = vec![0; converted.len() / 3 * 4];
        for (dest, (alpha, rgb)) in rgba
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(alpha.iter().zip(converted.as_chunks::<3>().0.iter()))
        {
            dest[..3].copy_from_slice(rgb);
            dest[3] = *alpha;
        }
        let result =
            image::RgbaImage::from_raw(width, height, rgba).ok_or("invalid transformed pixels")?;
        *image = DynamicImage::ImageRgba8(result);
    } else {
        let result = image::RgbImage::from_raw(width, height, converted)
            .ok_or("invalid transformed pixels")?;
        *image = DynamicImage::ImageRgb8(result);
    }
    Ok(())
}

/// Read the embedded ICC profile of a TIFF through the raw decoder, where the
/// profile size is checked against the real byte limit instead of a scaled
/// buffer budget.
fn tiff_icc_profile(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = usize::try_from(MAX_DECODE_BYTES).unwrap_or(usize::MAX);
    limits.intermediate_buffer_size = usize::try_from(MAX_DECODE_BYTES).unwrap_or(usize::MAX);
    limits.ifd_value_size = usize::try_from(MAX_ENCODED_BYTES).unwrap_or(usize::MAX);
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
        .ok()?
        .with_limits(limits);
    decoder.get_tag_u8_vec(tiff::tags::Tag::IccProfile).ok()
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

/// image's TIFF decoder converts CMYK samples to RGB naively, which discards
/// the embedded profile. Decode the raw 8-bit CMYK samples instead and run
/// them through Little CMS when the profile is CMYK.
fn decode_profiled_cmyk_tiff(bytes: &[u8], icc: &[u8]) -> Result<DynamicImage, String> {
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = usize::try_from(MAX_DECODE_BYTES).unwrap_or(usize::MAX);
    limits.intermediate_buffer_size = usize::try_from(MAX_DECODE_BYTES).unwrap_or(usize::MAX);
    limits.ifd_value_size = usize::try_from(MAX_ENCODED_BYTES).unwrap_or(usize::MAX);
    let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
        .map_err(|e| e.to_string())?
        .with_limits(limits);
    let (width, height) = decoder.dimensions().map_err(|e| e.to_string())?;
    if decoder.colortype().map_err(|e| e.to_string())? != tiff::ColorType::CMYK(8) {
        return Err("TIFF is not 8-bit CMYK".into());
    }
    let needed = u64::from(width) * u64::from(height) * 4;
    if needed > MAX_DECODE_BYTES {
        return Err("TIFF decoded buffer exceeds memory limit".into());
    }
    let cmyk = match decoder.read_image().map_err(|e| e.to_string())? {
        tiff::decoder::DecodingResult::U8(samples) => samples,
        _ => return Err("unsupported TIFF sample format".into()),
    };
    if cmyk.len() as u64 != needed {
        return Err("planar CMYK TIFF is not supported".into());
    }
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
    check_webp_chunks(bytes, 12, limit)
}

/// Walk RIFF chunks and enforce frame dimension limits on every VP8/VP8L
/// bitstream, including the ones nested inside animated WebP `ANMF` frames.
fn check_webp_chunks(bytes: &[u8], mut offset: usize, limit: u64) -> Result<(), PreviewError> {
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
        if tag == b"VP8X" && data.len() >= 10 {
            let width = 1 + u24le(&data[4..7]);
            let height = 1 + u24le(&data[7..10]);
            check_limit(width, height, limit)?;
        } else if tag == b"ANMF" && data.len() >= 16 {
            let width = 1 + u24le(&data[6..9]);
            let height = 1 + u24le(&data[9..12]);
            check_limit(width, height, limit)?;
            check_webp_chunks(bytes, data_start + 16, limit)?;
        } else if let Some((width, height)) = webp_bitstream_dimensions(tag, data) {
            check_limit(width, height, limit)?;
        }
        offset = data_end + (length & 1);
    }
    Ok(())
}

fn u24le(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn webp_bitstream_dimensions(tag: &[u8], data: &[u8]) -> Option<(u32, u32)> {
    if tag == b"VP8 " && data.len() >= 10 && data[3..6] == [0x9d, 0x01, 0x2a] {
        Some((
            u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
            u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
        ))
    } else if tag == b"VP8L" && data.len() >= 5 && data[0] == 0x2f {
        let w = 1 + u32::from(data[1]) + (u32::from(data[2] & 0x3f) << 8);
        let h = 1
            + (u32::from(data[2] >> 6))
            + (u32::from(data[3]) << 2)
            + (u32::from(data[4] & 0x0f) << 10);
        Some((w, h))
    } else {
        None
    }
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

/// A cache entry location. On unix the containing directory is pinned by an
/// `O_NOFOLLOW`-opened descriptor, so later path component swaps cannot
/// redirect reads or writes outside the containing directory. On windows
/// every path component is re-validated without following reparse points
/// before publication and the published entry is re-verified after the
/// rename.
struct CacheTarget {
    #[cfg(windows)]
    /// Nominal published path, used for path-based validation and write.
    path: PathBuf,
    #[cfg(windows)]
    /// Canonical library root the published path must stay inside.
    root: PathBuf,
    #[cfg(unix)]
    name: OsString,
    #[cfg(unix)]
    dir: File,
}

fn cache_shard(file: &Path) -> OsString {
    file.parent()
        .and_then(Path::file_name)
        .map(|name| name.to_owned())
        .unwrap_or_default()
}

/// Ensure the cache directory chain for `file` exists, contains no symlinks
/// and stays inside the library root, returning a pinned target used by
/// validation and publication. Concurrent first-time creators retry until
/// the winning directory passes validation.
fn prepare_cache_dir(root: &Path, file: &Path) -> Result<CacheTarget, PreviewError> {
    let canonical_root = fs::canonicalize(root)?;
    let shard = cache_shard(file);
    let components: [&OsStr; 4] = [
        OsStr::new(".dimagine"),
        OsStr::new("cache"),
        OsStr::new("previews"),
        shard.as_os_str(),
    ];
    if components.iter().any(|component| component.is_empty()) {
        return Err(PreviewError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache file path misses its shard component",
        )));
    }
    #[cfg(unix)]
    {
        let mut dir = fs_check::open_dir(&canonical_root)?;
        for component in components {
            dir = fs_check::ensure_dir_at(&dir, component)?;
        }
        Ok(CacheTarget {
            name: file
                .file_name()
                .map_or_else(OsString::new, |n| n.to_owned()),
            dir,
        })
    }
    #[cfg(windows)]
    {
        let mut current = canonical_root.clone();
        for component in components {
            current = validate_cache_component(&canonical_root, &current, component, true)?;
        }
        Ok(CacheTarget {
            path: file.to_owned(),
            root: canonical_root,
        })
    }
}

fn rendition_image_format(format: RenditionFormat) -> ImageFormat {
    match format {
        RenditionFormat::Jpeg => ImageFormat::Jpeg,
        RenditionFormat::Png => ImageFormat::Png,
    }
}

/// Cache entries only count as hits when the published file survives a full
/// decode: intact headers over truncated or corrupt pixel data invalidate.
fn valid_cache_hit(target: &CacheTarget, expected: (u32, u32), format: RenditionFormat) -> bool {
    let expected_format = rendition_image_format(format);
    #[cfg(unix)]
    let opened = fs_check::open_file_at(&target.dir, &target.name);
    #[cfg(windows)]
    let opened = open_regular_file_at(&target.path);
    let Ok(mut file) = opened else {
        return false;
    };
    let Ok(clone) = file.try_clone() else {
        return false;
    };
    let Ok(dims) =
        ImageReader::with_format(BufReader::new(clone), expected_format).into_dimensions()
    else {
        return false;
    };
    if dims != expected {
        return false;
    }
    // `into_dimensions` consumed the rewindable clone; seek back and decode
    // the whole file so truncated renditions miss.
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    ImageReader::with_format(BufReader::new(file), expected_format)
        .decode()
        .is_ok()
}

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Open a directory without following a reparse point at the final
/// component, the Windows counterpart of an `O_NOFOLLOW | O_DIRECTORY`
/// open.
#[cfg(windows)]
fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Open a regular cache file without following a reparse point, so an
/// entry swapped for one between preparation and this open cannot
/// redirect the read; the handle metadata is authoritative about what
/// was opened.
#[cfg(windows)]
fn open_regular_file_at(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = file.metadata()?;
    if is_reparse_point(&meta) || !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache entry is not a regular file",
        ));
    }
    Ok(file)
}

/// True when the metadata describes any reparse point, not only the
/// symlink and junction tags `FileType::is_symlink` reports.
#[cfg(windows)]
fn is_reparse_point(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// The error returned when a cache path component is a reparse point
/// or not a directory.
#[cfg(windows)]
fn cache_component_error() -> PreviewError {
    PreviewError::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "cache path contains a symlink or non-directory",
    ))
}

/// Validate one cache path component: it must be a directory and not
/// a reparse point, both on disk and through a handle opened without
/// following reparse points, and it must resolve inside `root`. With
/// `create`, a missing component is created first; an `AlreadyExists`
/// failure means a concurrent writer won the creation race, and the
/// winner is validated instead.
#[cfg(windows)]
fn validate_cache_component(
    root: &Path,
    parent: &Path,
    component: &OsStr,
    create: bool,
) -> Result<PathBuf, PreviewError> {
    let path = parent.join(component);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if is_reparse_point(&meta) || !meta.is_dir() {
                return Err(cache_component_error());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            if let Err(error) = fs::create_dir(&path) {
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        Err(error) => return Err(error.into()),
    }
    let handle = open_dir_nofollow(&path)?;
    let meta = handle.metadata()?;
    if is_reparse_point(&meta) || !meta.is_dir() {
        return Err(cache_component_error());
    }
    let resolved = fs::canonicalize(&path)?;
    if !resolved.starts_with(root) {
        return Err(PreviewError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache path resolves outside the library",
        )));
    }
    Ok(resolved)
}

/// Re-validate every cache path component between the library root and
/// the cache shard, refusing when any component is a reparse point, is
/// not a directory, or resolves outside the root.
#[cfg(windows)]
fn revalidate_cache_chain(root: &Path, shard: &OsStr) -> Result<(), PreviewError> {
    let components: [&OsStr; 4] = [
        OsStr::new(".dimagine"),
        OsStr::new("cache"),
        OsStr::new("previews"),
        shard,
    ];
    if components.iter().any(|component| component.is_empty()) {
        return Err(PreviewError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache file path misses its shard component",
        )));
    }
    let mut current = root.to_owned();
    for component in components {
        current = validate_cache_component(root, &current, component, false)?;
    }
    Ok(())
}

/// Re-verify a cache entry after publication: it must be a regular
/// file resolving inside the library root. A component swapped for a
/// reparse point during publication redirects the rename, and this
/// check detects the escape.
#[cfg(windows)]
fn verify_published_entry(root: &Path, path: &Path) -> Result<(), PreviewError> {
    let meta = fs::symlink_metadata(path)?;
    if is_reparse_point(&meta) || !meta.is_file() {
        return Err(PreviewError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache entry is not a regular file",
        )));
    }
    let resolved = fs::canonicalize(path)?;
    if !resolved.starts_with(root) {
        return Err(PreviewError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache entry resolves outside the library",
        )));
    }
    Ok(())
}

fn encode_rendition(
    image: &DynamicImage,
    format: RenditionFormat,
) -> Result<Vec<u8>, PreviewError> {
    let mut encoded = Vec::new();
    match format {
        RenditionFormat::Jpeg => {
            let rgb = image.to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 88)
                .encode_image(&DynamicImage::ImageRgb8(rgb))
                .map_err(|e| PreviewError::Decode(e.to_string()))?;
        }
        RenditionFormat::Png => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut encoded)
                .write_image(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| PreviewError::Decode(e.to_string()))?;
        }
    }
    Ok(encoded)
}

/// Publish an encoded rendition atomically into the pinned cache directory.
/// On unix the temporary file is created, written, synced and renamed through
/// the same directory descriptor, so swapping a path component between
/// preparation and publication cannot redirect the bytes elsewhere. On
/// windows every component is re-validated without following reparse points
/// before the rename and the published entry is re-verified after it, so a
/// component swapped for a reparse point cannot redirect the write.
fn write_atomic(
    target: &CacheTarget,
    image: &DynamicImage,
    format: RenditionFormat,
) -> Result<(), PreviewError> {
    let encoded = encode_rendition(image, format)?;
    #[cfg(unix)]
    fs_check::publish(&target.dir, &target.name, &encoded).map_err(PreviewError::Io)?;
    #[cfg(windows)]
    {
        use std::io::Write;
        use tempfile::Builder;
        let parent = target.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent")
        })?;
        // Refuse to write when any component became a reparse point
        // or escaped the library since the target was prepared.
        revalidate_cache_chain(&target.root, &cache_shard(&target.path))?;
        let mut temp = Builder::new().prefix(".preview-").tempfile_in(parent)?;
        temp.write_all(&encoded)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        // Re-validate right before mutating the cache so a component
        // swapped while the rendition was written cannot redirect the
        // removal or the rename.
        revalidate_cache_chain(&target.root, &cache_shard(&target.path))?;
        // The rename replaces an existing file or reparse point entry
        // atomically without following it; only a stale directory
        // blocks the rename and must be removed first.
        if let Ok(meta) = fs::symlink_metadata(&target.path) {
            if meta.is_dir() && !is_reparse_point(&meta) {
                fs::remove_dir_all(&target.path)?;
            }
        }
        fs::rename(temp.path(), &target.path)?;
        verify_published_entry(&target.root, &target.path)?;
    }
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

/// Cache directory operations pinned to open descriptors.
///
/// Every cache component is opened with `O_NOFOLLOW | O_DIRECTORY`, rejects
/// non-directories, and handles the `EEXIST` race of concurrent first-time
/// creation by re-validating the winner. All validation and publication
/// (temporary file plus `renameat`) run relative to the pinned descriptor,
/// so replacing a checked directory with a symlink can no longer redirect
/// reads or writes outside the library.
#[cfg(unix)]
mod fs_check {
    use std::{
        ffi::{CStr, CString, OsStr, OsString},
        fs::File,
        io::{self, Write},
        os::fd::{AsRawFd, FromRawFd},
        os::unix::ffi::OsStrExt,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    const MAX_COMPONENT_CREATE_ATTEMPTS: usize = 8;
    const MAX_DEBRIS_DEPTH: u32 = 8;

    fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    fn c_string(name: &OsStr) -> Result<CString, io::Error> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))
    }

    fn cvt(fd: i32) -> Result<i32, io::Error> {
        if fd < 0 {
            Err(last_error())
        } else {
            Ok(fd)
        }
    }

    enum Entry {
        Missing,
        Directory,
        Regular,
        Other,
    }

    fn stat_at(dir: &File, name: &OsStr) -> Result<Entry, io::Error> {
        let c_name = c_string(name)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            let error = last_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(Entry::Missing);
            }
            return Err(error);
        }
        let mode = stat.st_mode;
        if mode & libc::S_IFMT == libc::S_IFDIR {
            Ok(Entry::Directory)
        } else if mode & libc::S_IFMT == libc::S_IFREG {
            Ok(Entry::Regular)
        } else {
            Ok(Entry::Other)
        }
    }

    fn open_child_dir(dir: &File, name: &OsStr) -> Result<File, io::Error> {
        let c_name = c_string(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Open the (already canonicalized) library root directory.
    pub fn open_dir(root: &Path) -> Result<File, io::Error> {
        let meta = std::fs::metadata(root)?;
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "library root is not a directory",
            ));
        }
        File::open(root)
    }

    /// Create or validate one cache component inside `dir` and return the
    /// pinned child directory.
    pub fn ensure_dir_at(dir: &File, name: &OsStr) -> Result<File, io::Error> {
        let c_name = c_string(name)?;
        let mut attempts = 0;
        loop {
            match stat_at(dir, name)? {
                Entry::Directory => return open_child_dir(dir, name),
                Entry::Missing => {
                    attempts += 1;
                    if attempts > MAX_COMPONENT_CREATE_ATTEMPTS {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "cache directory keeps disappearing",
                        ));
                    }
                    let result = unsafe { libc::mkdirat(dir.as_raw_fd(), c_name.as_ptr(), 0o755) };
                    if result != 0 {
                        let error = last_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                }
                Entry::Regular | Entry::Other => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "cache path contains a symlink or non-directory",
                    ));
                }
            }
        }
    }

    /// Open a regular file in `dir` without following symlinks.
    pub fn open_file_at(dir: &File, name: &OsStr) -> Result<File, io::Error> {
        if !matches!(stat_at(dir, name)?, Entry::Regular) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache entry is not a regular file",
            ));
        }
        let c_name = c_string(name)?;
        let fd = cvt(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Publish `bytes` as `name` in `dir` through a same-directory temporary
    /// file and `renameat`, replacing an existing file or symlink entry
    /// atomically. A stale directory at the destination is removed first.
    pub fn publish(dir: &File, name: &OsStr, bytes: &[u8]) -> Result<(), io::Error> {
        let (temp_name, mut temp_file) = create_temp(dir)?;
        let result = write_temp_and_rename(dir, name, bytes, &temp_name, &mut temp_file);
        if result.is_err() {
            if let Ok(Entry::Regular) = stat_at(dir, &temp_name) {
                if let Ok(c_temp) = c_string(&temp_name) {
                    unsafe { libc::unlinkat(dir.as_raw_fd(), c_temp.as_ptr(), 0) };
                }
            }
        }
        result
    }

    fn write_temp_and_rename(
        dir: &File,
        name: &OsStr,
        bytes: &[u8],
        temp_name: &OsStr,
        temp_file: &mut File,
    ) -> Result<(), io::Error> {
        temp_file.write_all(bytes)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        match stat_at(dir, name) {
            Ok(Entry::Directory) => remove_tree_at(dir, name, 0)?,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let c_temp = c_string(temp_name)?;
        let c_name = c_string(name)?;
        let result = unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                c_temp.as_ptr(),
                dir.as_raw_fd(),
                c_name.as_ptr(),
            )
        };
        if result != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn temp_name() -> OsString {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        OsString::from(format!(
            ".preview-{:x}-{count:x}-{nanos:x}",
            std::process::id()
        ))
    }

    fn create_temp(dir: &File) -> Result<(OsString, File), io::Error> {
        loop {
            let name = temp_name();
            let c_name = c_string(&name)?;
            let fd = cvt(unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    c_name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            });
            match fd {
                Ok(fd) => return Ok((name, unsafe { File::from_raw_fd(fd) })),
                Err(error) => {
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Remove a directory tree relative to the pinned descriptor. Symlinked
    /// entries are unlinked, never followed.
    fn remove_tree_at(dir: &File, name: &OsStr, depth: u32) -> Result<(), io::Error> {
        if depth > MAX_DEBRIS_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache debris is nested too deeply",
            ));
        }
        let child = open_child_dir(dir, name)?;
        let dup = cvt(unsafe { libc::dup(child.as_raw_fd()) })?;
        let stream = unsafe { libc::fdopendir(dup) };
        if stream.is_null() {
            return Err(last_error());
        }
        let mut failure: Option<io::Error> = None;
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes() };
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let entry_name = OsStr::from_bytes(bytes);
            match stat_at(&child, entry_name) {
                Ok(Entry::Directory) => {
                    if let Err(error) = remove_tree_at(&child, entry_name, depth + 1) {
                        failure = Some(error);
                        break;
                    }
                }
                Ok(_) => {
                    if let Ok(c_entry) = c_string(entry_name) {
                        unsafe { libc::unlinkat(child.as_raw_fd(), c_entry.as_ptr(), 0) };
                    }
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        unsafe { libc::closedir(stream) };
        if let Some(error) = failure {
            return Err(error);
        }
        let c_name = c_string(name)?;
        let result =
            unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) };
        if result != 0 {
            let error = last_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};
    use std::sync::{Arc, Barrier};
    use tempfile::Builder;

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
        Builder::new()
            .prefix("preview-test-")
            .tempdir_in(std::env::temp_dir())
            .expect("temp dir")
    }

    /// Grayscale ICC profile with gamma 1.0; conversion to sRGB shifts the
    /// mid-tones far away from the identity mapping (60 -> ~133).
    fn gray_gamma_one_profile() -> Vec<u8> {
        let white = lcms2::CIExyY {
            x: 0.3457,
            y: 0.3585,
            Y: 1.0,
        };
        lcms2::Profile::new_gray(&white, &lcms2::ToneCurve::new(1.0))
            .expect("gray profile")
            .icc()
            .expect("icc serialization")
    }

    /// A synthetic CMYK printer profile: an `mft1` A2B0 CLUT that maps the
    /// cyan channel to neutral lightness (L* 100 at C=0, L* 0 at C=255) and
    /// ignores M, Y and K. White/black samples therefore decode to
    /// white/black, while image-rs's naive device conversion would turn
    /// (255, 0, 0, 0) into cyan.
    fn synthetic_cmyk_profile() -> Vec<u8> {
        let (grid, in_chan, out_chan) = (2usize, 4usize, 3usize);
        let mut lut: Vec<u8> = Vec::new();
        lut.extend_from_slice(b"mft1");
        lut.extend_from_slice(&[0, 0, 0, 0]);
        lut.push(in_chan as u8);
        lut.push(out_chan as u8);
        lut.push(grid as u8);
        lut.push(0);
        for idx in 0..9 {
            let cell: u32 = if idx == 0 || idx == 4 || idx == 8 {
                0x0001_0000
            } else {
                0
            };
            lut.extend_from_slice(&cell.to_be_bytes());
        }
        for _ in 0..in_chan {
            for v in 0..=255u16 {
                lut.push(v as u8);
            }
        }
        for c in 0..grid {
            for _m in 0..grid {
                for _y in 0..grid {
                    for _k in 0..grid {
                        let l_star = 100.0 - 100.0 * (c as f64);
                        lut.push((l_star * 255.0 / 100.0).round() as u8);
                        lut.push(128);
                        lut.push(128);
                    }
                }
            }
        }
        for _ in 0..out_chan {
            for v in 0..=255u16 {
                lut.push(v as u8);
            }
        }
        while !lut.len().is_multiple_of(4) {
            lut.push(0);
        }

        let mut header = vec![0u8; 128];
        header[4..8].copy_from_slice(b"lcms");
        header[8..12].copy_from_slice(&[0x02, 0x40, 0x00, 0x00]);
        header[12..16].copy_from_slice(b"prtr");
        header[16..20].copy_from_slice(b"CMYK");
        header[20..24].copy_from_slice(b"Lab ");
        header[36..40].copy_from_slice(b"acsp");
        header[64..68].copy_from_slice(&0u32.to_be_bytes());

        let mut wtpt = Vec::new();
        wtpt.extend_from_slice(b"XYZ ");
        wtpt.extend_from_slice(&[0; 4]);
        for value in [0.9642_f64, 1.0, 0.8249] {
            wtpt.extend_from_slice(&((value * 65536.0).round() as u32).to_be_bytes());
        }

        let tags = [(*b"wtpt", wtpt), (*b"A2B0", lut)];
        let tag_table_len = 4 + tags.len() * 12;
        let mut data_area: Vec<u8> = Vec::new();
        let mut offsets = Vec::new();
        for (_, data) in &tags {
            while !data_area.len().is_multiple_of(4) {
                data_area.push(0);
            }
            offsets.push((data_area.len(), data.len()));
            data_area.extend_from_slice(data);
        }
        let total = 128 + tag_table_len + data_area.len();
        header[0..4].copy_from_slice(&(total as u32).to_be_bytes());

        let mut icc = Vec::new();
        icc.extend_from_slice(&header);
        icc.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        for (i, (sig, _)) in tags.iter().enumerate() {
            icc.extend_from_slice(sig);
            icc.extend_from_slice(&((128 + tag_table_len + offsets[i].0) as u32).to_be_bytes());
            icc.extend_from_slice(&(offsets[i].1 as u32).to_be_bytes());
        }
        icc.extend_from_slice(&data_area);
        icc
    }

    /// Minimal little-endian TIFF whose strip samples are 4-bit; its headers
    /// parse far enough to report dimensions, but the sample format is
    /// unsupported, which must surface as `Unsupported`.
    fn minimal_four_bit_tiff() -> Vec<u8> {
        let entries: [(u16, u16, [u8; 4]); 9] = [
            (256, 3, [2, 0, 0, 0]),
            (257, 3, [1, 0, 0, 0]),
            (258, 3, [4, 0, 0, 0]),
            (259, 3, [1, 0, 0, 0]),
            (262, 3, [1, 0, 0, 0]),
            (273, 4, [0, 0, 0, 0]),
            (277, 3, [1, 0, 0, 0]),
            (278, 3, [1, 0, 0, 0]),
            (279, 4, [1, 0, 0, 0]),
        ];
        let count = entries.len() as u16;
        let data_offset = (8 + 2 + entries.len() * 12 + 4) as u32;
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&8u32.to_le_bytes());
        data.extend_from_slice(&count.to_le_bytes());
        for (tag, kind, value) in entries {
            data.extend_from_slice(&tag.to_le_bytes());
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&1u32.to_le_bytes());
            let value = if tag == 273 {
                data_offset.to_le_bytes()
            } else {
                value
            };
            data.extend_from_slice(&value);
        }
        data.extend_from_slice(&0u32.to_le_bytes());
        data.push(0x0f);
        data
    }

    /// header-only BMP claiming 14000x14000 at 24 bpp: 196 million pixels
    /// pass the default pixel limit but the decoded buffer (588 MB) exceeds
    /// the 512 MiB ceiling.
    fn huge_output_bmp_header() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"BM");
        data.extend_from_slice(&54u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&54u32.to_le_bytes());
        data.extend_from_slice(&40u32.to_le_bytes());
        data.extend_from_slice(&14000i32.to_le_bytes());
        data.extend_from_slice(&14000i32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&24u16.to_le_bytes());
        data.extend_from_slice(&[0u8; 24]);
        data
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
        let result = ensure(dir.path(), path, &[Kind::Thumb]);
        assert!(
            matches!(result, Err(PreviewError::Decode(_))),
            "got {result:?}"
        );
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
    fn webp_canvas_dimensions_are_checked_before_decode() {
        let mut data = b"RIFF\0\0\0\0WEBP".to_vec();
        data.extend_from_slice(b"VP8X");
        data.extend_from_slice(&10_u32.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let riff_len = (data.len() - 8) as u32;
        data[4..8].copy_from_slice(&riff_len.to_le_bytes());
        assert!(matches!(
            check_webp_frame_limits(&data, 4),
            Err(PreviewError::PixelLimit {
                width: 16_777_216,
                height: 16_777_216,
                ..
            })
        ));
    }

    fn riff_container(chunks: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut data = b"RIFF\0\0\0\0WEBP".to_vec();
        for (tag, payload) in chunks {
            data.extend_from_slice(tag);
            data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            data.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                data.push(0);
            }
        }
        let riff_len = (data.len() - 8) as u32;
        data[4..8].copy_from_slice(&riff_len.to_le_bytes());
        data
    }

    #[test]
    fn animated_webp_nested_vp8_dimensions_are_checked() {
        let mut nested_vp8 = b"VP8 ".to_vec();
        nested_vp8.extend_from_slice(&10_u32.to_le_bytes());
        nested_vp8.extend_from_slice(&[0, 0, 0, 0x9d, 1, 0x2a, 0xff, 0x3f, 0xff, 0x3f]);
        // ANMF: X, Y, frame size (2x2 after the minus-one field), duration,
        // followed by the frame's subchunks that earlier escaped the scan.
        let mut anmf: Vec<u8> = Vec::new();
        anmf.extend_from_slice(&[0; 6]);
        anmf.extend_from_slice(&[1, 0, 0, 1, 0, 0]);
        anmf.extend_from_slice(&0u32.to_le_bytes());
        anmf.extend_from_slice(&nested_vp8);
        let data = riff_container(&[(b"VP8X".to_vec(), vec![0; 10]), (b"ANMF".to_vec(), anmf)]);
        assert!(matches!(
            check_webp_frame_limits(&data, 4),
            Err(PreviewError::PixelLimit {
                width: 16_383,
                height: 16_383,
                ..
            })
        ));
    }

    #[test]
    fn animated_webp_nested_vp8l_dimensions_are_checked() {
        let mut nested_vp8l = b"VP8L".to_vec();
        nested_vp8l.extend_from_slice(&5_u32.to_le_bytes());
        nested_vp8l.extend_from_slice(&[0x2f, 0xff, 0xff, 0xff, 0x0f]);
        let mut anmf: Vec<u8> = Vec::new();
        anmf.extend_from_slice(&[0; 6]);
        anmf.extend_from_slice(&[1, 0, 0, 1, 0, 0]);
        anmf.extend_from_slice(&0u32.to_le_bytes());
        anmf.extend_from_slice(&nested_vp8l);
        let data = riff_container(&[(b"VP8X".to_vec(), vec![0; 10]), (b"ANMF".to_vec(), anmf)]);
        assert!(matches!(
            check_webp_frame_limits(&data, 4),
            Err(PreviewError::PixelLimit {
                width: 16_384,
                height: 16_384,
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
        let target = prepare_cache_dir(dir.path(), &repaired[0].path).unwrap();
        assert!(valid_cache_hit(&target, (2, 2), RenditionFormat::Jpeg));
    }

    #[test]
    fn oversized_output_buffer_is_reserved_before_decoding() {
        let dir = temp_library();
        let source = dir.path().join("huge.bmp");
        fs::write(&source, huge_output_bmp_header()).unwrap();
        // 14000x14000 = 196 million pixels passes the default pixel limit,
        // but the decoded RGB buffer is 588 MB, over the 512 MiB ceiling.
        let result = ensure(dir.path(), source, &[Kind::Thumb]);
        assert!(
            matches!(result, Err(PreviewError::ResourceLimit(_))),
            "got {result:?}"
        );
    }

    #[test]
    fn transparent_profiled_png_keeps_alpha_and_png_format() {
        let dir = temp_library();
        let source = dir.path().join("profiled-transparent.png");
        let icc = Profile::new_srgb().icc().unwrap();
        let file = File::create(&source).unwrap();
        let image = ImageBuffer::from_fn(8, 8, |x, _| {
            if x == 0 {
                Rgba([1_u8, 2, 3, 100])
            } else {
                Rgba([1_u8, 2, 3, 255])
            }
        });
        let mut encoder = image::codecs::png::PngEncoder::new(file);
        encoder.set_icc_profile(icc).unwrap();
        encoder
            .write_image(&image, 8, 8, image::ExtendedColorType::Rgba8)
            .unwrap();
        let rendition = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert_eq!(rendition[0].colour_status, ColourStatus::Srgb);
        assert_eq!(rendition[0].format, RenditionFormat::Png);
        let decoded = image::open(&rendition[0].path).unwrap().to_rgba8();
        let visible = decoded.get_pixel(0, 0);
        assert_eq!(visible[3], 100, "alpha channel must survive conversion");
        let opaque = decoded.get_pixel(7, 7);
        assert_eq!(opaque[3], 255);
        for channel in visible.0[..3].iter() {
            assert!(
                (i16::from(*channel) - 2).abs() <= 2,
                "converted colour drifted: {visible:?}"
            );
        }
    }

    #[test]
    fn profiled_apng_converts_the_selected_first_frame() {
        let dir = temp_library();
        let source = dir.path().join("profiled-apng.png");
        let icc = gray_gamma_one_profile();
        let mut info = png::Info::with_size(2, 2);
        info.color_type = png::ColorType::Grayscale;
        info.bit_depth = png::BitDepth::Eight;
        info.animation_control = Some(png::AnimationControl {
            num_frames: 2,
            num_plays: 0,
        });
        info.frame_control = Some(png::FrameControl {
            sequence_number: 0,
            width: 2,
            height: 2,
            x_offset: 0,
            y_offset: 0,
            delay_num: 0,
            delay_den: 100,
            dispose_op: png::DisposeOp::None,
            blend_op: png::BlendOp::Source,
        });
        info.icc_profile = Some(std::borrow::Cow::Owned(icc));
        let file = File::create(&source).unwrap();
        let encoder = png::Encoder::with_info(file, info).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[60_u8; 4]).unwrap();
        writer.write_image_data(&[10_u8; 4]).unwrap();
        drop(writer);
        let rendition = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert_eq!(rendition[0].colour_status, ColourStatus::Srgb);
        let decoded = image::open(&rendition[0].path).unwrap().to_luma8();
        for pixel in decoded.pixels() {
            // 60 in gamma-1.0 grey converts to about 133 in sRGB; the
            // unconverted frame would stay 60 and the poster image 10 -> 55.
            assert!(
                (125..=141).contains(&pixel.0[0]),
                "frame was not converted: {}",
                pixel.0[0]
            );
        }
    }

    #[test]
    fn concurrent_first_time_generation_succeeds() {
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let root = dir.path().to_owned();
            let source = source.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                ensure(&root, &source, &[Kind::Thumb])
            }));
        }
        let mut expected_path = None;
        for handle in handles {
            let renditions = handle.join().unwrap().expect("concurrent generation");
            assert_eq!(renditions.len(), 1);
            expected_path.get_or_insert_with(|| renditions[0].path.clone());
            assert_eq!(renditions[0].path, *expected_path.as_ref().unwrap());
        }
        let target = prepare_cache_dir(dir.path(), expected_path.as_ref().unwrap()).unwrap();
        assert!(valid_cache_hit(&target, (2, 2), RenditionFormat::Jpeg));
    }

    #[cfg(unix)]
    #[test]
    fn component_swap_after_pin_cannot_redirect_writes() {
        use std::os::unix::fs::symlink;
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        let target = prepare_cache_dir(dir.path(), &first[0].path).unwrap();
        let shard = first[0].path.parent().unwrap().to_owned();
        let outside = temp_library();
        let swapped = shard.parent().unwrap().join(format!(
            "swapped-{}",
            shard.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(&shard, &swapped).unwrap();
        symlink(outside.path(), &shard).unwrap();
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, 2, Rgb([9_u8, 9, 9])));
        write_atomic(&target, &image, RenditionFormat::Jpeg).expect("pinned publish");
        assert_eq!(
            fs::read_dir(outside.path()).unwrap().count(),
            0,
            "bytes must not escape through the swapped component"
        );
        let published = swapped.join(first[0].path.file_name().unwrap());
        assert!(fs::symlink_metadata(&published).unwrap().is_file());
        assert!(image::open(&published).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cache_shard_symlink_is_refused_at_publication() {
        use std::os::unix::fs::symlink;
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        fs::remove_file(&first[0].path).unwrap();
        let shard = first[0].path.parent().unwrap().to_owned();
        fs::remove_dir(&shard).unwrap();
        let outside = temp_library();
        symlink(outside.path(), &shard).unwrap();
        assert!(matches!(
            ensure(dir.path(), source, &[Kind::Thumb]),
            Err(PreviewError::Io(_))
        ));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn cache_entry_symlink_is_replaced_with_fresh_rendition() {
        use std::os::unix::fs::symlink;
        let dir = temp_library();
        let source = dir.path().join("source.bmp");
        ImageBuffer::from_pixel(2, 2, Rgb([1_u8, 2, 3]))
            .save(&source)
            .unwrap();
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        let decoy = dir.path().join("decoy.jpg");
        fs::write(&decoy, b"stale bytes").unwrap();
        fs::remove_file(&first[0].path).unwrap();
        symlink(&decoy, &first[0].path).unwrap();
        let second = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        assert_eq!(second[0].path, first[0].path);
        let meta = fs::symlink_metadata(&second[0].path).unwrap();
        assert!(meta.is_file() && !meta.file_type().is_symlink());
        assert!(image::open(&second[0].path).is_ok());
    }

    #[test]
    fn header_stage_unsupported_error_keeps_category() {
        let dir = temp_library();
        let source = dir.path().join("four-bit.tif");
        fs::write(&source, minimal_four_bit_tiff()).unwrap();
        assert!(
            matches!(
                ensure(dir.path(), source, &[Kind::Thumb]),
                Err(PreviewError::Unsupported(_))
            ),
            "unsupported 4-bit TIFF must not be reported as corrupt"
        );
    }

    #[test]
    fn grayscale_profile_is_converted_to_srgb() {
        let dir = temp_library();
        let source = dir.path().join("gray-profiled.png");
        let icc = gray_gamma_one_profile();
        let file = File::create(&source).unwrap();
        let image = ImageBuffer::from_pixel(16, 16, image::Luma([60_u8]));
        let mut encoder = image::codecs::png::PngEncoder::new(file);
        encoder.set_icc_profile(icc).unwrap();
        encoder
            .write_image(&image, 16, 16, image::ExtendedColorType::L8)
            .unwrap();
        let rendition = ensure(dir.path(), source, &[Kind::Thumb]).unwrap();
        assert_eq!(rendition[0].colour_status, ColourStatus::Srgb);
        let decoded = image::open(&rendition[0].path).unwrap().to_luma8();
        for pixel in decoded.pixels() {
            // 60 in gamma-1.0 grey converts to about 133 in sRGB.
            assert!(
                (127..=139).contains(&pixel.0[0]),
                "gray pixels were not converted: {}",
                pixel.0[0]
            );
        }
    }

    #[test]
    fn profiled_cmyk_tiff_converts_raw_samples() {
        let dir = temp_library();
        let source = dir.path().join("profiled-cmyk.tif");
        let icc = synthetic_cmyk_profile();
        let parsed = Profile::new_icc(&icc).expect("fixture profile parses");
        assert_eq!(parsed.color_space(), ColorSpaceSignature::CmykData);
        let width = 32_u32;
        let height = 32_u32;
        let mut samples = Vec::new();
        for y in 0..height {
            for _ in 0..width {
                if y < height / 2 {
                    // device-space white: C=M=Y=K=0
                    samples.extend_from_slice(&[0_u8, 0, 0, 0]);
                } else {
                    // full cyan plate: converts to black through the profile,
                    // to cyan through image-rs's naive conversion
                    samples.extend_from_slice(&[255_u8, 0, 0, 0]);
                }
            }
        }
        let file = File::create(&source).unwrap();
        let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
        let mut image = encoder
            .new_image::<tiff::encoder::colortype::CMYK8>(width, height)
            .unwrap();
        image
            .encoder()
            .write_tag(tiff::tags::Tag::IccProfile, icc.as_slice())
            .unwrap();
        image.write_data(&samples).unwrap();

        let rendition = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        assert_eq!(rendition[0].colour_status, ColourStatus::Srgb);
        let decoded = image::open(&rendition[0].path).unwrap().to_rgb8();
        let white = decoded.get_pixel(16, 8);
        let black = decoded.get_pixel(16, 24);
        assert!(
            white.0.iter().all(|c| *c >= 248),
            "CMYK (0,0,0,0) should convert to white, got {white:?}"
        );
        assert!(
            black.0.iter().all(|c| *c <= 6),
            "CMYK (255,0,0,0) should convert to black, got {black:?}"
        );
    }

    #[test]
    fn truncated_cache_rendition_is_regenerated() {
        let dir = temp_library();
        let source = dir.path().join("noisy-transparent.png");
        let pixels = ImageBuffer::from_fn(32, 32, |x, y| {
            let colour = ((x * 19 + y * 7) % 256) as u8;
            let alpha = if (x + y) % 16 == 0 { 100 } else { 255 };
            Rgba([colour, 255 - colour, x as u8, alpha])
        });
        pixels.save(&source).unwrap();
        let first = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        assert_eq!(first[0].format, RenditionFormat::Png);
        let original = fs::read(&first[0].path).unwrap();
        let cut = original.len() / 2;
        fs::write(&first[0].path, &original[..cut]).unwrap();
        let repaired = ensure(dir.path(), &source, &[Kind::Thumb]).unwrap();
        assert_eq!(repaired[0].path, first[0].path);
        let mended = fs::read(&repaired[0].path).unwrap();
        assert_ne!(mended.len(), cut, "truncated rendition must be replaced");
        assert!(mended.len() > cut);
        assert!(image::open(&repaired[0].path).is_ok());
    }
}
