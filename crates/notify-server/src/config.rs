//! Configuration: defaults < TOML file < `WNK_*` environment (design §11).
//! Every key is overridable as `WNK_` + path joined by `__` (e.g. `WNK_OIDC__ISSUER`).
//! The types and their structural checks are `notify_config`'s (ADR-0031); this
//! module layers them and checks the secret values the layering brought in.

use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};

pub(crate) use notify_config::*;

/// The environment prefix, shared with the clients (design §9.1) so one habit
/// covers both sides.
const ENV_PREFIX: &str = "WNK_";

/// Read the layered configuration; a missing file is fine (env-only deployments are supported).
pub(crate) fn load(path: &Path) -> anyhow::Result<Config> {
    load_digested(path).map(|(config, _)| config)
}

/// [`load`], and the SHA-256 of the file's bytes exactly as they were parsed (`None`
/// with no file), which `/healthz` reports so a renderer can match its own (ADR-0032).
pub(crate) fn load_digested(path: &Path) -> anyhow::Result<(Config, Option<[u8; 32]>)> {
    use sha2::{Digest as _, Sha256};

    let mut figment = Figment::from(Serialized::defaults(Config::default()));
    let mut digest = None;

    if path.exists() {
        // Read once, so the digest is of the bytes parsed and not of a later write.
        let bytes = std::fs::read(path)
            .map_err(|error| anyhow::anyhow!("{} cannot be read: {error}", path.display()))?;
        digest = Some(Sha256::digest(&bytes).into());
        let text = String::from_utf8(bytes)
            .map_err(|error| anyhow::anyhow!("{} is not UTF-8: {error}", path.display()))?;
        figment = figment.merge(Toml::string(&text));
    } else {
        tracing::warn!(path = %path.display(), "no configuration file; using defaults and WNK_*");
    }

    // `split("__")` maps `WNK_OIDC__ISSUER` to `[oidc] issuer`, for every key (§11).
    let config: Config = figment
        .merge(Env::prefixed(ENV_PREFIX).split("__"))
        .extract()
        // `Toml::string` does not name the file the way `Toml::file` did.
        .map_err(|error| match digest {
            Some(_) => anyhow::anyhow!("{}: {error}", path.display()),
            None => error.into(),
        })?;

    config.validate()?;
    validate_secrets(&config)?;
    Ok((config, digest))
}

/// The checks that need a secret's value, so cannot run where the file is
/// rendered without them (ADR-0031): only here, once `WNK_*` is merged.
fn validate_secrets(config: &Config) -> anyhow::Result<()> {
    if let Some(attachments) = &config.attachments
        && attachments.kind == AttachmentsBackend::Filesystem
    {
        // Same floor as the fingerprint keys: shorter makes the URLs forgeable.
        let min = notify_ports::FingerprintKey::MIN_BYTES;
        anyhow::ensure!(
            attachments.token_secret.expose().len() >= min,
            "[attachments] kind = \"filesystem\" requires a `token_secret` of at \
             least {min} bytes, as it signs presigned URLs (`openssl rand -base64 48`)"
        );
    }
    validate_fingerprint_keys(config)
}

/// `[sinks] fingerprint_key` and its predecessors.
///
/// Checked whenever they are set, and the current one required once a webhook
/// needs it: a previous key is only ever read to re-key, so a typo in one would
/// orphan exactly the rows it was kept for.
fn validate_fingerprint_keys(config: &Config) -> anyhow::Result<()> {
    config.sinks.previous_fingerprint_keys()?;
    if config
        .channels
        .managed
        .iter()
        .flat_map(|channel| &channel.sinks)
        .any(|sink| sink.kind == "webhook")
    {
        config.sinks.fingerprint_key().map_err(|error| {
            error.context("a webhook sink is configured, so [sinks] fingerprint_key is required")
        })?;
    }
    if config
        .channels
        .managed
        .iter()
        .any(|channel| !channel.feeds.is_empty())
    {
        config.sinks.fingerprint_key().map_err(|error| {
            error.context(
                "a feed is declared, so [sinks] fingerprint_key is required: it \
                 redacts feed urls in the journal and the logs",
            )
        })?;
    }
    Ok(())
}

/// What `[reload] watch` compares the file against: the digest of the content the
/// running configuration was last read from (ADR-0031).
///
/// The path is read anew on every poll, so a ConfigMap's `..data` symlink swap is a
/// change like any other. A file that fails to load still moves the digest, so a
/// broken file is reported once rather than on every poll.
#[derive(Debug, Default)]
pub(crate) struct FileWatch {
    digest: Option<[u8; 32]>,
    unreadable: bool,
}

