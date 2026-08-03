//! The identities the mock serves, read from `dev/oidc-mock/identities.toml`.
//!
//! TOML rather than the provider-specific YAML the third-party mock wanted: the
//! rest of the project configures itself in TOML, and owning the format is the
//! point of owning the mock.

use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Format as _, Toml};
use serde::Deserialize;

/// Everything the mock knows: who it is, which client may talk to it, and who can
/// log in.
#[derive(Debug, Clone, Deserialize)]
pub struct Identities {
    /// What the mock calls itself in its discovery document.
    ///
    /// This is the string the server compares against its own `[oidc] issuer`, so
    /// the two have to agree byte for byte — hence the normalisation in
    /// [`Identities::with_issuer`].
    #[serde(default = "default_issuer")]
    pub issuer: String,

    /// The single registered client.
    pub client: Client,

    /// The accounts that can log in.
    #[serde(default)]
    pub users: Vec<User>,
}

/// The registered public client.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    /// `client_id`, and also the audience of every access token it is issued.
    pub id: String,

    /// Where the mock is allowed to send a browser back. Anything else is refused
    /// in the browser rather than redirected to.
    pub redirect_uris: Vec<String>,

    /// Where logout may send a browser back.
    #[serde(default)]
    pub post_logout_redirect_uris: Vec<String>,

    /// Advertised in discovery; a request for more is trimmed to this set.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,

    /// Access token lifetime, in seconds.
    #[serde(default = "default_access_ttl")]
    pub access_token_ttl_secs: i64,

    /// ID token lifetime, in seconds.
    #[serde(default = "default_id_ttl")]
    pub id_token_ttl_secs: i64,

    /// Refresh token lifetime, in seconds.
    #[serde(default = "default_refresh_ttl")]
    pub refresh_token_ttl_secs: i64,
}

/// One account.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// The stable identifier, which is what channel bindings key on.
    pub sub: String,
    /// What is typed at the login form.
    pub username: String,
    /// Committed on purpose: this account exists only on a laptop or a CI runner.
    pub password: String,
    /// Display name.
    pub name: String,
    /// Address, which is the other thing an individual binding can key on (§11).
    pub email: String,
    /// Whether the provider vouches for the address.
    #[serde(default = "default_true")]
    pub email_verified: bool,
    /// Group memberships. **Empty means no `groups` claim at all**, not an empty
    /// array — the T29 degradation path.
    #[serde(default)]
    pub groups: Vec<String>,
}

/// What went wrong before the mock could start.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file is missing, or is not valid identities.
    #[error("{path} is not a valid identities file: {source}")]
    Unreadable {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: Box<figment::Error>,
    },

    /// A provider with no accounts can only ever refuse logins.
    #[error("{path} describes no users — nobody could log in")]
    NoUsers {
        /// Which file.
        path: PathBuf,
    },
}

impl Identities {
    /// Read an identities file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let identities: Self =
            Figment::from(Toml::file(path))
                .extract()
                .map_err(|error| ConfigError::Unreadable {
                    path: path.to_path_buf(),
                    source: Box::new(error),
                })?;

        if identities.users.is_empty() {
            return Err(ConfigError::NoUsers {
                path: path.to_path_buf(),
            });
        }

        let issuer = identities.issuer.clone();
        Ok(identities.with_issuer(issuer))
    }

    /// Replace the issuer, normalised.
    ///
    /// The trailing slash is stripped because the server builds its discovery URL
    /// by concatenation — a rule that keeps path-carrying issuers like Authentik's
    /// working, and that turns one stray `/` into a mismatch nobody enjoys
    /// debugging.
    #[must_use]
    pub fn with_issuer(mut self, mut issuer: String) -> Self {
        issuer.truncate(issuer.trim_end_matches('/').len());
        self.issuer = issuer;
        self
    }

    /// Find the account behind a username and password.
    pub(crate) fn authenticate(&self, username: &str, password: &str) -> Option<&User> {
        self.users
            .iter()
            // Constant-time comparison would be theatre here: the passwords are in
            // a committed file that ships with the repository.
            .find(|user| user.username == username && user.password == password)
    }

    /// Find the account behind a subject.
    pub(crate) fn user(&self, sub: &str) -> Option<&User> {
        self.users.iter().find(|user| user.sub == sub)
    }
}

fn default_issuer() -> String {
    "http://localhost:9110".to_owned()
}

fn default_scopes() -> Vec<String> {
    ["openid", "profile", "email", "groups", "offline_access"]
        .iter()
        .map(|scope| (*scope).to_owned())
        .collect()
}

const fn default_access_ttl() -> i64 {
    900
}

const fn default_id_ttl() -> i64 {
    300
}

const fn default_refresh_ttl() -> i64 {
    28_800
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// The identities the development stack actually ships.
    fn committed() -> Identities {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/oidc-mock/identities.toml")
            .canonicalize()
            .expect("the committed identities file");

        Identities::load(&path).expect("the committed identities file parses")
    }

    #[test]
    fn the_committed_identities_describe_the_three_documented_accounts() {
        let identities = committed();

        let names: Vec<_> = identities
            .users
            .iter()
            .map(|user| user.username.as_str())
            .collect();

        assert_eq!(names, ["alice", "bob", "carol"]);
        assert_eq!(identities.client.id, "weebo-notify");
    }

    #[test]
    fn carol_has_no_groups_at_all() {
        let identities = committed();
        let carol = identities.user("carol").unwrap();

        assert!(
            carol.groups.is_empty(),
            "carol is the fixture for a provider that emits no groups (T29); \
             giving her one silently deletes that test"
        );
    }

    #[test]
    fn a_wrong_password_authenticates_nobody() {
        let identities = committed();

        assert!(identities.authenticate("alice", "weebo-dev").is_some());
        assert!(identities.authenticate("alice", "hunter2").is_none());
        assert!(identities.authenticate("mallory", "weebo-dev").is_none());
    }

    #[test]
    fn the_issuer_never_keeps_a_trailing_slash() {
        // The server concatenates `/.well-known/openid-configuration` onto the
        // issuer, so a trailing slash here becomes a double slash there.
        let identities = committed().with_issuer("https://route.example/".to_owned());

        assert_eq!(identities.issuer, "https://route.example");
    }

    #[test]
    fn a_file_that_is_not_there_is_an_error_and_not_an_empty_provider() {
        // Starting anyway would produce a provider that refuses every login, which
        // reads as "my password is wrong" rather than "my config path is wrong".
        let error = Identities::load(Path::new("/nonexistent/identities.toml")).unwrap_err();

        assert!(matches!(error, ConfigError::Unreadable { .. }));
    }
}
