//! The desktop leg of Authorization Code + PKCE (RFC 8252, design §10bis.2, §5.1).
//!
//! # Why the shell needs a leg of its own at all
//!
//! In a browser the redirect URI is `${location.origin}/auth/callback` and the
//! whole flow is a navigation: the page leaves for the provider and comes back to
//! itself. A packaged shell has neither half of that. Its origin is
//! `tauri://localhost`, which no identity provider will register — Authentik and
//! Dex both refuse a non-http redirect — and navigating the *shell's own window*
//! to a login page leaves the app showing somebody else's site, with no way back
//! if the person closes the flow halfway.
//!
//! RFC 8252 answers both: the authorization request goes to the system browser (an
//! external user-agent, so the person sees a real URL bar and an already-warm IdP
//! session), and the provider redirects to a listener this process opens on
//! loopback. The app keeps its window, its PKCE verifier and its state value the
//! whole time; only the code travels back.
//!
//! # Why the port is fixed
//!
//! RFC 8252 §7.3 prefers an ephemeral port, and this uses 1420 instead, because a
//! redirect URI has to be *registered* before it can be used — and the two
//! providers this repository configures do not agree on how much they will
//! register. Authentik matches `http://127.0.0.1:[0-9]+/auth/callback` as a regex
//! (`dev/authentik/blueprints/weebo-notify.yaml`) and would take any port; the
//! development mock names one string exactly (`dev/oidc-mock/identities.toml`),
//! and so do plenty of real providers. A fixed port is the only choice that works
//! against both. The cost is stated rather than hidden: a second instance cannot
//! sign in while the first is waiting, which is the `EADDRINUSE` branch below.
//!
//! `127.0.0.1` and never `0.0.0.0`: the second would accept an authorization code
//! from anything that can route to this machine.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt as _;

/// Half of the redirect URI registered with the provider. The other half is
/// `LOOPBACK_REDIRECT_URI` in `web/src/auth/loopback.ts`, and the two are only
/// ever right together — see this module's doc on why it is not ephemeral.
const REDIRECT_PORT: u16 = 1420;

/// The path the provider comes back to. Anything else that reaches the listener
/// (`/favicon.ico`, above all) is answered and ignored rather than treated as a
/// callback, which is why [`serve`] can say "not this one, keep waiting".
const REDIRECT_PATH: &str = "/auth/callback";

/// How long an unfinished sign-in may hold the port.
///
/// Something has to bound it: a person who closes the browser tab instead of
/// signing in would otherwise leave a thread and a bound port for the life of the
/// process, and the *next* attempt would fail on `EADDRINUSE` with no way out but
/// a restart. Five minutes is long enough to find a password manager and short
/// enough that "try again" is a real option.
const DEADLINE: Duration = Duration::from_secs(300);

/// How long to wait for the request line once a connection is accepted. A socket
/// that connects and then says nothing is not a browser.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Open the provider's authorization URL and wait for the code it redirects with.
///
/// Returns the raw query string of the callback — `code=…&state=…`, or
/// `error=…&error_description=…` when the provider refuses. Deliberately raw:
/// `completeLogin` in `web/src/auth/session.ts` already knows how to read every
/// one of those cases and checks `state` against what it generated, and a second
/// implementation of that here would be a second place for the CSRF check to be
/// subtly wrong.
#[tauri::command]
pub(crate) async fn loopback_authorize(
    app: AppHandle,
    authorize_url: String,
) -> Result<String, String> {
    web_url(&authorize_url)?;
    // Bound *before* the browser opens, not after: the reverse order races a fast
    // provider (an already-signed-in session redirects immediately) against our
    // own `bind`, and loses that race as a connection refused the person can only
    // read as "the app is broken".
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "Port {REDIRECT_PORT} is already taken — a sign-in is probably already \
                 waiting in another window. Finish or close it, then try again."
            )
        } else {
            format!("Could not listen on 127.0.0.1:{REDIRECT_PORT}: {error}")
        }
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not arm the sign-in listener: {error}"))?;

    // Printed every time, and not only when opening fails, because "it failed" is
    // not reliably knowable: a headless Linux container is the normal case here
    // (the `desktop` sidecar of devfile.yaml has `xdg-open` and nothing for it to
    // launch), and a launcher that exits 0 having opened nothing is indistinguishable
    // from one that worked. The URL in the log is what makes that recoverable
    // instead of a spinner with no explanation — `task desktop:logs` is where it
    // lands, and on a machine with a real browser nobody ever reads it.
    eprintln!("weebo-notify: sign in here if no browser opened:\n{authorize_url}");

    // A failure is reported and not returned: the listener is already up, so the
    // flow stays completable with the URL above.
    if let Err(error) = app.opener().open_url(&authorize_url, None::<&str>) {
        eprintln!("weebo-notify: could not open a browser ({error}).");
    }

    // `spawn_blocking` rather than an async listener: this is one accept and one
    // short read, and it would cost a whole async TCP stack in a binary that
    // otherwise has no use for one.
    tauri::async_runtime::spawn_blocking(move || wait_for_callback(&listener))
        .await
        .map_err(|error| format!("The sign-in listener stopped unexpectedly: {error}"))?
}

