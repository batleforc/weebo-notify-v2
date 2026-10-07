//! Bcrypts the Dex dev password (§3.2) where neither caddy nor htpasswd exists (Che).
//! Read from stdin to stay out of `ps`; emitted as `$2a$`, the shape the other branches produce.

use std::io::Read as _;

use anyhow::Context as _;

/// Read a password from stdin and print its bcrypt hash, in the `$2a$` form.
pub(crate) fn bcrypt_stdin(cost: u32) -> anyhow::Result<()> {
    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .context("reading the password from stdin")?;

    // Only the trailing newline `echo` appends; a leading space may be intentional.
    let password = password.trim_end_matches(['\n', '\r']);
    anyhow::ensure!(
        !password.is_empty(),
        "no password on stdin — pipe one in:\n  \
         printf %s \"$password\" | cargo run --quiet -p xtask -- bcrypt-hash"
    );

    let hashed = bcrypt::hash_with_result(password, cost)
        .context("bcrypting the password")?
        .format_for_version(bcrypt::Version::TwoA);

    println!("{hashed}");
    Ok(())
}
