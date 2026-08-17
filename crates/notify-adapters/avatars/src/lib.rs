//! Image decoding (design §2.4, §2.3, §15).
//!
//! This crate is the server's **only** media-decoding surface — a boundary D30
//! widened from "the avatar" to "the avatar and an attachment's derived
//! thumbnail", both decoded by the same rules below, which is why it stays one
//! crate rather than two. The dependency list is explicit about which decoders
//! exist, and the order of operations is not an implementation detail.
//!
//! # The order is the defence
//!
//! 1. **Size, before anything is parsed**, where the caller can cheaply enforce
//!    one — an avatar's 512 KiB ceiling costs a comparison. A thumbnail's source
//!    has no separate ceiling here: it was already bounded at upload time by
//!    `MAX_ATTACHMENT_BYTES` (design §8.2), enforced in the presigned URL that
//!    was the only way those bytes could have reached the bucket.
//! 2. **Format, from the magic bytes** — not from the `Content-Type` a client sent,
//!    which is a claim rather than a fact.
//! 3. **Dimensions, from the header, before a full decode.** This is the
//!    decompression-bomb defence and it only works here: 30 000 × 30 000 pixels is
//!    a few kilobytes of PNG and 3.6 GB of decoded RGBA. Checking after decoding
//!    would mean the allocation already happened.
//! 4. **Animation, refused.** An animated format is a loop over frames, which is an
//!    amplification factor on everything above.
//! 5. **Decode, resize, re-encode.** The re-encode is what strips metadata — an
//!    avatar taken on a phone carries GPS coordinates in EXIF, and writing a new
//!    file from the pixels is a stronger promise than deleting the tags we thought
//!    to look for.

use image::{DynamicImage, ImageFormat, ImageReader};
use notify_ports::{
    AttachmentThumbnailer, AvatarProcessor, ImageRejection, PortResult, ProcessedImage,
};

/// The ceiling on an upload (design §2.4, §8.2).
pub const MAX_AVATAR_BYTES: usize = 512 * 1024;

/// The square an avatar is stored at (design §2.4).
pub const AVATAR_SIZE: u32 = 256;

/// The square a row's derived thumbnail is stored at (design §2.3, §10.3, D30).
///
/// Smaller than the avatar: a thumbnail sits next to a title in a list row,
/// where §10.3's other furniture (a status dot, badges, timestamps) already
/// shares the line, rather than filling a settings page on its own.
pub const THUMBNAIL_SIZE: u32 = 128;

/// The most pixels a source image may claim before it is decoded.
///
/// Generous for a photograph — a 24-megapixel camera fits — and far below what it
/// takes to exhaust a replica. The number that matters is the product, not either
/// side: a 100 000 × 1 image is as much of a problem as a square one.
///
/// Shared between the avatar and the thumbnail: the defence is against decoded
/// pixel count, which the encoded byte ceiling upstream of each caller does not
/// bound on its own — a small file can still claim an enormous canvas.
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;

/// What [`ImageAvatarProcessor`] and [`ImageAttachmentThumbnailer`] share: decode
/// with the decompression-bomb defence applied, refusing anything this build does
/// not read.
///
/// Not parameterised over which formats are allowed — both callers accept
/// exactly PNG, JPEG and WebP today, and a thumbnailer that accepted more than
/// the avatar it shares a decoder with would be a second, silently different
/// answer to "what does this project decode" (§15).
fn decode_capped(bytes: &[u8], max_source_pixels: u64) -> Result<DynamicImage, ImageRejection> {
    // The format comes from the bytes. A client's `Content-Type` is a claim, and
    // a JPEG announced as a PNG must be treated as what it is or refused as what
    // it claims — never decoded as the other.
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageRejection::Unsupported)?;

    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) => {}
        // Includes GIF, which is refused for being animated in spirit even when
        // a particular file is not: only static formats are decoded here.
        _ => return Err(ImageRejection::Unsupported),
    }

    // The header, and only the header. This is the check that has to happen
    // before the pixels are allocated.
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| ImageRejection::Corrupt)?;
    if u64::from(width) * u64::from(height) > max_source_pixels {
        return Err(ImageRejection::TooLarge);
    }

    ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageRejection::Unsupported)?
        .decode()
        .map_err(|_| ImageRejection::Corrupt)
}

