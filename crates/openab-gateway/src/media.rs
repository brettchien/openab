use image::ImageReader;
use std::io::Cursor;

/// Media type for download functions — avoids stringly-typed branching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
}

pub const IMAGE_MAX_DIMENSION_PX: u32 = 1200;
pub const IMAGE_JPEG_QUALITY: u8 = 75;
pub const IMAGE_MAX_DOWNLOAD: u64 = 10 * 1024 * 1024; // 10 MB
pub const FILE_MAX_DOWNLOAD: u64 = 20 * 1024 * 1024; // 20 MB (same as store cap)
pub const AUDIO_MAX_DOWNLOAD: u64 = 20 * 1024 * 1024; // 20 MB
pub const GIF_MAX_SIZE: usize = 5 * 1024 * 1024; // 5 MB — prevents base64 bloat exceeding LLM payload limits

/// Resize image so longest side <= 1200px, then encode as JPEG.
/// GIFs under 5MB are passed through unchanged to preserve animation.
pub fn resize_and_compress(raw: &[u8]) -> Result<(Vec<u8>, String), image::ImageError> {
    resize_and_compress_limited(raw, None)
}

/// Longest side, in pixels, [`resize_and_compress_bounded`] will decode.
pub const IMAGE_DECODE_MAX_DIMENSION_PX: u32 = 10_000;
/// Decoder allocation cap for [`resize_and_compress_bounded`] — room for a 5K RGBA screenshot.
pub const IMAGE_DECODE_MAX_ALLOC: u64 = 128 * 1024 * 1024;

/// [`resize_and_compress`] with decoder limits, for images a remote client sends directly.
///
/// The encoded-size caps bound the bytes, not the pixels: a few hundred KiB of flat-colour PNG
/// can declare tens of thousands of pixels a side and decode to hundreds of MiB under the `image`
/// crate's defaults (no dimension cap, 512 MiB allocation). These limits reject such an image
/// before its pixels are allocated.
pub fn resize_and_compress_bounded(raw: &[u8]) -> Result<(Vec<u8>, String), image::ImageError> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_DECODE_MAX_DIMENSION_PX);
    limits.max_image_height = Some(IMAGE_DECODE_MAX_DIMENSION_PX);
    limits.max_alloc = Some(IMAGE_DECODE_MAX_ALLOC);
    resize_and_compress_limited(raw, Some(limits))
}

fn resize_and_compress_limited(
    raw: &[u8],
    limits: Option<image::Limits>,
) -> Result<(Vec<u8>, String), image::ImageError> {
    let mut reader = ImageReader::new(Cursor::new(raw)).with_guessed_format()?;
    if let Some(limits) = limits {
        reader.limits(limits);
    }
    let format = reader.format();
    if format == Some(image::ImageFormat::Gif) {
        if raw.len() > GIF_MAX_SIZE {
            return Err(image::ImageError::Limits(
                image::error::LimitError::from_kind(image::error::LimitErrorKind::DimensionError),
            ));
        }
        return Ok((raw.to_vec(), "image/gif".to_string()));
    }
    let img = reader.decode()?;
    let (w, h) = (img.width(), img.height());
    let img = if w > IMAGE_MAX_DIMENSION_PX || h > IMAGE_MAX_DIMENSION_PX {
        let max_side = std::cmp::max(w, h);
        let ratio = f64::from(IMAGE_MAX_DIMENSION_PX) / f64::from(max_side);
        let new_w = (f64::from(w) * ratio) as u32;
        let new_h = (f64::from(h) * ratio) as u32;
        img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, IMAGE_JPEG_QUALITY);
    img.write_with_encoder(encoder)?;
    Ok((buf.into_inner(), "image/jpeg".to_string()))
}

/// Derive file extension from Content-Type for audio files.
pub fn audio_extension(content_type: &str) -> &'static str {
    if content_type.contains("mpeg") || content_type.contains("mp3") {
        "mp3"
    } else if content_type.contains("m4a") || content_type.contains("mp4") {
        "m4a"
    } else {
        "ogg"
    }
}

/// Check if a filename has a text-like extension suitable for reading as UTF-8.
pub fn is_text_extension(filename: &str) -> bool {
    const TEXT_EXTS: &[&str] = &[
        "txt", "csv", "log", "md", "json", "jsonl", "yaml", "yml", "toml", "xml", "rs", "py", "js",
        "ts", "jsx", "tsx", "go", "java", "c", "cpp", "h", "hpp", "rb", "sh", "bash", "sql",
        "html", "css", "ini", "cfg", "conf",
    ];
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    TEXT_EXTS.contains(&ext.as_str())
}

/// Format a byte count as a human-readable string (B / KB / MB).
pub fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG whose header declares more pixels than the bounded decoder allows is rejected
    /// before decode, even though it is only a few bytes on the wire.
    #[test]
    fn bounded_rejects_oversized_dimensions() {
        let img = image::GrayImage::new(IMAGE_DECODE_MAX_DIMENSION_PX + 1, 1);
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        assert!(png.len() < 64 * 1024, "the point is a small file");
        assert!(matches!(
            resize_and_compress_bounded(&png),
            Err(image::ImageError::Limits(_))
        ));
    }

    #[test]
    fn bounded_accepts_an_ordinary_image() {
        let img = image::RgbImage::from_pixel(1600, 900, image::Rgb([10, 20, 30]));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (_, mime) = resize_and_compress_bounded(&png.into_inner()).unwrap();
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn gif_under_limit_passes_through() {
        let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00!\xf9\x04\x00\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";
        let result = resize_and_compress(gif);
        assert!(result.is_ok());
        let (data, mime) = result.unwrap();
        assert_eq!(mime, "image/gif");
        assert_eq!(data, gif);
    }

    #[test]
    fn gif_over_limit_returns_error() {
        let mut data = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00".to_vec();
        data.resize(GIF_MAX_SIZE + 1, 0);
        let result = resize_and_compress(&data);
        assert!(result.is_err());
    }

    #[test]
    fn small_jpeg_not_resized() {
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        let result = resize_and_compress(&buf.into_inner());
        assert!(result.is_ok());
        assert_eq!(result.unwrap().1, "image/jpeg");
    }

    #[test]
    fn large_image_gets_resized() {
        let img = image::RgbImage::from_pixel(2000, 2000, image::Rgb([0, 128, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        let result = resize_and_compress(&buf.into_inner());
        assert!(result.is_ok());
        let (data, mime) = result.unwrap();
        assert_eq!(mime, "image/jpeg");
        let decoded = image::load_from_memory(&data).unwrap();
        assert!(decoded.width() <= IMAGE_MAX_DIMENSION_PX);
        assert!(decoded.height() <= IMAGE_MAX_DIMENSION_PX);
    }

    #[test]
    fn text_extension_check() {
        assert!(is_text_extension("main.rs"));
        assert!(is_text_extension("data.csv"));
        assert!(!is_text_extension("archive.zip"));
        assert!(!is_text_extension("photo.jpg"));
    }

    #[test]
    fn audio_extension_detects_common_line_formats() {
        assert_eq!(audio_extension("audio/ogg"), "ogg");
        assert_eq!(audio_extension("audio/mpeg"), "mp3");
        assert_eq!(audio_extension("audio/mp4"), "m4a");
        assert_eq!(audio_extension("audio/x-m4a"), "m4a");
    }
}
