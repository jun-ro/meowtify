use std::fmt;

use image::imageops::FilterType;

/// Decoded, display-sized RGBA8 pixels (`width * height * 4` bytes).
#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub enum CacheError {
    Decode(String),
    /// Requested size was zero.
    InvalidSize,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Decode(e) => write!(f, "image decode failed: {e}"),
            CacheError::InvalidSize => write!(f, "invalid target size"),
        }
    }
}

impl std::error::Error for CacheError {}

/// The most pixels a source image may have. A full-size RGBA copy of this is 80 MB, which
/// is the most one decode can add to the memory of the app.
const MAX_SOURCE_PIXELS: u64 = 20_000_000;

/// Decodes `bytes` after checking the size in the header, so a small file that claims a
/// huge picture is refused before any pixel memory is allocated.
fn load_limited(bytes: &[u8]) -> Result<image::DynamicImage, CacheError> {
    let reader = || image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| CacheError::Decode(e.to_string()));
    let (w, h) = reader()?.into_dimensions().map_err(|e| CacheError::Decode(e.to_string()))?;
    if u64::from(w) * u64::from(h) > MAX_SOURCE_PIXELS {
        return Err(CacheError::Decode(format!("image is too large ({w} x {h})")));
    }
    let mut reader = reader()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_SOURCE_PIXELS * 4 * 2);
    reader.limits(limits);
    reader.decode().map_err(|e| CacheError::Decode(e.to_string()))
}

/// Decode `bytes`, cut the largest centred square, and scale it to `side` x `side`.
/// For cover art padded to 16:9 this keeps the art and drops the bars.
/// Blocking and CPU heavy: call from a worker thread.
pub fn decode_square(bytes: &[u8], side: u32) -> Result<DecodedImage, CacheError> {
    if side == 0 {
        return Err(CacheError::InvalidSize);
    }
    let img = load_limited(bytes)?;
    let edge = img.width().min(img.height());
    let img = img.crop_imm((img.width() - edge) / 2, (img.height() - edge) / 2, edge, edge);
    let img = if edge > side { img.resize_exact(side, side, FilterType::Triangle) } else { img };
    let (width, height) = (img.width(), img.height());
    Ok(DecodedImage { width, height, rgba: img.into_rgba8().into_raw() })
}

#[cfg(test)]
pub(crate) fn test_jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, ((x ^ y) & 0xff) as u8])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .encode_image(&img)
        .unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_crop_of_wide_image() {
        let jpg = test_jpeg(320, 180);
        let d = decode_square(&jpg, 96).unwrap();
        assert_eq!((d.width, d.height), (96, 96));
        assert_eq!(d.rgba.len(), 96 * 96 * 4);
        assert!(decode_square(&jpg, 0).is_err());
    }

    #[test]
    fn a_small_cover_is_not_scaled_up() {
        let d = decode_square(&test_jpeg(40, 30), 96).unwrap();
        assert_eq!((d.width, d.height), (30, 30));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(decode_square(b"not an image", 64).is_err());
    }
}
