//! RFC 8291 message encryption (`aes128gcm`, RFC 8188), on aws-lc: its ECDH, HKDF and
//! AES-GCM are constant-time, which the `p256` 0.13 stack `web-push-native` sat on was not
//! (RUSTSEC-2026-0387). A test pins the RFC's Appendix A example byte for byte.

use aws_lc_rs::error::Unspecified;
use aws_lc_rs::{aead, agreement, hkdf, rand};

/// The record size announced in the header; one record carries the whole message.
const RECORD_SIZE: u32 = 4_096;

/// Encrypt `plaintext` for one subscription, with a fresh key pair and salt (RFC 8291 §3.4).
///
/// `ua_public` is the subscriber's uncompressed P-256 point (`p256dh`), `auth_secret` its `auth`.
pub(crate) fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8; 16],
) -> Result<Vec<u8>, Unspecified> {
    let as_private = agreement::PrivateKey::generate(&agreement::ECDH_P256)?;
    let mut salt = [0_u8; 16];
    rand::fill(&mut salt)?;
    encrypt_with(plaintext, ua_public, auth_secret, &as_private, salt)
}

/// [`encrypt`] with the ephemeral key and salt supplied, so the RFC's example can be replayed.
fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8; 16],
    as_private: &agreement::PrivateKey,
    salt: [u8; 16],
) -> Result<Vec<u8>, Unspecified> {
    let as_public = as_private.compute_public_key()?;
    let as_public = as_public.as_ref();

    // §3.3: the ECDH secret, keyed with `auth_secret` and bound to both public keys.
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let ikm: [u8; 32] = agreement::agree(as_private, peer, Unspecified, |ecdh| {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(ecdh);
        expand(&prk, &[b"WebPush: info\0", ua_public, as_public])
    })?;

    // RFC 8188 §2.2-2.3: the content key and nonce, from the per-message salt.
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
    let cek: [u8; 16] = expand(&prk, &[b"Content-Encoding: aes128gcm\0"])?;
    let nonce: [u8; 12] = expand(&prk, &[b"Content-Encoding: nonce\0"])?;

    // The only record is also the last: the plaintext, then the 0x02 delimiter, no padding.
    let mut record = Vec::with_capacity(
        plaintext
            .len()
            .saturating_add(1)
            .saturating_add(aead::AES_128_GCM.tag_len()),
    );
    record.extend_from_slice(plaintext);
    record.push(0x02);
    aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek)?)
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::empty(),
            &mut record,
        )?;
    if u32::try_from(record.len()).map_or(true, |len| len > RECORD_SIZE) {
        return Err(Unspecified);
    }

    // Header: salt, record size, key id length, key id (the ephemeral public point).
    let id_len = u8::try_from(as_public.len()).map_err(|_| Unspecified)?;
    let mut body = Vec::with_capacity(
        salt.len()
            .saturating_add(5)
            .saturating_add(as_public.len())
            .saturating_add(record.len()),
    );
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(id_len);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// HKDF-Expand into a fixed-size array.
fn expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> Result<[u8; N], Unspecified> {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }

    let mut out = [0_u8; N];
    prk.expand(info, Len(N))?.fill(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).unwrap()
    }

    #[test]
    fn the_rfc_8291_example_encrypts_to_the_published_bytes() {
        let as_private = agreement::PrivateKey::from_private_key(
            &agreement::ECDH_P256,
            &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        )
        .unwrap();

        let body = encrypt_with(
            b"When I grow up, I want to be a watermelon",
            &b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &b64("BTBZMqHH6r4Tts7J_aSIgg").try_into().unwrap(),
            &as_private,
            b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
        )
        .unwrap();

        // Appendix A gives the 86-octet header and the ciphertext separately.
        let header = b64(concat!(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
            "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        ));
        let ciphertext = b64(concat!(
            "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEs",
            "bI_0LpXMuGvnzQ",
        ));
        assert_eq!(header.len(), 86);
        assert_eq!(body, [header, ciphertext].concat());
    }

    #[test]
    fn a_subscriber_key_that_is_not_on_the_curve_is_an_error_and_not_a_message() {
        let mut off_curve = vec![0x04];
        off_curve.extend([0x11; 64]);

        assert!(encrypt(b"x", &off_curve, &[0; 16]).is_err());
    }
}
