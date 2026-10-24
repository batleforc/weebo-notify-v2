//! Hashing the secret half of an API key (design §5.2).

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use notify_ports::{PortError, PortResult, SecretHasher};

/// Failure label; not a `/readyz` dependency, since a broken KDF is a broken process.
const WHAT: &str = "hasher";

/// Argon2id at default cost, deliberately not tuned down; tests use a stub [`notify_ports::SecretHasher`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Argon2SecretHasher;

#[async_trait::async_trait]
impl SecretHasher for Argon2SecretHasher {
    async fn hash(&self, secret: &str) -> PortResult<String> {
        let secret = secret.to_owned();

        // CPU-bound: keep it off async workers.
        blocking(move || {
            // The salt comes from the OS CSPRNG (password-hash's `getrandom` feature).
            Argon2::default()
                .hash_password(secret.as_bytes())
                .map(|hash| hash.to_string())
                .map_err(|error| PortError::unavailable(WHAT, HashingFailed(error)))
        })
        .await
    }

    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
        let secret = secret.to_owned();
        let hash = hash.to_owned();

        blocking(move || {
            // An unparseable hash is our corruption, not a wrong secret.
            let parsed = PasswordHash::new(&hash)
                .map_err(|error| PortError::corrupt("apikey.secret_hash", error))?;

            // Constant-time; a mismatch is an ordinary answer, not an error.
            Ok(Argon2::default()
                .verify_password(secret.as_bytes(), &parsed)
                .is_ok())
        })
        .await
    }
}

/// Run CPU-bound work off the async runtime.
async fn blocking<T, F>(work: F) -> PortResult<T>
where
    F: FnOnce() -> PortResult<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(PortError::unavailable(WHAT, error)),
    }
}

/// Carries `argon2::password_hash::Error` (not a `std::error::Error`) into [`PortError`].
#[derive(Debug)]
struct HashingFailed(argon2::password_hash::Error);

impl core::fmt::Display for HashingFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "argon2 could not hash the secret: {}", self.0)
    }
}

impl core::error::Error for HashingFailed {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[tokio::test]
    async fn a_secret_verifies_against_its_own_hash_and_nothing_else() {
        let hasher = Argon2SecretHasher;
        let hash = hasher.hash("wnk-secret-half").await.unwrap();

        assert!(hasher.verify("wnk-secret-half", &hash).await.unwrap());
        assert!(!hasher.verify("wnk-secret-hall", &hash).await.unwrap());
    }

    #[tokio::test]
    async fn the_same_secret_hashes_differently_every_time() {
        let hasher = Argon2SecretHasher;

        let first = hasher.hash("same").await.unwrap();
        let second = hasher.hash("same").await.unwrap();

        assert_ne!(
            first, second,
            "an unsalted hash lets one rainbow table cover every key on the instance"
        );
        assert!(hasher.verify("same", &second).await.unwrap());
    }

    #[tokio::test]
    async fn it_is_argon2id_and_says_so_in_the_stored_string() {
        let hash = Argon2SecretHasher.hash("secret").await.unwrap();

        assert!(
            hash.starts_with("$argon2id$"),
            "the stored format has to name its own KDF, or rotating it later is guesswork: {hash}"
        );
    }

    #[tokio::test]
    async fn a_stored_hash_we_cannot_parse_is_our_fault_not_the_callers() {
        let error = Argon2SecretHasher
            .verify("secret", "this is not a PHC string")
            .await
            .unwrap_err();

        assert!(matches!(error, PortError::Corrupt { .. }));
        assert!(!error.is_retryable());
    }
}
