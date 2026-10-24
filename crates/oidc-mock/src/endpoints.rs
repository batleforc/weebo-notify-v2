//! The HTTP surface: discovery, JWKS, authorize, token, userinfo, logout.

use actix_cors::Cors;
use actix_web::http::header;
use actix_web::{HttpRequest, HttpResponse, Responder, ResponseError, web};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::state::{AuthRequest, GrantRefusal, Mock, Tokens};

/// Mount the provider. CORS is wide open: the web client is public and exchanges tokens from
/// the browser, and this binary only ever runs in dev and CI.
pub fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::scope("")
            .wrap(Cors::permissive())
            .route(
                "/.well-known/openid-configuration",
                web::get().to(discovery),
            )
            .route("/jwks.json", web::get().to(jwks))
            .route("/authorize", web::get().to(authorize))
            .route("/authorize", web::post().to(login))
            .route("/token", web::post().to(token))
            .route("/userinfo", web::get().to(userinfo))
            .route("/end-session", web::get().to(end_session))
            .route("/admin/rotate-keys", web::post().to(rotate_keys)),
    );
}

/// A refusal the browser sees as a page and a client sees as OAuth JSON.
#[derive(Debug)]
struct Refusal {
    status: actix_web::http::StatusCode,
    code: &'static str,
    description: String,
}

impl Refusal {
    fn bad_request(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            status: actix_web::http::StatusCode::BAD_REQUEST,
            code,
            description: description.into(),
        }
    }

    fn unauthorized(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            status: actix_web::http::StatusCode::UNAUTHORIZED,
            code,
            description: description.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.description)
    }
}

impl ResponseError for Refusal {
    fn status_code(&self) -> actix_web::http::StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status).json(json!({
            "error": self.code,
            "error_description": self.description,
        }))
    }
}

// --- Discovery ------------------------------------------------------------

async fn discovery(mock: web::Data<Mock>) -> impl Responder {
    let issuer = mock.issuer();

    // Echoed exactly: the server refuses an issuer that doesn't match its own `[oidc] issuer`.
    HttpResponse::Ok().json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks.json"),
        "userinfo_endpoint": format!("{issuer}/userinfo"),
        "end_session_endpoint": format!("{issuer}/end-session"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "scopes_supported": mock.client().scopes,
        "claims_supported": [
            "sub", "iss", "aud", "exp", "iat", "auth_time", "nonce",
            "preferred_username", "name", "email", "email_verified", "groups",
        ],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

async fn jwks(mock: web::Data<Mock>) -> impl Responder {
    HttpResponse::Ok().json(mock.jwks())
}

// --- Authorize ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    nonce: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

async fn authorize(
    mock: web::Data<Mock>,
    params: web::Query<AuthorizeParams>,
) -> Result<HttpResponse, Refusal> {
    let params = params.into_inner();
    let client = mock.client();

    // Checked first, and refused as a page: redirecting to an unregistered URI is an open redirect.
    if params.client_id.as_deref() != Some(client.id.as_str()) {
        return Err(Refusal::bad_request(
            "unauthorized_client",
            format!(
                "unknown client_id; this provider only knows `{}`",
                client.id
            ),
        ));
    }

    let Some(redirect_uri) = params.redirect_uri.filter(|uri| {
        client
            .redirect_uris
            .iter()
            .any(|registered| registered == uri)
    }) else {
        return Err(Refusal::bad_request(
            "invalid_request",
            format!(
                "redirect_uri is missing or not registered; dev/oidc-mock/identities.toml \
                 allows {}",
                client.redirect_uris.join(", ")
            ),
        ));
    };

    // From here the URI is trusted, so errors travel back per RFC 6749 §4.1.2.1.
    let state = params.state.clone();
    let fail = |code: &str, description: &str| {
        redirect_error(&redirect_uri, code, description, state.as_deref())
    };

    if params.response_type.as_deref() != Some("code") {
        return Ok(fail(
            "unsupported_response_type",
            "this provider only implements the authorization code flow",
        ));
    }
    if params.code_challenge_method.as_deref() != Some("S256") {
        return Ok(fail(
            "invalid_request",
            "code_challenge_method=S256 is required — PKCE is not optional here",
        ));
    }
    let Some(challenge) = params.code_challenge.filter(|c| !c.is_empty()) else {
        return Ok(fail("invalid_request", "code_challenge is required"));
    };

    // Unregistered scopes are dropped, not refused, as real providers do.
    let scope = granted_scopes(mock.client(), params.scope.as_deref());

    let id = mock.begin(AuthRequest {
        redirect_uri,
        state: params.state,
        nonce: params.nonce,
        challenge,
        scope,
        expires_at: crate::state::request_deadline(),
    });

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(login_page(&mock, &id, None)))
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    request: String,
    username: String,
    password: String,
}

async fn login(mock: web::Data<Mock>, form: web::Form<LoginForm>) -> Result<HttpResponse, Refusal> {
    let form = form.into_inner();

    if mock.pending(&form.request).is_none() {
        return Err(Refusal::bad_request(
            "invalid_request",
            "this login page has expired — start the flow again",
        ));
    }

    let Some(user) = mock
        .identities()
        .authenticate(&form.username, &form.password)
        .cloned()
    else {
        // Re-render rather than redirect, without saying which half was wrong.
        return Ok(HttpResponse::Unauthorized()
            .content_type("text/html; charset=utf-8")
            .body(login_page(
                &mock,
                &form.request,
                Some("Unknown username or password."),
            )));
    };

    let Some((code, request)) = mock.authorize(&form.request, &user) else {
        return Err(Refusal::bad_request(
            "invalid_request",
            "this login page has expired — start the flow again",
        ));
    };

    let mut redirect = parse_redirect(&request.redirect_uri)?;
    redirect.query_pairs_mut().append_pair("code", &code);
    if let Some(state) = &request.state {
        redirect.query_pairs_mut().append_pair("state", state);
    }

    Ok(HttpResponse::Found()
        .insert_header((header::LOCATION, redirect.to_string()))
        .finish())
}

// --- Token ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
}

