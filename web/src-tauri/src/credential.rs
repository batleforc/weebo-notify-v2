//! Where the desktop shell keeps the credential that outlives the process.
//!
//! §10bis.2 asks for tokens in the **OS vault** — Secret Service, Keychain,
//! Credential Manager — and "jamais en localStorage ni dans la webview". The
//! webview had neither: it kept its refresh token in `sessionStorage`, which is
//! scoped to the browsing session and therefore dies with the process. That is a
//! defensible place for a *browser* to keep it — [ADR-0020] weighs exactly that
//! and decides to leave it there — but it means the shell has no session to resume
//! at startup, ever. Every launch lands on the login screen, which is why the
//! window cannot start in the tray the way §10bis.3 describes.
//!
//! So this module is the durable half, and it is deliberately the *only* half that
//! is durable: the access token stays in webview memory, because it is the
//! short-lived one and putting it here would buy nothing.
//!
//! # Why a fallback file exists
//!
//! A vault is not a given. A headless host — a server, a container, the `desktop`
//! sidecar of `devfile.yaml` — has no Secret Service to talk to, and §9.3bis names
//! the answer rather than leaving it to chance: "fallback fichier `0600` dans le
//! répertoire du profil". The failure to avoid is a sign-in that works and then
//! silently forgets, which is indistinguishable from the bug this whole module
//! exists to fix.
//!
//! The two stores are never both authoritative. Whichever one a save lands in, the
//! other is cleared, so a machine that grows a vault later does not keep answering
//! from a file written before it had one.

use std::fs;
use std::io;
use std::path::PathBuf;

use notify_client_core::{DEFAULT_PROFILE, store_path};

/// What the secret is filed under in the vault.
///
/// The account is the profile name (§9.1): two profiles are two servers and two
/// identities, so they must not share an entry — the same reason `store_path` is
/// per-profile.
const SERVICE: &str = "weebo-notify";

/// The `0600` file used when there is no vault, next to that profile's store.
///
/// `store_path` and not a path spelled out again here: the profile directory is
/// its answer to give, and two spellings of it would disagree the day either moves.
fn fallback_path(profile: &str) -> PathBuf {
    store_path(profile).with_file_name("credential")
}

fn vault(profile: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(SERVICE, profile)
}

/// Write the fallback file, creating the profile directory and locking the mode
/// down *before* the secret reaches the disk.
///
/// The order matters: `write` then `set_permissions` leaves a window in which the
/// secret is on disk and world-readable, which on a shared host is the whole
/// exposure this is trying to avoid.
fn write_fallback(profile: &str, secret: &str) -> io::Result<()> {
    let path = fallback_path(profile);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(secret.as_bytes())
    }

    // Windows and macOS reach this only when their own vault is unavailable, which
    // is not a normal state on either. The file inherits the directory's ACL rather
    // than a mode this platform does not have.
    #[cfg(not(unix))]
    fs::write(&path, secret)
}

fn clear_fallback(profile: &str) -> io::Result<()> {
    match fs::remove_file(fallback_path(profile)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Persist the credential, in the vault if there is one.
///
/// A vault that refuses is reported on stderr and not returned as an error: the
/// fallback is a supported configuration (§9.3bis), not a degraded one, and a
/// sign-in must not fail because the machine has no keyring daemon.
#[tauri::command]
pub fn credential_save(secret: String) -> Result<(), String> {
    match vault(DEFAULT_PROFILE).and_then(|entry| entry.set_password(&secret)) {
        Ok(()) => {
            // A file written before this machine had a vault must not outlive it,
            // or `credential_load` below would keep answering from the older of two
            // secrets after the next rotation.
            if let Err(error) = clear_fallback(DEFAULT_PROFILE) {
                eprintln!("weebo-notify: could not remove the fallback credential: {error}");
            }
            Ok(())
        }
        Err(error) => {
            eprintln!("weebo-notify: no OS vault ({error}); using a 0600 file instead");
            write_fallback(DEFAULT_PROFILE, &secret).map_err(|error| error.to_string())
        }
    }
}

/// Read the credential back, or `None` when this machine holds none.
///
/// "None" and an error are different answers and the caller acts on them
/// differently: no credential is the ordinary first-launch state and means "show
/// the login screen", while a vault that is present and failing is worth saying out
/// loud rather than silently treating as a fresh install.
#[tauri::command]
pub fn credential_load() -> Result<Option<String>, String> {
    match vault(DEFAULT_PROFILE).and_then(|entry| entry.get_password()) {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => read_fallback(DEFAULT_PROFILE),
        Err(error) => {
            eprintln!("weebo-notify: no OS vault ({error}); reading the 0600 file instead");
            read_fallback(DEFAULT_PROFILE)
        }
    }
}

fn read_fallback(profile: &str) -> Result<Option<String>, String> {
    match fs::read_to_string(fallback_path(profile)) {
        Ok(secret) => Ok(Some(secret)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Forget the credential, in both stores.
///
/// Both, unconditionally, and errors swallowed after being printed: this runs on
/// sign-out, and a logout that reports failure while having removed one of the two
/// copies is worse than one that removes what it can and says so on stderr.
#[tauri::command]
pub fn credential_clear() -> Result<(), String> {
    match vault(DEFAULT_PROFILE).and_then(|entry| entry.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(error) => eprintln!("weebo-notify: could not clear the vault entry: {error}"),
    }

    if let Err(error) = clear_fallback(DEFAULT_PROFILE) {
        eprintln!("weebo-notify: could not remove the fallback credential: {error}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests read `XDG_DATA_HOME` and one sets it; cargo runs them in parallel.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The fallback lives beside the profile's store rather than at some path of
    /// this module's own invention — the property that keeps a second profile from
    /// reading the first one's secret.
    #[test]
    fn the_fallback_sits_next_to_that_profile_store() {
        let _env = ENV
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = fallback_path("work");
        assert_eq!(path.file_name().unwrap(), "credential");
        assert_eq!(path.parent(), store_path("work").parent());
        assert_ne!(path, fallback_path("default"));
    }

    /// `0600` is the point of the fallback, not a detail of it: the whole reason
    /// §9.3bis allows a file at all is that it is no more readable than the vault
    /// entry it stands in for.
    #[cfg(unix)]
    #[test]
    fn the_fallback_file_is_not_readable_by_anybody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let _env = ENV
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: `ENV` keeps every test that touches the environment serial, and
        // the variable is what `store_path` reads.
        unsafe { std::env::set_var("XDG_DATA_HOME", dir.path()) };

        write_fallback("test-profile", "s3cr3t").unwrap();

        let path = fallback_path("test-profile");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        assert_eq!(fs::read_to_string(&path).unwrap(), "s3cr3t");

        // And it goes away again, idempotently — sign-out runs this on machines
        // that never wrote one.
        clear_fallback("test-profile").unwrap();
        assert!(!path.exists());
        clear_fallback("test-profile").unwrap();
    }
}
