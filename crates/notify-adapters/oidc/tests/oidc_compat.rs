//! The identity-provider compatibility matrix (design §3.2, §15, T29): live-provider tokens through
//! the real [`OidcVerifier`]. `WNK_TEST_OIDC_PROVIDER` selects `mock` (default, in-process), `dex`
//! (no `groups` claim) or `authentik`. Never skips: an external provider without an issuer panics.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a failing assertion is a failing test"
)]
#![allow(
    clippy::panic,
    reason = "a missing provider must stop this suite loudly, and a panic carrying \
              the name of the variable it wants is the message a CI log needs"
)]

use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use notify_oidc::{OidcConfig, OidcVerifier};
use notify_oidc_mock::{Identities, Mock};
use notify_ports::{Clock, SystemClock, TokenVerifier};
use url::Url;

/// Which provider this run reads OIDC through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Mock,
    Dex,
    Authentik,
}

impl Provider {
    fn from_env() -> Self {
        match std::env::var("WNK_TEST_OIDC_PROVIDER").as_deref() {
            Ok("dex") => Self::Dex,
            Ok("authentik") => Self::Authentik,
            // An unset CI variable expands to empty: the default.
            Ok("mock" | "") | Err(_) => Self::Mock,
            Ok(other) => panic!(
                "WNK_TEST_OIDC_PROVIDER={other} is not a provider this suite knows \
                 (mock, dex, authentik). Refusing to run something else and call it \
                 compatibility."
            ),
        }
    }

    /// Whether this provider emits a `groups` claim; Dex does not, exercising the T29 degradation path.
    const fn emits_groups(self) -> bool {
        !matches!(self, Self::Dex)
    }

    /// Whether `sub` is the sign-in username: only where this suite arranged it (mock, Authentik).
    /// OIDC `sub` is opaque (Dex base64-encodes its id); stability is what is asserted for all.
    const fn subject_is_username(self) -> bool {
        !matches!(self, Self::Dex)
    }
}

/// Which of the test accounts to sign in as.
#[derive(Debug, Clone, Copy)]
enum Account {
    /// Somebody the provider puts in groups — where it has any.
    InGroups,
    /// Somebody with no group membership at all.
    Groupless,
}

/// A live provider, and what it takes to get a token out of it.
struct Fixture {
    provider: Provider,
    issuer: Url,
}

impl Fixture {
    /// Bring up, or point at, the provider this run is about.
    fn start() -> Self {
        let provider = Provider::from_env();
        let issuer = match provider {
            Provider::Mock => start_local_provider(),
            _ => Url::parse(&required("WNK_TEST_OIDC_ISSUER", provider))
                .expect("WNK_TEST_OIDC_ISSUER is not a URL"),
        };
        Self { provider, issuer }
    }

    /// The account for `role`; `WNK_TEST_OIDC_USERNAME` overrides it (Dex logs in by email).
    fn username(&self, account: Account) -> String {
        if let Ok(named) = std::env::var("WNK_TEST_OIDC_USERNAME")
            && !named.is_empty()
        {
            return named;
        }

        match (self.provider, account) {
            // Dex emits no groups, so any account is group-less.
            (Provider::Dex, _) | (_, Account::InGroups) => "alice".to_owned(),
            (_, Account::Groupless) => "carol".to_owned(),
        }
    }

    /// A token for that account, obtained the way this provider allows.
    async fn token_for(&self, account: Account) -> String {
        let username = self.username(account);
        match self.provider {
            // Our mock: drive Authorization Code + PKCE end to end.
            Provider::Mock => sign_in_through_the_browser(&self.issuer, &username).await,
            // Third parties: the password grant, so the test starts at verification.
            _ => password_grant(&self.issuer, &username, self.provider).await,
        }
    }
}

/// A variable an external provider cannot run without.
fn required(name: &str, provider: Provider) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "WNK_TEST_OIDC_PROVIDER={provider:?} needs {name}, and it is not set. \
             This suite fails rather than skips: a compatibility job that is green \
             because no provider was reachable is worse than no job at all (§15)."
        )
    })
}

/// The mock on an ephemeral port; call from inside an actix runtime.
fn start_local_provider() -> Url {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../dev/oidc-mock/identities.toml");

    // Port 0 so parallel tests do not collide.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
    let issuer = Url::parse(&format!(
        "http://{}",
        listener.local_addr().expect("a bound address")
    ))
    .expect("a valid issuer");

    let identities = Identities::load(&path)
        .expect("the committed identities file")
        .with_issuer(issuer.as_str().trim_end_matches('/').to_owned());
    let mock = web::Data::new(Mock::new(identities).expect("a provider builds"));

    let server = HttpServer::new(move || {
        App::new()
            .app_data(mock.clone())
            .configure(notify_oidc_mock::configure)
    })
    .workers(1)
    .listen(listener)
    .expect("the provider binds")
    .run();

    actix_web::rt::spawn(server);
    issuer
}

