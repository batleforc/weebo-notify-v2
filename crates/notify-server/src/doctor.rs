//! Deployment-owed guarantees checked against the loaded config (SEC-P2-20, SEC-P2-21): `WARN` lines at
//! boot, and `notify-server doctor` exits non-zero. What needs an outside probe is listed, not guessed.

use crate::config::{AttachmentsBackend, Config};

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    /// Works, but leaves a guarantee to something outside this process.
    Note,
    /// Very likely wrong, and the failure will not look like this when it arrives.
    Warn,
}

/// One thing worth an operator's attention.
#[derive(Debug, Clone)]
pub(crate) struct Finding {
    pub level: Level,
    /// What is wrong, in the deployment's terms rather than the code's.
    pub what: String,
    /// What to do about it.
    pub fix: String,
}

impl Finding {
    fn warn(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            what: what.into(),
            fix: fix.into(),
        }
    }

    fn note(what: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            level: Level::Note,
            what: what.into(),
            fix: fix.into(),
        }
    }
}

/// `[sinks] fingerprint_key` in `config/dev.toml` — public by being committed.
const DEV_FINGERPRINT_KEY: &str = "dev-only-sink-fingerprint-key-never-deploy-this";

/// The development stack's defaults this config still runs on.
fn development_defaults(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    // Dev-stack defaults must not boot in production; see `refuse_development_defaults`.
    if url::Url::parse(&config.oidc.issuer)
        .ok()
        .and_then(|issuer| issuer.host_str().map(is_loopback_host))
        .unwrap_or(false)
    {
        findings.push(Finding::warn(
            format!(
                "[oidc] issuer is {}, a loopback address: the development identity \
                 provider, or whatever else answers there",
                config.oidc.issuer
            ),
            "set [oidc] issuer (WNK_OIDC__ISSUER) to your identity provider",
        ));
    }
    if config.sinks.fingerprint_key.expose() == DEV_FINGERPRINT_KEY {
        findings.push(Finding::warn(
            "[sinks] fingerprint_key is the development key from config/dev.toml: anybody \
             can recompute webhook sink ids under it and brute-force the urls back",
            "set WNK_SINKS__FINGERPRINT_KEY from a Secret (server.sinks.fingerprintKey.existingSecret)",
        ));
    }
    if config.database.url.expose().contains("weebo:weebo@") {
        findings.push(Finding::warn(
            "[database] url carries the development stack's credentials",
            "set WNK_DATABASE__URL from a Secret (server.database.existingSecret)",
        ));
    }
    findings
}

/// Refuse to boot on the development defaults unless `[server] dev` says this is
/// a development instance.
///
/// # Errors
///
/// Each default still in place, with its fix.
pub(crate) fn refuse_development_defaults(config: &Config) -> anyhow::Result<()> {
    if config.server.dev {
        return Ok(());
    }
    let found = development_defaults(config);
    if found.is_empty() {
        return Ok(());
    }
    let reasons = found
        .iter()
        .map(|finding| format!("{} — {}", finding.what, finding.fix))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!(
        "refusing to start on the development defaults ({reasons}); set [server] dev = true \
         (WNK_SERVER__DEV) only for a development instance"
    )
}

