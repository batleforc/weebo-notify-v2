//! Decoding an avatar (design §2.4, §15).
//!
//! # Why this is a port and not a function call
//!
//! §15 names avatar decoding **the only media-decoding surface the server has**:
//! bytes supplied by a user, handed to a codec, inside the process that holds
//! everybody's notifications. That earns the same treatment as password hashing —
//! a named contract with the rules written down, and an implementation swappable
//! behind it (the [`SecretHasher`] precedent).
//!
//! It also means the rules live here, where they can be read, rather than being
//! implied by whichever decoder happens to be wired in:
//!
//! * **the dimensions are checked before the image is fully decoded** — that is the
//!   decompression-bomb defence, and it only works if the check happens on the
//!   header rather than on the result;
//! * **animated formats are refused** — an animation is a loop over frames, and a
//!   loop over frames is an amplification factor;
//! * **the output is re-encoded, never passed through** — which is what actually
//!   strips the metadata. An avatar taken on a phone carries GPS coordinates in
//!   EXIF, and "we removed the tags we know about" is a weaker promise than "we
//!   wrote a new file from the pixels".
//!
//! [`SecretHasher`]: crate::identities::SecretHasher

use crate::error::PortResult;

/// A decoded, resized, metadata-free image ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    /// What it was encoded as, e.g. `image/png`.
    pub content_type: String,
    /// The bytes to store.
    pub bytes: Vec<u8>,
}

/// Why an avatar was refused.
///
/// Separate from [`crate::error::PortError`] because none of these are the world
/// misbehaving: they are a caller's file being wrong, and they map to a `422` with
/// a message somebody can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageRejection {
    /// The bytes are not an image this build can read.
    #[error("not a supported image; use PNG, JPEG or WebP")]
    Unsupported,
    /// The declared dimensions exceed the ceiling, before any full decode.
    #[error("the image is larger than the maximum accepted dimensions")]
    TooLarge,
    /// An animated image: refused whatever its size.
    #[error("animated images are not accepted as avatars")]
    Animated,
    /// It decoded, but into something unusable.
    #[error("the image could not be decoded")]
    Corrupt,
}

/// Turns user-supplied bytes into something safe to store and serve.
#[async_trait::async_trait]
pub trait AvatarProcessor: Send + Sync {
    /// Decode, cap, resize to the avatar square and re-encode.
    ///
    /// Returns `Err(ImageRejection)` for anything wrong with the input — the caller
    /// turns that into a `422`. A `PortError` is reserved for the implementation
    /// itself failing.
    async fn process(&self, bytes: &[u8]) -> PortResult<Result<ProcessedImage, ImageRejection>>;
}
