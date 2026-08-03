//! Where a client learns which server it is talking to, and as whom (design §9.1).

use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};
use serde::{Deserialize, Serialize};
use url::Url;

/// The environment prefix, shared with the server (design §11) so one habit
/// covers both sides.
const ENV_PREFIX: &str = "WNK_";

/// One server, as a profile names it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Where the API lives, without the `/api/v1` suffix — the generated client
    /// appends it.
    pub url: Url,

    /// The credential to present: an OIDC access token or an API key.
    ///
    /// Optional here because the usual place for it is the environment or a
    /// mode-0600 file, not a config file people paste into issues. An ephemeral
    /// caller — a debug pod, a CI step with the key in its env — works with no
    /// persistence at all (design §9.3bis).
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            // The dev server, so `notify publish` works out of the box against
            // `task dev:run` with no configuration.
            #[allow(
                clippy::expect_used,
                reason = "a literal that fails to parse is a compile-time mistake, \
                          caught by this crate's own tests"
            )]
            url: Url::parse("http://localhost:8080").expect("a valid literal URL"),
            token: None,
        }
    }
}

/// What went wrong before any request was made.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists and is not readable as configuration.
    #[error("{path} is not valid configuration: {source}")]
    Unreadable {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: Box<figment::Error>,
    },

    /// A profile was asked for by name and the file does not describe it.
    ///
    /// Refused rather than silently falling back to the default: publishing to the
    /// wrong server because of a typo in `--profile` is the failure this prevents.
    #[error("no profile named `{name}` in {path}")]
    UnknownProfile {
        /// The name asked for.
        name: String,
        /// Where it was looked for.
        path: PathBuf,
    },
}

impl Profile {
    /// Resolve a profile: defaults, then the config file, then `WNK_*`.
    ///
    /// Same layering as the server (design §11), same order, and every key
    /// overridable from the environment — which is what makes the CLI usable from
    /// a CI step that has no home directory to write into.
    pub fn load(name: &str) -> Result<Self, ConfigError> {
        let path = config_path();
        Self::load_from(name, &path)
    }

    /// The same, against an explicit path — what the tests use.
    pub fn load_from(name: &str, path: &std::path::Path) -> Result<Self, ConfigError> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        if path.exists() {
            let file: File = Figment::from(Toml::file(path)).extract().map_err(|error| {
                ConfigError::Unreadable {
                    path: path.to_path_buf(),
                    source: Box::new(error),
                }
            })?;

            match file.profiles.get(name) {
                Some(section) => figment = figment.merge(Serialized::defaults(section)),
                // A named profile the file does not mention is a typo, and a typo
                // that silently publishes somewhere else is worse than an error.
                // `default` is exempt: not having written a config file yet is not
                // a mistake.
                None if name != DEFAULT_PROFILE => {
                    return Err(ConfigError::UnknownProfile {
                        name: name.to_owned(),
                        path: path.to_path_buf(),
                    });
                }
                None => {}
            }
        }

        // `WNK_URL` and `WNK_TOKEN`: flat, because a caller exporting one variable
        // in a pipeline should not have to know about profiles.
        figment
            .merge(Env::prefixed(ENV_PREFIX).only(&["url", "token"]))
            .extract()
            .map_err(|error| ConfigError::Unreadable {
                path: path.to_path_buf(),
                source: Box::new(error),
            })
    }
}

/// The profile everything falls back to.
pub const DEFAULT_PROFILE: &str = "default";

/// The config file, which is nothing but a table of profiles.
#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    profiles: std::collections::BTreeMap<String, Section>,
}

/// A profile as the file spells it: every key optional, so a section can override
/// the URL without restating the token or the other way round.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Section {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<Url>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

/// `$XDG_CONFIG_HOME/weebo-notify/config.toml`, or the usual fallback.
fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));

    base.join("weebo-notify").join("config.toml")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// Write a config file somewhere only this test will look.
    ///
    /// Named per test rather than per process: the suite runs in parallel, and two
    /// tests sharing one path race on it.
    fn with_config(test: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-cfg-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn with_no_file_at_all_the_defaults_point_at_the_dev_server() {
        let profile = Profile::load_from("default", std::path::Path::new("/nonexistent")).unwrap();

        assert_eq!(profile.url.as_str(), "http://localhost:8080/");
        assert!(profile.token.is_none());
    }

    #[test]
    fn a_named_profile_selects_its_own_server() {
        let path = with_config(
            "named",
            "[profiles.default]\n\
             url = \"http://localhost:8080\"\n\
             \n\
             [profiles.work]\n\
             url = \"https://notify.corp.example\"\n\
             token = \"wnk_abcd1234_secret\"\n",
        );

        let work = Profile::load_from("work", &path).unwrap();

        assert_eq!(work.url.as_str(), "https://notify.corp.example/");
        assert_eq!(work.token.as_deref(), Some("wnk_abcd1234_secret"));
    }

    #[test]
    fn a_profile_the_file_does_not_describe_is_a_typo_and_not_a_fallback() {
        let path = with_config(
            "typo",
            "[profiles.default]\nurl = \"http://localhost:8080\"\n",
        );

        let error = Profile::load_from("wrok", &path).unwrap_err();

        assert!(
            matches!(error, ConfigError::UnknownProfile { .. }),
            "publishing to the wrong server over a typo is the failure this prevents"
        );
    }
}
