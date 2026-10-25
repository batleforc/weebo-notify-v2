//! Users, service accounts and API keys (design §5): three ports with three different writers.
//! A key's secret hash lives only in [`StoredApiKey`], which never reaches a response (T12).

use notify_domain::{
    ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, AttachmentId, ChannelId, Origin, Principal,
    Profile, ProfileField, Scope, ServiceAccount, ServiceAccountId, User, UserId,
};
use notify_ports::{
    ApiKeyRepository, EmailMatch, Page, PageRequest, PortResult, ServiceAccountRepository,
    StoredApiKey, UserRepository,
};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{
    cursor, enum_from_text, enum_to_text, fail, from_json, parse, sequence, to_json,
};

// --- Users -----------------------------------------------------------------

#[async_trait::async_trait]
impl UserRepository for PgStore {
    async fn get(&self, id: UserId) -> PortResult<Option<User>> {
        self.one_user("WHERE id = $1", &id.to_string()).await
    }

    async fn get_many(&self, ids: &[UserId]) -> PortResult<Vec<User>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let rows = sqlx::query(
            "SELECT id, oidc_subject, email, display_name, timezone, locale, \
             overridden_fields, avatar, created_at FROM users WHERE id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("user", error))?;
        rows.iter().map(hydrate_user).collect()
    }

    async fn find_by_subject(&self, subject: &str) -> PortResult<Option<User>> {
        // The lookup every authenticated request makes, hence the unique index.
        self.one_user("WHERE oidc_subject = $1", subject).await
    }

    async fn find_by_email(&self, email: &str) -> PortResult<EmailMatch> {
        // Case-insensitive, so an IdP changing capitalisation does not orphan a binding.
        // `LIMIT 2`: the column is not unique, and two already means ambiguous.
        let rows = sqlx::query(
            "SELECT id, oidc_subject, email, display_name, timezone, locale, \
             overridden_fields, avatar, created_at FROM users \
             WHERE lower(email) = lower($1) LIMIT 2",
        )
        .bind(email)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("user", error))?;

        Ok(match rows.as_slice() {
            [] => EmailMatch::Nobody,
            [row] => EmailMatch::One(Box::new(hydrate_user(row)?)),
            _ => EmailMatch::Ambiguous,
        })
    }

    async fn is_avatar(&self, avatar: AttachmentId) -> PortResult<bool> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE avatar = $1)")
            .bind(avatar.to_string())
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("user", error))
    }

    async fn lookup(&self, text: &str, limit: u32) -> PortResult<Vec<User>> {
        // `%`/`_` in the input are matched literally.
        let pattern = format!(
            "%{}%",
            text.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows = sqlx::query(
            "SELECT id, oidc_subject, email, display_name, timezone, locale, \
             overridden_fields, avatar, created_at FROM users \
             WHERE display_name ILIKE $1 OR email ILIKE $1 \
             ORDER BY lower(coalesce(display_name, email, id)) LIMIT $2",
        )
        .bind(pattern)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("user", error))?;
        rows.iter().map(hydrate_user).collect()
    }

    async fn upsert(&self, user: &User) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO users
                 (id, oidc_subject, email, display_name, timezone, locale,
                  overridden_fields, avatar, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE
               SET oidc_subject      = EXCLUDED.oidc_subject,
                   email             = EXCLUDED.email,
                   display_name      = EXCLUDED.display_name,
                   timezone          = EXCLUDED.timezone,
                   locale            = EXCLUDED.locale,
                   overridden_fields = EXCLUDED.overridden_fields",
        )
        .bind(user.id.to_string())
        .bind(&user.oidc_subject)
        .bind(user.email.as_deref())
        .bind(user.display_name.as_deref())
        .bind(user.profile.timezone.as_deref())
        .bind(user.profile.locale.as_deref())
        .bind(overridden(user))
        .bind(user.profile.avatar.map(|id| id.to_string()))
        .bind(user.created_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("user", error))
        .map(|_| ())
    }

    async fn replace(&self, before: &User, after: &User) -> PortResult<bool> {
        let replaced = sqlx::query(
            "UPDATE users
                SET oidc_subject = $2, email = $3, display_name = $4, timezone = $5,
                    locale = $6, overridden_fields = $7
              WHERE id = $1
                AND (oidc_subject, email, display_name, timezone, locale, overridden_fields)
                    IS NOT DISTINCT FROM ($8, $9, $10, $11, $12, $13)",
        )
        .bind(after.id.to_string())
        .bind(&after.oidc_subject)
        .bind(after.email.as_deref())
        .bind(after.display_name.as_deref())
        .bind(after.profile.timezone.as_deref())
        .bind(after.profile.locale.as_deref())
        .bind(overridden(after))
        .bind(&before.oidc_subject)
        .bind(before.email.as_deref())
        .bind(before.display_name.as_deref())
        .bind(before.profile.timezone.as_deref())
        .bind(before.profile.locale.as_deref())
        .bind(overridden(before))
        .execute(self.pool())
        .await
        .map_err(|error| fail("user", error))?;

        Ok(replaced.rows_affected() == 1)
    }

    async fn set_avatar(
        &self,
        user: UserId,
        avatar: AttachmentId,
    ) -> PortResult<Option<AttachmentId>> {
        // `previous` is read before the row changes, under its lock: a concurrent upload
        // waits, then reads our value as the one it replaces.
        let previous: Option<(Option<String>,)> = sqlx::query_as(
            "UPDATE users u SET avatar = $2
             FROM (SELECT id, avatar FROM users WHERE id = $1 FOR UPDATE) previous
             WHERE u.id = previous.id
             RETURNING previous.avatar",
        )
        .bind(user.to_string())
        .bind(avatar.to_string())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("user", error))?;

        previous
            .and_then(|(previous,)| previous)
            .map(|id| parse("user.avatar", &id, str::parse::<AttachmentId>))
            .transpose()
    }
}

