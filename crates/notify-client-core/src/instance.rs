//! The instance a native client talks to, chosen at first launch (RFC 0001).
//!
//! [`probe`] reads what the instance publishes (`GET /api/v1/client-config`) and checks
//! the identity provider it names the way `web/src/auth/discovery.ts` does, so the
//! shell can show the user both hosts before anything is trusted. [`save`] and
//! [`load`] keep the confirmed answer next to the local store. Every check here is a
//! plain function, so the rules are tested without a network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

/// How long either request of a probe may take, connection included.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The most a probe reads from either answer: both are a few hundred bytes.
const MAX_BODY: usize = 16 * 1024;

/// An instance the user confirmed: where its API is and how to sign in to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    /// The API base, normalised by [`normalise`] (no trailing slash, no query).
    pub api: Url,
    /// The OIDC issuer the instance validates tokens against.
    pub issuer: Url,
    /// The public client to sign in as.
    pub client_id: String,
    /// Where attachments download from, when not from the API itself.
    pub attachment_origin: Option<Url>,
}

/// Why an instance was refused. Each message is shown to the person as written.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InstanceError {
    #[error("this is not a web address")]
    NotAUrl,
    #[error("only https addresses are accepted")]
    NotHttps,
    #[error("the address must not carry a user name, a query or a fragment")]
    Ambiguous,
    #[error("the server did not answer in time, or not at all")]
    Unreachable,
    #[error("this server does not look like a weebo-notify instance ({0})")]
    NotAnInstance(String),
    #[error("the identity provider's configuration is not acceptable: {0}")]
    BadProvider(String),
    #[error("the chosen server could not be saved: {0}")]
    Store(String),
}

/// The address a person typed, reduced to what identifies the instance.
///
/// `https` only; plain `http` is accepted for a loopback host when the build allows
/// it (`allow_loopback_http`, development builds), because a token must never cross
/// a network in clear. User info, query and fragment are refused rather than
/// dropped: an address that means two things is how one ends up meaning another host.
///
/// # Errors
///
/// [`InstanceError::NotAUrl`], [`InstanceError::NotHttps`], [`InstanceError::Ambiguous`].
pub fn normalise(raw: &str, allow_loopback_http: bool) -> Result<Url, InstanceError> {
    let mut url = Url::parse(raw.trim()).map_err(|_| InstanceError::NotAUrl)?;
    check_scheme(&url, allow_loopback_http)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(InstanceError::Ambiguous);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

fn check_scheme(url: &Url, allow_loopback_http: bool) -> Result<(), InstanceError> {
    match url.scheme() {
        "https" if url.host_str().is_some() => Ok(()),
        "http" if allow_loopback_http && is_loopback(url) => Ok(()),
        "http" | "https" => Err(InstanceError::NotHttps),
        _ => Err(InstanceError::NotAUrl),
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// What `GET /api/v1/client-config` answers.
#[derive(Debug, Deserialize)]
struct ClientConfig {
    oidc_issuer: String,
    oidc_client_id: String,
    attachment_origin: Option<String>,
}

/// The parts of an OIDC discovery document a sign-in depends on.
#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    end_session_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    code_challenge_methods_supported: Option<Vec<String>>,
}

/// Turn the instance's answer into an [`Instance`], refusing what the web client would.
fn from_client_config(
    api: Url,
    body: &[u8],
    allow_loopback_http: bool,
) -> Result<Instance, InstanceError> {
    let config: ClientConfig = serde_json::from_slice(body)
        .map_err(|_| InstanceError::NotAnInstance("unexpected answer".to_owned()))?;
    let issuer = Url::parse(&config.oidc_issuer)
        .map_err(|_| InstanceError::BadProvider("the issuer is not an address".to_owned()))?;
    check_scheme(&issuer, allow_loopback_http)
        .map_err(|_| InstanceError::BadProvider("the issuer is not https".to_owned()))?;
    if config.oidc_client_id.is_empty() {
        return Err(InstanceError::NotAnInstance("no client id".to_owned()));
    }
    let attachment_origin = match config.attachment_origin {
        Some(raw) => {
            let origin = Url::parse(&raw).map_err(|_| {
                InstanceError::NotAnInstance("the attachment origin is not an address".to_owned())
            })?;
            check_scheme(&origin, allow_loopback_http).map_err(|_| {
                InstanceError::NotAnInstance("the attachment origin is not https".to_owned())
            })?;
            Some(origin)
        }
        None => None,
    };
    Ok(Instance {
        api,
        issuer,
        client_id: config.oidc_client_id,
        attachment_origin,
    })
}

/// `discovery.ts`'s rules: the document names this issuer, has the two endpoints a
/// sign-in needs, keeps every endpoint on the issuer's origin (the CSP assumes it),
/// and supports PKCE S256 when it says which methods it supports.
fn check_discovery(issuer: &Url, body: &[u8]) -> Result<(), InstanceError> {
    let document: Discovery = serde_json::from_slice(body)
        .map_err(|_| InstanceError::BadProvider("no readable discovery document".to_owned()))?;
    if document.issuer.trim_end_matches('/') != issuer.as_str().trim_end_matches('/') {
        return Err(InstanceError::BadProvider(format!(
            "the document names another issuer ({})",
            document.issuer
        )));
    }
    let (Some(authorization), Some(token)) =
        (document.authorization_endpoint, document.token_endpoint)
    else {
        return Err(InstanceError::BadProvider(
            "an endpoint a sign-in needs is missing".to_owned(),
        ));
    };
    let origin = issuer.origin();
    for endpoint in [
        Some(authorization),
        Some(token),
        document.end_session_endpoint,
        document.revocation_endpoint,
    ]
    .into_iter()
    .flatten()
    {
        let same = Url::parse(&endpoint).is_ok_and(|url| url.origin() == origin);
        if !same {
            return Err(InstanceError::BadProvider(format!(
                "an endpoint points outside the issuer's origin ({endpoint})"
            )));
        }
    }
    if let Some(methods) = document.code_challenge_methods_supported
        && !methods.iter().any(|method| method == "S256")
    {
        return Err(InstanceError::BadProvider(
            "PKCE with S256 is not supported".to_owned(),
        ));
    }
    Ok(())
}

/// Ask the instance how to sign in to it, and check the identity provider it names.
///
/// Two GETs, each bounded in time ([`PROBE_TIMEOUT`]) and size ([`MAX_BODY`]), and
/// neither follows a redirect: the address the person typed must answer for itself.
///
/// # Errors
///
/// Any [`InstanceError`]; nothing is stored either way.
pub async fn probe(raw: &str, allow_loopback_http: bool) -> Result<Instance, InstanceError> {
    let api = normalise(raw, allow_loopback_http)?;
    let http = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .connect_timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| InstanceError::Unreachable)?;

    let config_url = format!(
        "{}/api/v1/client-config",
        api.as_str().trim_end_matches('/')
    );
    let body = fetch(&http, &config_url).await?;
    let instance = from_client_config(api, &body, allow_loopback_http)?;

    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        instance.issuer.as_str().trim_end_matches('/')
    );
    let document = fetch(&http, &discovery_url)
        .await
        .map_err(|error| match error {
            InstanceError::NotAnInstance(detail) => InstanceError::BadProvider(detail),
            other => other,
        })?;
    check_discovery(&instance.issuer, &document)?;
    Ok(instance)
}

