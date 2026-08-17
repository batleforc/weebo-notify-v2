#!/usr/bin/env bash
#
# Seed the development environment (design §12.6).
#
# Idempotent by construction: safe to re-run after `task dev:reset`, on a recreated
# Eclipse Che workspace, or just because you are not sure. Every step either
# reports "already there" or does the minimum to get there.
#
# Steps that depend on an optional service (Authentik) skip cleanly when it is not
# running, so the same script serves `task dev:core`, `task dev:up` and
# `task dev:auth` without variants.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# Committed dev credentials; dev/.env.local holds the generated Authentik secrets.
# `if` and not `[[ … ]] && source …`: under `set -e` that one-liner ends the script
# with status 1 when the file is absent, which is exactly the case it was written to
# tolerate — `task dev:seed` on a clone where `task dev:env` has never run.
# shellcheck disable=SC1091
source dev/.env
if [[ -f dev/.env.local ]]; then
  # shellcheck disable=SC1091
  source dev/.env.local
fi

DATABASE_URL="${WNK_DATABASE__URL:-postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@localhost:5432/${POSTGRES_DB}}"
S3_ENDPOINT="${WNK_ATTACHMENTS__ENDPOINT:-http://localhost:9000}"
AUTHENTIK_URL="${AUTHENTIK_URL:-http://localhost:9100}"
MIGRATIONS_DIR="crates/notify-adapters/persistence-pg/migrations"

step()  { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
info()  { printf '    %s\n' "$1"; }
skip()  { printf '    \033[2mskipped: %s\033[0m\n' "$1"; }
warn()  { printf '    \033[33m%s\033[0m\n' "$1" >&2; }

# Wait for a TCP endpoint to answer, so seeding right after `up -d` is not a race.
wait_for() { # $1 = label, $2 = url, $3 = seconds
  local label="$1" url="$2" deadline=$(( SECONDS + ${3:-60} ))
  until curl -fsS --max-time 2 -o /dev/null "$url" 2>/dev/null; do
    if (( SECONDS >= deadline )); then
      echo "    timed out waiting for $label at $url" >&2
      return 1
    fi
    sleep 1
  done
}

# ---------------------------------------------------------------------------
step "Database migrations"
# ---------------------------------------------------------------------------
if [[ ! -d "$MIGRATIONS_DIR" ]]; then
  skip "no migrations directory — persistence-pg is not bootstrapped in this checkout"
elif ! command -v sqlx >/dev/null; then
  skip "sqlx-cli not on PATH — run \`mise install\`"
else
  # `database create` is a no-op when the database already exists, and `migrate run`
  # only applies what is missing: re-running this is free (T24).
  sqlx database create --database-url "$DATABASE_URL"
  sqlx migrate run --database-url "$DATABASE_URL" --source "$MIGRATIONS_DIR"
  info "schema up to date"
fi

# ---------------------------------------------------------------------------
step "Attachment bucket"
# ---------------------------------------------------------------------------
bucket="${RUSTFS_BUCKET:-weebo-notify-attachments}"
if ! command -v aws >/dev/null; then
  skip "awscli not on PATH — run \`mise install\`"
else
  export AWS_ACCESS_KEY_ID="$RUSTFS_ACCESS_KEY"
  export AWS_SECRET_ACCESS_KEY="$RUSTFS_SECRET_KEY"
  export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-east-1}"

  if aws --endpoint-url "$S3_ENDPOINT" s3api head-bucket --bucket "$bucket" 2>/dev/null; then
    info "bucket $bucket already exists"
  else
    aws --endpoint-url "$S3_ENDPOINT" s3api create-bucket --bucket "$bucket" >/dev/null
    info "created bucket $bucket"
  fi

  # The bucket sits inside the trust boundary but must never be anonymously
  # readable: a presigned URL is the only path to an attachment (§6.1).
  aws --endpoint-url "$S3_ENDPOINT" s3api put-public-access-block \
    --bucket "$bucket" \
    --public-access-block-configuration \
    "BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true" \
    2>/dev/null || info "public-access-block unsupported by this store — verify privacy manually"
fi

# ---------------------------------------------------------------------------
step "Authentik blueprint"
# ---------------------------------------------------------------------------
BLUEPRINT_PATH="weebo/weebo-notify.yaml"
# What the blueprint is *for*, and therefore the only honest test that it worked.
OIDC_DISCOVERY_URL="$AUTHENTIK_URL/application/o/weebo-notify/.well-known/openid-configuration"

