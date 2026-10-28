-- RFC 0004: who closed a notification by hand, as `revisions.author` names a
-- publisher. NULL for every earlier closure: a server's or a publisher's
-- revision, with nobody to name.
ALTER TABLE notifications ADD COLUMN closed_by JSONB;
-- A hand closure's fan-out row pushes the closure to Web Push devices, not the
-- notification.
ALTER TABLE fanout_outbox ADD COLUMN closure BOOLEAN NOT NULL DEFAULT false;
