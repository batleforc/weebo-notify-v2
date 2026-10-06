//! How the verifier is configured (design §11).

use time::Duration;
use url::Url;

/// Everything the OIDC verifier needs, and nothing it can discover for itself.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The issuer, exactly as in `iss`; discovery must name it back.
    pub issuer: Url,

    /// The audiences a token may carry; plural since some issuers put the API, not the client, in `aud`.
    pub audiences: Vec<String>,

    /// Which claim carries group membership; no standard name, and absent means no groups (T29).
    pub groups_claim: String,

    /// Clock skew tolerated on `exp` and `nbf`; nonzero to avoid login loops on skewed hosts.
    pub leeway: Duration,

    /// The shortest interval between two JWKS fetches; also throttles forged `kid`s.
    pub min_refresh_interval: Duration,

    /// How long to wait on the IdP before calling it unavailable.
    pub http_timeout: Duration,
}

impl OidcConfig {
    /// The defaults, for an issuer and a client id.
    #[must_use]
    pub fn new(issuer: Url, client_id: impl Into<String>) -> Self {
        Self {
            issuer,
            audiences: vec![client_id.into()],
            groups_claim: "groups".to_owned(),
            leeway: Duration::seconds(60),
            min_refresh_interval: Duration::seconds(60),
            http_timeout: Duration::seconds(5),
        }
    }
}
