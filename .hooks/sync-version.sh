#!/usr/bin/env bash
#
# Propagate the version cocogitto computed to every place that carries one.
#
# ONE version for the whole product (design §3.2bis): the server, the CLI, the web
# frontend, the documentation site and — from M5 — the Tauri shell all ship the same
# number. A user reporting "0.4.2" must identify one build, not four that happen to
# share a name.
#
# Run as a cocogitto pre-bump hook, so the bump commit already contains the synced
# files. Idempotent: re-running with the same version changes nothing.

set -euo pipefail

version="${1:?usage: sync-version.sh <version>}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

updated=()

# The cargo workspace: every crate inherits `version.workspace = true`, so exactly
# one line needs changing. Anchored to the [workspace.package] section so a
# dependency's version specifier can never be hit by accident.
if [[ -f Cargo.toml ]]; then
  awk -v version="$version" '
    /^\[workspace\.package\]/ { in_section = 1 }
    /^\[/ && !/^\[workspace\.package\]/ { in_section = 0 }
    in_section && /^version = / { print "version = \"" version "\""; next }
    { print }
  ' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
  updated+=("Cargo.toml")
  # Keep Cargo.lock consistent so the bump commit is not followed by a dirty tree.
  cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
fi

# The JavaScript projects and, from M5, the Tauri configuration. All JSON with a
# top-level "version", so one loop covers them.
for manifest in web/package.json docs/package.json web/src-tauri/tauri.conf.json; do
  [[ -f "$manifest" ]] || continue
  if command -v yq >/dev/null; then
    yq -i -o=json ".version = \"$version\"" "$manifest"
  else
    # Anchored to the first top-level "version" key: a dependency's version lives
    # nested and is indented further, so it cannot match.
    sed -i -E "0,/^  \"version\": \".*\",/s//  \"version\": \"$version\",/" "$manifest"
  fi
  updated+=("$manifest")
done

printf 'synced version %s into: %s\n' "$version" "${updated[*]}"
