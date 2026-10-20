//! Perceptual fingerprints for near-duplicate detection (W77).
//!
//! Both fingerprints are computed from the oriented, colour-managed pixels the
//! preview pipeline already decodes, so a library's fingerprints and its
//! renditions describe the same pixels. Nothing here touches the file system:
//! the callers pass a [`DynamicImage`] and store the result.
//!
//! - [`dhash`] is a 64-bit difference hash: a coarse shape that makes two
//!   versions of the same picture land close in Hamming distance, cheap enough
//!   to compare against every other image in a library.
//! - [`signature`] is a compact grayscale thumbnail of a fixed width; the exact
//!   per-pixel comparison [`p99`] runs over it is what tells a re-encode of one
//!   picture (a `same` pair) from two similar-but-different pictures (a
//!   `variant`, reported but never merged).
//!
//! The thresholds that turn these numbers into a decision live in
//! `dimagine_core::check`, not here: this module only measures.

use image::{imageops, DynamicImage, GenericImageView, GrayImage};

/// A dHash reduces the picture to this grid: nine columns so eight horizontal
/// gradients fit across each of eight rows, sixty-four bits in all.
const DHASH_COLS: u32 = 9;
const DHASH_ROWS: u32 = 8;

/// The width a comparison signature is rescaled to. A fixed width makes two
/// versions of the same picture line up pixel for pixel; the height follows the
/// aspect so the shape is not distorted, within [`SIGNATURE_MAX_HEIGHT`].
const SIGNATURE_WIDTH: u32 = 128;

/// A tall strip is capped rather than kept proportional, because the pixel
/// comparison only ever runs between images already gated to a near-equal
/// aspect; a signature of the full height would cost memory it never compares.
const SIGNATURE_MAX_HEIGHT: u32 = 256;

/// A compact grayscale signature: the pixels a [`p99`] comparison runs over,
/// rescaled to [`SIGNATURE_WIDTH`] wide with the height following the aspect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Signature width in pixels (always [`SIGNATURE_WIDTH`] for a real image).
    pub width: u32,
    /// Signature height in pixels, the aspect-following height capped at
    /// [`SIGNATURE_MAX_HEIGHT`].
    pub height: u32,
    /// The grayscale samples, row-major, `width * height` of them.
    pub gray: Vec<u8>,
}

/// A 64-bit difference hash of an image's shape.
///
/// The picture is reduced to a `DHASH_COLS` x `DHASH_ROWS` grayscale grid, then
/// each of the eight gradients in a row is one bit, set when the left pixel is
/// darker than its right neighbour. Bits are laid row-major, most significant
/// first, so bit `r * 8 + c` (counting from the top) is the gradient at column
/// `c` of row `r`. Only the *relative* order matters for a distance, so the
/// convention only has to be the one both sides compute.
pub fn dhash(image: &DynamicImage) -> u64 {
    let grid = DynamicImage::ImageLuma8(grayscale(image))
        .resize_exact(DHASH_COLS, DHASH_ROWS, imageops::FilterType::Lanczos3)
        .to_luma8();
    let mut hash = 0_u64;
    for row in 0..DHASH_ROWS {
        for col in 0..8_u32 {
            let left = grid.get_pixel(col, row)[0];
            let right = grid.get_pixel(col + 1, row)[0];
            hash = (hash << 1) | u64::from(left < right);
        }
    }
    hash
}

/// The Hamming distance between two difference hashes: how many of the
/// sixty-four gradients disagree.
pub fn dhash_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// The fixed-width grayscale signature a [`p99`] comparison runs over.
pub fn signature(image: &DynamicImage) -> Signature {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Signature {
            width: 0,
            height: 0,
            gray: Vec::new(),
        };
    }
    let target_height = ((u64::from(SIGNATURE_WIDTH) * u64::from(height) / u64::from(width)).max(1)
        as u32)
        .min(SIGNATURE_MAX_HEIGHT);
    let scaled = DynamicImage::ImageLuma8(grayscale(image))
        .resize_exact(
            SIGNATURE_WIDTH,
            target_height,
            imageops::FilterType::Lanczos3,
        )
        .to_luma8();
    Signature {
        width: SIGNATURE_WIDTH,
        height: target_height,
        gray: scaled.into_raw(),
    }
}

