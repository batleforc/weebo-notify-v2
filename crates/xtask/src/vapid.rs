//! Mints the development VAPID key (design §11, §12.5) without depending on `openssl`,
//! and prints the `applicationServerKey` a browser subscribes with.

use std::path::Path;

use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::elliptic_curve::sec1::ToSec1Point as _;
use p256::pkcs8::EncodePrivateKey as _;

/// Write a fresh P-256 key in the PKCS#8 PEM form `[push.webpush]` reads.
pub(crate) fn generate(output: &Path, force: bool) -> anyhow::Result<()> {
    // A replaced key silently invalidates every existing subscription, so it takes --force.
    if output.exists() && !force {
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

    // What `GET /push/webpush` serves and `pushManager.subscribe()` takes.
    let point = secret.public_key().to_sec1_point(false);
    println!("wrote {}", output.display());
    println!("applicationServerKey: {}", base64url(point.as_bytes()));

    Ok(())
}

/// A private scalar from the OS CSPRNG, retried in the (astronomically unlikely) out-of-range case.
fn random_key() -> anyhow::Result<p256::SecretKey> {
    for _ in 0..8 {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("reading from the system CSPRNG: {error}"))?;
        if let Ok(secret) = p256::SecretKey::from_slice(&bytes) {
            return Ok(secret);
        }
    }
    anyhow::bail!("could not draw a valid P-256 scalar in eight attempts")
}

/// Make the key readable by its owner alone (no-op off Unix).
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

/// Base64url without padding, as RFC 8292 spells it.
fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
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
