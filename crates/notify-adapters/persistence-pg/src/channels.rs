//! Channels, their members and their policy.
//!
//! Membership is the only door to content (design §5.4), so
//! [`ChannelRepository::subscribed_ids`] is the query almost everything else hangs
//! off — the inbox scope, the stream filter, the search scope. It is one indexed
//! read on `channel_members`, deliberately unpaginated: a filter that has to be
//! complete to be correct should not offer a first page.

use notify_domain::{
    Channel, ChannelId, ChannelPolicy, GroupBinding, Membership, Origin, Principal, Scope,
};
use notify_ports::{ChannelRepository, Page, PageRequest, PortResult};
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::PgStore;
use crate::mapping::{cursor, enum_from_text, enum_to_text, fail, from_json, sequence, to_json};

/// The two doors to a channel, as one predicate over `channels c`.
///
/// `$1` is the principal reference, `$2` the groups their token carried, `$3` the
/// scope. Written once because a read that checked memberships and forgot group
/// bindings would show an empty inbox to everybody whose access is group-based —
/// and that failure looks like "the database is empty", not like a missing join.
///
/// `EXISTS` rather than a `JOIN`: a caller who is both a named member and in a
/// bound group would otherwise get the channel twice, and the pagination cursor
/// would skip a row every time it happened.
///
/// A macro rather than a `const`, so the result is still one `&'static str`: sqlx
/// refuses a runtime-built query string, and it is right to.
macro_rules! scoped_by_access {
    ($($tail:literal),* $(,)?) => {
        concat!(
            "SELECT c.id, c.display_name, c.origin, c.policy, c.seq
             FROM channels c
             WHERE (
                EXISTS (
                    SELECT 1 FROM channel_members m
                    WHERE m.channel_id = c.id AND m.principal_ref = $1
                      AND $3 = ANY (m.scopes)
                )
                OR EXISTS (
                    SELECT 1 FROM channel_group_access g
                    WHERE g.channel_id = c.id AND g.oidc_group = ANY ($2)
                      AND $3 = ANY (g.scopes)
                )
            ) ",
            $($tail),*
        )
    };
}

#[async_trait::async_trait]
impl ChannelRepository for PgStore {
    async fn get(&self, id: &ChannelId) -> PortResult<Option<Channel>> {
        let Some(row) =
            sqlx::query("SELECT id, display_name, origin, policy, seq FROM channels WHERE id = $1")
                .bind(id.as_str())
                .fetch_optional(self.pool())
                .await
                .map_err(|error| fail("channel", error))?
        else {
            return Ok(None);
        };

        self.hydrate(row).await.map(Some)
    }

    async fn list_for(
        &self,
        principal: &Principal,
        groups: &[String],
        scope: Scope,
        page: PageRequest,
    ) -> PortResult<Page<Channel>> {
        let limit = page.effective_limit();
        let rows = sqlx::query(scoped_by_access!(
            "AND ($4::bigint IS NULL OR c.seq > $4)
             ORDER BY c.seq
             -- One more than asked for: the extra row is how we know whether to
             -- hand back a `next` cursor, without a second COUNT query.
             LIMIT $5"
        ))
        .bind(principal.reference())
        .bind(groups)
        .bind(enum_to_text("scope", &scope)?)
        .bind(page.after.map(sequence))
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("channel", error))?;

