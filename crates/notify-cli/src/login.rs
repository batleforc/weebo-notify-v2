//! `notify login` (design §9.1, §5.2).
//!
//! # Why this is not an OAuth client
//!
//! The obvious shape for "log in from a terminal" is RFC 8252: open the browser
//! straight at the authorize URL, catch the redirect on a loopback port, exchange
//! the code. It was tried and set aside — not for lack of code, but because the
//! premise breaks the moment this binary is not on the same machine as the
//! browser it opened. A cloud workspace is exactly that case: the browser the
//! person actually clicks through is on their laptop, and it cannot reach a
//! `127.0.0.1` port inside the workspace's pod. `NO_BROWSER` would then need a
//! *second*, different fallback — paste a short-lived authorization code fast
//! enough to beat its own expiry — on top of the first.
//!
//! What is here instead: the web client already has a working OIDC login, and
//! the server already has a self-service personal API key (`POST /apikeys` with
//! `owner: {user, id}`, guarded exactly like a personal key should be — see
//! `notify_app::CreateApiKey`). So `notify login` points at that, in a browser
//! near this machine or far from it, and asks for what comes back. One fallback
//! covers both cases, because auto-opening a browser was never the part that
//! could fail differently depending on where this binary runs — only the
//! copy-paste at the end ever has to.

use std::io::Write as _;

use notify_client_core::{Profile, Session};

use crate::CliError;

/// Open (or print) a link to mint a personal token, then save what comes back.
pub(crate) async fn run(profile: &str, web_url: &str) -> Result<String, CliError> {
    let target = format!("{}/settings", web_url.trim_end_matches('/'));

    println!("Ouvre ce lien, connecte-toi, puis crée/copie un jeton personnel :");
    println!("  {target}");

    // `NO_BROWSER` only decides whether this line also tries to open something —
    // the link above is printed either way, and a browser that fails to open
    // (no display, no default handler) is not worth failing the command over.
    if std::env::var_os("NO_BROWSER").is_none() {
        let _ = open::that(&target);
    }

    print!("\nColle le jeton ici : ");
    std::io::stdout()
        .flush()
        .map_err(|error| CliError::Api(error.to_string()))?;

    let mut pasted = String::new();
    std::io::stdin()
        .read_line(&mut pasted)
        .map_err(|error| CliError::Api(error.to_string()))?;
    let token = pasted.trim();
    if token.is_empty() {
        return Err(CliError::Usage("aucun jeton collé".to_owned()));
    }

    // The existing profile's `url` (or `WNK_URL`, or the dev-server default) is
    // kept; only the credential changes — a login must not silently repoint
    // `notify` at a different server.
    let mut config = Profile::load(profile)?;
    config.token = Some(token.to_owned());
    let session = Session::open(&config)?;

    // A lightweight authenticated call, so a bad paste is caught here rather
    // than saved and discovered at the next `notify ls`. `devices` rather than
    // something narrower: every credential this crate accepts — an OIDC token
    // or an API key — can call it.
    session.devices().await?;

    Profile::save_token(profile, token)?;

    Ok(format!("jeton enregistré pour le profil « {profile} »"))
}