/// `resize_to_fill` plus the re-encode both processors finish with.
///
/// Cropped to the square rather than squashed — a face, or a screenshot's
/// subject, that arrives 16:9 should not come out oval — and re-encoded as PNG
/// whatever came in: one output format means one decoder for every client that
/// renders the result, and lossless means a re-derivation of the same source
/// does not decay.
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
        let outcome = Self::run(bytes);
        if let Err(rejection) = &outcome {
            // Refusals are logged at debug: they are ordinary, and a bad upload must
            // not be a way to fill somebody's log.
            tracing::debug!(%rejection, "avatar refused");
        }
        Ok(outcome)
    }
}

/// Derives thumbnails with the same rules `ImageAvatarProcessor` applies,
/// cropped to [`THUMBNAIL_SIZE`] instead (design §2.3, D30).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageAttachmentThumbnailer;

impl ImageAttachmentThumbnailer {
    /// Build one.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Everything except the async wrapper, so the rules are testable directly.
    ///
    /// No byte-size ceiling here — see the module docs: the source already
    /// passed through one, at upload, before this crate ever saw it.
    fn run(bytes: &[u8]) -> Result<ProcessedImage, ImageRejection> {
        square_png(&decode_capped(bytes, MAX_SOURCE_PIXELS)?, THUMBNAIL_SIZE)
    }
}

#[async_trait::async_trait]
impl AttachmentThumbnailer for ImageAttachmentThumbnailer {
    async fn thumbnail(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>> {
        let outcome = Self::run(bytes);
        if let Err(rejection) = &outcome {
            // Debug, not warn: an unsupported source (a GIF, say) is an ordinary
            // outcome nobody is paged for — the row simply shows no thumbnail.
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
        // The re-encode is what strips EXIF, so "the output is a PNG" is the
        // assertion that metadata cannot have survived (§2.4).
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

    /// A real, tiny PNG whose header has been rewritten to announce `width` by
    /// `height`.
    ///
    /// This is what a decompression bomb *is*: a small file that claims to be an
    /// enormous image. Building it by re-encoding a genuinely huge one would spend a
    /// minute allocating the very gigabytes this rule exists to refuse, and would
    /// come out over the size ceiling — so the test would pass for the wrong reason.
    /// Patching a real file keeps every other chunk valid, so what the decoder
    /// objects to is the size and nothing else.
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

        // 8-byte signature, then the IHDR chunk: 4 length, 4 type, 13 data, 4 CRC.
        // Width and height are the first two fields of that data.
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
        // Guards the test above from rotting into "an unparseable file is refused":
        // the header has to be valid enough that the dimension check is what runs.
        let ordinary = png_claiming(64, 64);

        let reader = ImageReader::new(std::io::Cursor::new(&ordinary))
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format(), Some(ImageFormat::Png));
        assert_eq!(reader.into_dimensions().unwrap(), (64, 64));
    }

    #[tokio::test]
    async fn the_port_returns_a_rejection_rather_than_an_error() {
        // A caller's bad file is a `422`, not a failure of the world: the port says
        // so by returning `Ok(Err(_))`.
        let outcome = ImageAvatarProcessor::new().process(b"nope").await.unwrap();

        assert!(outcome.is_err());
    }

    // --- The thumbnailer shares the decoder, not just the rules (D30) ----------

    #[test]
    fn a_png_comes_back_as_a_128_square_png() {
        let processed =
            ImageAttachmentThumbnailer::run(&encoded(800, 600, ImageFormat::Png)).unwrap();

        assert_eq!(processed.content_type, "image/png");

        let decoded = image::load_from_memory(&processed.bytes).unwrap();
        assert_eq!(decoded.width(), THUMBNAIL_SIZE);
        assert_eq!(decoded.height(), THUMBNAIL_SIZE);
    }

    /// A PNG PNG-compression cannot shrink much, unlike `encoded`'s flat colour —
    /// deflate crushes a single-colour image to a few hundred bytes regardless of
    /// its dimensions, which would make the size assertion below vacuous.
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
        // The avatar's 512 KiB ceiling is a caller-side check on bytes a stranger
        // posted; a thumbnail's source came out of this server's own bucket, where
        // it was already bounded by `MAX_ATTACHMENT_BYTES` at upload time (§8.2).
        // A source larger than an avatar could ever be must still thumbnail.
        let oversized_for_an_avatar = noisy_encoded(1_000, 1_000);
        assert!(oversized_for_an_avatar.len() > MAX_AVATAR_BYTES);

        assert!(ImageAttachmentThumbnailer::run(&oversized_for_an_avatar).is_ok());
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
        // The same fixture and the same defence as the avatar's — the whole point
        // of sharing `decode_capped` rather than writing this check twice.
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
