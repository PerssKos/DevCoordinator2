-- Rebuildable bounded-walker observations, never authority log contents.
CREATE TABLE test_log_retention_projection (
  worktree_id TEXT NOT NULL REFERENCES worktrees(worktree_id) ON DELETE CASCADE,
  generation TEXT NOT NULL,
  relative_path TEXT NOT NULL,
  run_id TEXT NOT NULL,
  history_key TEXT NOT NULL,
  finished_at_ms BIGINT,
  active BIGINT NOT NULL CHECK(active IN (0,1)),
  location_json JSONB NOT NULL,
  removed BIGINT NOT NULL DEFAULT 0 CHECK(removed IN (0,1)),
  PRIMARY KEY(worktree_id,relative_path)
);
CREATE INDEX test_log_retention_projection_history
  ON test_log_retention_projection(worktree_id,generation,history_key,finished_at_ms DESC,run_id DESC,relative_path DESC)
  WHERE active=0;
CREATE INDEX test_log_retention_projection_generation
  ON test_log_retention_projection(worktree_id,generation,relative_path);
CREATE INDEX test_log_retention_projection_run
  ON test_log_retention_projection(worktree_id,generation,run_id);
CREATE INDEX test_log_retention_projection_remaining
  ON test_log_retention_projection(worktree_id,generation,run_id) WHERE removed=0;
