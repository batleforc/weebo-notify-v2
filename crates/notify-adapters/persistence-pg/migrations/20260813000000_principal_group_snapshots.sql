-- Push fan-out (`delivery_targets`) can only route to a device whose owner has a
-- row in `channel_members` — the table `channels.upsert` fills only from
-- `[[channels.managed.access]]`'s *individual* bindings (`email`/`service_account`).
-- A subscriber who holds access purely through an `oidc_group` binding
-- (`channel_group_access`) has no such row, because nothing else in this system
-- persists which groups a principal belongs to: `channel.rs`'s `GroupBinding` is
-- deliberately resolved against the live token instead (design §11, "zero server
-- session"), so that a person removed from a group loses access on their next
-- token rather than whenever a reconciler next runs.
--
-- Push cannot make that promise: it has to reach someone who, by definition, has
-- no live token at that moment. This table is the one place that promise bends —
-- the groups a principal's token carried, last observed at a device enrolment or a
-- sync/stream session (crates/notify-app/src/devices.rs `EnrolDevice`,
-- crates/notify-app/src/sync.rs `SyncBacklog`) — so a push can still be routed
-- while the exact freshness of `channel_group_access` cannot.
--
-- Not written into `channel_members`: that table is wholesale-replaced by every
-- declarative reconcile pass (`channels.upsert`), and a row placed there for a
-- group-derived subscriber would be deleted within one reconcile cycle.
CREATE TABLE principal_group_snapshots (
    principal_ref  TEXT        PRIMARY KEY,
    groups         TEXT[]      NOT NULL,
    observed_at    TIMESTAMPTZ NOT NULL
);
