-- Only explicit candidate applies record an origin. Historical receipts keep
-- their original declared-source semantics; commit differences are not evidence.
CREATE TABLE generation_source_origins (
  deployment_id TEXT NOT NULL,
  number BIGINT NOT NULL,
  worktree_id TEXT NOT NULL,
  path TEXT NOT NULL CHECK (left(path, 1) = '/'),
  PRIMARY KEY (deployment_id, number),
  FOREIGN KEY (deployment_id, number) REFERENCES generations(deployment_id, number)
    ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
