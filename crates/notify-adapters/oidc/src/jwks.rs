//! Getting the issuer's signing keys, and keeping them fresh.

use jsonwebtoken::jwk::JwkSet;
use notify_ports::{PortError, PortResult};
use reqwest::Client;
use tokio::sync::RwLock;
use url::Url;

use crate::config::OidcConfig;

/// The dependency name this adapter reports on `/readyz` and in metrics.
pub(crate) const DEPENDENCY: &str = "jwks";

/// Most we read of a discovery document or key set, so a hostile issuer cannot make us buffer without end.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Where signing keys come from; a seam so unit tests hold a real keypair without an IdP.
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
    /// The body ran past [`MAX_BODY_BYTES`].
    TooLarge(&'static str),
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
            Self::TooLarge(what) => {
                write!(
                    f,
                    "the issuer's {what} is larger than {MAX_BODY_BYTES} bytes"
                )
            }
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
    /// Cached `jwks_uri`; cleared when the key set fails to load, in case it moved.
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
            // reqwest's `gzip` is on for the feeds (RFC 0002 §6.5); not here.
            .no_gzip()
            .build()
            .map_err(|error| PortError::unavailable(DEPENDENCY, error))?;

        Ok(Self {
            http,
            issuer: config.issuer.clone(),
            jwks_uri: RwLock::new(None),
        })
    }

    /// `{issuer}/.well-known/openid-configuration`, by concatenation since `Url::join` drops an issuer path.
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

        let document: Discovery = capped_json(response, "discovery document").await?;

        // The document must name the configured issuer.
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

/// `response.json()`, reading at most [`MAX_BODY_BYTES`] — chunk by chunk, so a
/// lying or absent `Content-Length` changes nothing.
async fn capped_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    what: &'static str,
) -> PortResult<T> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| PortError::unavailable(DEPENDENCY, error))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(PortError::unavailable(
                DEPENDENCY,
                DiscoveryError::TooLarge(what),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|error| PortError::unavailable(DEPENDENCY, error))
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

            capped_json::<JwkSet>(response, "key set").await
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

    #[actix_web::test]
    async fn an_oversized_discovery_document_is_refused_rather_than_buffered() {
        use actix_web::{App, HttpResponse, HttpServer, web};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let server = HttpServer::new(|| {
            App::new().route(
                "/.well-known/openid-configuration",
                web::get().to(|| async {
                    // Valid JSON, just far too much of it.
                    let padding = " ".repeat(MAX_BODY_BYTES + 1);
                    HttpResponse::Ok().body(format!(
                        "{{\"issuer\":\"x\",{padding}\"jwks_uri\":\"http://x\"}}"
                    ))
                }),
            )
        })
        .workers(1)
        .listen(listener)
        .unwrap()
        .run();
        actix_web::rt::spawn(server);

        let error = HttpJwksSource::new(&config(&issuer))
            .unwrap()
            .fetch()
            .await
            .unwrap_err();

        let rendered = core::error::Error::source(&error).unwrap().to_string();
        assert!(rendered.contains("larger than"), "{rendered}");
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
