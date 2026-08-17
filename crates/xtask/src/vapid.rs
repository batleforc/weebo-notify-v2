//! Minting the VAPID key a development deployment needs (design §11, §12.5).
//!
//! # Why this is Rust and not one line of `openssl`
//!
//! `openssl ecparam -genkey -name prime256v1` would produce the same key, and this
//! workspace deliberately does not depend on OpenSSL — not as a build input (M1
//! refused the C toolchain so the image builds from a plain `cargo build` and the
//! Tauri cross-compilations of M5–M7 stay possible), and not as a *tool a
//! developer's machine is assumed to have* either. An Eclipse Che image without it
//! would turn `task dev:setup` into a support question (§12.1). Anything cargo can
//! run, every world this project targets can run.
//!
//! It also does what `openssl` cannot do in one command: print the
//! `applicationServerKey` a browser subscribes with, which is the thing a person
//! setting up push actually wants to see.
//!
//! # What it refuses
//!
//! Overwriting an existing key, unless asked twice. A regenerated VAPID key does
//! not fail loudly: the server starts, the relay accepts the assertion, and every
//! subscription taken out under the old key stops being deliverable — silently, on
//! somebody else's machine. Refusing costs one flag; guessing costs an afternoon of
//! "push stopped working and nothing changed".

use std::path::Path;

use anyhow::Context as _;
use p256::elliptic_curve::sec1::ToSec1Point as _;
use p256::pkcs8::EncodePrivateKey as _;

/// Write a fresh P-256 key in the PKCS#8 PEM form `[push.webpush]` reads.
pub(crate) fn generate(output: &Path, force: bool) -> anyhow::Result<()> {
    if output.exists() && !force {
        // Not an error the caller has to handle: `task dev:setup` is idempotent by
        // design and runs on every `dev:up`, so "there is already a key" is the
        // normal case and printing it is the whole answer.
        println!(
            "{} already exists — keeping it (pass --force to replace it, \
             which invalidates every existing subscription)",
            output.display()
        );
        return Ok(());
    }

    let secret = random_key()?;
    let pem = secret
        .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
        .context("encoding the key as PKCS#8")?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(output, pem.as_bytes())
        .with_context(|| format!("writing {}", output.display()))?;
    restrict(output)?;

    // Base64url of the uncompressed point: exactly what `GET /push/webpush` serves
    // and what `pushManager.subscribe()` takes.
    let point = secret.public_key().to_sec1_point(false);
    println!("wrote {}", output.display());
    println!("applicationServerKey: {}", base64url(point.as_bytes()));

    Ok(())
}

/// A private scalar from the operating system's randomness.
///
/// Retried rather than unwrapped: a uniform 32 octets is a valid P-256 scalar unless
/// it is zero or lands past the curve order, which is astronomically unlikely and
/// still not impossible — and a `dev:setup` that panics once a decade is worse than
/// three lines.
fn random_key() -> anyhow::Result<p256::SecretKey> {
    for _ in 0..8 {
        let mut bytes = [0u8; 32];
        // Mapped rather than `context`ed: `getrandom::Error` is not a `std::Error`.
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("reading from the system CSPRNG: {error}"))?;
        if let Ok(secret) = p256::SecretKey::from_slice(&bytes) {
            return Ok(secret);
        }
    }
    anyhow::bail!("could not draw a valid P-256 scalar in eight attempts")
}

/// Make the key readable by its owner alone.
///
/// It is a private key on a developer's disk, and `0600` is what every tool that
/// writes one does. A no-op off Unix, where the equivalent is an ACL this chore has
/// no business editing.
#[cfg(unix)]
pub(crate) fn restrict(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn restrict(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Base64url without padding, as RFC 8292 and every browser API spell it.
///
/// Written out rather than pulled in: `base64` is not an xtask dependency, and one
/// alphabet lookup is cheaper than a dependency that exists to print one line.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(*chunk.get(i).unwrap_or(&0));
        let block = (b(0) << 16) | (b(1) << 8) | b(2);
        // One output character per 6 bits, and one fewer than four for every octet
        // the last chunk is missing — which is what "no padding" means.
        for i in 0..=chunk.len() {
            let index = (block >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[index as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use super::*;

    #[test]
    fn the_public_point_renders_as_the_87_characters_a_browser_expects() {
        let secret = random_key().unwrap();
        let point = secret.public_key().to_sec1_point(false);

        let rendered = base64url(point.as_bytes());

        assert_eq!(point.as_bytes().len(), 65, "an uncompressed P-256 point");
        assert_eq!(rendered.len(), 87, "65 octets, base64url, unpadded");
        assert!(
            !rendered.contains(['=', '+', '/']),
            "padded or standard base64 is refused by `pushManager.subscribe()`: {rendered}"
        );
    }

    #[test]
    fn the_alphabet_matches_the_one_every_other_crate_uses() {
        // The three vectors that catch a hand-written encoder: a full block, a
        // one-octet remainder and a two-octet remainder.
        for (raw, expected) in [
            (&b"abc"[..], "YWJj"),
            (&b"a"[..], "YQ"),
            (&b"ab"[..], "YWI"),
        ] {
            assert_eq!(base64url(raw), expected);
        }
    }

    #[test]
    fn an_existing_key_is_kept_rather_than_silently_replaced() {
        let path = std::env::temp_dir().join(format!("wnk-vapid-xtask-{}.pem", std::process::id()));
        std::fs::write(&path, "not a key, and that is the point\n").unwrap();

        generate(&path, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not a key, and that is the point\n",
            "replacing it would invalidate every subscription taken out under it"
        );

        generate(&path, true).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("-----BEGIN PRIVATE KEY-----"),
            "--force is the way to say it on purpose"
        );

        std::fs::remove_file(&path).unwrap();
    }
}