/// The 99th-percentile absolute grayscale difference between two signatures,
/// over the region they share (the common width and the shorter height).
///
/// Two encodings of one picture differ only by compression noise, so almost
/// every pixel agrees and the 99th percentile stays low; a real difference — a
/// crop, an added overlay, a retaken frame — puts enough pixels far apart to
/// raise it. Returns `None` when the two share no area.
pub fn p99(a: &Signature, b: &Signature) -> Option<u8> {
    let width = a.width.min(b.width);
    let height = a.height.min(b.height);
    if width == 0 || height == 0 {
        return None;
    }
    let mut diffs = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let pa = a.gray[(y * a.width + x) as usize];
            let pb = b.gray[(y * b.width + x) as usize];
            diffs.push(pa.abs_diff(pb));
        }
    }
    diffs.sort_unstable();
    let index = (diffs.len() - 1) * 99 / 100;
    Some(diffs[index])
}

/// The picture as 8-bit grayscale, the base both fingerprints reduce.
fn grayscale(image: &DynamicImage) -> GrayImage {
    image.to_luma8()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Rgb, RgbImage};

    fn solid(width: u32, height: u32, tone: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([tone, tone, tone])))
    }

    /// A left-to-right gradient, so its dHash is all ones: every column pair
    /// rises. A flat image is the opposite, all zeros.
    fn gradient(width: u32, height: u32) -> DynamicImage {
        let mut img = RgbImage::from_pixel(width, height, Rgb([0, 0, 0]));
        for x in 0..width {
            let tone = ((x * 255) / width.max(1)) as u8;
            for y in 0..height {
                img.put_pixel(x, y, Rgb([tone, tone, tone]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn a_rising_gradient_sets_every_gradient_bit() {
        assert_eq!(dhash(&gradient(200, 100)), u64::MAX);
    }

    #[test]
    fn a_flat_field_sets_no_bit() {
        assert_eq!(dhash(&solid(200, 100, 128)), 0);
    }

    #[test]
    fn a_solid_and_its_reencode_share_a_hash() {
        let small = solid(600, 400, 90);
        let large = solid(1800, 1200, 90);
        assert_eq!(dhash(&small), dhash(&large));
        assert_eq!(dhash_distance(dhash(&small), dhash(&large)), 0);
    }

    #[test]
    fn different_shapes_are_far_apart() {
        let flat = dhash(&solid(200, 100, 128));
        let rising = dhash(&gradient(200, 100));
        assert_eq!(dhash_distance(flat, rising), 64);
    }

    #[test]
    fn a_signature_is_fixed_width_with_an_aspect_following_height() {
        let sig = signature(&solid(1000, 500, 128));
        assert_eq!(sig.width, SIGNATURE_WIDTH);
        assert_eq!(sig.height, 64);
        assert_eq!(sig.gray.len(), (sig.width * sig.height) as usize);
    }

    #[test]
    fn a_tall_signature_height_is_capped() {
        let sig = signature(&solid(100, 10_000, 128));
        assert_eq!(sig.width, SIGNATURE_WIDTH);
        assert_eq!(sig.height, SIGNATURE_MAX_HEIGHT);
        assert_eq!(sig.gray.len(), (sig.width * sig.height) as usize);
    }

    #[test]
    fn two_encodings_of_one_tone_differ_by_nothing() {
        let a = signature(&solid(600, 400, 200));
        let b = signature(&solid(1800, 1200, 200));
        assert_eq!(p99(&a, &b), Some(0));
    }

    #[test]
    fn opposite_tones_are_maximally_distant() {
        let a = signature(&solid(600, 400, 0));
        let b = signature(&solid(600, 400, 255));
        assert_eq!(p99(&a, &b), Some(255));
    }

    #[test]
    fn p99_ignores_a_slim_band_of_outliers() {
        // Same base tone, but a thin darker bar over well under 1% of the rows:
        // the 99th percentile still reads the base agreement, not the bar.
        let clean = GrayImage::from_pixel(SIGNATURE_WIDTH, 200, image::Luma([120u8]));
        let mut dirty = clean.clone();
        for y in 0..1 {
            for x in 0..SIGNATURE_WIDTH {
                dirty.put_pixel(x, y, image::Luma([10u8]));
            }
        }
        let a = signature_of(clean);
        let b = signature_of(dirty);
        assert!(p99(&a, &b).unwrap() < 40);
    }

    fn signature_of(gray: GrayImage) -> Signature {
        Signature {
            width: gray.width(),
            height: gray.height(),
            gray: gray.into_raw(),
        }
    }

    #[test]
    fn an_empty_image_signatures_to_nothing_and_p99_is_none() {
        let empty = signature(&solid(0, 0, 0));
        assert_eq!(empty.width, 0);
        assert_eq!(empty.gray.len(), 0);
        let real = signature(&solid(600, 400, 0));
        assert_eq!(p99(&empty, &real), None);
    }
}
