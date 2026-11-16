CREATE TABLE IF NOT EXISTS storage_artifacts (
  artifact_id TEXT PRIMARY KEY, repository_id TEXT, kind TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0), removed_at_ms INTEGER,
  record_json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS storage_artifacts_repository ON storage_artifacts(repository_id,artifact_id);
CREATE TABLE IF NOT EXISTS storage_policies (
  scope TEXT PRIMARY KEY, revision INTEGER NOT NULL, policy_json TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL, actor TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS storage_roots (
  root_id TEXT PRIMARY KEY, revision INTEGER NOT NULL, root_json TEXT NOT NULL,
  actor TEXT NOT NULL, updated_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS storage_plans (
  plan_id TEXT PRIMARY KEY, plan_json TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL, actor TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS storage_jobs (
  job_id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL,
  idempotency_key TEXT NOT NULL, request_sha256 TEXT NOT NULL, actor TEXT NOT NULL,
  caller_uid INTEGER NOT NULL, job_json TEXT NOT NULL, request_json TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL, UNIQUE(actor,kind,idempotency_key)
);
CREATE INDEX IF NOT EXISTS storage_jobs_pending ON storage_jobs(state,created_at_ms);
CREATE TABLE IF NOT EXISTS storage_item_receipts (
  job_id TEXT NOT NULL REFERENCES storage_jobs(job_id), artifact_id TEXT NOT NULL,
  step INTEGER NOT NULL, receipt_json TEXT NOT NULL,
  PRIMARY KEY(job_id,artifact_id,step)
);
CREATE TABLE IF NOT EXISTS storage_leases (
  lease_id TEXT PRIMARY KEY, actor TEXT NOT NULL,
  lease_json TEXT NOT NULL, expires_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS storage_changes (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT, artifact_id TEXT, kind TEXT NOT NULL,
  actor TEXT NOT NULL, created_at_ms INTEGER NOT NULL, change_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS storage_scan_state (
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1), revision INTEGER NOT NULL,
  last_scan_at_ms INTEGER, filesystem_json TEXT NOT NULL, coverage_json TEXT NOT NULL
);
INSERT OR IGNORE INTO storage_scan_state VALUES(1,1,NULL,'[]','["not_scanned"]');
CREATE TABLE IF NOT EXISTS storage_resource_locks (
  resource_key TEXT NOT NULL,
  job_id TEXT NOT NULL REFERENCES storage_jobs(job_id),
  exclusive INTEGER NOT NULL CHECK(exclusive IN (0,1)),
  PRIMARY KEY(resource_key,job_id)
);
CREATE TABLE IF NOT EXISTS storage_item_intents (
  job_id TEXT NOT NULL REFERENCES storage_jobs(job_id), artifact_id TEXT NOT NULL,
  fingerprint TEXT NOT NULL, revision INTEGER NOT NULL, started_at_ms INTEGER NOT NULL,
  PRIMARY KEY(job_id,artifact_id)
);
