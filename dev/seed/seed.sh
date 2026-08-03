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
# shellcheck disable=SC1091
source dev/.env
[[ -f dev/.env.local ]] && source dev/.env.local

DATABASE_URL="${WNK_DATABASE__URL:-postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@localhost:5432/${POSTGRES_DB}}"
S3_ENDPOINT="${WNK_ATTACHMENTS__ENDPOINT:-http://localhost:9000}"
AUTHENTIK_URL="${AUTHENTIK_URL:-http://localhost:9100}"
MIGRATIONS_DIR="crates/notify-adapters/persistence-pg/migrations"

step()  { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
info()  { printf '    %s\n' "$1"; }
skip()  { printf '    \033[2mskipped: %s\033[0m\n' "$1"; }

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
if ! curl -fsS --max-time 2 -o /dev/null "$AUTHENTIK_URL/-/health/live/" 2>/dev/null; then
  skip "Authentik is not running (start it with \`task dev:auth\`)"
elif [[ -z "${AUTHENTIK_BOOTSTRAP_TOKEN:-}" ]]; then
  skip "no AUTHENTIK_BOOTSTRAP_TOKEN — run \`task dev:env\`"
else
  wait_for "Authentik" "$AUTHENTIK_URL/-/health/ready/" 120
  # Authentik applies blueprints found in its own /blueprints mount on a schedule;
  # this triggers the reconciliation immediately so seeding is synchronous.
  curl -fsS -X POST \
    -H "Authorization: Bearer ${AUTHENTIK_BOOTSTRAP_TOKEN}" \
    -H "Content-Type: application/json" \
    "$AUTHENTIK_URL/api/v3/managed/blueprints/apply/" \
    -d '{"path": "weebo/weebo-notify.yaml"}' >/dev/null \
    && info "blueprint applied (alice, bob, carol, groups, OIDC provider)" \
    || info "blueprint apply endpoint refused — Authentik will pick it up on its next scan"
fi

printf '\n\033[1mSeed complete.\033[0m\n'
info "identity : task dev:oidc   -> http://localhost:9110"
info "server   : task dev:run    -> http://localhost:8080"
info "web      : task dev:web    -> http://localhost:5173"
info "log in as: alice / bob / carol, password ${DEV_USER_PASSWORD:-weebo-dev}"
