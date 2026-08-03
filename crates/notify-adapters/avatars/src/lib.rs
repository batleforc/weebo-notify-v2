//! Avatar decoding (design §2.4, §15).
//!
//! This crate is the server's **only** media-decoding surface, which is why it is
//! small, why its dependency list is explicit about which decoders exist, and why
//! the order of operations below is not an implementation detail.
//!
//! # The order is the defence
//!
//! 1. **Size, before anything is parsed.** A 512 KiB ceiling costs a comparison.
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

use image::{ImageFormat, ImageReader};
use notify_ports::{AvatarProcessor, ImageRejection, PortResult, ProcessedImage};

/// The ceiling on an upload (design §2.4, §8.2).
pub const MAX_AVATAR_BYTES: usize = 512 * 1024;

/// The square an avatar is stored at (design §2.4).
pub const AVATAR_SIZE: u32 = 256;

/// The most pixels a source image may claim before it is decoded.
///
/// Generous for a photograph — a 24-megapixel camera fits — and far below what it
/// takes to exhaust a replica. The number that matters is the product, not either
/// side: a 100 000 × 1 image is as much of a problem as a square one.
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;

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

        // The format comes from the bytes. A client's `Content-Type` is a claim, and
        // a JPEG announced as a PNG must be treated as what it is or refused as what
        // it claims — never decoded as the other.
        let reader = ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| ImageRejection::Unsupported)?;

        match reader.format() {
            Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) => {}
            // Includes GIF, which is refused for being animated in spirit even when
            // a particular file is not: §2.4 says static formats only.
            _ => return Err(ImageRejection::Unsupported),
        }

        // The header, and only the header. This is the check that has to happen
        // before the pixels are allocated.
        let (width, height) = reader
            .into_dimensions()
            .map_err(|_| ImageRejection::Corrupt)?;
        if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
            return Err(ImageRejection::TooLarge);
        }

        let decoded = ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| ImageRejection::Unsupported)?
            .decode()
            .map_err(|_| ImageRejection::Corrupt)?;

        // `resize_to_fill` crops to the square rather than squashing: a face that
        // arrives 16:9 should not come out oval.
        let square = decoded.resize_to_fill(
            AVATAR_SIZE,
            AVATAR_SIZE,
            image::imageops::FilterType::Lanczos3,
        );

        let mut out = std::io::Cursor::new(Vec::new());
        square
            .write_to(&mut out, ImageFormat::Png)
            .map_err(|_| ImageRejection::Corrupt)?;

        Ok(ProcessedImage {
            // PNG whatever came in: one output format means one decoder for every
            // client that renders an avatar, and lossless means a re-upload of the
            // same picture does not decay.
            content_type: "image/png".to_owned(),
            bytes: out.into_inner(),
        })
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
}
