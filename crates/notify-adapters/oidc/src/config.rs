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

    /// How long a fetched key set is trusted before it is fetched again, even
    /// when every `kid` it names is still being presented.
    ///
    /// Without it a key the issuer withdrew — the compromise case, where removing
    /// it is the whole response — would keep verifying on this replica until the
    /// process restarted.
    pub key_max_age: Duration,

    /// How long past [`Self::key_max_age`] an expired set keeps serving while the
    /// issuer cannot be reached — and so how long, worst case, a key the issuer
    /// withdrew keeps verifying here during an outage. Bounded so an IdP outage
    /// degrades to "keys a little stale" rather than "keys never refreshed again".
    ///
    /// Zero drops the set on the first failed refetch past its max age: no
    /// withdrawn-key window, at the price of logging everybody out on any IdP blip
    /// and of every request waiting on the refresh rather than only one.
    pub stale_key_grace: Duration,

    /// Whether an `email` the issuer does not mark `email_verified: true` is still
    /// trusted. Off: an unverified address is whatever the user typed at sign-up,
    /// and an individual `email` binding (design §11) would hand its channel to
    /// whoever typed it first. Only for an issuer that never sends the claim.
    pub trust_unverified_email: bool,

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
            key_max_age: Duration::hours(1),
            stale_key_grace: Duration::hours(1),
            trust_unverified_email: false,
            http_timeout: Duration::seconds(5),
        }
    }
}