impl PgStore {
    /// The unique-key user lookups differ only in their one-text-argument `WHERE`; email is not unique.
    async fn one_user(&self, filter: &'static str, argument: &str) -> PortResult<Option<User>> {
        let sql = [
            "SELECT id, oidc_subject, email, display_name, timezone, locale, \
             overridden_fields, avatar, created_at FROM users ",
            filter,
        ]
        .concat();

        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(argument)
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("user", error))?;

        row.as_ref().map(hydrate_user).transpose()
    }
}

fn hydrate_user(row: &PgRow) -> PortResult<User> {
    let id: String = row.try_get("id").map_err(|e| fail("user", e))?;
    let display_name: Option<String> = row.try_get("display_name").map_err(|e| fail("user", e))?;
    let overridden: Vec<String> = row
        .try_get("overridden_fields")
        .map_err(|e| fail("user", e))?;

    Ok(User {
        id: parse("user.id", &id, str::parse::<UserId>)?,
        oidc_subject: row.try_get("oidc_subject").map_err(|e| fail("user", e))?,
        email: row.try_get("email").map_err(|e| fail("user", e))?,
        profile: Profile {
            display_name: display_name.clone(),
            timezone: row.try_get("timezone").map_err(|e| fail("user", e))?,
            locale: row.try_get("locale").map_err(|e| fail("user", e))?,
            avatar: row
                .try_get::<Option<String>, _>("avatar")
                .map_err(|e| fail("user", e))?
                .map(|raw| parse("user.avatar", &raw, str::parse::<AttachmentId>))
                .transpose()?,
            overridden: overridden
                .iter()
                .filter_map(|name| field_of(name))
                .collect(),
        },
        display_name,
        created_at: row.try_get("created_at").map_err(|e| fail("user", e))?,
    })
}

/// The wire name of a claimed field, and the only place it is spelled.
const fn field_name(field: ProfileField) -> &'static str {
    match field {
        ProfileField::DisplayName => "display_name",
        ProfileField::Timezone => "timezone",
        ProfileField::Locale => "locale",
    }
}

