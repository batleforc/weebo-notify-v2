//! Minting the Ed25519 key that signs federated CI tokens (design §5.3, §11).
//!
//! Same shape as [`crate::vapid`] and for the same reasons — cargo runs it, so
//! every world this project targets can run it, and no OpenSSL is assumed on a
//! developer's machine (§12.1). What differs is what a wrong answer costs.
//!
//! # Why this key cannot be generated at startup
//!
//! It would be tempting: nothing outside this instance ever needs the public half,
//! so a key drawn at boot would "just work" on one replica. It would break the
//! moment there are two (§11 `[ha]`) — replica A mints a token, the request lands
//! on replica B, and B refuses a signature it cannot verify. The failure is
//! intermittent, load-balancer-dependent, and looks like a flaky pipeline. So the
//! key is configuration, mounted like any other secret.
//!
//! # What it refuses
//!
//! Overwriting an existing key without being asked twice, exactly like the VAPID
//! one. Regenerating it invalidates every token in flight — at most fifteen minutes
//! of CI jobs (§5.3), which is milder than the VAPID case but still a pipeline that
//! fails for a reason nobody changed.

use std::path::Path;

use anyhow::Context as _;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey as _;

/// Write a fresh Ed25519 key in the PKCS#8 PEM form `[federation]` reads.
pub(crate) fn generate(output: &Path, force: bool) -> anyhow::Result<()> {
    if output.exists() && !force {
        println!(
            "{} already exists — keeping it (pass --force to replace it, \
             which invalidates every CI token currently in flight)",
            output.display()
        );
        return Ok(());
    }

    // No retry loop, unlike P-256: every 32-octet string is a valid Ed25519 secret
    // scalar by construction — the algorithm clamps it. There is nothing to reject.
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("reading from the system CSPRNG: {error}"))?;
    let signing = SigningKey::from_bytes(&bytes);

    let pem = signing
        .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
        .context("encoding the key as PKCS#8")?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(output, pem.as_bytes())
        .with_context(|| format!("writing {}", output.display()))?;
    crate::vapid::restrict(output)?;

    println!("wrote {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn an_existing_key_is_kept_rather_than_silently_replaced() {
        let dir = std::env::temp_dir().join(format!("wnk-fed-{}", std::process::id()));
        let path = dir.join("federation.pem");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"an existing key").unwrap();

        generate(&path, false).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"an existing key",
            "regenerating invalidates every token in flight; it has to be asked for"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn what_it_writes_is_a_key_jsonwebtoken_can_load() {
        // The point of the test: PKCS#8 PEM is a family, and "it parsed as PEM" is
        // not the same statement as "the signer accepts it".
        let dir = std::env::temp_dir().join(format!("wnk-fed-new-{}", std::process::id()));
        let path = dir.join("federation.pem");
        std::fs::create_dir_all(&dir).unwrap();

        generate(&path, false).unwrap();
        let pem = std::fs::read(&path).unwrap();

        assert!(jsonwebtoken::EncodingKey::from_ed_pem(&pem).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
