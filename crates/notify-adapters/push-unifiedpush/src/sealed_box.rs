//! The sealed box of design §6.2: ephemeral X25519 ECDH, HKDF-SHA256, then
//! XChaCha20-Poly1305.
//!
//! Not libsodium's `crypto_box_seal` — that construction derives its nonce from the
//! two public keys, which works because `NaCl`'s `crypto_box` already runs the ECDH
//! output through `HSalsa20` as a key-derivation step. `x25519-dalek`'s
//! [`SharedSecret`](x25519_dalek::SharedSecret) is the *raw* ECDH point, so this box
//! adds back the two things `NaCl` bundles: `HKDF` turns the point into a key
//! indistinguishable from random, and a nonce travels with the ciphertext instead of
//! being derived — a fresh random 192-bit nonce makes collision risk negligible at
//! any volume this product will ever push, which is not true of `ChaCha20Poly1305`'s
//! plain 96-bit nonce and is exactly why the extended variant was picked over it.
//!
//! Wire layout: `ephemeral_public(32) || nonce(24) || ciphertext_with_tag(..)`. The
//! ephemeral key travels in the clear — worthless without the recipient's private
//! half, which never leaves hardware (design §6.2) — and it is what lets the device,
//! holding only its own keypair, redo the same ECDH without a second round trip.

use chacha20poly1305::aead::{Aead as _, Generate as _, KeyInit as _};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use notify_domain::DevicePublicKey;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

/// Binds the derived key to this one use, so a shared secret could never double as
/// key material for anything else this codebase might one day HKDF from the same
/// ECDH. There is only one such use today; the string costs nothing and a second one
/// is exactly the day this would matter.
const HKDF_INFO: &[u8] = b"weebo-notify sealed-box v1";

const EPHEMERAL_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Seal `plaintext` so that only the holder of `recipient`'s private key can read it.
///
/// `None` on a recipient key that Diffie-Hellman flags as non-contributory (design
/// §6.2's threat model trusts the device that enrolled the key, but a 32-byte hex
/// string that parses is not the same guarantee as a point that behaves like one —
/// the all-zero key is valid hex and a low-order point at once). Sealing to it would
/// produce ciphertext anyone who has ever seen this scheme run could open; refusing
/// costs nothing a legitimate device would ever trigger.
#[must_use]
pub(crate) fn seal(recipient: &DevicePublicKey, plaintext: &[u8]) -> Option<Vec<u8>> {
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

    // The only failure `encrypt` can report here is a key or nonce of the wrong
    // length, and both came from this function's own constants — a bug that a
    // fourth `Result` layer the caller cannot act on would not catch any sooner
    // than this one panicking loudly in its test suite.
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

/// HKDF-SHA256 the raw ECDH point into a key `XChaCha20Poly1305` can use directly.
///
/// No salt: both sides derive from the same ephemeral, single-use secret, so a salt
/// would add nothing a fresh ephemeral key does not already give — RFC 5869 treats a
/// missing salt as a hash-length string of zeroes, which is a defined and safe input,
/// not a shortcut.
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
/// Never called in production — decryption happens on the device, in Kotlin or
/// Swift, outside this repository — but the round trip is exactly what proves
/// [`seal`] produced something *that* code can open, and "indecipherable without the
/// private key" (design's T18) is not a property a test can see without opening one.
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
        // The digest path (design §7.4) can in principle carry very little; nothing
        // here should assume a non-empty payload.
        let (secret, public) = device_keypair();

        let sealed = seal(&public, b"").unwrap();

        assert_eq!(open(&secret, &sealed).unwrap(), b"");
    }
}