        self.paginate(rows, limit).await
    }

    async fn subscribed_ids(
        &self,
        principal: &Principal,
        groups: &[String],
    ) -> PortResult<Vec<ChannelId>> {
        // The same predicate, but only the id: the stream and the sync backlog want
        // the whole set on every reconnect and never the channel bodies.
        let rows: Vec<(String,)> = sqlx::query_as(scoped_by_access!("ORDER BY c.id"))
            .bind(principal.reference())
            .bind(groups)
            .bind("subscribe")
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("channel", error))?;
        let rows = rows.into_iter().map(|(id,)| id);

        rows.map(|id| crate::mapping::parse("channel.id", &id, ChannelId::parse))
            .collect()
    }

    async fn upsert(&self, channel: &Channel) -> PortResult<()> {
        self.write_channel(
            channel,
            "INSERT INTO channels (id, display_name, origin, policy)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
               SET display_name = EXCLUDED.display_name,
                   origin       = EXCLUDED.origin,
                   policy       = EXCLUDED.policy",
        )
        .await
    }

    async fn insert(&self, channel: &Channel) -> PortResult<()> {
        // No `ON CONFLICT`: the primary key refusing a second creator is the whole
        // point, and `fail` turns that refusal into `Conflict`.
        self.write_channel(
            channel,
            "INSERT INTO channels (id, display_name, origin, policy) VALUES ($1, $2, $3, $4)",
        )
        .await
    }

    async fn put_member(&self, id: &ChannelId, membership: &Membership) -> PortResult<()> {
        let scopes = membership
            .scopes
            .iter()
            .map(|scope| enum_to_text("scope", scope))
            .collect::<PortResult<Vec<_>>>()?;

        sqlx::query(
            "INSERT INTO channel_members (channel_id, principal_ref, principal, scopes)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (channel_id, principal_ref) DO UPDATE
               SET principal = EXCLUDED.principal,
                   scopes    = EXCLUDED.scopes",
        )
        .bind(id.as_str())
        .bind(membership.principal.reference())
        .bind(to_json("channel.member", &membership.principal)?)
        .bind(&scopes)
        .execute(self.pool())
        .await
        .map_err(|error| fail("channel.member", error))?;

        Ok(())
    }

    async fn remove_member(&self, id: &ChannelId, principal: &Principal) -> PortResult<bool> {
        let removed =
            sqlx::query("DELETE FROM channel_members WHERE channel_id = $1 AND principal_ref = $2")
                .bind(id.as_str())
                .bind(principal.reference())
                .execute(self.pool())
                .await
                .map_err(|error| fail("channel.member", error))?;

        Ok(removed.rows_affected() > 0)
    }

    async fn set_policy(&self, id: &ChannelId, policy: &ChannelPolicy) -> PortResult<()> {
        sqlx::query("UPDATE channels SET policy = $2 WHERE id = $1")
            .bind(id.as_str())
            .bind(to_json("channel.policy", policy)?)
            .execute(self.pool())
            .await
            .map_err(|error| fail("channel", error))?;

        Ok(())
    }

    async fn delete(&self, id: &ChannelId) -> PortResult<bool> {
        let deleted = sqlx::query("DELETE FROM channels WHERE id = $1")
            .bind(id.as_str())
            .execute(self.pool())
            .await
            .map_err(|error| fail("channel", error))?;

        Ok(deleted.rows_affected() > 0)
    }

    async fn list_managed(&self) -> PortResult<Vec<Channel>> {
        let rows = sqlx::query(
            "SELECT id, display_name, origin, policy, seq
             FROM channels WHERE origin = 'managed' ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("channel", error))?;

        let mut channels = Vec::with_capacity(rows.len());
        for row in rows {
            channels.push(self.hydrate(row).await?);
        }
        Ok(channels)
    }

    async fn all_ids(&self) -> PortResult<Vec<ChannelId>> {
        // Ids straight off the index, with no member fan-out: the one caller wants
        // to match patterns against names (design §5.3).
        let rows = sqlx::query_scalar::<_, String>("SELECT id FROM channels ORDER BY id")
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("channel", error))?;

        rows.iter()
            .map(|id| crate::mapping::parse("channel.id", id, ChannelId::parse))
            .collect()
    }

    async fn all_policies(&self) -> PortResult<Vec<(ChannelId, ChannelPolicy)>> {
        // Two columns and no member fan-out, same as `all_ids` above: the caller is
        // the stale-closure sweep, which needs `close_after` and nothing else about
        // a channel (design §7.2bis).
        let rows: Vec<(String, serde_json::Value)> =
            sqlx::query_as("SELECT id, policy FROM channels ORDER BY id")
                .fetch_all(self.pool())
                .await
                .map_err(|error| fail("channel", error))?;

        rows.into_iter()
            .map(|(id, policy)| {
                Ok((
                    crate::mapping::parse("channel.id", &id, ChannelId::parse)?,
                    crate::mapping::from_json::<ChannelPolicy>("channel.policy", policy)?,
                ))
            })
            .collect()
    }
}

impl PgStore {
    /// Turn a channel row plus its members into a domain [`Channel`].
    /// Write a channel row with `statement`, then its members and group access.
    async fn write_channel(&self, channel: &Channel, statement: &'static str) -> PortResult<()> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("channel", error))?;

        sqlx::query(statement)
            .bind(channel.id.as_str())
            .bind(channel.display_name.as_deref())
            .bind(enum_to_text("channel.origin", &channel.origin)?)
            .bind(to_json("channel.policy", &channel.policy)?)
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("channel.id", error))?;

        // Members are replaced wholesale rather than diffed: the caller always
        // hands over the complete list, and a diff would be a second place for
        // "who is in this channel" to be decided.
        sqlx::query("DELETE FROM channel_members WHERE channel_id = $1")
            .bind(channel.id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("channel.member", error))?;

