-- Which devices a fan-out has already pushed a revision to (ADR-0025).
--
-- An outbox row that is retried — its worker died, or it ran past its ceiling —
-- used to push every device again. The drain now records each successful push here
-- and skips those devices on the retry, so only the devices still owed hear about
-- it. Keyed by the revision pushed (the head when the worker read it), not by the
-- outbox row: a later revision is new content and goes to every device again.
--
-- Rows go when the notification's last outbox row is completed, and with the
-- notification itself (the cascade) for whatever a dead outbox row left behind.
CREATE TABLE fanout_deliveries (
    notification_id TEXT    NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    device_id       TEXT    NOT NULL,
    PRIMARY KEY (notification_id, seq, device_id)
);
