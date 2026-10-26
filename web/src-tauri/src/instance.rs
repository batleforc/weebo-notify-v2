//! The instance this shell talks to, chosen at first launch (RFC 0001).
//!
//! The probe and every check live in `notify_client_core::instance`; this module
//! exposes them to the webview as commands and owns the two things only the shell
//! can do: ask the person through a native dialog the webview cannot draw or click
//! (decision 5), and write the CSP for the chosen instance into every page it serves
//! (`csp_for`, wired as the main window's `on_web_resource_request`).

// `#[tauri::command]` emits `unreachable!` beside an async command that borrows
// `State`, in an item of its own that an attribute on the function does not reach.
#![expect(
    clippy::unreachable,
    reason = "emitted by `#[tauri::command]` for the two async commands below"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use notify_client_core::instance::{self, Instance};
use serde_json::{Value, json};
use tauri::State;
use tauri_plugin_dialog::{DialogExt as _, MessageDialogButtons, MessageDialogKind};

/// What the shell knows about instances: the confirmed one, and what was probed but
/// not yet confirmed, by an id the webview only ever hands back.
#[derive(Debug, Default)]
pub(crate) struct Instances {
    current: Mutex<Option<Instance>>,
    probed: Mutex<HashMap<String, Instance>>,
    next: AtomicU64,
}

impl Instances {
    /// The stored instance, read once at start; a file that cannot be read means
    /// "none", so the person is asked again rather than sent somewhere guessed.
    pub(crate) fn load() -> Arc<Self> {
        let current = instance::load().unwrap_or_else(|error| {
            eprintln!("weebo-notify: the chosen server could not be read ({error}); asking again");
            None
        });
        Arc::new(Self {
            current: Mutex::new(current),
            ..Self::default()
        })
    }

    pub(crate) fn current(&self) -> Option<Instance> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Plain `http` is for a loopback dev stack, and only in a debug build (§4.3).
const ALLOW_LOOPBACK_HTTP: bool = cfg!(debug_assertions);

/// Probe what the person typed; nothing is trusted or stored yet.
///
/// Answers the probe id to confirm with, and the two hosts the confirmation names.
#[tauri::command]
pub(crate) async fn instance_probe(
    instances: State<'_, Arc<Instances>>,
    url: String,
) -> Result<Value, String> {
    let probed = instance::probe(&url, ALLOW_LOOPBACK_HTTP)
        .await
        .map_err(|error| error.to_string())?;
    let id = instances.next.fetch_add(1, Ordering::Relaxed).to_string();
    let answer = json!({
        "probe": id,
        "api": probed.api.as_str(),
        "issuerHost": probed.issuer.host_str().unwrap_or_default(),
    });
    instances
        .probed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, probed);
    Ok(answer)
}

/// Ask the person, natively, whether to use the instance behind `probe`; store it if so.
///
/// `false` when declined. The webview can only name a probe the shell made, and
/// cannot answer the dialog: script there can call commands, not click an OS window.
#[tauri::command]
pub(crate) async fn instance_save(
    app: tauri::AppHandle,
    instances: State<'_, Arc<Instances>>,
    probe: String,
) -> Result<bool, String> {
    let candidate = instances
        .probed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&probe)
        .ok_or_else(|| "This server was not checked first.".to_owned())?;

    let question = format!(
        "Use {}?\n\nYou will sign in at {}. Only continue if you trust both.",
        candidate.api,
        candidate.issuer.host_str().unwrap_or_default()
    );
    // Off the async runtime: the dialog blocks until the person answers.
    let accepted = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .message(question)
            .title("weebo-notify")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Use this server".to_owned(),
                "Cancel".to_owned(),
            ))
            .blocking_show()
    })
    .await
    .map_err(|error| format!("The confirmation could not be shown: {error}"))?;
    if !accepted {
        return Ok(false);
    }

    instance::save(&candidate).map_err(|error| error.to_string())?;
    *instances
        .current
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(candidate);
    Ok(true)
}

/// The chosen instance as the front's `window.__ENV__`, or `null` before one is chosen.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a Tauri command receives its arguments owned, `State` included"
)]
pub(crate) fn instance_current(instances: State<'_, Arc<Instances>>) -> Option<Value> {
    instances.current().map(|chosen| {
        json!({
            "apiBaseUrl": chosen.api.as_str().trim_end_matches('/'),
            "oidcIssuer": chosen.issuer.as_str(),
            "oidcClientId": chosen.client_id,
            "attachmentOrigin": chosen.attachment_origin.as_ref().map(|origin| origin.origin().ascii_serialization()),
        })
    })
}

/// Forget the chosen instance ("Change server", after signing out).
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a Tauri command receives its arguments owned, `State` included"
)]
pub(crate) fn instance_clear(instances: State<'_, Arc<Instances>>) -> Result<(), String> {
    instance::clear().map_err(|error| error.to_string())?;
    *instances
        .current
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    Ok(())
}

/// The CSP of a page served to the webview, with the remote origins replaced by the
/// chosen instance's.
///
/// Keeps every source that is not an `http(s)` origin (`'self'`, `ipc:`, `data:`,
/// `blob:`, hashes) and the IPC host, drops whatever origins the build named, and
/// appends the instance's: the API and issuer to `connect-src`, the API and the
/// attachment origin to `img-src`. Origins come from parsed URLs, never raw text.
pub(crate) fn csp_for(original: &str, chosen: &Instance) -> String {
    let (connect, images) = chosen.csp_origins();
    original
        .split(';')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .map(|directive| {
            let mut parts = directive.split_whitespace();
            let name = parts.next().unwrap_or_default();
            let extra = match name {
                "connect-src" => &connect,
                "img-src" => &images,
                _ => return directive.to_owned(),
            };
            let kept = parts.filter(|source| {
                *source == "http://ipc.localhost"
                    || !(source.starts_with("http://") || source.starts_with("https://"))
            });
            std::iter::once(name)
                .chain(kept)
                .chain(extra.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chosen() -> Instance {
        Instance {
            api: "https://notify.example.org".parse().unwrap(),
            issuer: "https://auth.example.org/app/".parse().unwrap(),
            client_id: "weebo-notify".to_owned(),
            attachment_origin: Some("https://files.example.org".parse().unwrap()),
        }
    }

    #[test]
    fn swaps_the_built_origins_for_the_chosen_instances() {
        let built = "default-src 'self'; connect-src 'self' ipc: http://ipc.localhost https://baked.example; img-src 'self' data: blob: https://baked.example; script-src 'self' 'sha256-abc'";
        assert_eq!(
            csp_for(built, &chosen()),
            "default-src 'self'; \
             connect-src 'self' ipc: http://ipc.localhost https://notify.example.org https://auth.example.org; \
             img-src 'self' data: blob: https://notify.example.org https://files.example.org; \
             script-src 'self' 'sha256-abc'"
        );
    }

    /// The directives a page could use to send data elsewhere are left as built.
    #[test]
    fn touches_nothing_but_connect_and_img() {
        let built = "form-action 'self'; frame-src 'none'; base-uri 'none'";
        assert_eq!(csp_for(built, &chosen()), built);
    }
}