        for member in &channel.members {
            let scopes = member
                .scopes
                .iter()
                .map(|scope| enum_to_text("scope", scope))
                .collect::<PortResult<Vec<_>>>()?;

            sqlx::query(
                "INSERT INTO channel_members (channel_id, principal_ref, principal, scopes)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (channel_id, principal_ref) DO UPDATE
                   SET principal = EXCLUDED.principal,
                       scopes    = EXCLUDED.scopes",
            )
            .bind(channel.id.as_str())
            .bind(member.principal.reference())
            .bind(to_json("channel.member", &member.principal)?)
            .bind(&scopes)
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("channel.member", error))?;
        }

        // Group bindings, same rule: replaced wholesale, so removing an
        // `oidc_group` line from the config actually removes the access.
        sqlx::query("DELETE FROM channel_group_access WHERE channel_id = $1")
            .bind(channel.id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("channel.group", error))?;

        for binding in &channel.group_access {
            let scopes = binding
                .scopes
                .iter()
                .map(|scope| enum_to_text("scope", scope))
                .collect::<PortResult<Vec<_>>>()?;

            sqlx::query(
                "INSERT INTO channel_group_access (channel_id, oidc_group, scopes)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (channel_id, oidc_group) DO UPDATE
                   SET scopes = EXCLUDED.scopes",
            )
            .bind(channel.id.as_str())
            .bind(&binding.group)
            .bind(&scopes)
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("channel.group", error))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| fail("channel", error))
    }

    async fn hydrate(&self, row: PgRow) -> PortResult<Channel> {
        let id: String = row.try_get("id").map_err(|e| fail("channel", e))?;
        let id = crate::mapping::parse("channel.id", &id, ChannelId::parse)?;

        let members = sqlx::query(
            "SELECT principal, scopes FROM channel_members
             WHERE channel_id = $1 ORDER BY principal_ref",
        )
        .bind(id.as_str())
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("channel.member", error))?;

        let members = members
            .into_iter()
            .map(|member| {
                let principal: serde_json::Value = member
                    .try_get("principal")
                    .map_err(|e| fail("channel.member", e))?;
                let scopes: Vec<String> = member
                    .try_get("scopes")
                    .map_err(|e| fail("channel.member", e))?;

                Ok(Membership {
                    principal: from_json("channel.member.principal", principal)?,
                    scopes: scopes
                        .iter()
                        .map(|scope| enum_from_text("scope", scope))
                        .collect::<PortResult<Vec<Scope>>>()?,
                })
            })
            .collect::<PortResult<Vec<_>>>()?;

        let bindings = sqlx::query(
            "SELECT oidc_group, scopes FROM channel_group_access
             WHERE channel_id = $1 ORDER BY oidc_group",
        )
        .bind(id.as_str())
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("channel.group", error))?;

        let group_access = bindings
            .into_iter()
            .map(|binding| {
                let group: String = binding
                    .try_get("oidc_group")
                    .map_err(|e| fail("channel.group", e))?;
                let scopes: Vec<String> = binding
                    .try_get("scopes")
                    .map_err(|e| fail("channel.group", e))?;

                Ok(GroupBinding {
                    group,
                    scopes: scopes
                        .iter()
                        .map(|scope| enum_from_text("scope", scope))
                        .collect::<PortResult<Vec<Scope>>>()?,
                })
            })
            .collect::<PortResult<Vec<_>>>()?;

        let origin: String = row.try_get("origin").map_err(|e| fail("channel", e))?;
        let policy: serde_json::Value = row.try_get("policy").map_err(|e| fail("channel", e))?;

        Ok(Channel {
            id,
            display_name: row
                .try_get("display_name")
                .map_err(|e| fail("channel", e))?,
            origin: enum_from_text::<Origin>("channel.origin", &origin)?,
            policy: from_json::<ChannelPolicy>("channel.policy", policy)?,
            members,
            group_access,
        })
    }

    /// Take the one extra row off the end and turn it into the `next` cursor.
    async fn paginate(&self, mut rows: Vec<PgRow>, limit: u32) -> PortResult<Page<Channel>> {
        let has_more = rows.len() > limit as usize;
        rows.truncate(limit as usize);

        let next = has_more
            .then(|| rows.last().map(|row| row.try_get::<i64, _>("seq")))
            .flatten()
            .transpose()
            .map_err(|error| fail("channel", error))?
            .map(cursor);

        let mut channels = Vec::with_capacity(rows.len());
        for row in rows {
            channels.push(self.hydrate(row).await?);
        }

        Ok(match next {
            Some(next) => Page::more(channels, next),
            None => Page::last(channels),
        })
    }
}
