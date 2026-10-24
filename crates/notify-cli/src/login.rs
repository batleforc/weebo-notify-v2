//! `notify login` (§9.1, §5.2): not an RFC 8252 loopback OAuth client, since a cloud workspace's
//! browser can't reach the pod's `127.0.0.1`. It links to the web client to mint a personal
//! API key and asks for it to be pasted back, which works wherever this binary runs.

use std::io::{IsTerminal as _, Read as _, Write as _};

use notify_client_core::{Profile, Session};

use crate::CliError;

/// What the token is filed under in the OS vault, per profile.
///
/// Not the desktop shell's `weebo-notify` (`web/src-tauri/src/credential.rs`):
/// that entry, under the same profile name, holds the shell's OIDC refresh token,
/// and a `notify login` overwriting it would sign the desktop app out.
const VAULT_SERVICE: &str = "weebo-notify-cli";

fn vault(profile: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(VAULT_SERVICE, profile)
}

/// [`Profile::load`], plus the token `notify login` put in the OS vault.
///
/// The vault is consulted last, only when neither `WNK_TOKEN` nor the config file
/// named a token: an explicit environment still wins, and a config file written
/// before the vault was used keeps working exactly as it did. A vault that is
/// absent or failing reads as "no token" — a headless host has no Secret Service,
/// and the 0600 file is the supported answer there (§9.3bis), not an error.
pub(crate) fn load_profile(profile: &str) -> Result<Profile, CliError> {
    let mut config = Profile::load(profile)?;
    if config.token.is_none() {
        config.token = vault(profile).and_then(|entry| entry.get_password()).ok();
    }
    Ok(config)
}

/// Persist a checked token: the OS vault when there is one, the 0600 file when not.
///
/// Whichever one takes it, the other must not keep an older copy — a plaintext
/// token left in the file would shadow the vault one (the file is read first).
fn save_token(profile: &str, token: &str) -> Result<&'static str, CliError> {
    match vault(profile).and_then(|entry| entry.set_password(token)) {
        Ok(()) => {
            Profile::clear_token(profile)?;
            Ok("dans le trousseau du système")
        }
        Err(error) => {
            eprintln!("pas de trousseau système ({error}) ; fichier 0600 à la place");
            Profile::save_token(profile, token)?;
            Ok("dans le fichier de configuration (0600)")
        }
    }
}

/// Read one line without echoing it: a token on screen ends up in scrollback,
/// screen shares and terminal recordings.
///
/// Raw mode is the only way to turn echo off that `crossterm` (already here for
/// the TUI) offers, and raw mode also turns off line editing and `ICRNL` — hence
/// the byte loop, ending on either `\r` or `\n`, and backspace handled by hand.
/// Bracketed paste is never enabled, so a paste arrives as plain bytes. A stdin
/// that is not a terminal (`echo $TOKEN | notify login`) has nothing to echo and
/// is read as a plain line.
fn read_secret() -> std::io::Result<String> {
    let mut line = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_line(&mut line)?;
        return Ok(line);
    }

    crossterm::terminal::enable_raw_mode()?;
    let mut bytes = Vec::new();
    let read: std::io::Result<()> = (|| {
        for byte in std::io::stdin().lock().bytes() {
            match byte? {
                b'\r' | b'\n' | 0x04 => break,
                // Ctrl-C: raw mode turned off the signal, so honour it here.
                0x03 => return Err(std::io::ErrorKind::Interrupted.into()),
                0x08 | 0x7f => {
                    bytes.pop();
                }
                other => bytes.push(other),
            }
        }
        Ok(())
    })();
    // Restored before anything else can return, or the shell is left raw.
    let restored = crossterm::terminal::disable_raw_mode();
    println!();
    read?;
    restored?;

    String::from_utf8(bytes).map_err(std::io::Error::other)
}

/// Open (or print) a link to mint a personal token, then save what comes back.
pub(crate) async fn run(profile: &str, web_url: &str) -> Result<String, CliError> {
    let target = format!("{}/settings", web_url.trim_end_matches('/'));

    println!("Ouvre ce lien, connecte-toi, puis crée/copie un jeton personnel :");
    println!("  {target}");

    // The link is printed either way; failing to open a browser isn't worth failing over.
    if std::env::var_os("NO_BROWSER").is_none() {
        #[expect(
            clippy::let_underscore_must_use,
            let_underscore_drop,
            reason = "the link is printed either way; failing to open a browser is not worth failing over"
        )]
        let _ = open::that(&target);
    }

    print!("\nColle le jeton ici : ");
    std::io::stdout()
        .flush()
        .map_err(|error| CliError::Api(error.to_string()))?;

    let pasted = read_secret().map_err(|error| CliError::Api(error.to_string()))?;
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

    let place = save_token(profile, token)?;

    Ok(format!(
        "jeton enregistré {place} pour le profil « {profile} »"
    ))
}