async fn token(mock: web::Data<Mock>, form: web::Form<TokenForm>) -> Result<HttpResponse, Refusal> {
    let form = form.into_inner();

    if let Some(client_id) = &form.client_id
        && client_id != &mock.client().id
    {
        return Err(Refusal::bad_request(
            "invalid_client",
            "client_id does not match the registered client",
        ));
    }

    let tokens = match form.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier), Some(redirect_uri)) =
                (&form.code, &form.code_verifier, &form.redirect_uri)
            else {
                return Err(Refusal::bad_request(
                    "invalid_request",
                    "code, code_verifier and redirect_uri are all required",
                ));
            };

            mock.redeem_code(code, verifier, redirect_uri)
        }
        "refresh_token" => {
            let Some(refresh_token) = &form.refresh_token else {
                return Err(Refusal::bad_request(
                    "invalid_request",
                    "refresh_token is required",
                ));
            };

            mock.redeem_refresh(refresh_token)
        }
        other => {
            return Err(Refusal::bad_request(
                "unsupported_grant_type",
                format!("`{other}` is not implemented; use authorization_code or refresh_token"),
            ));
        }
    };

    tokens.map(as_token_response).map_err(refusal_for)
}

fn as_token_response(tokens: Tokens) -> HttpResponse {
    let mut body = json!({
        "access_token": tokens.access_token,
        "id_token": tokens.id_token,
        "token_type": "Bearer",
        "expires_in": tokens.expires_in,
        "scope": tokens.scope,
    });
    if let Some(refresh) = tokens.refresh_token
        && let Some(object) = body.as_object_mut()
    {
        object.insert("refresh_token".to_owned(), json!(refresh));
    }

    HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .insert_header((header::PRAGMA, "no-cache"))
        .json(body)
}

fn refusal_for(refusal: GrantRefusal) -> Refusal {
    match refusal {
        GrantRefusal::UnknownGrant => Refusal::bad_request(
            "invalid_grant",
            "unknown, expired or already-redeemed grant",
        ),
        GrantRefusal::PkceMismatch => Refusal::bad_request(
            "invalid_grant",
            "code_verifier does not match the code_challenge this code was issued for",
        ),
        GrantRefusal::RedirectMismatch => Refusal::bad_request(
            "invalid_grant",
            "redirect_uri does not match the one this code was issued for",
        ),
        GrantRefusal::UnknownUser => Refusal::bad_request(
            "invalid_grant",
            "the account behind this grant is no longer in the identities file",
        ),
    }
}

