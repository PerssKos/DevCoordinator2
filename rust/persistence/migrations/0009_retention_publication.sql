-- Completed generations expose bounded, indexed run summaries to storage.
ALTER TABLE test_log_retention_projection ADD COLUMN eligible BIGINT CHECK(eligible IN (0,1));
CREATE TABLE dc2_test_log_retention_heads (
 worktree_id TEXT PRIMARY KEY REFERENCES worktrees(worktree_id) ON DELETE CASCADE,
 generation TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('building','published')),
 max_age_seconds BIGINT NOT NULL CHECK(max_age_seconds>=1),
 case_depth BIGINT NOT NULL CHECK(case_depth>=1),
 published_at_ms BIGINT,
 CHECK((status='published')=(published_at_ms IS NOT NULL))
);
CREATE TABLE dc2_test_log_retention_runs (
 worktree_id TEXT NOT NULL REFERENCES worktrees(worktree_id) ON DELETE CASCADE,
 generation TEXT NOT NULL,
 run_id TEXT NOT NULL,
 total_count BIGINT NOT NULL CHECK(total_count>=0),
 checked_count BIGINT NOT NULL CHECK(checked_count BETWEEN 0 AND total_count),
 ineligible_count BIGINT NOT NULL CHECK(ineligible_count BETWEEN 0 AND checked_count),
 active_count BIGINT NOT NULL CHECK(active_count BETWEEN 0 AND total_count),
 payload_leaves BIGINT NOT NULL CHECK(payload_leaves BETWEEN 0 AND total_count),
 PRIMARY KEY(worktree_id,generation,run_id)
);
INSERT INTO dc2_test_log_retention_runs
 SELECT worktree_id,generation,run_id,count(*),count(eligible),count(*) FILTER(WHERE eligible=0),sum(active),count(*) FILTER(WHERE removed=0)
 FROM test_log_retention_projection GROUP BY worktree_id,generation,run_id;
CREATE FUNCTION dc2_retention_run_observation_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND (OLD.worktree_id,OLD.generation,OLD.run_id)=(NEW.worktree_id,NEW.generation,NEW.run_id) THEN
  UPDATE dc2_test_log_retention_runs SET
   checked_count=checked_count+(NEW.eligible IS NOT NULL)::int-(OLD.eligible IS NOT NULL)::int,
   ineligible_count=ineligible_count+(NEW.eligible=0 IS TRUE)::int-(OLD.eligible=0 IS TRUE)::int,
   active_count=active_count+NEW.active-OLD.active,
   payload_leaves=payload_leaves+(NEW.removed=0)::int-(OLD.removed=0)::int
  WHERE worktree_id=NEW.worktree_id AND generation=NEW.generation AND run_id=NEW.run_id;
  IF NOT FOUND THEN RAISE EXCEPTION 'retention run projection is inconsistent'; END IF;
  RETURN NEW;
 END IF;
 IF TG_OP IN ('UPDATE','DELETE') THEN
  UPDATE dc2_test_log_retention_runs SET total_count=total_count-1,
   checked_count=checked_count-(OLD.eligible IS NOT NULL)::int,
   ineligible_count=ineligible_count-(OLD.eligible=0 IS TRUE)::int,
   active_count=active_count-OLD.active,
   payload_leaves=payload_leaves-(OLD.removed=0)::int
  WHERE worktree_id=OLD.worktree_id AND generation=OLD.generation AND run_id=OLD.run_id;
  IF NOT FOUND AND EXISTS(SELECT 1 FROM worktrees WHERE worktree_id=OLD.worktree_id) THEN
   RAISE EXCEPTION 'retention run projection is inconsistent';
  END IF;
  DELETE FROM dc2_test_log_retention_runs WHERE worktree_id=OLD.worktree_id AND generation=OLD.generation AND run_id=OLD.run_id AND total_count=0;
 END IF;
 IF TG_OP IN ('INSERT','UPDATE') THEN
  INSERT INTO dc2_test_log_retention_runs VALUES(NEW.worktree_id,NEW.generation,NEW.run_id,1,(NEW.eligible IS NOT NULL)::int,(NEW.eligible=0 IS TRUE)::int,NEW.active,(NEW.removed=0)::int)
  ON CONFLICT(worktree_id,generation,run_id) DO UPDATE SET
   total_count=dc2_test_log_retention_runs.total_count+excluded.total_count,
   checked_count=dc2_test_log_retention_runs.checked_count+excluded.checked_count,
   ineligible_count=dc2_test_log_retention_runs.ineligible_count+excluded.ineligible_count,
   active_count=dc2_test_log_retention_runs.active_count+excluded.active_count,
   payload_leaves=dc2_test_log_retention_runs.payload_leaves+excluded.payload_leaves;
  RETURN NEW;
 END IF;
 RETURN OLD;
END $$;
CREATE TRIGGER dc2_retention_run_observation_change AFTER INSERT OR UPDATE OR DELETE ON test_log_retention_projection FOR EACH ROW EXECUTE FUNCTION dc2_retention_run_observation_changed();
CREATE INDEX dc2_retention_remaining_path ON test_log_retention_projection(worktree_id,generation,run_id,relative_path) WHERE removed=0;
CREATE INDEX dc2_retention_removed_path ON test_log_retention_projection(worktree_id,generation,relative_path) WHERE removed=1;