/// Open a URL in the system browser, for the links the shell must not follow itself.
///
/// The logout endpoint is the one that matters (`web/src/auth/session.ts`): the
/// provider's session cookie lives in the browser that signed in, so ending it has
/// to happen there. Navigating the shell's own window to it would clear nothing
/// and strand the app on the provider's page.
///
/// `http`/`https` only — see [`web_url`].
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a Tauri command receives its arguments owned, deserialised from the webview"
)]
pub(crate) fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    web_url(&url)?;
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|error| format!("Could not open {url}: {error}"))
}

/// Refuse anything but a web page before handing a URL to the OS opener.
///
/// Both commands take their URL from the webview, and the opener runs whatever
/// handler the OS maps a scheme to: `file:` opens local files, and a custom
/// scheme reaches any application that registered one. The webview is trusted to
/// ask for a sign-in or sign-out page — never for that, so a script that got
/// into it (the thing CSP is there to stop) cannot use the shell as a launcher.
fn web_url(url: &str) -> Result<(), String> {
    match tauri::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(()),
        _ => Err(format!(
            "Refusing to open {url}: only http(s) pages are opened."
        )),
    }
}

/// Accept connections until one of them is the provider's redirect.
fn wait_for_callback(listener: &TcpListener) -> Result<String, String> {
    let deadline = Instant::now()
        .checked_add(DEADLINE)
        .ok_or_else(|| "the sign-in deadline is past the clock's range".to_owned())?;

    loop {
        match listener.accept() {
            Ok((stream, _)) => match serve(stream) {
                // A callback. The port is released when `listener` drops, which is
                // as soon as this returns.
                Ok(Some(query)) => return Ok(query),
                // Something else knocked — keep the deadline running.
                Ok(None) => {}
                // One bad connection is not a reason to abandon a sign-in the
                // person may still be in the middle of.
                Err(error) => eprintln!("weebo-notify: sign-in listener: {error}"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "The sign-in was not finished in time. Close the browser tab and \
                         try again."
                            .to_string(),
                    );
                }
                // Polling rather than blocking on `accept`: a blocked accept cannot
                // be woken by a deadline, and the thread would outlive the attempt.
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("The sign-in listener failed: {error}")),
        }
    }
}