/// The reverse, dropping names this build does not know. Caveat: an older replica's next write then
/// erases a newer field, so new profile fields ship read-only one release before being written.
fn field_of(name: &str) -> Option<ProfileField> {
    match name {
        "display_name" => Some(ProfileField::DisplayName),
        "timezone" => Some(ProfileField::Timezone),
        "locale" => Some(ProfileField::Locale),
        unknown => {
            tracing::debug!(field = unknown, "ignoring an unknown claimed profile field");
            None
        }
    }
}

// --- Service accounts ------------------------------------------------------

/// The locally edited fields, as stored.
fn overridden(user: &User) -> Vec<String> {
    user.profile
        .overridden
        .iter()
        .map(|field| field_name(*field).to_owned())
        .collect()
}

#[async_trait::async_trait]
impl ServiceAccountRepository for PgStore {
    async fn get(&self, id: &ServiceAccountId) -> PortResult<Option<ServiceAccount>> {
        let row = sqlx::query(
            "SELECT id, display_name, description, origin, created_by, created_at, disabled_at
             FROM service_accounts WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("service_account", error))?;

        row.as_ref().map(hydrate_service_account).transpose()
    }

    async fn list(&self) -> PortResult<Vec<ServiceAccount>> {
        let rows = sqlx::query(
            "SELECT id, display_name, description, origin, created_by, created_at, disabled_at
             FROM service_accounts ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("service_account", error))?;

        rows.iter().map(hydrate_service_account).collect()
    }

    async fn list_page(&self, page: PageRequest) -> PortResult<Page<ServiceAccount>> {
        let limit = page.effective_limit();
        // One more than asked for: the extra row says whether a next page exists.
        let mut rows = sqlx::query(
            "SELECT id, display_name, description, origin, created_by, created_at, disabled_at, seq
             FROM service_accounts
             WHERE ($1::bigint IS NULL OR seq > $1)
             ORDER BY seq
             LIMIT $2",
        )
        .bind(page.after.map(sequence))
        .bind(i64::from(limit).saturating_add(1))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("service_account", error))?;

        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next = match rows.last().filter(|_| has_more) {
            Some(row) => Some(cursor(
                row.try_get::<i64, _>("seq")
                    .map_err(|error| fail("service_account", error))?,
            )),
            None => None,
        };
        let accounts = rows
            .iter()
            .map(hydrate_service_account)
            .collect::<PortResult<Vec<_>>>()?;

        Ok(match next {
            Some(next) => Page::more(accounts, next),
            None => Page::last(accounts),
        })
    }

    async fn upsert(&self, account: &ServiceAccount) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO service_accounts
                 (id, display_name, description, origin, created_by, created_at, disabled_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE
               SET display_name = EXCLUDED.display_name,
                   description  = EXCLUDED.description,
                   origin       = EXCLUDED.origin,
                   disabled_at  = EXCLUDED.disabled_at",
            // `created_by` is set once at creation; the reconciler must not overwrite it.
        )
        .bind(account.id.as_str())
        .bind(account.display_name.as_deref())
        .bind(account.description.as_deref())
        .bind(enum_to_text("service_account.origin", &account.origin)?)
        .bind(account.created_by.map(|id| id.to_string()))
        .bind(account.created_at)
        .bind(account.disabled_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("service_account", error))
        .map(|_| ())
    }

    async fn list_managed(&self) -> PortResult<Vec<ServiceAccount>> {
        let rows = sqlx::query(
            "SELECT id, display_name, description, origin, created_by, created_at, disabled_at
             FROM service_accounts WHERE origin = 'managed' ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("service_account", error))?;

        rows.iter().map(hydrate_service_account).collect()
    }
}

