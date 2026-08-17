# Sourced — NOT executed — by nginx's own entrypoint (see this file's permission
# bits: it deliberately has no +x). `docker-entrypoint.sh` runs every executable
# `docker-entrypoint.d/*.sh` in its own subshell, whose exports die with it; a
# non-executable one is `.`-sourced into the entrypoint's own shell instead, which
# is the only way anything this script exports is still around when
# `20-envsubst-on-templates.sh` renders `default.conf.template` right after it
# (design §11, ADR-0021).
#
# Two renders, one env, and a hash that is not a deployment's to set:
#
#   * `env.js` — what the SPA reads at startup (`src/config.ts`).
#   * `WNK_CSP_HEADER` — the Content-Security-Policy nginx sends as one header
#     (`default.conf.template`). `csp-template.txt` was written at build time by
#     `web/vite.config.ts`, and already carries the one thing a deployment must
#     NOT override: the hash of this exact build's inline script. Everything else
#     in it is a `${WNK_*}` placeholder, unresolved until this line runs `envsubst`
#     on it a second time — `20-envsubst-on-templates.sh` never re-scans a value
#     it has already substituted in, so nesting the raw template straight into
#     `default.conf.template` would leave these placeholders as literal text in
#     the header nginx ships.

# No `set -e`/`-u` here: this runs inside nginx's own entrypoint shell (that is
# the whole point of being sourced), and turning those on would change how the
# rest of ITS script behaves, not just this file's few lines. Each step below
# checks and exits explicitly instead — deliberately: a container that starts
# with a stale or empty CSP header is a worse failure than one that refuses to
# start at all.

WNK_CSP_HEADER="$(envsubst '${WNK_API_BASE_URL} ${WNK_OIDC_ISSUER} ${WNK_ATTACHMENT_ORIGIN}' \
  < /etc/nginx/wnk-templates/csp-template.txt)" || exit 1
export WNK_CSP_HEADER

envsubst '${WNK_API_BASE_URL} ${WNK_OIDC_ISSUER} ${WNK_OIDC_CLIENT_ID}' \
  < /etc/nginx/wnk-templates/env.js.template \
  > /usr/share/nginx/html/env.js || exit 1