/// Read one request and answer it. `Ok(None)` means "not the callback".
fn serve(mut stream: TcpStream) -> Result<Option<String>, String> {
    // Accepted sockets do not reliably inherit the listener's blocking state, and
    // the difference between platforms here is a read that returns `WouldBlock`
    // instead of the request line.
    stream
        .set_nonblocking(false)
        .map_err(|error| format!("could not switch the connection to blocking: {error}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| format!("could not set a read timeout: {error}"))?;

    let peer = stream
        .try_clone()
        .map_err(|error| format!("could not read the request: {error}"))?;
    let mut request_line = String::new();
    // Bounded: a local process sending one endless line must not grow this without limit.
    BufReader::new(peer.take(8 * 1024))
        .read_line(&mut request_line)
        .map_err(|error| format!("could not read the request: {error}"))?;

    // "GET /auth/callback?code=…&state=… HTTP/1.1". Only the target is needed, and
    // the headers after it are read by nobody — the response says `close`.
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    if path != REDIRECT_PATH {
        respond(&mut stream, "404 Not Found", NOT_FOUND)?;
        return Ok(None);
    }

    respond(&mut stream, "200 OK", DONE)?;
    Ok(Some(query.to_string()))
}

/// Write one complete HTTP/1.0 response and close.
///
/// `Content-Length` and `Connection: close` are both required, not decoration: a
/// browser given neither waits for the connection to end before painting, and the
/// person watching sees a blank tab where the "you can close this" was supposed to
/// be — while the shell, which already has the code, looks like it hung.
fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<(), String> {
    let response = format!(
        "HTTP/1.0 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("could not answer the browser: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("could not answer the browser: {error}"))
}

/// Shown in the browser once the code has been handed over. No script and no
/// styling: this page is served by an app, into a browser it does not control, and
/// it has one sentence to say.
const DONE: &str = "<!doctype html><html lang=fr><meta charset=utf-8>\
<title>weebo-notify</title>\
<p>Connexion terminée. Vous pouvez fermer cet onglet et revenir à weebo-notify.";

/// Everything else that reaches the listener while it waits.
const NOT_FOUND: &str = "<!doctype html><html lang=fr><meta charset=utf-8>\
<title>weebo-notify</title>\
<p>Rien ici. Cette adresse ne sert qu'au retour de connexion de weebo-notify.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one real connection through [`serve`] and return what it decided,
    /// alongside what the browser on the other end was told.
    fn round_trip(request: &str) -> (Option<String>, String) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
        let address = listener.local_addr().expect("read the bound address");

        let sent = request.to_string();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(sent.as_bytes()).expect("send");
            let mut answer = String::new();
            stream.read_to_string(&mut answer).expect("read the answer");
            answer
        });

        let (stream, _) = listener.accept().expect("accept");
        let decided = serve(stream).expect("serve");
        (decided, client.join().expect("the client thread"))
    }

    #[test]
    fn returns_the_query_of_a_callback() {
        let (decided, answer) =
            round_trip("GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(decided.as_deref(), Some("code=abc&state=xyz"));
        assert!(answer.starts_with("HTTP/1.0 200 OK"), "{answer}");
    }

    /// The provider's refusal travels the same road as its success — `completeLogin`
    /// reads `error`/`error_description` out of exactly this string.
    #[test]
    fn returns_the_query_of_a_refusal() {
        let (decided, _) = round_trip(
            "GET /auth/callback?error=access_denied&error_description=nope HTTP/1.1\r\n\r\n",
        );
        assert_eq!(
            decided.as_deref(),
            Some("error=access_denied&error_description=nope")
        );
    }

    /// The one that made the loop necessary: a browser asks for a favicon before
    /// anybody has signed in, and that must not end the wait.
    #[test]
    fn ignores_anything_that_is_not_the_callback() {
        let (decided, answer) = round_trip("GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(decided, None);
        assert!(answer.starts_with("HTTP/1.0 404 Not Found"), "{answer}");
    }

    /// A callback with no query at all is still a callback, and `completeLogin`
    /// is the one that says "came back without an authorization code".
    #[test]
    fn accepts_a_callback_without_a_query() {
        let (decided, _) = round_trip("GET /auth/callback HTTP/1.1\r\n\r\n");
        assert_eq!(decided.as_deref(), Some(""));
    }

    /// The opener must never be handed a local file or another app's scheme.
    #[test]
    fn opens_only_web_pages() {
        assert!(web_url("https://idp.test/logout?client_id=x").is_ok());
        assert!(web_url("http://127.0.0.1:9110/authorize").is_ok());
        for refused in [
            "file:///etc/passwd",
            "weebo-notify://n/1",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(web_url(refused).is_err(), "{refused}");
        }
    }

    /// The response has to be complete on its own — see [`respond`]'s doc.
    #[test]
    fn answers_with_a_length_and_a_close() {
        let (_, answer) = round_trip("GET /auth/callback?code=abc HTTP/1.1\r\n\r\n");
        assert!(
            answer.contains(&format!("Content-Length: {}", DONE.len())),
            "{answer}"
        );
        assert!(answer.contains("Connection: close"), "{answer}");
    }
}