/// Discover the provider's endpoints rather than guessing their shape.
async fn discover(issuer: &Url, field: &str) -> String {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    let document: serde_json::Value = reqwest::get(&url)
        .await
        .unwrap_or_else(|error| panic!("{url} is unreachable: {error}"))
        .json()
        .await
        .unwrap_or_else(|error| panic!("{url} did not return a discovery document: {error}"));

    document[field]
        .as_str()
        .unwrap_or_else(|| panic!("the discovery document at {url} has no {field}"))
        .to_owned()
}

/// Walk the browser half of the flow and come back with an access token.
async fn sign_in_through_the_browser(issuer: &Url, username: &str) -> String {
    // A fixed PKCE verifier is fine in a test.
    let verifier = "an-entirely-unremarkable-code-verifier";
    let challenge = challenge_for(verifier);
    let redirect_uri = "http://localhost:5173/auth/callback";

    let http = reqwest::Client::builder()
        // Step by step so each redirect can be asserted on.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("an http client");

    let page = http
        .get(format!("{issuer}authorize"))
        .query(&[
            ("response_type", "code"),
            ("client_id", "weebo-notify"),
            ("redirect_uri", redirect_uri),
            ("scope", "openid profile email groups offline_access"),
            ("state", "opaque"),
            ("nonce", "once"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .expect("the login page")
        .text()
        .await
        .expect("a body");

    let request_id = page
        .split("name=\"request\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the login form carries a request handle");

    let response = http
        .post(format!("{issuer}authorize"))
        .form(&[
            ("request", request_id),
            ("username", username),
            ("password", "weebo-dev"),
        ])
        .send()
        .await
        .expect("the login is processed");

    assert_eq!(
        response.status(),
        302,
        "a good password bounces the browser"
    );
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("a callback URL");
    let callback = Url::parse(location).expect("a valid callback URL");
    let code = callback
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .expect("the callback carries a code");

    let tokens: serde_json::Value = http
        .post(format!("{issuer}token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("client_id", "weebo-notify"),
        ])
        .send()
        .await
        .expect("the token exchange")
        .json()
        .await
        .expect("a token response");

    tokens["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned()
}

/// Take a token from a third-party provider through the password grant.
async fn password_grant(issuer: &Url, username: &str, provider: Provider) -> String {
    let endpoint = discover(issuer, "token_endpoint").await;
    let client_id = client_id();
    let password = password_for(username, provider);
    let secret = std::env::var("WNK_TEST_OIDC_CLIENT_SECRET").unwrap_or_default();

    let mut form = vec![
        ("grant_type", "password".to_owned()),
        ("username", username.to_owned()),
        ("password", password),
        ("client_id", client_id),
        ("scope", "openid profile email groups".to_owned()),
    ];
    if !secret.is_empty() {
        form.push(("client_secret", secret));
    }

    let response = reqwest::Client::new()
        .post(&endpoint)
        .form(&form)
        .send()
        .await
        .unwrap_or_else(|error| panic!("{endpoint} is unreachable: {error}"));

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "{provider:?} refused the password grant ({status}): {body}"
    );

    let tokens: serde_json::Value =
        serde_json::from_str(&body).expect("the token endpoint returned JSON");
    tokens["access_token"]
        .as_str()
        .expect("an access token")
        .to_owned()
}

/// The password-grant secret: `WNK_TEST_OIDC_PASSWORD_<ACCOUNT>`, then `WNK_TEST_OIDC_PASSWORD`.
/// Per-account because Authentik's password grant takes a per-user app-password token.
fn password_for(username: &str, provider: Provider) -> String {
    let per_account = format!(
        "WNK_TEST_OIDC_PASSWORD_{}",
        username
            .to_uppercase()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    );

    for name in [per_account.as_str(), "WNK_TEST_OIDC_PASSWORD"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return value;
        }
    }

    // Name both variables; falling back to another account's secret would fail as `invalid_grant`.
    panic!(
        "WNK_TEST_OIDC_PROVIDER={provider:?} needs a secret for {username}, and \
         neither {per_account} nor WNK_TEST_OIDC_PASSWORD is set. This suite fails \
         rather than skips: a compatibility job that is green because no provider \
         was reachable is worse than no job at all (§15)."
    );
}

fn client_id() -> String {
    std::env::var("WNK_TEST_OIDC_CLIENT_ID").unwrap_or_else(|_| "weebo-notify".to_owned())
}

fn challenge_for(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The verifier as the server wires it, minus the refresh throttle.
fn verifier(issuer: &Url) -> OidcVerifier {
    let mut config = OidcConfig::new(issuer.clone(), client_id());
    // The production throttle (a minute) would make the rotation test sleep.
    config.min_refresh_interval = time::Duration::ZERO;

    OidcVerifier::new(config, Arc::new(SystemClock) as Arc<dyn Clock>)
        .expect("the verifier builds against a live issuer")
}

/// What every provider owes about `sub`: non-empty and stable (§5.1 `find_by_subject`).
/// Username equality only where arranged — see [`Provider::subject_is_username`].
fn assert_subject(fixture: &Fixture, subject: &str, account: Account) {
    assert!(
        !subject.is_empty(),
        "{:?} returned an empty `sub`; §5.1 keys every user on it",
        fixture.provider
    );

    if fixture.provider.subject_is_username() {
        assert_eq!(subject, fixture.username(account));
    }
}

#[actix_web::test]
async fn the_subject_is_the_same_on_a_second_login() {
    // A moving `sub` would mint a new account per login (§5.1).
    let fixture = Fixture::start();

    let first = verifier(&fixture.issuer)
        .verify(&fixture.token_for(Account::InGroups).await)
        .await
        .expect("the issuer is reachable")
        .expect("the token is accepted");
    let second = verifier(&fixture.issuer)
        .verify(&fixture.token_for(Account::InGroups).await)
        .await
        .expect("the issuer is reachable")
        .expect("the token is accepted");

    assert_eq!(
        first.subject, second.subject,
        "{:?} gave two different subjects for one account",
        fixture.provider
    );
}

#[actix_web::test]
async fn a_token_from_a_live_provider_verifies_through_the_production_path() {
    let fixture = Fixture::start();
    let token = fixture.token_for(Account::InGroups).await;

    let claims = verifier(&fixture.issuer)
        .verify(&token)
        .await
        .expect("the issuer is reachable")
        .expect("the token is accepted");

    assert_subject(&fixture, &claims.subject, Account::InGroups);

    if fixture.provider.emits_groups() {
        assert!(
            !claims.groups.is_empty(),
            "{:?} advertises groups, and channel access is derived from that claim \
             on every request (§14.2) — an empty list here means the mapping is not \
             wired and nobody would see a channel",
            fixture.provider
        );
    }
}

#[actix_web::test]
async fn an_account_with_no_groups_verifies_and_carries_none() {
    // T29: no `groups` claim degrades to nothing granted, never a default.
    let fixture = Fixture::start();
    let token = fixture.token_for(Account::Groupless).await;

    let claims = verifier(&fixture.issuer)
        .verify(&token)
        .await
        .expect("the issuer is reachable")
        .expect("a group-less token is still a valid token");

    assert_subject(&fixture, &claims.subject, Account::Groupless);
    assert!(
        claims.groups.is_empty(),
        "an account in no group must arrive carrying none, so that an empty list \
         grants nothing (§14.2)"
    );
}

#[actix_web::test]
async fn a_token_signed_by_a_rotated_key_verifies_after_the_cache_catches_up() {
    let fixture = Fixture::start();

    if fixture.provider != Provider::Mock {
        // Rotation cannot be triggered on a third party; the mock leg covers our cache.
        eprintln!(
            "skipping rotation against {:?}: only the mock can be told to rotate on demand",
            fixture.provider
        );
        return;
    }

    let issuer = &fixture.issuer;
    let verifier = verifier(issuer);

    // Warm the cache on the first key, so the refresh below is a real refresh.
    let before = fixture.token_for(Account::InGroups).await;
    assert!(verifier.verify(&before).await.unwrap().is_ok());

    reqwest::Client::new()
        .post(format!("{issuer}admin/rotate-keys"))
        .send()
        .await
        .expect("the provider rotates on demand");

    let after = fixture.token_for(Account::InGroups).await;

    assert!(
        verifier.verify(&after).await.unwrap().is_ok(),
        "an unknown kid must trigger a JWKS refresh rather than a rejection"
    );
    assert!(
        verifier.verify(&before).await.unwrap().is_ok(),
        "and the token minted before the rotation must survive it"
    );
}
