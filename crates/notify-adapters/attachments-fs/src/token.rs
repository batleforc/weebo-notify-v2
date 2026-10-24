//! Bearer tokens for the filesystem store's URLs: HMAC-SHA256, hex, constant-time compare,
//! the same construction as the webhook signer.

use hmac::{Hmac, Mac as _};
use notify_domain::AttachmentId;
use sha2::Sha256;
use time::OffsetDateTime;

/// What a token authorises; bound into the signature so an upload token cannot be a download.
/// `Upload` pins the content type and length (as `SigV4` signs them), so approval for a 1 KiB
/// `image/png` cannot store 25 MiB of `text/html`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose<'a> {
    /// Store exactly `content_length` bytes of exactly this (normalised) content
    /// type, and nothing else.
    Upload {
        content_type: &'a str,
        content_length: u64,
    },
    /// Hand back whatever was stored.
    Download,
}

impl Purpose<'_> {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Upload { .. } => "upload",
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
    purpose: Purpose<'_>,
    id: AttachmentId,
    expires_at: OffsetDateTime,
) -> String {
    let expires_unix = expires_at.unix_timestamp();
    let digest = digest(secret, purpose, id, expires_unix);

    format!("{expires_unix}.{}", base16ct::lower::encode_string(&digest))
}

/// Check that a token names this `id` and `purpose` and has not expired.
/// For `Upload`, pass the request's declared content type: it goes through the same MAC.
///
/// # Errors
///
/// See [`TokenError`].
pub fn verify(
    secret: &[u8],
    purpose: Purpose<'_>,
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
    let expected_hex = base16ct::lower::encode_string(&expected);

    // Constant time: which byte differs must not leak.
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

fn digest(secret: &[u8], purpose: Purpose<'_>, id: AttachmentId, expires_unix: i64) -> Vec<u8> {
    // HMAC accepts any key length, so the fallback is unreachable (panics are denied here).
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(secret).unwrap_or_else(|_| {
        <Hmac<Sha256> as hmac::digest::KeyInit>::new(&hmac::digest::Key::<Hmac<Sha256>>::default())
    });

    mac.update(purpose.as_str().as_bytes());
    mac.update(b".");
    mac.update(id.to_string().as_bytes());
    mac.update(b".");
    mac.update(expires_unix.to_string().as_bytes());

    // Appended last: no earlier field can contain `.`, the content type is already
    // normalised, and the length is digits, so the last `.` separates the two.
    if let Purpose::Upload {
        content_type,
        content_length,
    } = purpose
    {
        mac.update(b".");
        mac.update(content_type.as_bytes());
        mac.update(b".");
        mac.update(content_length.to_string().as_bytes());
    }

    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arithmetic_side_effects,
        reason = "test arithmetic on small, known values"
    )]

    use super::*;

    const SECRET: &[u8] = b"shh";

    /// The one an honest client is approved for, throughout.
    const PNG: Purpose<'static> = Purpose::Upload {
        content_type: "image/png",
        content_length: 1_024,
    };

    fn ttl(now: OffsetDateTime) -> OffsetDateTime {
        now + time::Duration::minutes(15)
    }

    #[test]
    fn a_token_verifies_for_the_id_and_purpose_it_was_signed_for() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, PNG, id, ttl(now));

        assert_eq!(verify(SECRET, PNG, id, &token, now), Ok(()));
    }

    #[test]
    fn an_upload_token_does_not_verify_as_a_download() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, PNG, id, ttl(now));

        assert_eq!(
            verify(SECRET, Purpose::Download, id, &token, now),
            Err(TokenError::Mismatch)
        );
    }

    #[test]
    fn a_token_approved_for_one_content_type_does_not_verify_for_another() {
        // Approval for an image must not authorise storing markup served from this origin.
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, PNG, id, ttl(now));

        for impostor in ["text/html", "image/svg+xml", "application/octet-stream"] {
            assert_eq!(
                verify(
                    SECRET,
                    Purpose::Upload {
                        content_type: impostor,
                        content_length: 1_024,
                    },
                    id,
                    &token,
                    now
                ),
                Err(TokenError::Mismatch),
                "a token for image/png accepted `{impostor}`"
            );
        }
    }

    #[test]
    fn a_token_approved_for_one_length_does_not_verify_for_a_larger_body() {
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(SECRET, PNG, id, ttl(now));

        assert_eq!(
            verify(
                SECRET,
                Purpose::Upload {
                    content_type: "image/png",
                    content_length: 25 * 1024 * 1024,
                },
                id,
                &token,
                now
            ),
            Err(TokenError::Mismatch)
        );
    }

    #[test]
    fn the_content_type_cannot_be_slid_into_the_expiry_field() {
        // Field concatenation must be unambiguous: `""` signed vs `.image/png` presented.
        let now = OffsetDateTime::now_utc();
        let id = AttachmentId::generate();
        let token = sign(
            SECRET,
            Purpose::Upload {
                content_type: "",
                content_length: 1_024,
            },
            id,
            ttl(now),
        );

        assert_eq!(
            verify(
                SECRET,
                Purpose::Upload {
                    content_type: ".image/png",
                    content_length: 1_024,
                },
                id,
                &token,
                now
            ),
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
