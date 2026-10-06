//! The sealed box of design §6.2: ephemeral X25519 ECDH, HKDF-SHA256, XChaCha20-Poly1305.
//! Not libsodium's `crypto_box_seal`: the raw ECDH point needs HKDF, and a random 192-bit nonce travels with it.
//! Wire layout: `ephemeral_public(32) || nonce(24) || ciphertext_with_tag(..)`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead as _, Generate as _, KeyInit as _};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use notify_domain::DevicePublicKey;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

/// Domain-separates the derived key from any other HKDF use of the same ECDH.
const HKDF_INFO: &[u8] = b"weebo-notify sealed-box v1";

const EPHEMERAL_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Seal `plaintext` so that only the holder of `recipient`'s private key can read it.
///
/// `None` for a low-order (non-contributory) recipient key: sealing to it would be readable by anyone.
#[must_use]
pub fn seal(recipient: &DevicePublicKey, plaintext: &[u8]) -> Option<Vec<u8>> {
    let their_public = PublicKey::from(*recipient.as_bytes());
    let our_secret = EphemeralSecret::random();
    let our_public = PublicKey::from(&our_secret);
    let shared = our_secret.diffie_hellman(&their_public);

    if !shared.was_contributory() {
        return None;
    }

    let key = derive_key(shared.as_bytes());
    let nonce = XNonce::generate();
    let cipher = XChaCha20Poly1305::new(&key);

    // `encrypt` only fails on wrong key/nonce lengths, which are fixed here.
    #[allow(
        clippy::unwrap_used,
        reason = "key and nonce are this function's own, fixed-length constants"
    )]
    let ciphertext = cipher.encrypt(&nonce, plaintext).unwrap();

    let mut sealed = Vec::with_capacity(EPHEMERAL_KEY_LEN + NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(our_public.as_bytes());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Some(sealed)
}

/// [`seal`], then base64url (no padding) — the `ciphertext_device` of a relayed envelope.
#[must_use]
pub fn seal_base64(recipient: &DevicePublicKey, plaintext: &[u8]) -> Option<String> {
    seal(recipient, plaintext).map(|sealed| URL_SAFE_NO_PAD.encode(sealed))
}

/// HKDF-SHA256 the raw ECDH point into a key `XChaCha20Poly1305` can use directly.
///
/// No salt: the secret is single-use, and RFC 5869 defines a missing salt as zeroes.
fn derive_key(shared_secret: &[u8; 32]) -> Key {
    let mut okm = [0_u8; 32];
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret);
    #[allow(
        clippy::unwrap_used,
        reason = "32 octets from SHA-256 is always a valid HKDF output length"
    )]
    hkdf.expand(HKDF_INFO, &mut okm).unwrap();
    Key::from(okm)
}

/// Open a sealed box with the recipient's private key.
///
/// Test-only: devices decrypt in production; this proves the round trip and T18.
#[cfg(test)]
fn open(secret: &x25519_dalek::StaticSecret, sealed: &[u8]) -> Option<Vec<u8>> {
    let ephemeral_public: [u8; EPHEMERAL_KEY_LEN] =
        sealed.get(..EPHEMERAL_KEY_LEN)?.try_into().ok()?;
    let nonce = sealed.get(EPHEMERAL_KEY_LEN..EPHEMERAL_KEY_LEN + NONCE_LEN)?;
    let ciphertext = sealed.get(EPHEMERAL_KEY_LEN + NONCE_LEN..)?;

    let shared = secret.diffie_hellman(&PublicKey::from(ephemeral_public));
    if !shared.was_contributory() {
        return None;
    }

    let key = derive_key(shared.as_bytes());
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::try_from(nonce).ok()?;
    cipher.decrypt(&nonce, ciphertext).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use x25519_dalek::StaticSecret;

    use super::*;

    fn device_keypair() -> (StaticSecret, DevicePublicKey) {
        let secret = StaticSecret::random();
        let public = DevicePublicKey::from_bytes(*PublicKey::from(&secret).as_bytes());
        (secret, public)
    }

    #[test]
    fn a_sealed_box_opens_with_the_matching_private_key() {
        let (secret, public) = device_keypair();

        let sealed = seal(&public, b"etcd degraded").unwrap();
        let opened = open(&secret, &sealed).unwrap();

        assert_eq!(opened, b"etcd degraded");
    }

    #[test]
    fn critical_t18_a_sealed_box_is_indecipherable_without_the_matching_key() {
        let (_secret, public) = device_keypair();
        let (someone_elses_secret, _their_public) = device_keypair();

        let sealed = seal(&public, b"etcd degraded").unwrap();

        assert_eq!(
            open(&someone_elses_secret, &sealed),
            None,
            "a device with any other keypair must not be able to open this box"
        );
    }

    #[test]
    fn two_seals_of_the_same_plaintext_are_not_the_same_bytes() {
        let (_secret, public) = device_keypair();

        let first = seal(&public, b"etcd degraded").unwrap();
        let second = seal(&public, b"etcd degraded").unwrap();

        assert_ne!(
            first, second,
            "a fresh ephemeral key and nonce per call, or two deliveries of the same \
             notification would be distinguishable to anyone watching the relay"
        );
    }

    #[test]
    fn the_all_zero_key_is_refused_rather_than_sealed_to() {
        let weak = DevicePublicKey::from_bytes([0_u8; 32]);

        assert_eq!(
            seal(&weak, b"etcd degraded"),
            None,
            "the identity point makes the shared secret predictable to anyone, \
             sealed box or not"
        );
    }

    #[test]
    fn an_empty_plaintext_still_seals_and_opens() {
        let (secret, public) = device_keypair();

        let sealed = seal(&public, b"").unwrap();

        assert_eq!(open(&secret, &sealed).unwrap(), b"");
    }

    #[test]
    fn a_relayed_push_carries_its_identifiers_inside_the_box() {
        use notify_domain::{ChannelId, NotificationId, Priority};
        use notify_ports::{PushBody, PushMessage};

        let (secret, public) = device_keypair();
        let notification_id = NotificationId::generate();

        for body in [
            PushBody::Poke,
            PushBody::Payload {
                bytes: br#"{"title":"etcd degraded"}"#.to_vec(),
                sealed: false,
            },
        ] {
            let message = PushMessage::Notification {
                channel: ChannelId::parse("ci-infra").unwrap(),
                notification_id,
                priority: Priority::Default,
                body: body.clone(),
            };

            let sealed = seal(&public, &message.sealed_plaintext()).unwrap();
            let opened: serde_json::Value =
                serde_json::from_slice(&open(&secret, &sealed).unwrap()).unwrap();

            assert_eq!(opened["kind"], "notification");
            assert_eq!(opened["channel"], "ci-infra");
            assert_eq!(opened["notification_id"], notification_id.to_string());
            assert_eq!(
                opened["content"]["title"].as_str(),
                matches!(body, PushBody::Payload { .. }).then_some("etcd degraded")
            );
        }

        let digest = PushMessage::Digest {
            total: 12,
            failures: 2,
        };
        let sealed = seal(&public, &digest.sealed_plaintext()).unwrap();
        let opened: serde_json::Value =
            serde_json::from_slice(&open(&secret, &sealed).unwrap()).unwrap();
        assert_eq!(opened["kind"], "digest");
        assert_eq!(opened["total"], 12);
    }
}
