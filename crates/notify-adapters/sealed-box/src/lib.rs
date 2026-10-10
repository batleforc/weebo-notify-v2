//! The sealed box of design §6.2: ephemeral X25519 ECDH, HKDF-SHA256, XChaCha20-Poly1305.
//! Not libsodium's `crypto_box_seal`: the raw ECDH point needs HKDF, and a random 192-bit nonce travels with it.
//! Wire layout: `ephemeral_public(32) || nonce(24) || ciphertext_with_tag(..)`; the envelope
//! version is the AEAD's associated data. One crate so APNs and UnifiedPush share one copy.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead as _, Generate as _, KeyInit as _, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use notify_domain::DevicePublicKey;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

/// Domain-separates the derived key from any other HKDF use of the same ECDH. `v2` since
/// both public keys joined the info ([`derive_key`]): a `v1` device fails the tag cleanly.
const HKDF_LABEL: &[u8] = b"weebo-notify sealed-box v2";

const EPHEMERAL_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Seal `plaintext` so that only the holder of `recipient`'s private key can read it.
///
/// `version` is the envelope's clear `v`, bound as associated data so a relay cannot rewrite it.
/// `None` for a low-order (non-contributory) recipient key: sealing to it would be readable by anyone.
#[must_use]
pub fn seal(recipient: &DevicePublicKey, version: u8, plaintext: &[u8]) -> Option<Vec<u8>> {
    let their_public = PublicKey::from(*recipient.as_bytes());
    let our_secret = EphemeralSecret::random();
    let our_public = PublicKey::from(&our_secret);
    let shared = our_secret.diffie_hellman(&their_public);

    if !shared.was_contributory() {
        return None;
    }

    Some(assemble(
        shared.as_bytes(),
        &our_public,
        &their_public,
        &XNonce::generate(),
        version,
        plaintext,
    ))
}

/// Everything after the ECDH, split out so a known-answer test can pin the bytes
/// with a fixed ephemeral key and nonce — which [`seal`] rightly never takes.
fn assemble(
    shared_secret: &[u8; 32],
    our_public: &PublicKey,
    their_public: &PublicKey,
    nonce: &XNonce,
    version: u8,
    plaintext: &[u8],
) -> Vec<u8> {
    let key = derive_key(shared_secret, our_public, their_public);
    let cipher = XChaCha20Poly1305::new(&key);

    // `encrypt` only fails on wrong key/nonce lengths, which are fixed here.
    #[allow(
        clippy::unwrap_used,
        reason = "key and nonce are this function's own, fixed-length constants"
    )]
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &[version],
            },
        )
        .unwrap();

    let mut sealed = Vec::with_capacity(EPHEMERAL_KEY_LEN + NONCE_LEN + ciphertext.len());
    sealed.extend_from_slice(our_public.as_bytes());
    sealed.extend_from_slice(nonce);
    sealed.extend_from_slice(&ciphertext);
    sealed
}

/// [`seal`], then base64url (no padding) — the `ciphertext_device` of a relayed envelope.
#[must_use]
pub fn seal_base64(recipient: &DevicePublicKey, version: u8, plaintext: &[u8]) -> Option<String> {
    seal(recipient, version, plaintext).map(|sealed| URL_SAFE_NO_PAD.encode(sealed))
}

/// HKDF-SHA256 the raw ECDH point into a key `XChaCha20Poly1305` can use directly.
///
/// info = `HKDF_LABEL || ephemeral_public || recipient_public` (as HPKE's `kem_context`), since
/// X25519 maps several public keys to one point. No salt: the secret is single-use (RFC 5869 zeroes).
fn derive_key(shared_secret: &[u8; 32], ephemeral: &PublicKey, recipient: &PublicKey) -> Key {
    let mut okm = [0_u8; 32];
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret);
    #[allow(
        clippy::unwrap_used,
        reason = "32 octets from SHA-256 is always a valid HKDF output length"
    )]
    hkdf.expand_multi_info(
        &[HKDF_LABEL, ephemeral.as_bytes(), recipient.as_bytes()],
        &mut okm,
    )
    .unwrap();
    Key::from(okm)
}

