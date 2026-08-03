//! The contract between this adapter and the development identity provider.
//!
//! Every other test in this crate signs its own tokens, which proves the
//! verification logic and nothing about the provider developers actually run. This
//! one boots `notify-oidc-mock` on a real socket, walks a real Authorization Code
//! flow over HTTP, and hands the resulting token to the real [`OidcVerifier`] —
//! discovery, JWKS fetch, `kid` lookup, signature, audience, issuer and expiry
//! included.
//!
//! If this passes, `task dev:up` produces a login that works. If it fails, the
//! development environment is broken before anyone types a password.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use notify_oidc::{OidcConfig, OidcVerifier};
use notify_oidc_mock::{Identities, Mock};
use notify_ports::{Clock, SystemClock, TokenVerifier};
use url::Url;

/// A provider on an ephemeral port, told to call itself by the address it got.
///
/// Synchronous, but it must be called from inside an actix runtime: the server it
/// spawns lives on the caller's.
fn start_provider() -> Url {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../dev/oidc-mock/identities.toml");

    // Port 0, then read back what the OS gave us: a fixed port makes two tests
    // running in parallel fight over it.
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

/// Walk the browser half of the flow and come back with an access token.
async fn sign_in(issuer: &Url, username: &str) -> String {
    // A fixed verifier: this is a test, and the value only has to survive the two
    // calls below.
    let verifier = "an-entirely-unremarkable-code-verifier";
    let challenge = challenge_for(verifier);
    let redirect_uri = "http://localhost:5173/auth/callback";

    let http = reqwest::Client::builder()
        // The flow is driven step by step so each redirect can be asserted on.
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

fn challenge_for(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The verifier as the server wires it, minus the refresh throttle.
fn verifier(issuer: &Url) -> OidcVerifier {
    let mut config = OidcConfig::new(issuer.clone(), "weebo-notify");
    // The production throttle is a minute, which would make the rotation test below
    // sleep for one. Everything else about the path is unchanged.
    config.min_refresh_interval = time::Duration::ZERO;

    OidcVerifier::new(config, Arc::new(SystemClock) as Arc<dyn Clock>)
        .expect("the verifier builds against a live issuer")
}

#[actix_web::test]
async fn a_token_from_the_dev_provider_verifies_through_the_production_path() {
    let issuer = start_provider();
    let token = sign_in(&issuer, "alice").await;

    let claims = verifier(&issuer)
        .verify(&token)
        .await
        .expect("the issuer is reachable")
        .expect("the token is accepted");

    assert_eq!(claims.subject, "alice");
    assert_eq!(claims.email.as_deref(), Some("alice@weebo.local"));
    assert_eq!(claims.groups, ["infra-admins", "platform-admins"]);
}

#[actix_web::test]
async fn an_account_with_no_groups_verifies_and_carries_none() {
    // T29: an issuer that emits no `groups` claim must degrade, not fail. carol is
    // the fixture, and this is the assertion that keeps her useful.
    let issuer = start_provider();
    let token = sign_in(&issuer, "carol").await;

    let claims = verifier(&issuer)
        .verify(&token)
        .await
        .expect("the issuer is reachable")
        .expect("a group-less token is still a valid token");

    assert_eq!(claims.subject, "carol");
    assert!(claims.groups.is_empty());
}

#[actix_web::test]
async fn a_token_signed_by_a_rotated_key_verifies_after_the_cache_catches_up() {
    let issuer = start_provider();
    let verifier = verifier(&issuer);

    // Warm the cache on the first key, so the refresh below is a real refresh.
    let before = sign_in(&issuer, "bob").await;
    assert!(verifier.verify(&before).await.unwrap().is_ok());

    reqwest::Client::new()
        .post(format!("{issuer}admin/rotate-keys"))
        .send()
        .await
        .expect("the provider rotates on demand");

    let after = sign_in(&issuer, "bob").await;

    assert!(
        verifier.verify(&after).await.unwrap().is_ok(),
        "an unknown kid must trigger a JWKS refresh rather than a rejection"
    );
    assert!(
        verifier.verify(&before).await.unwrap().is_ok(),
        "and the token minted before the rotation must survive it"
    );
}
