#!/usr/bin/env bash
# Render dev/dex/config.template.yaml with credentials generated right now, and
# print the environment the compatibility suite needs (design §3.2, D7).
#
# Called by the `oidc-compat` CI job before starting Dex, and by `task dev:dex` —
# under compose, where it brings a container up, and in an Eclipse Che workspace,
# where the sidecar is already scheduled and only the process has to start. Two
# properties are the point:
#
#   * the password and the client secret are generated per run and never committed.
#     Dex is CI-only, so no local ergonomics argue for a fixed one the way
#     alice/weebo-dev does for the mock (§12.5);
#   * the rendered file lands outside the repository tree by default, so a `git
#     status` after a run is still clean and nothing can be committed by accident.
#
# Usage:  eval "$(dev/dex/render.sh [output-path])"
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-${RUNNER_TEMP:-/tmp}/dex-config.yaml}"

password="$(openssl rand -base64 24)"
secret="$(openssl rand -hex 24)"

# Dex wants **bcrypt** and nothing else. Two tools can produce one; whichever is
# present wins, and the message names both rather than the one that happens to be
# missing.
if command -v caddy >/dev/null 2>&1; then
  # Caddy hashes with Go's bcrypt, so the prefix is already the one Dex accepts.
  hash="$(caddy hash-password --plaintext "$password")"
elif command -v htpasswd >/dev/null 2>&1; then
  # `-B` is bcrypt, `-C 10` its cost, `-n` prints instead of writing a file; the
  # leading colon is the empty user name, and Dex wants the hash alone.
  #
  # Go's bcrypt refuses the `$2y$` prefix Apache emits and accepts `$2a$` — the same
  # algorithm, and the one-character difference between "Dex starts" and an invalid
  # hash it only reports at the first login attempt.
  hash="$(htpasswd -bnBC 10 "" "$password" | tr -d ':\n' | sed 's/^\$2y/\$2a/')"
else
  # The branch that cannot be missing, and the reason this script now runs
  # everywhere. An Eclipse Che workspace has neither of the two above — mise ships
  # tools, not Apache — and this leg is precisely the one D7 says has never once
  # been observed running, so a script that stops here is a matrix that stays
  # unread. `cargo` is present by definition: this is a Rust workspace.
  #
  # Slower than a binary already on the box, hence last. The password goes in on
  # stdin rather than in an argument, unlike the caddy branch above: `ps` shows an
  # argument to every process on the machine.
  hash="$(printf %s "$password" |
    cargo run --quiet --manifest-path "$here/../../Cargo.toml" -p xtask -- bcrypt-hash)"
fi

mkdir -p "$(dirname "$out")"
sed -e "s|__CLIENT_SECRET__|$secret|" -e "s|__ALICE_HASH__|$hash|" \
  "$here/config.template.yaml" >"$out"
chmod 600 "$out"

# Printed as exports so the caller decides what to do with them; nothing is echoed
# into a log by this script itself.
cat <<EOF
export WNK_TEST_DEX_CONFIG='$out'
export WNK_TEST_OIDC_PROVIDER=dex
export WNK_TEST_OIDC_ISSUER='http://127.0.0.1:5556/dex'
export WNK_TEST_OIDC_CLIENT_ID='weebo-notify'
export WNK_TEST_OIDC_CLIENT_SECRET='$secret'
export WNK_TEST_OIDC_USERNAME='alice@weebo.local'
export WNK_TEST_OIDC_PASSWORD='$password'
EOF
