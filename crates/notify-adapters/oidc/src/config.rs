//! How the verifier is configured (design §11).

use time::Duration;
use url::Url;

/// Everything the OIDC verifier needs, and nothing it can discover for itself.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The issuer, exactly as it appears in the `iss` claim.
    ///
    /// Discovery hangs off it (`{issuer}/.well-known/openid-configuration`) and the
    /// document is required to name it back — see the crate docs.
    pub issuer: Url,

    /// The audiences a token may carry.
    ///
    /// Usually the one client id, but kept plural because an access token's `aud`
    /// names the *API* rather than the client on several issuers, and an instance
    /// migrating between the two has to accept both for a while.
    pub audiences: Vec<String>,

    /// Which claim carries group membership.
    ///
    /// Configurable because there is no standard one: Authentik says `groups`,
    /// Keycloak says whatever the mapper was called, and Dex says nothing at all
    /// (scenario T29). An issuer that omits it yields no groups, which is a
    /// supported answer — the user reaches their individual bindings and stops
    /// there.
    pub groups_claim: String,

    /// Clock skew tolerated on `exp` and `nbf`.
    ///
    /// Small but not zero: a token minted by a host thirty seconds ahead of ours is
    /// a fact of life on real deployments, and refusing it produces a login loop
    /// nobody can debug from the client side.
    pub leeway: Duration,

    /// The shortest interval between two JWKS fetches.
    ///
    /// Also the throttle that stops forged `kid`s from turning into IdP traffic.
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
