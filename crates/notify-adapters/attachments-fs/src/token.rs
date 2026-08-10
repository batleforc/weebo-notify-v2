//! Signing the bearer URLs the filesystem store hands out.
//!
//! An S3 bucket signs its own presigned URLs (`SigV4`); a directory on disk has no
//! signature of its own, so this crate mints one instead. Same construction as the
//! webhook signer (`notify-sinks-webhook`) — HMAC-SHA256 over a short string, hex
//! encoded, compared in constant time — because that is already a reviewed answer
//! to "how do we sign a short string in this codebase", not a reason to invent a
//! second one.

use core::fmt::Write as _;

use hmac::{Hmac, Mac as _};
use notify_domain::AttachmentId;
use sha2::Sha256;
use time::OffsetDateTime;

/// What a token authorises.
///
/// Bound into the signature so an upload token cannot be replayed as a download.
/// A thumbnail has no purpose of its own here: it is served by the existing
/// authenticated `GET /attachments/{id}/thumbnail` route, backend-agnostic since
/// M1, and never a filesystem-presigned URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Upload,
    Download,
}

impl Purpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Not `<expires_unix>.<hex digest>`.
    Malformed,
    /// Past the expiry it names.
    Expired,
    /// The digest does not match this id, purpose and expiry under this secret.
    Mismatch,
}

/// Mint a token good until `expires_at`, for `id` and `purpose`.
#[must_use]
pub fn sign(
    secret: &[u8],
    purpose: Purpose,
    id: AttachmentId,
    expires_at: OffsetDateTime,
) -> String {
    let expires_unix = expires_at.unix_timestamp();
    let digest = digest(secret, purpose, id, expires_unix);

    let mut token = format!("{expires_unix}.");
    for byte in digest {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

/// Check that a token names this `id` and `purpose`, and has not expired.
///
/// # Errors
///
/// See [`TokenError`].
pub fn verify(
    secret: &[u8],
    purpose: Purpose,
    id: AttachmentId,
    token: &str,
    now: OffsetDateTime,
) -> Result<(), TokenError> {
    let (expires_raw, given_hex) = token.split_once('.').ok_or(TokenError::Malformed)?;
    let expires_unix: i64 = expires_raw.parse().map_err(|_| TokenError::Malformed)?;

    if now.unix_timestamp() > expires_unix {
        return Err(TokenError::Expired);
    }

    let expected = digest(secret, purpose, id, expires_unix);
    let mut expected_hex = String::with_capacity(expected.len() * 2);
    for byte in expected {
        let _ = write!(expected_hex, "{byte:02x}");
    }

    // Constant time over the rendered hex, same reasoning as the webhook signer:
    // a length mismatch is public (SHA-256 hex is always 64 characters), but which
    // byte first differs must not be.
    let matched = expected_hex.len() == given_hex.len()
        && expected_hex
            .bytes()
            .zip(given_hex.bytes())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0;

    if matched {
        Ok(())
    } else {
        Err(TokenError::Mismatch)
    }
}

fn digest(secret: &[u8], purpose: Purpose, id: AttachmentId, expires_unix: i64) -> Vec<u8> {
    // HMAC accepts a key of any length, so the fallback is unreachable — an
    // infallible zero key rather than an `expect`, because panicking is denied in
    // this workspace and the worst a wrong key can do is mint a token nobody signed
    // with the real secret would accept.
    let mut mac = <Hmac<Sha256> as hmac::Mac>::new_from_slice(secret).unwrap_or_else(|_| {
        <Hmac<Sha256> as hmac::digest::KeyInit>::new(&hmac::digest::Key::<Hmac<Sha256>>::default())
    });

    mac.update(purpose.as_str().as_bytes());
    mac.update(b".");
    mac.update(id.to_string().as_bytes());
    mac.update(b".");
    mac.update(expires_unix.to_string().as_bytes());

    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    const SECRET: &[u8] = b"shh";

    fn ttl(now: OffsetDateTime) -> OffsetDateTime {
        now + time::Duration::minutes(15)
    }

    #[test]
    fn a_token_verifies_for_the_id_and_purpose_it_was_signed_for() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, Purpose::Upload, id, ttl(now));

        assert_eq!(verify(SECRET, Purpose::Upload, id, &token, now), Ok(()));
    }

    #[test]
    fn an_upload_token_does_not_verify_as_a_download() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, Purpose::Upload, id, ttl(now));

        assert_eq!(
            verify(SECRET, Purpose::Download, id, &token, now),
            Err(TokenError::Mismatch)
        );
    }

    #[test]
    fn a_token_does_not_verify_for_a_different_id() {
        let now = OffsetDateTime::now_utc();
        let token = sign(
            SECRET,
            Purpose::Download,
            AttachmentId::generate(),
            ttl(now),
        );

        assert_eq!(
            verify(
                SECRET,
                Purpose::Download,
                AttachmentId::generate(),
                &token,
                now
            ),
            Err(TokenError::Mismatch)
        );
    }

    #[test]
    fn an_expired_token_is_refused() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(
            SECRET,
            Purpose::Download,
            id,
            now - time::Duration::seconds(1),
        );

        assert_eq!(
            verify(SECRET, Purpose::Download, id, &token, now),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn another_secret_does_not_verify() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, Purpose::Download, id, ttl(now));

        assert_eq!(
            verify(b"guess", Purpose::Download, id, &token, now),
            Err(TokenError::Mismatch)
        );
    }

    #[test]
    fn a_token_that_is_not_the_shape_at_all_is_malformed_rather_than_a_mismatch() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();

        for token in ["", "notanumber.abcd", "12345"] {
            assert_eq!(
                verify(SECRET, Purpose::Download, id, token, now),
                Err(TokenError::Malformed),
                "`{token}` is not a token"
            );
        }
    }
}