/// One bounded GET: a 200 within [`MAX_BODY`] bytes, or a refusal.
async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, InstanceError> {
    let mut response = http
        .get(url)
        .send()
        .await
        .map_err(|_| InstanceError::Unreachable)?;
    if !response.status().is_success() {
        return Err(InstanceError::NotAnInstance(format!(
            "{url} answered {}",
            response.status()
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| InstanceError::Unreachable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(InstanceError::NotAnInstance(
                "the answer is too large".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Instance {
    /// The origins the shell's CSP lets the webview reach: `connect-src` gets the API
    /// and the issuer, `img-src` the API and the attachment origin. Built from parsed
    /// URLs only, so no value can carry a directive of its own.
    #[must_use]
    pub fn csp_origins(&self) -> (Vec<String>, Vec<String>) {
        let api = self.api.origin().ascii_serialization();
        let issuer = self.issuer.origin().ascii_serialization();
        let mut images = vec![api.clone()];
        if let Some(origin) = &self.attachment_origin {
            images.push(origin.origin().ascii_serialization());
        }
        (vec![api, issuer], images)
    }

    /// The key the shell files this instance's credential under, also a directory
    /// name for the `0600` fallback: host, port and path, everything outside
    /// `[A-Za-z0-9.-]` as `_`, so it can never climb out of the store directory.
    #[must_use]
    pub fn id(&self) -> String {
        let without_scheme = self
            .api
            .as_str()
            .split_once("://")
            .map_or(self.api.as_str(), |(_, rest)| rest)
            .trim_end_matches('/');
        without_scheme
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

/// Where the confirmed instance is kept, beside the local store.
fn instance_path() -> Result<PathBuf, InstanceError> {
    crate::config::app_dir(|dirs| dirs.data_dir())
        .map(|dir| dir.join("instance.toml"))
        .ok_or_else(|| InstanceError::Store("no home directory".to_owned()))
}

/// The instance the person confirmed, if any.
///
/// # Errors
///
/// [`InstanceError::Store`] when the file exists but cannot be read or parsed; the
/// shell then shows the instance screen again rather than guessing.
pub fn load() -> Result<Option<Instance>, InstanceError> {
    load_at(&instance_path()?)
}

/// Keep `instance` as the one this client talks to (mode 0600, atomic).
///
/// # Errors
///
/// [`InstanceError::Store`].
pub fn save(instance: &Instance) -> Result<(), InstanceError> {
    save_at(&instance_path()?, instance)
}

/// Forget the chosen instance, as "Change server" does after signing out.
///
/// # Errors
///
/// [`InstanceError::Store`] when the file exists and cannot be removed.
pub fn clear() -> Result<(), InstanceError> {
    match std::fs::remove_file(instance_path()?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(InstanceError::Store(error.to_string())),
    }
}

fn load_at(path: &Path) -> Result<Option<Instance>, InstanceError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|error| InstanceError::Store(error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(InstanceError::Store(error.to_string())),
    }
}

fn save_at(path: &Path, instance: &Instance) -> Result<(), InstanceError> {
    let text =
        toml::to_string(instance).map_err(|error| InstanceError::Store(error.to_string()))?;
    crate::config::write_private(path, text.as_bytes())
        .map_err(|error| InstanceError::Store(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_an_https_address_and_drops_the_trailing_slash() {
        assert_eq!(
            normalise(" https://Notify.Example.org/ ", false)
                .unwrap()
                .as_str(),
            "https://notify.example.org/"
        );
        assert_eq!(
            normalise("https://example.org/weebo/", false)
                .unwrap()
                .as_str(),
            "https://example.org/weebo"
        );
    }

    #[test]
    fn refuses_what_is_not_https_unless_loopback_in_a_dev_build() {
        assert_eq!(
            normalise("http://notify.example.org", true),
            Err(InstanceError::NotHttps)
        );
        assert_eq!(
            normalise("http://localhost:8080", false),
            Err(InstanceError::NotHttps)
        );
        assert!(normalise("http://localhost:8080", true).is_ok());
        assert!(normalise("http://127.0.0.1:8080", true).is_ok());
        assert_eq!(
            normalise("ftp://example.org", true),
            Err(InstanceError::NotAUrl)
        );
        assert_eq!(
            normalise("notify.example.org", true),
            Err(InstanceError::NotAUrl)
        );
    }

    #[test]
    fn refuses_an_address_that_means_two_things() {
        for raw in [
            "https://user@evil.example@notify.example.org",
            "https://notify.example.org/?next=evil",
            "https://notify.example.org/#evil",
        ] {
            assert_eq!(
                normalise(raw, false),
                Err(InstanceError::Ambiguous),
                "{raw}"
            );
        }
    }

    fn api() -> Url {
        Url::parse("https://notify.example.org").unwrap()
    }

    #[test]
    fn reads_what_the_instance_publishes() {
        let instance = from_client_config(
            api(),
            br#"{"oidc_issuer":"https://auth.example.org/app/","oidc_client_id":"weebo-notify","attachment_origin":"https://files.example.org"}"#,
            false,
        )
        .unwrap();
        assert_eq!(instance.issuer.as_str(), "https://auth.example.org/app/");
        assert_eq!(instance.client_id, "weebo-notify");
        let (connect, images) = instance.csp_origins();
        assert_eq!(
            connect,
            ["https://notify.example.org", "https://auth.example.org"]
        );
        assert_eq!(
            images,
            ["https://notify.example.org", "https://files.example.org"]
        );
    }

    #[test]
    fn refuses_an_issuer_in_clear_or_a_page_that_is_not_the_config() {
        assert!(matches!(
            from_client_config(
                api(),
                br#"{"oidc_issuer":"http://auth.example.org","oidc_client_id":"x"}"#,
                false
            ),
            Err(InstanceError::BadProvider(_))
        ));
        assert!(matches!(
            from_client_config(api(), b"<html>not json</html>", false),
            Err(InstanceError::NotAnInstance(_))
        ));
        assert!(matches!(
            from_client_config(
                api(),
                br#"{"oidc_issuer":"https://auth.example.org","oidc_client_id":""}"#,
                false
            ),
            Err(InstanceError::NotAnInstance(_))
        ));
    }

    fn issuer() -> Url {
        Url::parse("https://auth.example.org/app/").unwrap()
    }

    #[test]
    fn accepts_a_provider_that_keeps_to_its_origin() {
        let document = br#"{"issuer":"https://auth.example.org/app","authorization_endpoint":"https://auth.example.org/authorize","token_endpoint":"https://auth.example.org/token","code_challenge_methods_supported":["S256"]}"#;
        assert_eq!(check_discovery(&issuer(), document), Ok(()));
    }

    #[test]
    fn refuses_a_provider_that_points_elsewhere_or_lacks_s256() {
        let elsewhere = br#"{"issuer":"https://auth.example.org/app/","authorization_endpoint":"https://auth.example.org/authorize","token_endpoint":"https://evil.example/token"}"#;
        assert!(matches!(
            check_discovery(&issuer(), elsewhere),
            Err(InstanceError::BadProvider(_))
        ));
        let other_issuer = br#"{"issuer":"https://evil.example/","authorization_endpoint":"https://auth.example.org/a","token_endpoint":"https://auth.example.org/t"}"#;
        assert!(matches!(
            check_discovery(&issuer(), other_issuer),
            Err(InstanceError::BadProvider(_))
        ));
        let plain = br#"{"issuer":"https://auth.example.org/app/","authorization_endpoint":"https://auth.example.org/a","token_endpoint":"https://auth.example.org/t","code_challenge_methods_supported":["plain"]}"#;
        assert!(matches!(
            check_discovery(&issuer(), plain),
            Err(InstanceError::BadProvider(_))
        ));
    }

    #[test]
    fn files_an_instance_under_a_name_that_stays_in_its_directory() {
        let mut instance = from_client_config(
            api(),
            br#"{"oidc_issuer":"https://auth.example.org/","oidc_client_id":"x"}"#,
            false,
        )
        .unwrap();
        assert_eq!(instance.id(), "notify.example.org");
        instance.api = "https://example.org:8443/weebo/../x".parse().unwrap();
        assert_eq!(instance.id(), "example.org_8443_x");
        assert!(!instance.id().contains('/'));
    }

    #[test]
    fn keeps_and_forgets_the_chosen_instance() {
        let dir = std::env::temp_dir().join(format!("weebo-instance-{}", std::process::id()));
        let path = dir.join("instance.toml");
        assert_eq!(load_at(&path), Ok(None));
        let instance = from_client_config(
            api(),
            br#"{"oidc_issuer":"https://auth.example.org/","oidc_client_id":"weebo-notify"}"#,
            false,
        )
        .unwrap();
        save_at(&path, &instance).unwrap();
        assert_eq!(load_at(&path), Ok(Some(instance)));
        std::fs::write(&path, "not = [toml").unwrap();
        assert!(matches!(load_at(&path), Err(InstanceError::Store(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// One canned HTTP answer on an ephemeral port, then the server is gone.
    fn serve_once(status: &str, body: String) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0_u8; 2048];
                let _read = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _written = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }

    /// An instance and its identity provider on one local port: the config, then the
    /// discovery document whose issuer is that same port.
    fn serve_instance() -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let issuer = format!("{base}/idp");
        let answers = [
            format!(r#"{{"oidc_issuer":"{issuer}","oidc_client_id":"weebo-notify"}}"#),
            format!(
                r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","code_challenge_methods_supported":["S256"]}}"#
            ),
        ];
        std::thread::spawn(move || {
            for body in answers {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut request = [0_u8; 2048];
                    let _read = stream.read(&mut request);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _written = stream.write_all(response.as_bytes());
                }
            }
        });
        base
    }

    #[tokio::test]
    async fn a_real_instance_is_probed_end_to_end() {
        let base = serve_instance();
        let instance = probe(&format!("{base}/"), true).await.unwrap();
        assert_eq!(instance.api.as_str(), format!("{base}/"));
        assert_eq!(instance.issuer.as_str(), format!("{base}/idp"));
        assert_eq!(instance.client_id, "weebo-notify");
        assert_eq!(
            instance.id(),
            base.trim_start_matches("http://").replace(':', "_")
        );
    }

    #[tokio::test]
    async fn a_page_that_is_not_an_instance_is_refused_without_following_it() {
        let base = serve_once(
            "302 Found\r\nlocation: https://evil.example/",
            String::new(),
        );
        assert!(matches!(
            probe(&base, true).await,
            Err(InstanceError::NotAnInstance(_))
        ));
    }

    #[tokio::test]
    async fn an_oversized_answer_is_cut_off() {
        let base = serve_once("200 OK", "x".repeat(MAX_BODY + 1));
        assert!(matches!(
            probe(&base, true).await,
            Err(InstanceError::NotAnInstance(_))
        ));
    }
}