/// Open a sealed box with the recipient's private key.
///
/// Test-only: devices decrypt in production; this proves the round trip and T18.
#[cfg(test)]
fn open(secret: &x25519_dalek::StaticSecret, version: u8, sealed: &[u8]) -> Option<Vec<u8>> {
    let ephemeral_public: [u8; EPHEMERAL_KEY_LEN] =
        sealed.get(..EPHEMERAL_KEY_LEN)?.try_into().ok()?;
    let nonce = sealed.get(EPHEMERAL_KEY_LEN..EPHEMERAL_KEY_LEN + NONCE_LEN)?;
    let ciphertext = sealed.get(EPHEMERAL_KEY_LEN + NONCE_LEN..)?;

    let ephemeral_public = PublicKey::from(ephemeral_public);
    let shared = secret.diffie_hellman(&ephemeral_public);
    if !shared.was_contributory() {
        return None;
    }

    let key = derive_key(
        shared.as_bytes(),
        &ephemeral_public,
        &PublicKey::from(secret),
    );
    let cipher = XChaCha20Poly1305::new(&key);
    let nonce = XNonce::try_from(nonce).ok()?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: &[version],
            },
        )
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use x25519_dalek::StaticSecret;

    use super::*;

    /// The envelope version every caller passes today.
    const V: u8 = notify_ports::SEALED_ENVELOPE_VERSION;

    fn device_keypair() -> (StaticSecret, DevicePublicKey) {
        let secret = StaticSecret::random();
        let public = DevicePublicKey::from_bytes(*PublicKey::from(&secret).as_bytes());
        (secret, public)
    }

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    /// The known-answer vector device implementations (Kotlin, Swift — outside this
    /// repository) check themselves against; `hardening.mdx` publishes the same
    /// values. A change here is a wire-format change and needs a new label and
    /// envelope version, not a new expected string.
    #[test]
    fn the_published_test_vector_still_holds() {
        let recipient = StaticSecret::from([0x11_u8; 32]);
        let ephemeral = StaticSecret::from([0x22_u8; 32]);
        let nonce = XNonce::from([0x33_u8; NONCE_LEN]);

        let recipient_public = PublicKey::from(&recipient);
        let ephemeral_public = PublicKey::from(&ephemeral);
        let shared = ephemeral.diffie_hellman(&recipient_public);
        let sealed = assemble(
            shared.as_bytes(),
            &ephemeral_public,
            &recipient_public,
            &nonce,
            V,
            b"etcd degraded",
        );

        // Cross-checked against an independent implementation (Python
        // `cryptography`: X25519, HKDF, HChaCha20 + ChaCha20-Poly1305, associated
        // data the single byte `03`), not merely recorded from this one.
        assert_eq!(
            hex(&sealed),
            concat!(
                "0faa684ed28867b97f4a6a2dee5df8ce974e76b7018e3f22a1c4cf2678570f20",
                "333333333333333333333333333333333333333333333333",
                "99ba07a2f75b5cc4827ccc147bb97d305acc43b245287c3e52e747fc6b",
            )
        );
        assert_eq!(open(&recipient, V, &sealed).unwrap(), b"etcd degraded");
    }

    #[test]
    fn a_rewritten_envelope_version_fails_the_tag() {
        let (secret, public) = device_keypair();
        let sealed = seal(&public, V, b"etcd degraded").unwrap();

        assert!(open(&secret, V - 1, &sealed).is_none());
        assert!(open(&secret, V + 1, &sealed).is_none());
    }

    #[test]
    fn a_sealed_box_opens_with_the_matching_private_key() {
        let (secret, public) = device_keypair();

        let sealed = seal(&public, V, b"etcd degraded").unwrap();
        let opened = open(&secret, V, &sealed).unwrap();

        assert_eq!(opened, b"etcd degraded");
    }

    #[test]
    fn critical_t18_a_sealed_box_is_indecipherable_without_the_matching_key() {
        let (_secret, public) = device_keypair();
        let (someone_elses_secret, _their_public) = device_keypair();

        let sealed = seal(&public, V, b"etcd degraded").unwrap();

        assert_eq!(
            open(&someone_elses_secret, V, &sealed),
            None,
            "a device with any other keypair must not be able to open this box"
        );
    }

    #[test]
    fn two_seals_of_the_same_plaintext_are_not_the_same_bytes() {
        let (_secret, public) = device_keypair();

        let first = seal(&public, V, b"etcd degraded").unwrap();
        let second = seal(&public, V, b"etcd degraded").unwrap();

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
            seal(&weak, V, b"etcd degraded"),
            None,
            "the identity point makes the shared secret predictable to anyone, \
             sealed box or not"
        );
    }

    #[test]
    fn an_empty_plaintext_still_seals_and_opens() {
        let (secret, public) = device_keypair();

        let sealed = seal(&public, V, b"").unwrap();

        assert_eq!(open(&secret, V, &sealed).unwrap(), b"");
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

            let sealed = seal(&public, V, &message.sealed_plaintext()).unwrap();
            let opened: serde_json::Value =
                serde_json::from_slice(&open(&secret, V, &sealed).unwrap()).unwrap();

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
        let sealed = seal(&public, V, &digest.sealed_plaintext()).unwrap();
        let opened: serde_json::Value =
            serde_json::from_slice(&open(&secret, V, &sealed).unwrap()).unwrap();
        assert_eq!(opened["kind"], "digest");
        assert_eq!(opened["total"], 12);
    }
}