# Every API call goes through here because Authentik answers 403 both to a token it
# does not accept and to a route that does not exist, and a bare `curl -f` throws
# away the one thing that tells those apart. Sets ak_status and ak_body.
ak_status=""
ak_body=""
ak_api() { # $1 = method, $2 = path, [$3 = json body]
  local method="$1" path="$2" response
  local -a args=(
    -sS -X "$method"
    -H "Authorization: Bearer ${AUTHENTIK_BOOTSTRAP_TOKEN}"
    -H "Content-Type: application/json"
    --max-time 30
    -w $'\n%{http_code}'
  )
  if (( $# >= 3 )); then
    args+=(--data "$3")
  fi
  response=$(curl "${args[@]}" "$AUTHENTIK_URL$path") || return 1
  ak_status="${response##*$'\n'}"
  ak_body="${response%$'\n'*}"
}

# Ask Authentik to apply the blueprint NOW rather than on its next scan, so that
# editing dev/authentik/blueprints/ and re-running `task dev:seed` means something.
#
# Best-effort by design: it returns non-zero and says why instead of aborting,
# because the wait below is what actually has to hold. Authentik discovers the
# /blueprints mount at startup on its own (§12.5) — this only removes the delay.
#
# Note the two calls. There is no "apply the blueprint at this path" endpoint: a
# blueprint is applied by the uuid of the *instance* discovery created for it. This
# used to POST to /api/v3/managed/blueprints/apply/, which is not a route at all;
# Authentik answered 403, the script called that "refused" and carried on, and the
# OIDC provider did not exist for another minute — long enough for the oidc-compat
# job to start and fail on a discovery document that was still a 404 page.
force_apply_blueprint() {
  if ! ak_api GET "/api/v3/managed/blueprints/?path=$BLUEPRINT_PATH"; then
    warn "the blueprint API is unreachable"
    return 1
  fi
  if [[ "$ak_status" != "200" ]]; then
    warn "the blueprint API answered HTTP $ak_status — is AUTHENTIK_BOOTSTRAP_TOKEN the one this instance booted with?"
    return 1
  fi

  local uuid
  uuid=$(printf '%s' "$ak_body" \
    | grep -oE '"pk" *: *"[0-9a-fA-F-]{36}"' \
    | head -n1 \
    | grep -oE '[0-9a-fA-F-]{36}') || true
  if [[ -z "$uuid" ]]; then
    info "Authentik has not discovered $BLUEPRINT_PATH yet"
    return 1
  fi

  if ak_api POST "/api/v3/managed/blueprints/$uuid/apply/" && [[ "$ak_status" == "200" ]]; then
    return 0
  fi
  warn "applying blueprint $uuid returned HTTP $ak_status"
  return 1
}

if ! curl -fsS --max-time 2 -o /dev/null "$AUTHENTIK_URL/-/health/live/" 2>/dev/null; then
  skip "Authentik is not running (start it with \`task dev:auth\`)"
elif [[ -z "${AUTHENTIK_BOOTSTRAP_TOKEN:-}" ]]; then
  skip "no AUTHENTIK_BOOTSTRAP_TOKEN — run \`task dev:env\`"
else
  wait_for "Authentik" "$AUTHENTIK_URL/-/health/ready/" 120

  if force_apply_blueprint; then
    info "blueprint applied (alice, bob, carol, groups, OIDC provider)"
  else
    info "falling back to Authentik's own discovery of the /blueprints mount"
  fi

  # The gate, and the reason the apply above is allowed to be best-effort: nothing
  # downstream cares whether an API call succeeded, only whether the provider
  # answers. Waiting here is what makes "Seed complete." true — `task dev:seed`
  # used to print it over an Authentik with no weebo-notify application on it, and
  # the next thing to run inherited a 404 with no idea why.
  if wait_for "the weebo-notify OIDC provider" "$OIDC_DISCOVERY_URL" 120; then
    info "OIDC provider live at $OIDC_DISCOVERY_URL"
  else
    # Authentik records why on the instance itself, and "no provider after two
    # minutes" is the least useful half of that. A blueprint that names a flow or a
    # certificate this version does not ship fails here and nowhere else.
    if ak_api GET "/api/v3/managed/blueprints/?path=$BLUEPRINT_PATH" && [[ "$ak_status" == "200" ]]; then
      warn "Authentik says: $ak_body"
    fi
    warn "the blueprint produced no OIDC provider — inspect it with \`task dev:logs:tail\`"
    exit 1
  fi
fi

printf '\n\033[1mSeed complete.\033[0m\n'
info "identity : task dev:oidc   -> http://localhost:9110"
info "server   : task dev:run    -> http://localhost:8080"
info "web      : task dev:web    -> http://localhost:5173"
info "log in as: alice / bob / carol, password ${DEV_USER_PASSWORD:-weebo-dev}"
