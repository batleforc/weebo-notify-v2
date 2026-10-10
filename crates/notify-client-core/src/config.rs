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

    /// Neither `XDG_CONFIG_HOME` nor `HOME` is set, so there is nowhere to save.
    ///
    /// Refused rather than written to the current directory: see `config_path`.
    #[error(
        "neither XDG_CONFIG_HOME nor HOME is set, so there is nowhere to save the \
         credential; set one, or pass the token through WNK_TOKEN instead"
    )]
    NoConfigDir,

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
        // No config directory is the ephemeral case §9.3bis supports — defaults
        // and `WNK_*` only — so a path that cannot exist stands in for the file.
        match config_path() {
            Some(path) => Self::load_from(name, &path),
            None => Self::load_from(name, std::path::Path::new("")),
        }
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
        Self::save_token_to(name, token, &writable_config_path()?)
    }

    /// The same, against an explicit path — what the tests use.
    pub fn save_token_to(
        name: &str,
        token: &str,
        path: &std::path::Path,
    ) -> Result<(), ConfigError> {
        Self::write_token_to(name, Some(token), path)
    }

    /// Drop a profile's stored `token` from the file, leaving its `url` alone.
    ///
    /// What `notify login` calls once the credential has gone to the OS vault
    /// instead: a plaintext copy left behind would keep winning over the newer
    /// one there (the file is read first), and would keep sitting in a file
    /// people paste into issues. A missing file is already the wanted state.
    ///
    /// # Errors
    ///
    /// As [`Self::save_token`].
    pub fn clear_token(name: &str) -> Result<(), ConfigError> {
        // Nowhere to have stored one is the wanted state too.
        let Some(path) = config_path() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        Self::write_token_to(name, None, &path)
    }

    fn write_token_to(
        name: &str,
        token: Option<&str>,
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

        file.profiles.entry(name.to_owned()).or_default().token = token.map(str::to_owned);

        let rendered = toml::to_string_pretty(&file).map_err(|error| ConfigError::Unwritable {
            path: path.to_path_buf(),
            source: std::io::Error::other(error),
        })?;

        write_private(path, rendered.as_bytes()).map_err(|error| ConfigError::Unwritable {
            path: path.to_path_buf(),
            source: error,
        })
    }
}

/// Replace `path` with `contents`, readable by its owner only, atomically.
///
/// A credential, not a config file to leave world-readable — the same "mode-0600
/// file" [`Profile::token`]'s doc comment assumes, and the shape of the Tauri
/// shell's vault fallback too, which is why this is public.
///
/// Written to a sibling temporary file, flushed to disk, then renamed over the
/// target: an in-place truncate-and-write that dies half way (a full disk, a
/// killed process) leaves a truncated file holding every *other* profile's token
/// too, and the rename is the step that cannot half-happen. It also re-narrows an
/// older file for free — what ends up at `path` is always the new 0600 inode,
/// whatever mode the previous one had been given. The temporary file is 0600
/// *before* any byte reaches it, so there is no window where the secret sits in a
/// readable file. Unix only for the mode: there is no equivalent bit on the other
/// platforms this ships to, and leaving their default is the honest thing rather
/// than a false sense of parity.
///
/// # Errors
///
/// Fails if the directory (created if missing) or either file cannot be written.
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // `create_new` (`O_CREAT|O_EXCL`) under a name nobody can guess: the
    // directory may be shared or writable by someone else, and a predictable
    // `<path>.<pid>.tmp` opened with plain `create` would follow a symlink planted
    // there and truncate whatever it points at. `O_EXCL` refuses any existing
    // entry — a symlink included, dangling or not, so no `O_NOFOLLOW` is needed on
    // top — and the random suffix keeps a squatter from turning that refusal into
    // a denial of service. A collision just draws again.
    let mut attempts = 0;
    let (temporary, mut handle) = loop {
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(format!(".{:016x}.tmp", random_suffix()));
        let temporary = std::path::PathBuf::from(temporary);

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // 0600 at `open` itself, so there is no window where the secret sits in a
        // file anyone else can read.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&temporary) {
            Ok(handle) => break (temporary, handle),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(error),
        }
    };

    let written = (|| {
        handle.write_all(contents)?;
        handle.sync_all()?;
        std::fs::rename(&temporary, path)
    })();

    if written.is_err() {
        // Ours for certain — `create_new` made it — so removing it cannot hit
        // somebody else's file.
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// 64 unpredictable bits from std alone: `RandomState` is seeded from the OS
/// RNG per process and perturbed per instance. Not a CSPRNG contract, but the
/// safety here is `O_EXCL`'s; this only has to be unguessable enough that a
/// squatter cannot pre-create the name.
fn random_suffix() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    hasher.finish()
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

/// `<config dir>/weebo-notify/config.toml`. `None` without a home — never the current
/// directory, where a planted `config.toml` could send the token elsewhere.
fn config_path() -> Option<PathBuf> {
    app_dir(|dirs| dirs.config_dir()).map(|dir| dir.join("config.toml"))
}

/// [`config_path`], for the callers that have to write there.
fn writable_config_path() -> Result<PathBuf, ConfigError> {
    config_path().ok_or(ConfigError::NoConfigDir)
}

/// `weebo-notify` under the platform's config or data directory (XDG on Linux); `None` without a home.
pub(crate) fn app_dir(
    pick: impl FnOnce(&dyn etcetera::BaseStrategy) -> PathBuf,
) -> Option<PathBuf> {
    etcetera::base_strategy::choose_native_strategy()
        .ok()
        .map(|dirs| pick(&dirs).join("weebo-notify"))
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

    #[test]
    fn clearing_a_token_keeps_the_profiles_url() {
        let path = with_config(
            "clear",
            "[profiles.default]\nurl = \"https://notify.example\"\ntoken = \"plaintext\"\n",
        );

        Profile::write_token_to("default", None, &path).unwrap();

        let loaded = Profile::load_from("default", &path).unwrap();
        assert_eq!(loaded.url.as_str(), "https://notify.example/");
        assert!(loaded.token.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saving_a_token_re_narrows_a_file_somebody_left_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = with_config("renarrow", "[profiles.default]\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        Profile::save_token_to("default", "wnk_abcd1234_secret", &path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temporary file is renamed, not left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writing_never_follows_or_truncates_whatever_already_sits_beside_the_target() {
        // The old temporary name was `<path>.<pid>.tmp`, guessable by anyone who
        // can write to the directory: a symlink planted there sent the secret into
        // (and truncated) the file it pointed at. Plant one at that exact name, and
        // a plain leftover beside it, and check neither is touched.
        let path = fresh_path("symlink");
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "precious").unwrap();
        let planted = dir.join(format!("config.toml.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let leftover = dir.join("config.toml.0000000000000000.tmp");
        std::fs::write(&leftover, "leftover").unwrap();

        write_private(&path, b"secret").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        assert!(std::fs::symlink_metadata(&planted).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&leftover).unwrap(), "leftover");
    }
}
