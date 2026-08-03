//! Hashing the secret half of an API key (design §5.2).

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use notify_ports::{PortError, PortResult, SecretHasher};

/// The label this hasher reports failures under. Not a `/readyz` dependency: a KDF
/// that cannot run is a broken process, not an unreachable service.
const WHAT: &str = "hasher";

/// Argon2id, at the crate's default cost.
///
/// The cost is the point, so it is *not* tuned down: the whole reason a stolen
/// database of hashes is survivable is that each guess is expensive. Which is also
/// why [`notify_ports::SecretHasher`] is a port — the test suite runs on a stub
/// rather than paying it thousands of times, instead of the usual resolution of
/// lowering the production parameters until the tests are bearable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Argon2SecretHasher;

#[async_trait::async_trait]
impl SecretHasher for Argon2SecretHasher {
    async fn hash(&self, secret: &str) -> PortResult<String> {
        let secret = secret.to_owned();

        // Deliberately expensive and entirely CPU-bound: left on an async worker it
        // would stall every other request that worker was serving.
        blocking(move || {
            let salt = SaltString::generate(&mut OsRng);
            Argon2::default()
                .hash_password(secret.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|error| PortError::unavailable(WHAT, HashingFailed(error)))
        })
        .await
    }

    async fn verify(&self, secret: &str, hash: &str) -> PortResult<bool> {
        let secret = secret.to_owned();
        let hash = hash.to_owned();

        blocking(move || {
            // A hash we cannot parse is our data being wrong, not the caller's
            // secret being wrong — answering `false` would tell somebody holding a
            // perfectly good key that it is invalid.
            let parsed = PasswordHash::new(&hash)
                .map_err(|error| PortError::corrupt("apikey.secret_hash", error))?;

            // Argon2's own comparison is constant-time; a mismatch is an ordinary
            // answer rather than an error, so a key-scanning attempt costs no log
            // line with a stack trace.
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

/// `argon2::password_hash::Error` is not a `std::error::Error`, and [`PortError`]
/// keeps the cause. This carries it across.
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
