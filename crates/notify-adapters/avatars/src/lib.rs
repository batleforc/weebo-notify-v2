//! The server's only image-decoding surface: avatars and attachment thumbnails (design §2.3, §2.4, §15).
//! Order is the defence: size, format from magic bytes, header dimensions (decompression bombs),
//! no animation, then decode/resize/re-encode (which strips EXIF).

use image::{DynamicImage, ImageError, ImageFormat, ImageReader, Limits};
use notify_ports::{
    AttachmentThumbnailer, AvatarProcessor, ImageRejection, PortError, PortResult, ProcessedImage,
};

/// The ceiling on an upload (design §2.4, §8.2).
pub const MAX_AVATAR_BYTES: usize = 512 * 1024;

/// The square an avatar is stored at (design §2.4).
pub const AVATAR_SIZE: u32 = 256;

/// The square a derived attachment thumbnail is stored at (design §2.3, §10.3).
pub const THUMBNAIL_SIZE: u32 = 128;

/// Max pixels a source may claim before decoding (24 MP fits); the product matters, not either side.
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;

/// The longest side a source may have; a 2 000 000 × 20 strip passes the pixel product.
pub const MAX_SOURCE_SIDE: u32 = 20_000;

/// The decoder's own allocation cap, covering buffers the header does not announce
/// (40 MP RGBA8 is 160 MB).
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// The largest thumbnail source read at all: the attachment ceiling (§8.2).
pub const MAX_THUMBNAIL_SOURCE_BYTES: usize = 25 * 1024 * 1024;

/// Decodes running at once, process-wide: each can hold [`MAX_DECODE_ALLOC`], and
/// unbounded blocking threads would run a replica out of memory with valid files.
static DECODES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// The decoder limits every reader here is built with.
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// A reader over `bytes` with its format guessed and [`limits`] applied.
fn limited_reader(bytes: &[u8]) -> Result<ImageReader<std::io::Cursor<&[u8]>>, ImageRejection> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageRejection::Unsupported)?;
    reader.limits(limits());
    Ok(reader)
}

/// A limit hit is "too large"; anything else is a file that does not parse.
fn rejected(error: &ImageError) -> ImageRejection {
    match error {
        ImageError::Limits(_) => ImageRejection::TooLarge,
        _ => ImageRejection::Corrupt,
    }
}

/// Decode PNG/JPEG/WebP with the decompression-bomb defence; shared by avatars and thumbnails (§15).
fn decode_capped(bytes: &[u8], max_source_pixels: u64) -> Result<DynamicImage, ImageRejection> {
    // Format from the bytes, never the client's `Content-Type`.
    let reader = limited_reader(bytes)?;

    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) => {}
        // Includes GIF: only static formats are decoded.
        _ => return Err(ImageRejection::Unsupported),
    }

    // Header only: must run before the pixels are allocated.
    let (width, height) = reader.into_dimensions().map_err(|error| rejected(&error))?;
    if u64::from(width) * u64::from(height) > max_source_pixels {
        return Err(ImageRejection::TooLarge);
    }

    limited_reader(bytes)?
        .decode()
        .map_err(|error| rejected(&error))
}

/// Crop to a square and re-encode as lossless PNG, whatever came in.
fn square_png(decoded: &DynamicImage, size: u32) -> Result<ProcessedImage, ImageRejection> {
    let square = decoded.resize_to_fill(size, size, image::imageops::FilterType::Lanczos3);

    let mut out = std::io::Cursor::new(Vec::new());
    square
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|_| ImageRejection::Corrupt)?;

    Ok(ProcessedImage {
        content_type: "image/png".to_owned(),
        bytes: out.into_inner(),
    })
}

/// Up to 40 MP plus a Lanczos3 resize is seconds of CPU; keep it off async workers,
/// at most [`DECODES`] at a time.
async fn off_the_runtime(
    bytes: &[u8],
    run: fn(&[u8]) -> Result<ProcessedImage, ImageRejection>,
) -> PortResult<Result<ProcessedImage, ImageRejection>> {
    // Held until the blocking task finishes, not until it is spawned: the
    // permit is what bounds the memory, and the memory lives in the task.
    let _permit = DECODES
        .acquire()
        .await
        .map_err(|error| PortError::unavailable("images", error))?;
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || run(&bytes))
        .await
        .map_err(|error| PortError::unavailable("images", error))
}

