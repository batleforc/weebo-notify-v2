//! Where a client learns which server it is talking to, and as whom (design §9.1).

use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};
use serde::{Deserialize, Serialize};
use url::Url;

/// The environment prefix, shared with the server (§11).
const ENV_PREFIX: &str = "WNK_";

/// One server, as a profile names it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Where the API lives, without the `/api/v1` suffix the generated client appends.
    pub url: Url,

    /// The credential to present (OIDC access token or API key); usually from the environment
    /// or a mode-0600 file rather than a config file.
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            // The dev server, so `notify publish` works against `task dev:run` out of the box.
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

    /// A profile was asked for by name and the file does not describe it (a `--profile` typo
    /// must not publish to the default server).
    #[error("no profile named `{name}` in {path}")]
    UnknownProfile {
        /// The name asked for.
        name: String,
        /// Where it was looked for.
        path: PathBuf,
    },

    /// The file (or the directory it lives in) could not be written.
    #[error("{path} could not be written: {source}")]
    Unwritable {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

impl Profile {
    /// Resolve a profile: defaults, then the config file, then `WNK_*` (same layering as the server, §11).
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
                // An unknown named profile is a typo; only `default` may be absent.
                None if name != DEFAULT_PROFILE => {
                    return Err(ConfigError::UnknownProfile {
                        name: name.to_owned(),
                        path: path.to_path_buf(),
                    });
                }
                None => {}
            }
        }

        // Flat `WNK_URL`/`WNK_TOKEN`, so a pipeline needn't know about profiles.
        figment
            .merge(Env::prefixed(ENV_PREFIX).only(&["url", "token"]))
            .extract()
            .map_err(|error| ConfigError::Unreadable {
                path: path.to_path_buf(),
                source: Box::new(error),
            })
    }

    /// Persist a credential for a profile (`notify login`, §9.1), touching only that profile's
    /// `token` and carrying every other key and profile over.
    ///
    /// # Errors
    ///
    /// Fails if the existing file is not valid configuration, or if the file
    /// (or its directory, created if missing) cannot be written.
    pub fn save_token(name: &str, token: &str) -> Result<(), ConfigError> {
        Self::save_token_to(name, token, &config_path())
    }

    /// The same, against an explicit path — what the tests use.
    pub fn save_token_to(
        name: &str,
        token: &str,
        path: &std::path::Path,
    ) -> Result<(), ConfigError> {
        let mut file: File = if path.exists() {
            Figment::from(Toml::file(path))
                .extract()
                .map_err(|error| ConfigError::Unreadable {
                    path: path.to_path_buf(),
                    source: Box::new(error),
                })?
        } else {
            File::default()
        };

        file.profiles.entry(name.to_owned()).or_default().token = Some(token.to_owned());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| ConfigError::Unwritable {
                path: path.to_path_buf(),
                source: error,
            })?;
        }

        let rendered = toml::to_string_pretty(&file).map_err(|error| ConfigError::Unwritable {
            path: path.to_path_buf(),
            source: std::io::Error::other(error),
        })?;

        let unwritable = |error| ConfigError::Unwritable {
            path: path.to_path_buf(),
            source: error,
        };

        // A credential: created 0600 before the token is written (Unix only).
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut handle = options.open(path).map_err(unwritable)?;
        #[cfg(unix)]
        {
            // `mode` only applies on creation; narrow an existing file too.
            use std::os::unix::fs::PermissionsExt as _;
            handle
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(unwritable)?;
        }
        std::io::Write::write_all(&mut handle, rendered.as_bytes()).map_err(unwritable)?;

        Ok(())
    }
}

/// The profile everything falls back to.
pub const DEFAULT_PROFILE: &str = "default";

/// The config file, which is nothing but a table of profiles.
#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    profiles: std::collections::BTreeMap<String, Section>,
}

/// A profile as the file spells it; every key optional.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Section {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<Url>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

/// `<config dir>/weebo-notify/config.toml`.
fn config_path() -> PathBuf {
    app_dir(|dirs| dirs.config_dir()).join("config.toml")
}

/// `weebo-notify` under the platform's config or data directory (XDG on Linux), or `.` without a home.
pub(crate) fn app_dir(pick: impl FnOnce(&dyn etcetera::BaseStrategy) -> PathBuf) -> PathBuf {
    etcetera::base_strategy::choose_native_strategy()
        .map_or_else(|_| PathBuf::from("."), |dirs| pick(&dirs))
        .join("weebo-notify")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failing assertion is a failing test"
    )]

    use super::*;

    /// Write a config file at a per-test path (tests run in parallel).
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

    /// A fresh path in a per-test directory, for the first-login case.
    fn fresh_path(test: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("wnk-cfg-save-{}-{test}", std::process::id()))
            .join("config.toml")
    }

    #[test]
    fn saving_a_token_creates_the_file_and_its_directory_from_nothing() {
        let path = fresh_path("fresh");
        assert!(!path.exists());

        Profile::save_token_to("default", "wnk_abcd1234_secret", &path).unwrap();

        let loaded = Profile::load_from("default", &path).unwrap();
        assert_eq!(loaded.token.as_deref(), Some("wnk_abcd1234_secret"));
    }

    #[test]
    fn saving_a_token_leaves_the_profiles_url_and_every_other_profile_alone() {
        let path = with_config(
            "preserve",
            "[profiles.default]\n\
             url = \"http://localhost:8080\"\n\
             \n\
             [profiles.work]\n\
             url = \"https://notify.corp.example\"\n\
             token = \"old-work-token\"\n",
        );

        Profile::save_token_to("default", "new-default-token", &path).unwrap();

        let default = Profile::load_from("default", &path).unwrap();
        assert_eq!(default.url.as_str(), "http://localhost:8080/");
        assert_eq!(default.token.as_deref(), Some("new-default-token"));

        let work = Profile::load_from("work", &path).unwrap();
        assert_eq!(
            work.token.as_deref(),
            Some("old-work-token"),
            "a login to `default` must not touch `work`'s own token"
        );
    }

    #[test]
    fn saving_a_token_twice_replaces_it_rather_than_duplicating_the_profile() {
        let path = fresh_path("twice");

        Profile::save_token_to("default", "first", &path).unwrap();
        Profile::save_token_to("default", "second", &path).unwrap();

        assert_eq!(
            Profile::load_from("default", &path)
                .unwrap()
                .token
                .as_deref(),
            Some("second")
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_a_token_locks_the_file_down_to_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = fresh_path("perms");
        Profile::save_token_to("default", "wnk_abcd1234_secret", &path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a credential is not a config file to leave world-readable"
        );
    }
}