impl FileWatch {
    /// The file as read now: whether it differs from the last read, so warrants a
    /// reload. An unreadable file is never a reload, and is logged once per outage.
    pub(crate) fn observe(&mut self, read: std::io::Result<Vec<u8>>) -> bool {
        use sha2::{Digest as _, Sha256};

        match read {
            Ok(contents) => {
                if std::mem::take(&mut self.unreadable) {
                    tracing::info!("the configuration file is readable again");
                }
                let digest: [u8; 32] = Sha256::digest(&contents).into();
                self.digest.replace(digest) != Some(digest)
            }
            Err(error) => {
                if !std::mem::replace(&mut self.unreadable, true) {
                    tracing::warn!(
                        ?error,
                        "the configuration file cannot be read; keeping the running configuration"
                    );
                }
                false
            }
        }
    }
}

/// What the persistence adapter is handed.
pub(crate) const fn pool(database: &Database) -> notify_persistence_pg::PoolSettings {
    notify_persistence_pg::PoolSettings {
        max_connections: database.max_connections,
        acquire_timeout: database.acquire_timeout.as_duration(),
        idle_timeout: database.idle_timeout.as_duration(),
        max_lifetime: database.max_lifetime.as_duration(),
        statement_timeout: database.statement_timeout.as_duration(),
        idle_in_transaction_timeout: database.idle_in_transaction_timeout.as_duration(),
    }
}

/// Resolve an `env:NAME` / `file:/path` reference (design §11); inline secrets are refused.
pub(crate) fn resolve_secret(what: &str, reference: &str) -> anyhow::Result<String> {
    if let Some(name) = reference.strip_prefix("env:") {
        return std::env::var(name).map_err(|_| {
            anyhow::anyhow!("{what} names environment variable `{name}`, which is not set")
        });
    }

    if let Some(path) = reference.strip_prefix("file:") {
        let contents = std::fs::read_to_string(path).map_err(|error| {
            anyhow::anyhow!("{what} names `{path}`, which cannot be read: {error}")
        })?;
        // Trimmed: `echo` into a file adds a newline that would poison the HMAC.
        return Ok(contents.trim().to_owned());
    }

    anyhow::bail!("{what} must be `env:NAME` or `file:/path`, not `{reference}`")
}

