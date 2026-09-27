-- `has_actions` arrived with `DEFAULT false` and no backfill, so every notification
-- published before it reads as having no buttons: `?has_actions=true` hides it
-- until its next revision. The head revision's content is the source of truth, and
-- an empty action list is not serialised at all, so a non-empty array is the test.
UPDATE notifications n
   SET has_actions = true
  FROM revisions r
 WHERE r.notification_id = n.id
   AND r.seq = n.head_seq
   AND NOT n.has_actions
   AND jsonb_typeof(r.content->'actions') = 'array'
   AND jsonb_array_length(r.content->'actions') > 0;
