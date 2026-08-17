//! The deployment-owed guarantees, checked against the configuration that is
//! actually loaded (advisory SEC-P2-20, SEC-P2-21).
//!
//! `hardening.mdx` lists what weebo-notify deliberately does not do for you:
//! encryption at rest, a private bucket, `frame-ancestors`, HSTS, per-address rate
//! limiting, keeping `/metrics` off the ingress. That list is honest and it has one
//! failure mode — **every item on it fails silently**. Nothing logs "your bucket is
//! world-readable"; the product runs perfectly, right up until it is somebody's
//! incident.
//!
//! This module is the part of that list a process can check *about itself*, from
//! configuration alone, and it exists in two shapes:
//!
//! * at **boot**, [`review`] runs and each finding becomes a `WARN` line, so an
//!   operator who never reads the documentation still meets it once;
//! * on **demand**, `notify-server doctor` prints the same review and exits
//!   non-zero if anything is wrong, which is something to paste into an issue.
//!
//! # What it deliberately cannot see
//!
//! Everything requiring a network or a cluster: whether TLS terminates in front of
//! it, whether the bucket answers an anonymous `GET`, whether the ingress publishes
//! `/metrics`, whether `frame-ancestors` is sent. Those need a probe from outside,
//! and a check that quietly answers "fine" because it could not look is worse than
//! no check. [`review`] therefore ends by naming them, so the output is a complete
//! list of what to verify rather than a complete list of what was verified.

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

/// Everything this process can tell about its own configuration.
pub(crate) fn review(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    // ── CORS ─────────────────────────────────────────────────────────────────
    if config
        .server
        .cors_origins
        .iter()
        .any(|origin| origin.trim() == "*")
    {
        findings.push(Finding::warn(
            "[server] cors_origins contains `*`, so any website can call this API from a \
             visitor's browser",
            "name the web bundle's origin explicitly; there is no cookie here, but a wildcard \
             still lets any page read responses a visitor's token authorises",
        ));
    }

    // ── Attachments ──────────────────────────────────────────────────────────
    match &config.attachments {
        Some(attachments) if attachments.kind == AttachmentsBackend::Filesystem => {
            let secret = attachments.token_secret.trim();
            if secret.is_empty() {
                // Refused at startup already; kept here so `doctor` explains it
                // rather than the process merely refusing to boot.
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
         /metrics, /healthz or /readyz, whether the bundle is served with frame-ancestors and \
         HSTS, and whether per-address rate limits are applied",
        "walk the checklist in docs/content/docs/ops/hardening.mdx — these are the items it \
         exists for",
    ));

    findings
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

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn a_default_configuration_warns_about_nothing() {
        // Notes are expected — they are the guarantees a deployment owes. A *warning*
        // on a default config would mean the defaults are wrong.
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
    fn a_wildcard_cors_origin_is_a_warning() {
        let mut config = config();
        config.server.cors_origins = vec!["*".to_owned()];

        assert!(
            review(&config)
                .iter()
                .any(|f| f.level == Level::Warn && f.what.contains("cors_origins")),
            "a wildcard origin passed the review"
        );
    }

    #[test]
    fn encryption_at_rest_is_always_mentioned() {
        // The one nobody discovers until a backup leaks. It has no configuration to
        // get wrong, which is exactly why it has to be said out loud every boot.
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