// --- Userinfo and logout --------------------------------------------------

async fn userinfo(mock: web::Data<Mock>, request: HttpRequest) -> Result<HttpResponse, Refusal> {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty());

    let Some(claims) = bearer.and_then(|token| mock.verify(token)) else {
        return Err(Refusal::unauthorized(
            "invalid_token",
            "a valid Bearer access token issued by this provider is required",
        ));
    };

    Ok(HttpResponse::Ok().json(claims))
}

#[derive(Debug, Deserialize)]
struct LogoutParams {
    post_logout_redirect_uri: Option<String>,
    state: Option<String>,
}

async fn end_session(
    mock: web::Data<Mock>,
    params: web::Query<LogoutParams>,
) -> Result<HttpResponse, Refusal> {
    // Token-only model (§5.1): logout just gives the client's redirect somewhere to land.
    let Some(target) = params.post_logout_redirect_uri.clone() else {
        return Ok(HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(page("Signed out", "<p>You are signed out.</p>")));
    };

    if !mock.client().post_logout_redirect_uris.contains(&target) {
        return Err(Refusal::bad_request(
            "invalid_request",
            "post_logout_redirect_uri is not registered",
        ));
    }

    let mut redirect = parse_redirect(&target)?;
    if let Some(state) = &params.state {
        redirect.query_pairs_mut().append_pair("state", state);
    }

    Ok(HttpResponse::Found()
        .insert_header((header::LOCATION, redirect.to_string()))
        .finish())
}

async fn rotate_keys(mock: web::Data<Mock>) -> Result<HttpResponse, Refusal> {
    // Not OIDC: a hook so the auth suite can rotate keys on demand (T29).
    let kid = mock.rotate_keys().map_err(|error| {
        Refusal::bad_request(
            "server_error",
            format!("could not rotate the keys: {error}"),
        )
    })?;

    Ok(HttpResponse::Ok().json(json!({ "kid": kid })))
}

// --- Plumbing -------------------------------------------------------------

fn parse_redirect(uri: &str) -> Result<Url, Refusal> {
    Url::parse(uri).map_err(|error| {
        // Unreachable over HTTP (the URI came from the registered list); still no panic.
        Refusal::bad_request(
            "invalid_request",
            format!("`{uri}` in dev/oidc-mock/identities.toml is not a URL: {error}"),
        )
    })
}

fn redirect_error(
    redirect_uri: &str,
    code: &str,
    description: &str,
    state: Option<&str>,
) -> HttpResponse {
    let Ok(mut redirect) = Url::parse(redirect_uri) else {
        return HttpResponse::BadRequest().json(json!({
            "error": code,
            "error_description": description,
        }));
    };

    redirect.query_pairs_mut().append_pair("error", code);
    redirect
        .query_pairs_mut()
        .append_pair("error_description", description);
    if let Some(state) = state {
        redirect.query_pairs_mut().append_pair("state", state);
    }

    HttpResponse::Found()
        .insert_header((header::LOCATION, redirect.to_string()))
        .finish()
}

/// Keep only the scopes the client registered, always including `openid`.
fn granted_scopes(client: &crate::config::Client, requested: Option<&str>) -> String {
    let requested = requested.unwrap_or("openid");
    let mut granted: Vec<&str> = requested
        .split_whitespace()
        .filter(|scope| client.scopes.iter().any(|allowed| allowed == scope))
        .collect();

    if !granted.contains(&"openid") {
        granted.insert(0, "openid");
    }

    granted.join(" ")
}

