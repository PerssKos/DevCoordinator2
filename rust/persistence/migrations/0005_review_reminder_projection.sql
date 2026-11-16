ALTER TABLE review_policies ADD COLUMN next_reminder_at_ms BIGINT;
CREATE INDEX review_policies_due ON review_policies(next_reminder_at_ms,repository_id,workstream_key)
  WHERE active=1 AND next_reminder_at_ms IS NOT NULL;

CREATE FUNCTION dc2_rebuild_review_reminder_projection() RETURNS void LANGUAGE SQL AS $$
  UPDATE review_policies p SET next_reminder_at_ms=CASE
    WHEN p.active<>1 THEN NULL
    WHEN NOT EXISTS(SELECT 1 FROM review_reminders r
      WHERE r.repository_id=p.repository_id AND r.workstream_key=p.workstream_key
        AND r.window_start_ms=p.window_start_ms AND r.window_end_ms=p.window_end_ms AND r.escalation=0)
      THEN p.window_end_ms
    WHEN NOT EXISTS(SELECT 1 FROM review_reminders r
      WHERE r.repository_id=p.repository_id AND r.workstream_key=p.workstream_key
        AND r.window_start_ms=p.window_start_ms AND r.window_end_ms=p.window_end_ms AND r.escalation=1)
      THEN p.window_end_ms+p.escalation_ms
    ELSE NULL END;
$$;
SELECT dc2_rebuild_review_reminder_projection();
