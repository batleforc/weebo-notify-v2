//! Getting the issuer's signing keys, and keeping them fresh.

use jsonwebtoken::jwk::JwkSet;
use notify_ports::{PortError, PortResult};
use reqwest::Client;
use tokio::sync::RwLock;
use url::Url;

use crate::config::OidcConfig;

/// The dependency name this adapter reports on `/readyz` and in metrics.
pub(crate) const DEPENDENCY: &str = "jwks";

/// Where signing keys come from.
///
/// A seam rather than a hardcoded HTTP call, so the verification path — the part
/// with the security properties — is exercised by unit tests holding a real
/// keypair instead of only by an integration suite that needs an IdP running.
#[async_trait::async_trait]
pub trait JwksSource: Send + Sync + core::fmt::Debug {
    /// Fetch the current key set.
    async fn fetch(&self) -> PortResult<JwkSet>;
}

/// What went wrong talking to the IdP, before it becomes a [`PortError`].
#[derive(Debug)]
pub(crate) enum DiscoveryError {
    /// The IdP answered, with something other than success.
    Status(reqwest::StatusCode, &'static str),
    /// The discovery document names an issuer that is not the one we configured.
    IssuerMismatch {
        announced: String,
        configured: String,
    },
    /// We hold no usable key, and asking again is not due yet.
    Cold,
}

impl core::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Status(status, what) => write!(f, "the issuer answered {status} for its {what}"),
            Self::IssuerMismatch {
                announced,
                configured,
            } => write!(
                f,
                "the discovery document announces issuer {announced}, not the configured {configured}"
            ),
            Self::Cold => write!(
                f,
                "no usable signing key, and the refresh interval has not elapsed"
            ),
        }
    }
}

impl core::error::Error for DiscoveryError {}

/// The discovery document, reduced to the two fields we act on.
#[derive(Debug, serde::Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: Url,
}

/// The real source: OIDC discovery, then the key set it points at.
#[derive(Debug)]
pub struct HttpJwksSource {
    http: Client,
    issuer: Url,
    /// Remembered so the common path is one request rather than two. Cleared
    /// whenever the key set behind it fails to load, because a moved `jwks_uri` and
    /// an unreachable one look identical from here and only one of them is fixed by
    /// asking again.
    jwks_uri: RwLock<Option<Url>>,
}

impl HttpJwksSource {
    /// Build one from the adapter's configuration.
    pub fn new(config: &OidcConfig) -> PortResult<Self> {
        let timeout = config
            .http_timeout
            .try_into()
            .unwrap_or(core::time::Duration::from_secs(5));
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| PortError::unavailable(DEPENDENCY, error))?;

        Ok(Self {
            http,
            issuer: config.issuer.clone(),
            jwks_uri: RwLock::new(None),
        })
    }

    /// `{issuer}/.well-known/openid-configuration`, built the way the spec says.
    ///
    /// By concatenation rather than by [`Url::join`]: an issuer with a path —
    /// Authentik's `/application/o/weebo-notify/` — must keep it, and `join` would
    /// throw it away.
    fn discovery_url(&self) -> PortResult<Url> {
        let base = self.issuer.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}/.well-known/openid-configuration"))
            .map_err(|error| PortError::unavailable(DEPENDENCY, error))
    }

    async fn discover(&self) -> PortResult<Url> {
        if let Some(known) = self.jwks_uri.read().await.clone() {
            return Ok(known);
        }

        let url = self.discovery_url()?;
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| PortError::unavailable(DEPENDENCY, error))?;

        let status = response.status();
        if !status.is_success() {
            return Err(PortError::unavailable(
                DEPENDENCY,
                DiscoveryError::Status(status, "discovery document"),
            ));
        }

        let document: Discovery = response
            .json()
            .await
            .map_err(|error| PortError::unavailable(DEPENDENCY, error))?;

        // The document has to name the issuer we configured. One that names another
        // is not a key source, it is somewhere we did not mean to go.
        if document.issuer.trim_end_matches('/') != self.issuer.as_str().trim_end_matches('/') {
            return Err(PortError::unavailable(
                DEPENDENCY,
                DiscoveryError::IssuerMismatch {
                    announced: document.issuer,
                    configured: self.issuer.to_string(),
                },
            ));
        }

        *self.jwks_uri.write().await = Some(document.jwks_uri.clone());
        Ok(document.jwks_uri)
    }
}

#[async_trait::async_trait]
impl JwksSource for HttpJwksSource {
    async fn fetch(&self) -> PortResult<JwkSet> {
        let uri = self.discover().await?;

        let attempt = async {
            let response = self
                .http
                .get(uri)
                .send()
                .await
                .map_err(|error| PortError::unavailable(DEPENDENCY, error))?;

            let status = response.status();
            if !status.is_success() {
                return Err(PortError::unavailable(
                    DEPENDENCY,
                    DiscoveryError::Status(status, "key set"),
                ));
            }

            response
                .json::<JwkSet>()
                .await
                .map_err(|error| PortError::unavailable(DEPENDENCY, error))
        };

        match attempt.await {
            Ok(keys) => Ok(keys),
            Err(error) => {
                // Re-discover next time: the endpoint may simply have moved.
                *self.jwks_uri.write().await = None;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    fn config(issuer: &str) -> OidcConfig {
        OidcConfig::new(Url::parse(issuer).unwrap(), "weebo-notify")
    }

    #[test]
    fn discovery_hangs_off_the_issuer_path_rather_than_replacing_it() {
        let source = HttpJwksSource::new(&config("https://idp.example/application/o/weebo/"))
            .expect("a client with a plain timeout always builds");

        assert_eq!(
            source.discovery_url().unwrap().as_str(),
            "https://idp.example/application/o/weebo/.well-known/openid-configuration",
            "Authentik-shaped issuers keep their path; Url::join would drop it"
        );
    }

    #[test]
    fn a_bare_issuer_gets_exactly_one_slash() {
        let source = HttpJwksSource::new(&config("https://idp.example"))
            .expect("a client with a plain timeout always builds");

        assert_eq!(
            source.discovery_url().unwrap().as_str(),
            "https://idp.example/.well-known/openid-configuration"
        );
    }

    #[test]
    fn a_mismatched_issuer_reads_as_a_jwks_outage_naming_both_sides() {
        let error = PortError::unavailable(
            DEPENDENCY,
            DiscoveryError::IssuerMismatch {
                announced: "https://evil.example".to_owned(),
                configured: "https://idp.example".to_owned(),
            },
        );

        assert_eq!(error.dependency(), Some("jwks"));
        let rendered = core::error::Error::source(&error).unwrap().to_string();
        assert!(rendered.contains("evil.example") && rendered.contains("idp.example"));
    }
}