/// Decodes avatars with the rules of §2.4 applied in the order of §15.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageAvatarProcessor;

impl ImageAvatarProcessor {
    /// Build one.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Everything except the async wrapper, so the rules are testable directly.
    fn run(bytes: &[u8]) -> Result<ProcessedImage, ImageRejection> {
        if bytes.len() > MAX_AVATAR_BYTES {
            return Err(ImageRejection::TooLarge);
        }

        square_png(&decode_capped(bytes, MAX_SOURCE_PIXELS)?, AVATAR_SIZE)
    }
}

#[async_trait::async_trait]
impl AvatarProcessor for ImageAvatarProcessor {
    async fn process(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>> {
        // Checked before the copy `off_the_runtime` makes.
        if bytes.len() > MAX_AVATAR_BYTES {
            return Ok(Err(ImageRejection::TooLarge));
        }
        let outcome = off_the_runtime(bytes, Self::run).await?;
        if let Err(rejection) = &outcome {
            // Debug: refusals are ordinary and must not be a way to flood logs.
            tracing::debug!(%rejection, "avatar refused");
        }
        Ok(outcome)
    }
}

/// Derives thumbnails with the avatar rules, cropped to [`THUMBNAIL_SIZE`] (design §2.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageAttachmentThumbnailer;

impl ImageAttachmentThumbnailer {
    /// Build one.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Bounded again here: the filesystem store or a misconfigured bucket bypass the upload ceiling.
    fn run(bytes: &[u8]) -> Result<ProcessedImage, ImageRejection> {
        if bytes.len() > MAX_THUMBNAIL_SOURCE_BYTES {
            return Err(ImageRejection::TooLarge);
        }
        square_png(&decode_capped(bytes, MAX_SOURCE_PIXELS)?, THUMBNAIL_SIZE)
    }
}

#[async_trait::async_trait]
impl AttachmentThumbnailer for ImageAttachmentThumbnailer {
    async fn thumbnail(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>> {
        // Before the copy and before queueing for a permit, like the avatar.
        if bytes.len() > MAX_THUMBNAIL_SOURCE_BYTES {
            return Ok(Err(ImageRejection::TooLarge));
        }
        let outcome = off_the_runtime(bytes, Self::run).await?;
        if let Err(rejection) = &outcome {
            // Debug: an unsupported source just means no thumbnail.
            tracing::debug!(%rejection, "attachment thumbnail not derived");
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use image::{DynamicImage, RgbaImage};

    use super::*;

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::new(width, height));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_png_comes_back_as_a_256_square_png() {
        let processed = ImageAvatarProcessor::run(&encoded(800, 600, ImageFormat::Png)).unwrap();

        assert_eq!(processed.content_type, "image/png");

        let decoded = image::load_from_memory(&processed.bytes).unwrap();
        assert_eq!(decoded.width(), AVATAR_SIZE);
        assert_eq!(decoded.height(), AVATAR_SIZE);
    }

    #[test]
    fn a_jpeg_is_accepted_and_re_encoded_rather_than_passed_through() {
        // The re-encode strips EXIF, so "output is PNG" proves metadata is gone (§2.4).
        let processed = ImageAvatarProcessor::run(&encoded(400, 400, ImageFormat::Jpeg)).unwrap();

        assert_eq!(processed.content_type, "image/png");
        assert_eq!(
            image::guess_format(&processed.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn a_format_we_do_not_accept_is_refused_by_its_bytes() {
        let gif = encoded(10, 10, ImageFormat::Gif);

        assert_eq!(
            ImageAvatarProcessor::run(&gif),
            Err(ImageRejection::Unsupported)
        );
    }

    #[test]
    fn something_that_is_not_an_image_is_refused_rather_than_decoded() {
        assert!(matches!(
            ImageAvatarProcessor::run(b"just some bytes, honestly"),
            Err(ImageRejection::Unsupported | ImageRejection::Corrupt)
        ));
    }

    #[test]
    fn a_file_over_the_ceiling_never_reaches_a_decoder() {
        let oversized = vec![0_u8; MAX_AVATAR_BYTES + 1];

        assert_eq!(
            ImageAvatarProcessor::run(&oversized),
            Err(ImageRejection::TooLarge)
        );
    }

    /// A real tiny PNG whose IHDR announces `width` × `height`: a decompression bomb
    /// without allocating one.
    fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFF_u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    let mask = 0_u32.wrapping_sub(crc & 1);
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }

        // 8-byte signature, then IHDR: 4 length, 4 type, then width and height.
        let mut png = encoded(1, 1, ImageFormat::Png);
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    #[test]
    fn a_decompression_bomb_is_refused_on_its_header() {
        // 30 000 × 30 000 RGBA is 3.6 GB decoded, and a few bytes announced.
        let bomb = png_claiming(30_000, 30_000);
        assert!(
            bomb.len() < MAX_AVATAR_BYTES,
            "the fixture has to be small, or the size ceiling would refuse it first"
        );

        assert_eq!(
            ImageAvatarProcessor::run(&bomb),
            Err(ImageRejection::TooLarge)
        );
    }

    #[test]
    fn the_bomb_fixture_is_a_png_this_build_would_otherwise_read() {
        // The header must be valid enough that the dimension check is what refuses it.
        let ordinary = png_claiming(64, 64);

        let reader = ImageReader::new(std::io::Cursor::new(&ordinary))
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format(), Some(ImageFormat::Png));
        assert_eq!(reader.into_dimensions().unwrap(), (64, 64));
    }

    #[tokio::test]
    async fn the_port_returns_a_rejection_rather_than_an_error() {
        // A bad file is `Ok(Err(_))`, i.e. a `422`.
        let outcome = ImageAvatarProcessor::new().process(b"nope").await.unwrap();

        assert!(outcome.is_err());
    }

    // --- The thumbnailer shares the decoder ---

    #[test]
    fn a_png_comes_back_as_a_128_square_png() {
        let processed =
            ImageAttachmentThumbnailer::run(&encoded(800, 600, ImageFormat::Png)).unwrap();

        assert_eq!(processed.content_type, "image/png");

        let decoded = image::load_from_memory(&processed.bytes).unwrap();
        assert_eq!(decoded.width(), THUMBNAIL_SIZE);
        assert_eq!(decoded.height(), THUMBNAIL_SIZE);
    }

    /// A noisy PNG, so deflate cannot make the size assertion vacuous.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "pseudo-noise for a test fixture; the low bytes of the mix are the point"
    )]
    fn noisy_encoded(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = RgbaImage::new(width, height);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            let mix = x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40_503));
            *pixel = image::Rgba([mix as u8, (mix >> 8) as u8, (mix >> 16) as u8, 255]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn a_thumbnail_source_is_not_held_to_the_avatar_byte_ceiling() {
        // The 512 KiB ceiling is avatar-only; larger attachment sources must still thumbnail.
        let oversized_for_an_avatar = noisy_encoded(1_000, 1_000);
        assert!(oversized_for_an_avatar.len() > MAX_AVATAR_BYTES);

        assert!(ImageAttachmentThumbnailer::run(&oversized_for_an_avatar).is_ok());
    }

    #[test]
    fn a_thumbnail_source_over_the_attachment_ceiling_is_never_decoded() {
        let oversized = vec![0_u8; MAX_THUMBNAIL_SOURCE_BYTES + 1];

        assert_eq!(
            ImageAttachmentThumbnailer::run(&oversized),
            Err(ImageRejection::TooLarge)
        );
    }

    #[test]
    fn a_strip_within_the_pixel_budget_is_refused_on_its_longest_side() {
        // 40 000 × 10 is 400 000 pixels — nowhere near the product cap — and
        // still twice the side any decoder here will accept.
        assert_eq!(
            ImageAvatarProcessor::run(&png_claiming(40_000, 10)),
            Err(ImageRejection::TooLarge)
        );
    }

    #[test]
    fn an_animated_format_is_not_thumbnailed() {
        let gif = encoded(10, 10, ImageFormat::Gif);

        assert_eq!(
            ImageAttachmentThumbnailer::run(&gif),
            Err(ImageRejection::Unsupported)
        );
    }

    #[test]
    fn a_thumbnail_decompression_bomb_is_refused_on_its_header_too() {
        let bomb = png_claiming(30_000, 30_000);

        assert_eq!(
            ImageAttachmentThumbnailer::run(&bomb),
            Err(ImageRejection::TooLarge)
        );
    }

    #[tokio::test]
    async fn the_thumbnailer_port_returns_a_rejection_rather_than_an_error() {
        let outcome = ImageAttachmentThumbnailer::new()
            .thumbnail(b"nope")
            .await
            .unwrap();

        assert!(outcome.is_err());
    }
}