/// Escape the few characters that would otherwise close a tag or an attribute.
fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>\
         :root{{color-scheme:light dark}}\
         body{{font:16px/1.5 ui-sans-serif,system-ui,sans-serif;margin:0;display:grid;\
         place-items:center;min-height:100vh}}\
         main{{width:min(24rem,90vw);padding:2rem;border:1px solid #8883;border-radius:.75rem}}\
         h1{{font-size:1.25rem;margin:0 0 1.5rem}}\
         label{{display:block;margin:0 0 1rem}}\
         input{{width:100%;padding:.5rem;font:inherit;border:1px solid #8886;border-radius:.375rem;\
         background:transparent;color:inherit}}\
         button{{width:100%;padding:.6rem;font:inherit;border:0;border-radius:.375rem;\
         background:#4f46e5;color:#fff;cursor:pointer}}\
         .error{{color:#dc2626;margin:0 0 1rem}}\
         .hint{{color:#8889;font-size:.875rem;margin:1.5rem 0 0}}\
         </style></head><body><main><h1>{title}</h1>{body}</main></body></html>"
    )
}

fn login_page(mock: &Mock, request_id: &str, error: Option<&str>) -> String {
    let error = error.map_or_else(String::new, |message| {
        format!("<p class=\"error\">{}</p>", escape(message))
    });

    let accounts = mock
        .identities()
        .users
        .iter()
        .map(|user| escape(&user.username))
        .collect::<Vec<_>>()
        .join(", ");

    let body = format!(
        "{error}\
         <form method=\"post\" action=\"/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <label>Username<input name=\"username\" autocomplete=\"username\" autofocus required></label>\
         <label>Password<input name=\"password\" type=\"password\" \
         autocomplete=\"current-password\" required></label>\
         <button type=\"submit\">Sign in</button></form>\
         <p class=\"hint\">Development provider. Accounts: {accounts} — \
         see <code>dev/oidc-mock/identities.toml</code>.</p>",
        request = escape(request_id),
    );

    page("weebo-notify", &body)
}

#[cfg(test)]
mod tests {
    use actix_web::{App, test};

    use super::*;
    use crate::config::Identities;

    fn provider() -> web::Data<Mock> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/oidc-mock/identities.toml");
        let identities = Identities::load(&path).expect("the committed identities file");

        web::Data::new(Mock::new(identities).expect("a provider builds"))
    }

    macro_rules! service {
        ($mock:expr) => {
            test::init_service(App::new().app_data($mock.clone()).configure(configure)).await
        };
    }

    #[actix_web::test]
    async fn discovery_points_every_endpoint_at_the_configured_issuer() {
        let mock = provider();
        let app = service!(mock);

        let document: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/.well-known/openid-configuration")
                .to_request(),
        )
        .await;

        assert_eq!(document["issuer"], mock.issuer());
        assert_eq!(
            document["jwks_uri"],
            format!("{}/jwks.json", mock.issuer()),
            "the server fetches this URL; a wrong host here is a cold JWKS cache"
        );
        assert_eq!(
            document["code_challenge_methods_supported"],
            json!(["S256"])
        );
    }

    #[actix_web::test]
    async fn the_jwks_publishes_a_signing_key() {
        let app = service!(provider());

        let jwks: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/jwks.json").to_request(),
        )
        .await;

        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["alg"], "ES256");
        assert!(keys[0]["kid"].is_string());
        assert!(
            keys[0].get("d").is_none(),
            "publishing the private scalar would be the one unforgivable bug here"
        );
    }

    #[actix_web::test]
    async fn an_unregistered_redirect_uri_is_a_page_and_never_a_redirect() {
        let app = service!(provider());

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(
                    "/authorize?response_type=code&client_id=weebo-notify\
                     &redirect_uri=https://evil.example/steal&code_challenge=x\
                     &code_challenge_method=S256",
                )
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 400);
        assert!(
            response.headers().get(header::LOCATION).is_none(),
            "bouncing the browser to an unregistered URI to report the error is how \
             an open redirect gets built by accident"
        );
    }

    #[actix_web::test]
    async fn a_request_without_pkce_comes_back_as_an_oauth_error() {
        let app = service!(provider());

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(
                    "/authorize?response_type=code&client_id=weebo-notify\
                     &redirect_uri=http://localhost:5173/auth/callback&state=opaque",
                )
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 302);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(location.starts_with("http://localhost:5173/auth/callback?"));
        assert!(location.contains("error=invalid_request"));
        assert!(
            location.contains("state=opaque"),
            "state is echoed on errors too"
        );
    }

    #[actix_web::test]
    async fn an_unknown_client_is_refused_before_anything_else_is_looked_at() {
        let app = service!(provider());

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/authorize?response_type=code&client_id=someone-else")
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 400);
    }

    #[actix_web::test]
    async fn a_wrong_password_re_renders_the_form_instead_of_redirecting() {
        let mock = provider();
        let app = service!(mock);

        let id = mock.begin(AuthRequest {
            redirect_uri: "http://localhost:5173/auth/callback".to_owned(),
            state: None,
            nonce: None,
            challenge: crate::state::challenge_for("verifier"),
            scope: "openid".to_owned(),
            expires_at: crate::state::request_deadline(),
        });

        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/authorize")
                .set_form([
                    ("request", id.as_str()),
                    ("username", "alice"),
                    ("password", "hunter2"),
                ])
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 401);
        assert!(response.headers().get(header::LOCATION).is_none());
    }

    #[actix_web::test]
    async fn a_full_browser_flow_ends_with_tokens_and_userinfo() {
        let mock = provider();
        let app = service!(mock);
        let verifier = "a-verifier-long-enough-to-be-plausible";
        let challenge = crate::state::challenge_for(verifier);

        // 1. The browser lands on the login page.
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/authorize?response_type=code&client_id=weebo-notify\
                     &redirect_uri=http://localhost:5173/auth/callback\
                     &scope=openid+profile+email+groups+offline_access&state=opaque\
                     &nonce=once&code_challenge={challenge}&code_challenge_method=S256"
                ))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);

        let html = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
        let request_id = html
            .split("name=\"request\" value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the form carries the request handle");

        // 2. Alice signs in and is bounced back with a code.
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/authorize")
                .set_form([
                    ("request", request_id),
                    ("username", "alice"),
                    ("password", "weebo-dev"),
                ])
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 302);

        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let callback = Url::parse(&location).unwrap();
        let code = callback
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .expect("the callback carries a code");
        assert_eq!(
            callback
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .as_deref(),
            Some("opaque")
        );

        // 3. The SPA exchanges it, from the browser, for real tokens.
        let tokens: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri("/token")
                .set_form([
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("code_verifier", verifier),
                    ("redirect_uri", "http://localhost:5173/auth/callback"),
                    ("client_id", "weebo-notify"),
                ])
                .to_request(),
        )
        .await;

        assert_eq!(tokens["token_type"], "Bearer");
        assert!(
            tokens["refresh_token"].is_string(),
            "offline_access was granted"
        );
        let access = tokens["access_token"].as_str().unwrap();

        // 4. And the token works.
        let claims: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/userinfo")
                .insert_header((header::AUTHORIZATION, format!("Bearer {access}")))
                .to_request(),
        )
        .await;

        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["groups"], json!(["infra-admins", "platform-admins"]));
    }

    #[actix_web::test]
    async fn userinfo_without_a_token_is_a_401() {
        let app = service!(provider());

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/userinfo").to_request()).await;

        assert_eq!(response.status(), 401);
    }

    #[actix_web::test]
    async fn an_unsupported_grant_type_says_which_ones_exist() {
        let app = service!(provider());

        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/token")
                .set_form([("grant_type", "password")])
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 400);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "unsupported_grant_type");
    }

    #[actix_web::test]
    async fn logout_refuses_a_destination_the_client_never_registered() {
        let app = service!(provider());

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/end-session?post_logout_redirect_uri=https://evil.example/")
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), 400);
        assert!(response.headers().get(header::LOCATION).is_none());
    }

    #[actix_web::test]
    async fn rotating_the_keys_publishes_a_second_one() {
        let app = service!(provider());

        let rotated: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri("/admin/rotate-keys")
                .to_request(),
        )
        .await;
        assert!(rotated["kid"].is_string());

        let jwks: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::get().uri("/jwks.json").to_request(),
        )
        .await;

        assert_eq!(
            jwks["keys"].as_array().unwrap().len(),
            2,
            "the retired key stays published until the tokens it signed expire"
        );
    }

    // `use actix_web::test` shadows the built-in attribute.
    #[actix_web::test]
    async fn scopes_the_client_never_registered_are_dropped_rather_than_refused() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/oidc-mock/identities.toml");
        let identities = Identities::load(&path).unwrap();

        let granted = granted_scopes(&identities.client, Some("openid email admin:everything"));

        assert_eq!(granted, "openid email");
    }

    #[actix_web::test]
    async fn openid_is_granted_even_when_the_client_forgot_to_ask() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/oidc-mock/identities.toml");
        let identities = Identities::load(&path).unwrap();

        assert_eq!(
            granted_scopes(&identities.client, Some("email")),
            "openid email"
        );
    }
}