/// [`load`] under figment's global `Jail` lock, so the T23 test's `WNK_*`
/// variables cannot leak into a load running concurrently on another thread.
#[cfg(test)]
pub(crate) fn load_isolated(path: &Path) -> anyhow::Result<Config> {
    let mut loaded = None;
    #[expect(
        clippy::result_large_err,
        reason = "the signature belongs to figment::Jail"
    )]
    figment::Jail::expect_with(|_| {
        loaded = Some(load(path));
        Ok(())
    });
    loaded.unwrap_or_else(|| anyhow::bail!("the jail did not run"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a config file somewhere only this test will look.
    fn with_config(test: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wnk-server-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Every scalar leaf of a serialised config, as `(path, value)` (T23); sequences are GitOps material.
    fn scalar_leaves(
        value: &serde_json::Value,
        prefix: &mut Vec<String>,
        found: &mut Vec<(Vec<String>, serde_json::Value)>,
    ) {
        match value {
            serde_json::Value::Object(fields) => {
                for (name, child) in fields {
                    prefix.push(name.clone());
                    scalar_leaves(child, prefix, found);
                    prefix.pop();
                }
            }
            // `null` is an absent optional section.
            serde_json::Value::Null | serde_json::Value::Array(_) => {}
            scalar => found.push((prefix.clone(), scalar.clone())),
        }
    }

    #[test]
    fn critical_t23_every_configuration_key_can_be_set_from_the_environment() {
        // T23 (§3.4): enumerate every key, from `config/dev.toml` since defaults leave most sections empty.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let mut leaves = Vec::new();
        scalar_leaves(
            &serde_json::to_value(load_isolated(&base).unwrap()).unwrap(),
            &mut Vec::new(),
            &mut leaves,
        );

        // A floor, so the test fails when the walk stops walking.
        assert!(
            leaves.len() >= 20,
            "the walk found only {} keys, which means the walk is broken rather than \
             the configuration being small: {:?}",
            leaves.len(),
            leaves
                .iter()
                .map(|(path, _)| path.join("."))
                .collect::<Vec<_>>()
        );

        // Count keys taken, or all-refused would pass.
        let mut taken = 0_usize;
        let total = leaves.len();

        for (path, value) in leaves {
            let variable = format!("{ENV_PREFIX}{}", path.join("__").to_uppercase());
            let sentinel = match &value {
                serde_json::Value::Bool(flag) => (!flag).to_string(),
                serde_json::Value::Number(number) => number
                    .as_i64()
                    .map_or_else(|| "1".to_owned(), |n| (n + 1).to_string()),
                _ => "weebo-t23".to_owned(),
            };

            #[expect(
                clippy::result_large_err,
                reason = "the signature belongs to figment::Jail"
            )]
            figment::Jail::expect_with(|jail| {
                jail.set_env(&variable, &sentinel);

                // Taken, or refused by name: both prove the variable arrived; silence is the failure.
                match load(&base) {
                    Ok(config) => {
                        let loaded = serde_json::to_value(config).unwrap();
                        let mut at = &loaded;
                        for segment in &path {
                            at = &at[segment];
                        }
                        let rendered = match at {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        assert_eq!(
                            rendered,
                            sentinel,
                            "{variable} loaded without changing {}",
                            path.join(".")
                        );
                        taken += 1;
                    }
                    Err(error) => {
                        let message = error.to_string().to_lowercase();
                        let key = path.last().unwrap();
                        assert!(
                            message.contains(key) || message.contains(&sentinel),
                            "{variable} was refused without naming {key}: {error}"
                        );
                    }
                }

                Ok(())
            });
        }

        // Ensure some keys are actually taken, not only refused by name.
        assert!(
            taken * 2 > total,
            "only {taken} of {total} keys were actually taken from the environment; \
             the rest merely produced an error naming themselves"
        );
    }

    #[test]
    fn traces_can_be_switched_on_by_environment_alone() {
        // A whole optional section from the environment: figment must build the `Some`.
        #[expect(
            clippy::result_large_err,
            reason = "the signature belongs to figment::Jail"
        )]
        figment::Jail::expect_with(|jail| {
            jail.set_env("WNK_TELEMETRY__OTLP_ENDPOINT", "http://collector:4318");

            let config = load(Path::new("/nonexistent/config.toml")).unwrap();

            let telemetry = config
                .telemetry
                .expect("naming a collector is enough to turn traces on");
            assert_eq!(telemetry.otlp_endpoint, "http://collector:4318");
            assert_eq!(
                telemetry.service_name, "weebo-notify",
                "the rest of the section still comes from its defaults"
            );
            assert!((telemetry.sample_ratio - 1.0).abs() < f64::EPSILON);

            Ok(())
        });
    }

    #[test]
    fn a_short_filesystem_token_secret_is_refused() {
        let path = with_config(
            "fs-short-secret",
            "[attachments]\n\
             kind = \"filesystem\"\n\
             path = \"/data/attachments\"\n\
             public_base_url = \"https://notify.weebo.poc\"\n\
             token_secret = \"changeme\"\n",
        );

        assert!(
            load_isolated(&path).is_err(),
            "a guessable key signs forgeable URLs"
        );
    }

    #[test]
    fn a_webhook_without_a_fingerprint_key_of_full_length_is_refused_at_startup() {
        for (name, sinks) in [
            ("unkeyed-webhook", ""),
            (
                "short-keyed-webhook",
                "[sinks]\nfingerprint_key = \"too-short\"\n",
            ),
        ] {
            let path = with_config(
                name,
                &format!(
                    "{sinks}[[channels.managed]]\n\
                     id = \"ci-infra\"\n\
                     [[channels.managed.sinks]]\n\
                     type = \"webhook\"\n\
                     url = \"https://hooks.weebo.poc/incidents\"\n\
                     secret_ref = \"env:WNK_SINK_HMAC\"\n"
                ),
            );

            // No fallback to an unkeyed (brute-forceable) id; the message names the key and its env var.
            let error = format!("{:#}", load_isolated(&path).unwrap_err());
            assert!(error.contains("[sinks] fingerprint_key"), "{error}");
            assert!(error.contains("WNK_SINKS__FINGERPRINT_KEY"), "{error}");
        }
    }

    const FEED: &str = "url = \"https://blog.example.org/feed.xml\"\npublisher = \"bot-feeds\"";

    #[test]
    fn a_feed_without_a_fingerprint_key_is_refused() {
        let path = with_config(
            "feed-unkeyed",
            &format!(
                "[[service_accounts]]\n\
                 id = \"bot-feeds\"\n\
                 [[channels.managed]]\n\
                 id = \"veille\"\n\
                 [[channels.managed.access]]\n\
                 service_account = \"bot-feeds\"\n\
                 scopes = [\"publish\"]\n\
                 [[channels.managed.feeds]]\n\
                 {FEED}\n"
            ),
        );

        let error = format!("{:#}", load_isolated(&path).unwrap_err());
        assert!(error.contains("a feed is declared"), "{error}");
    }

    #[test]
    fn the_database_defaults_migrate_at_boot_and_bound_every_wait() {
        let database = load_isolated(Path::new("/nonexistent/config.toml"))
            .unwrap()
            .database;

        assert!(database.migrate_on_start);
        let pool = pool(&database);
        assert_eq!(pool.acquire_timeout.as_secs(), 5);
        assert_eq!(pool.statement_timeout.as_secs(), 30);
        assert_eq!(pool.idle_in_transaction_timeout.as_secs(), 60);
    }

    #[test]
    fn migrations_can_be_left_to_the_subcommand_from_the_environment() {
        #[expect(
            clippy::result_large_err,
            reason = "the signature belongs to figment::Jail"
        )]
        figment::Jail::expect_with(|jail| {
            jail.set_env("WNK_DATABASE__MIGRATE_ON_START", "false");
            let config = load(Path::new("/nonexistent/config.toml")).unwrap();
            assert!(!config.database.migrate_on_start);
            Ok(())
        });
    }

    #[test]
    fn the_config_defaults_are_the_application_defaults() {
        // notify-config cannot depend on notify-app, so it copies these two.
        let config = Config::default();
        assert_eq!(
            config.devices.max_per_user,
            notify_app::DEFAULT_MAX_DEVICES_PER_USER
        );
        assert_eq!(
            config.auth.group_snapshot_ttl.as_secs(),
            notify_app::DEFAULT_GROUP_SNAPSHOT_TTL
                .unsigned_abs()
                .as_secs()
        );
    }

    #[test]
    fn a_misspelt_transforms_table_survives_the_figment_layering() {
        // `transform` is a `toml::Value` now; figment must still fill it.
        let config = load_isolated(&with_config(
            "transform-typo",
            "[[transform]]\nid = \"news\"\n",
        ))
        .unwrap();
        assert!(
            config
                .unhonoured()
                .iter()
                .any(|pending| pending.contains("[[transform]]"))
        );
    }

    #[test]
    fn a_filesystem_store_without_its_token_secret_is_refused_at_boot() {
        let path = with_config(
            "fs-no-secret",
            "[attachments]\n\
             kind = \"filesystem\"\n\
             path = \"/data/attachments\"\n\
             public_base_url = \"https://notify.weebo.poc\"\n",
        );

        let error = load_isolated(&path).unwrap_err().to_string();
        assert!(error.contains("token_secret"), "{error}");
    }

    #[test]
    fn the_watch_reloads_once_per_change_and_never_on_an_unreadable_file() {
        let gone = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let mut watch = FileWatch::default();

        assert!(
            watch.observe(Ok(b"a".to_vec())),
            "the first read is the boot file"
        );
        assert!(!watch.observe(Ok(b"a".to_vec())), "same content, no reload");
        assert!(
            watch.observe(Ok(b"b".to_vec())),
            "changed content, one reload"
        );
        assert!(!watch.observe(Ok(b"b".to_vec())));
        assert!(!watch.observe(gone()), "unreadable: keep the running one");
        assert!(!watch.observe(gone()));
        assert!(
            !watch.observe(Ok(b"b".to_vec())),
            "back as it was: nothing to reload"
        );
        assert!(!watch.observe(gone()));
        assert!(watch.observe(Ok(b"c".to_vec())), "back changed: reload");
    }

    #[test]
    fn the_digest_is_of_the_bytes_parsed_and_absent_without_a_file() {
        use sha2::{Digest as _, Sha256};

        let contents = "[reconcile]\nmax_removals = 7\n";
        let path = with_config("digest", contents);
        let mut loaded = None;
        #[expect(
            clippy::result_large_err,
            reason = "the signature belongs to figment::Jail"
        )]
        figment::Jail::expect_with(|_| {
            loaded = Some((
                load_digested(&path).unwrap(),
                load_digested(Path::new("/nonexistent/config.toml")).unwrap(),
            ));
            Ok(())
        });
        let ((config, digest), (_, none)) = loaded.unwrap();

        assert_eq!(config.reconcile.max_removals, 7);
        // What `sha256sum config.toml` prints, so the operator's hash matches.
        assert_eq!(digest, Some(Sha256::digest(contents).into()));
        assert_eq!(none, None);
    }

    #[test]
    fn a_parse_error_still_names_the_file() {
        let path = with_config("named", "[reconcile]\nmax_removals = \"many\"\n");
        let error = load_isolated(&path).unwrap_err().to_string();
        assert!(error.contains("config.toml"), "{error}");
        assert!(error.contains("max_removals"), "{error}");
    }
}
