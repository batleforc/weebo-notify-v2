//! Decoding user-supplied images: avatars and attachment thumbnails (§2.4, §15).
//! Implementations must check dimensions before a full decode (decompression bombs),
//! refuse animations, and re-encode from pixels so EXIF/GPS metadata is stripped.

use crate::error::PortResult;

/// A decoded, resized, metadata-free image ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    /// What it was encoded as, e.g. `image/png`.
    pub content_type: String,
    /// The bytes to store.
    pub bytes: Vec<u8>,
}

/// Why an image was refused: the caller's file is wrong, mapped to a `422`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageRejection {
    /// The bytes are not an image this build can read.
    #[error("not a supported image; use PNG, JPEG or WebP")]
    Unsupported,
    /// The declared dimensions exceed the ceiling, before any full decode.
    #[error("the image is larger than the maximum accepted dimensions")]
    TooLarge,
    /// An animated image: refused whatever its size.
    #[error("animated images are not accepted")]
    Animated,
    /// It decoded, but into something unusable.
    #[error("the image could not be decoded")]
    Corrupt,
}

/// Turns user-supplied bytes into something safe to store and serve.
#[async_trait::async_trait]
pub trait AvatarProcessor: Send + Sync {
    /// Decode, cap, resize to the avatar square and re-encode. `Err(ImageRejection)`
    /// is a bad input; a `PortError` is the implementation failing.
    async fn process(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>>;
}

/// Turns an attachment's bytes into a list-row thumbnail (§10.3), with the same
/// defences as [`AvatarProcessor`]. Best-effort: rejections never reach HTTP.
#[async_trait::async_trait]
pub trait AttachmentThumbnailer: Send + Sync {
    /// Decode, crop to the thumbnail square and re-encode.
    async fn thumbnail(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>>;
}
