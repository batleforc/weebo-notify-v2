//! `notify login` (§9.1, §5.2): not an RFC 8252 loopback OAuth client, since a cloud workspace's
//! browser can't reach the pod's `127.0.0.1`. It links to the web client to mint a personal
//! API key and asks for it to be pasted back, which works wherever this binary runs.

use std::io::Write as _;

use notify_client_core::{Profile, Session};

use crate::CliError;

/// Open (or print) a link to mint a personal token, then save what comes back.
pub(crate) async fn run(profile: &str, web_url: &str) -> Result<String, CliError> {
    let target = format!("{}/settings", web_url.trim_end_matches('/'));

    println!("Ouvre ce lien, connecte-toi, puis crée/copie un jeton personnel :");
    println!("  {target}");

    // The link is printed either way; failing to open a browser isn't worth failing over.
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

    // Keep the profile's `url`: a login must not silently repoint `notify` at another server.
    let mut config = Profile::load(profile)?;
    config.token = Some(token.to_owned());
    let session = Session::open(&config)?;

    // Catch a bad paste now; every credential kind can call `devices`.
    session.devices().await?;

    Profile::save_token(profile, token)?;

    Ok(format!("jeton enregistré pour le profil « {profile} »"))
}
