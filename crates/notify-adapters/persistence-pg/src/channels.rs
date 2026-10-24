//! Channels, their members and their policy. Membership is the only door to content (§5.4), so
//! `subscribed_ids` is deliberately unpaginated: a scope filter must be complete.

use notify_domain::{
    Channel, ChannelId, ChannelPolicy, GroupBinding, Membership, Origin, Principal, Scope,
};
use notify_ports::{ChannelRepository, Page, PageRequest, PortResult};
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::PgStore;
use crate::mapping::{cursor, enum_from_text, enum_to_text, fail, from_json, sequence, to_json};

/// The two doors (member or group binding) as one predicate over `channels c`: `$1` principal,
/// `$2` token groups, `$3` scope. `EXISTS`, not `JOIN`, so a double-door caller is not duplicated;
/// a macro so the query stays a `&'static str`.
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
        .bind(i64::from(limit).saturating_add(1))
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
        // Ids only: stream and backlog need the set, not the bodies.
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
        // No `ON CONFLICT`: the primary key refusing a second creator becomes `Conflict`.
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
        // Ids only, for pattern matching (§5.3).
        let rows = sqlx::query_scalar::<_, String>("SELECT id FROM channels ORDER BY id")
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("channel", error))?;

        rows.iter()
            .map(|id| crate::mapping::parse("channel.id", id, ChannelId::parse))
            .collect()
    }

    async fn all_policies(&self) -> PortResult<Vec<(ChannelId, ChannelPolicy)>> {
        // Only `close_after`, for the stale-closure sweep (§7.2bis).
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

        // Replaced wholesale: the caller always passes the complete list.
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

        // Same for group bindings, so removing an `oidc_group` removes the access.
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
