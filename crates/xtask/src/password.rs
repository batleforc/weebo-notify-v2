//! Bcrypting the Dex password of the identity matrix (design §3.2, §14.4/D7).
//!
//! Same reasoning as [`crate::vapid`] and [`crate::federation`], applied to a third
//! chore. `dev/dex/render.sh` could produce the one hash Dex accepts through `caddy`
//! or through `htpasswd`, and an Eclipse Che workspace has neither — mise ships
//! tools, not Apache. The rule this repository already wrote for the VAPID key
//! covers it: a chore that needs real code should not be a shell one-liner with an
//! assumption about somebody's machine in it.
//!
//! The two existing branches stay, and stay first: CI runs on an image that has
//! them, and a hash is cheaper from a binary already on the box than from a cargo
//! build. This is the branch that makes the script run *everywhere*, which is what
//! D7 needs — the Dex leg has never once been observed running.
//!
//! # Why the password arrives on stdin
//!
//! An argument is visible in `ps` to every process on the machine for as long as the
//! command runs, and it lands in a shell history besides. This is a throwaway
//! development password generated per run, so the exposure is genuinely small — but
//! the habit is what carries to the next generator, and the caller pipes it in for
//! free.
//!
//! # Why `$2a$` and not the crate's own `$2b$`
//!
//! So that all three branches emit the same shape. `$2a$` is what the `caddy` branch
//! produces and what the `htpasswd` branch rewrites to, and Go's bcrypt — the one
//! inside Dex — is what both were checked against. A third prefix here would be a
//! fourth thing to verify for no gain, and the failure it risks is the one
//! `render.sh` already documents: a hash Dex only rejects at the first login attempt,
//! long after the step that wrote it went green.

use std::io::Read as _;

use anyhow::Context as _;

/// Read a password from stdin and print its bcrypt hash, in the `$2a$` form.
pub(crate) fn bcrypt_stdin(cost: u32) -> anyhow::Result<()> {
    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .context("reading the password from stdin")?;

    // What `echo` appends and nobody means to hash. Trimmed at the end only: a
    // leading space is a character somebody could genuinely have chosen.
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