/// Everything this process can tell about its own configuration.
pub(crate) fn review(config: &Config) -> Vec<Finding> {
    let mut findings = development_defaults(config);

    // A wildcard CORS origin is refused by `Config::validate`.

    // ── Attachments ──────────────────────────────────────────────────────────
    match &config.attachments {
        Some(attachments) if attachments.kind == AttachmentsBackend::Filesystem => {
            let secret = attachments.token_secret.expose().trim();
            if secret.is_empty() {
                // Refused at startup; listed so `doctor` explains why.
                findings.push(Finding::warn(
                    "[attachments] token_secret is empty: every presigned attachment URL would \
                     be unsigned, which is an open directory",
                    "set WNK_ATTACHMENTS__TOKEN_SECRET from a Secret (server.attachments.existingSecret)",
                ));
            } else if looks_like_a_placeholder(secret) {
                findings.push(Finding::warn(
                    "[attachments] token_secret looks like a placeholder, so attachment URLs are \
                     signed with a value anybody reading this chart can guess",
                    "generate one: `openssl rand -base64 48`",
                ));
            }
            if attachments.public_base_url.starts_with("http://") {
                findings.push(Finding::warn(
                    "[attachments] public_base_url is http://, so presigned attachment URLs — \
                     which carry their own credential in the query string — travel in clear",
                    "use https, or terminate TLS in front and set the https origin here",
                ));
            }
            findings.push(Finding::note(
                "the filesystem attachment backend is single-replica (ADR-0022): no HA, and \
                 every upgrade is a full outage",
                "use `kind = \"s3\"` for any deployment with an availability target",
            ));
        }
        Some(_) => {
            findings.push(Finding::note(
                "attachment confidentiality rests entirely on the bucket being private — this \
                 process cannot check that",
                "confirm an anonymous GET to an object URL is refused, and that a presigned URL \
                 is the only path in",
            ));
        }
        None => {}
    }

    // ── Documentation surface ────────────────────────────────────────────────
    if config.server.docs {
        findings.push(Finding::note(
            "/api/docs serves the API documentation with no credential",
            "fine for most deployments — it discloses the API's shape and never any data. Set \
             [server] docs = false if this instance should not publish it",
        ));
    }

    // ── Encryption at rest ───────────────────────────────────────────────────
    findings.push(Finding::note(
        "this application performs no encryption at rest (ADR-0007): notification content is \
         plaintext in Postgres and in every dump taken of it",
        "encrypted disks for Postgres and the object store, and encrypted backups — a pg_dump \
         leaves the encrypted disk the moment it is written",
    ));

    // ── What only a probe can answer ─────────────────────────────────────────
    findings.push(Finding::note(
        "not checkable from here: TLS in front of this process, whether the ingress publishes \
         /metrics, /healthz or /readyz, whether the bundle is served with frame-ancestors, \
         whether the ingress adds Strict-Transport-Security to the API and the bundle (this \
         process sends none — ADR-0023), and whether per-address rate limits are applied",
        "walk the checklist in docs/content/docs/ops/hardening.mdx — these are the items it \
         exists for",
    ));

    findings
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether a secret looks like something copied out of an example.
fn looks_like_a_placeholder(secret: &str) -> bool {
    const TELLS: &[&str] = &[
        "changeme",
        "change-me",
        "placeholder",
        "example",
        "secret",
        "password",
        "xxxx",
        "todo",
        "dev",
        "test",
    ];

    let lowered = secret.to_ascii_lowercase();
    // Short enough to guess is a tell on its own, whatever it says.
    secret.len() < 16 || TELLS.iter().any(|tell| lowered == *tell)
}

/// Log the review at boot.
pub(crate) fn log_review(config: &Config) {
    for finding in review(config) {
        match finding.level {
            Level::Warn => {
                tracing::warn!(fix = %finding.fix, "{}", finding.what);
            }
            Level::Note => {
                tracing::info!(fix = %finding.fix, "{}", finding.what);
            }
        }
    }
}

/// `notify-server doctor` — print the review, exit non-zero if anything warns.
pub(crate) fn report(config: &Config) -> bool {
    let findings = review(config);
    let warnings = findings.iter().filter(|f| f.level == Level::Warn).count();

    println!("weebo-notify — deployment review\n");
    for finding in &findings {
        let tag = match finding.level {
            Level::Warn => "WARN",
            Level::Note => "note",
        };
        println!("[{tag}] {}", finding.what);
        println!("       → {}\n", finding.fix);
    }

    if warnings == 0 {
        println!("No warnings. The notes above are guarantees this deployment owes; nothing");
        println!("here can confirm them from the inside.");
    } else {
        println!("{warnings} warning(s).");
    }

    warnings == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults, with the two that are the development stack replaced.
    fn config() -> Config {
        let mut config = Config::default();
        config.oidc.issuer = "https://id.weebo.example".to_owned();
        config.database.url = "postgres://notify@db.internal/weebo_notify".into();
        config
    }

    #[test]
    fn the_development_defaults_are_warned_about() {
        let warnings: Vec<_> = review(&Config::default())
            .into_iter()
            .filter(|f| f.level == Level::Warn)
            .map(|f| f.what)
            .collect();

        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("[oidc] issuer"));
        assert!(warnings[1].contains("[database] url"));
    }

    #[test]
    fn the_development_defaults_refuse_to_boot_unless_declared() {
        let mut config = Config::default();
        assert!(refuse_development_defaults(&config).is_err());

        config.server.dev = true;
        assert!(refuse_development_defaults(&config).is_ok());
    }

    #[test]
    fn the_development_fingerprint_key_refuses_to_boot_outside_dev() {
        // The value checked is the one the dev file actually ships, so the two
        // cannot drift apart and quietly disarm the check.
        assert!(include_str!("../../../config/dev.toml").contains(DEV_FINGERPRINT_KEY));

        let mut config = config();
        config.sinks.fingerprint_key = DEV_FINGERPRINT_KEY.into();
        let refused = refuse_development_defaults(&config)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("[sinks] fingerprint_key"), "{refused}");

        config.server.dev = true;
        assert!(refuse_development_defaults(&config).is_ok());
    }

    #[test]
    fn a_default_configuration_warns_about_nothing() {
        // Notes are expected; a warning on defaults would mean the defaults are wrong.
        let findings = review(&config());
        let warnings: Vec<_> = findings
            .iter()
            .filter(|f| f.level == Level::Warn)
            .map(|f| f.what.clone())
            .collect();

        assert!(warnings.is_empty(), "default config warns: {warnings:?}");
        assert!(
            !findings.is_empty(),
            "a review that says nothing at all has stopped being useful"
        );
    }

    #[test]
    fn encryption_at_rest_is_always_mentioned() {
        // Nothing to configure, so it must be said every boot.
        assert!(
            review(&config())
                .iter()
                .any(|f| f.what.contains("no encryption at rest"))
        );
    }

    #[test]
    fn a_short_or_obvious_token_secret_is_a_placeholder() {
        for tell in ["", "changeme", "dev", "secret", "short"] {
            assert!(
                looks_like_a_placeholder(tell),
                "`{tell}` should not pass as a signing secret"
            );
        }
        assert!(!looks_like_a_placeholder(
            "0G0m0hQ7pQm8kZ0h3sVJ2tYqLxN4rW6uB1cD8eF0aI2="
        ));
    }
}