fn hydrate_service_account(row: &PgRow) -> PortResult<ServiceAccount> {
    let id: String = row.try_get("id").map_err(|e| fail("service_account", e))?;
    let origin: String = row
        .try_get("origin")
        .map_err(|e| fail("service_account", e))?;

    let created_by: Option<String> = row
        .try_get("created_by")
        .map_err(|e| fail("service_account", e))?;

    Ok(ServiceAccount {
        created_by: created_by
            .map(|raw| parse("service_account.created_by", &raw, str::parse::<UserId>))
            .transpose()?,
        id: parse("service_account.id", &id, ServiceAccountId::parse)?,
        display_name: row
            .try_get("display_name")
            .map_err(|e| fail("service_account", e))?,
        description: row
            .try_get("description")
            .map_err(|e| fail("service_account", e))?,
        origin: enum_from_text::<Origin>("service_account.origin", &origin)?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| fail("service_account", e))?,
        disabled_at: row
            .try_get("disabled_at")
            .map_err(|e| fail("service_account", e))?,
    })
}

// --- API keys --------------------------------------------------------------

/// Every column of a key, secret hash included. Only [`ApiKeyRepository::find_by_prefix`]
/// asks for the hash; everything else goes through [`SELECT_KEY`].
const SELECT_STORED_KEY: &str = "
    SELECT id, prefix, secret_hash, label, owner, scopes, channels, publish_per_minute,
           created_at, expires_at, last_used_at, revoked_at
    FROM api_keys ";

/// The public half.
const SELECT_KEY: &str = "
    SELECT id, prefix, NULL AS secret_hash, label, owner, scopes, channels,
           publish_per_minute, created_at, expires_at, last_used_at, revoked_at
    FROM api_keys ";

#[async_trait::async_trait]
impl ApiKeyRepository for PgStore {
    async fn find_by_prefix(&self, prefix: &ApiKeyPrefix) -> PortResult<Option<StoredApiKey>> {
        // One indexed read; without the prefix, every key would need an Argon2 comparison.
        let sql = [SELECT_STORED_KEY, "WHERE prefix = $1"].concat();
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(prefix.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("api_key", error))?;

        let Some(row) = row else { return Ok(None) };
        let secret_hash: Option<String> =
            row.try_get("secret_hash").map_err(|e| fail("api_key", e))?;

        Ok(Some(StoredApiKey {
            key: hydrate_key(&row)?,
            secret_hash: secret_hash.unwrap_or_default(),
        }))
    }

    async fn get(&self, id: ApiKeyId) -> PortResult<Option<ApiKey>> {
        let sql = [SELECT_KEY, "WHERE id = $1"].concat();
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("api_key", error))?;

        row.as_ref().map(hydrate_key).transpose()
    }

    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<ApiKey>> {
        let sql = [SELECT_KEY, "WHERE owner_ref = $1 ORDER BY created_at DESC"].concat();
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(owner.reference())
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("api_key", error))?;

        rows.iter().map(hydrate_key).collect()
    }

    async fn list_for_channel(&self, channel: &ChannelId) -> PortResult<Vec<ApiKey>> {
        let sql = [
            SELECT_KEY,
            "WHERE owner->>'owner' = 'channel' AND owner->>'id' = $1 ORDER BY created_at DESC",
        ]
        .concat();
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(channel.as_str())
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("api_key", error))?;

        rows.iter().map(hydrate_key).collect()
    }

    async fn insert(&self, stored: &StoredApiKey) -> PortResult<()> {
        let key = &stored.key;
        let scopes = key
            .scopes
            .iter()
            .map(|scope| enum_to_text("scope", scope))
            .collect::<PortResult<Vec<_>>>()?;
        let channels: Vec<String> = key
            .channels
            .iter()
            .map(|channel| channel.as_str().to_owned())
            .collect();

        sqlx::query(
            "INSERT INTO api_keys
                 (id, prefix, secret_hash, label, owner_ref, owner, scopes, channels,
                  publish_per_minute, created_at, expires_at, last_used_at, revoked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(key.id.to_string())
        .bind(key.prefix.as_str())
        .bind(&stored.secret_hash)
        .bind(key.label.as_deref())
        .bind(owner_reference(key))
        .bind(to_json("api_key.owner", &key.owner)?)
        .bind(&scopes)
        .bind(&channels)
        // Never set at creation (§8.2), but bound so every field `hydrate_key` reads is named.
        .bind(key.publish_per_minute.map(i64::from))
        .bind(key.created_at)
        .bind(key.expires_at)
        .bind(key.last_used_at)
        .bind(key.revoked_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("api_key.prefix", error))
        .map(|_| ())
    }

    async fn set_publish_limit(
        &self,
        id: ApiKeyId,
        publish_per_minute: Option<u32>,
    ) -> PortResult<()> {
        sqlx::query("UPDATE api_keys SET publish_per_minute = $2 WHERE id = $1")
            .bind(id.to_string())
            .bind(publish_per_minute.map(i64::from))
            .execute(self.pool())
            .await
            .map_err(|error| fail("api_key", error))
            .map(|_| ())
    }

    async fn touch(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<()> {
        // Best-effort: on every authenticated request, so no transaction.
        sqlx::query("UPDATE api_keys SET last_used_at = $2 WHERE id = $1")
            .bind(id.to_string())
            .bind(at)
            .execute(self.pool())
            .await
            .map_err(|error| fail("api_key", error))
            .map(|_| ())
    }

    async fn revoke(&self, id: ApiKeyId, at: OffsetDateTime) -> PortResult<bool> {
        let revoked =
            sqlx::query("UPDATE api_keys SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
                .bind(id.to_string())
                .bind(at)
                .execute(self.pool())
                .await
                .map_err(|error| fail("api_key", error))?;

        Ok(revoked.rows_affected() > 0)
    }

    async fn revoke_all_for(&self, owner: &Principal, at: OffsetDateTime) -> PortResult<u64> {
        // Every key in one statement, so none created in between survives (T25).
        let revoked = sqlx::query(
            "UPDATE api_keys SET revoked_at = $2 WHERE owner_ref = $1 AND revoked_at IS NULL",
        )
        .bind(owner.reference())
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("api_key", error))?;

        Ok(revoked.rows_affected())
    }
}

/// The `owner_ref` a key indexes under, built via [`ApiKeyOwner::principal`] to match
/// [`Principal::reference`]; a channel key references itself (§5.2).
fn owner_reference(key: &ApiKey) -> String {
    key.owner.principal(key.id).reference()
}

fn hydrate_key(row: &PgRow) -> PortResult<ApiKey> {
    let id: String = row.try_get("id").map_err(|e| fail("api_key", e))?;
    let prefix: String = row.try_get("prefix").map_err(|e| fail("api_key", e))?;
    let owner: serde_json::Value = row.try_get("owner").map_err(|e| fail("api_key", e))?;
    let scopes: Vec<String> = row.try_get("scopes").map_err(|e| fail("api_key", e))?;
    let channels: Vec<String> = row.try_get("channels").map_err(|e| fail("api_key", e))?;
    let publish_per_minute: Option<i64> = row
        .try_get("publish_per_minute")
        .map_err(|e| fail("api_key", e))?;

    Ok(ApiKey {
        id: parse("api_key.id", &id, str::parse::<ApiKeyId>)?,
        prefix: parse("api_key.prefix", &prefix, ApiKeyPrefix::parse)?,
        label: row.try_get("label").map_err(|e| fail("api_key", e))?,
        owner: from_json::<ApiKeyOwner>("api_key.owner", owner)?,
        scopes: scopes
            .iter()
            .map(|scope| enum_from_text::<Scope>("scope", scope))
            .collect::<PortResult<Vec<_>>>()?,
        channels: channels
            .iter()
            .map(|channel| parse("api_key.channel", channel, ChannelId::parse))
            .collect::<PortResult<Vec<_>>>()?,
        // Stored as `BIGINT`, so a value from `set_publish_limit`'s `u32` always fits.
        publish_per_minute: publish_per_minute.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        created_at: row.try_get("created_at").map_err(|e| fail("api_key", e))?,
        expires_at: row.try_get("expires_at").map_err(|e| fail("api_key", e))?,
        last_used_at: row
            .try_get("last_used_at")
            .map_err(|e| fail("api_key", e))?,
        revoked_at: row.try_get("revoked_at").map_err(|e| fail("api_key", e))?,
    })
}
