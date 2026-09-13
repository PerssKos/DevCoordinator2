CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS repositories (
  repository_id TEXT PRIMARY KEY,
  root_path TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  registered_at TEXT NOT NULL,
  registered_by_uid INTEGER NOT NULL,
  last_seen_at TEXT NOT NULL,
  archived_at TEXT,
  archived_by_uid INTEGER,
  archive_note TEXT,
  merged_into_repository_id TEXT
);
CREATE TABLE IF NOT EXISTS repository_presentation (
  repository_id TEXT PRIMARY KEY REFERENCES repositories(repository_id),
  display_name TEXT,
  icon TEXT,
  updated_at TEXT NOT NULL,
  updated_by_uid INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS worktrees (
  worktree_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  worktree_path TEXT NOT NULL UNIQUE,
  registered_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deployments (
  deployment_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  worktree_id TEXT NOT NULL REFERENCES worktrees(worktree_id),
  name TEXT NOT NULL,
  source TEXT NOT NULL,
  domain TEXT,
  spec_fingerprint TEXT NOT NULL,
  spec_json TEXT NOT NULL,
  state TEXT NOT NULL,
  current_generation INTEGER,
  previous_generation INTEGER,
  created_at TEXT NOT NULL,
  created_by_uid INTEGER NOT NULL,
  client TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  ttl_expires_at TEXT,
  public INTEGER NOT NULL DEFAULT 0,
  domain_override TEXT,
  UNIQUE(worktree_id, name, source)
);
CREATE TABLE IF NOT EXISTS generations (
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  number INTEGER NOT NULL,
  commit_hash TEXT,
  dirty INTEGER NOT NULL DEFAULT 0,
  path TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  state TEXT NOT NULL,
  PRIMARY KEY(deployment_id, number)
);
CREATE TABLE IF NOT EXISTS components (
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  name TEXT NOT NULL,
  type TEXT NOT NULL,
  order_index INTEGER NOT NULL,
  spec_fingerprint TEXT NOT NULL,
  desired_state TEXT NOT NULL,
  state TEXT NOT NULL,
  health TEXT NOT NULL,
  generation INTEGER,
  binding_kind TEXT,
  binding_identity TEXT,
  restarts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(deployment_id, name)
);
CREATE TABLE IF NOT EXISTS port_assignments (
  port INTEGER PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  component TEXT NOT NULL,
  generation INTEGER NOT NULL,
  assigned_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS domain_routes (
  domain TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  component TEXT NOT NULL,
  port INTEGER,
  generation INTEGER,
  published_at TEXT
);
CREATE TABLE IF NOT EXISTS metric_minutes (
  subject_kind TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  metric TEXT NOT NULL,
  minute_utc TEXT NOT NULL,
  min_value REAL NOT NULL,
  avg_value REAL NOT NULL,
  max_value REAL NOT NULL,
  samples INTEGER NOT NULL,
  PRIMARY KEY(subject_kind, subject_id, metric, minute_utc)
);
CREATE INDEX IF NOT EXISTS metric_minutes_time ON metric_minutes(minute_utc);
CREATE TABLE IF NOT EXISTS alerts (
  alert_key TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  subject_kind TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  severity TEXT NOT NULL,
  message TEXT NOT NULL,
  opened_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS users (
  user_id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  subject TEXT,
  display_name TEXT,
  administrator INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  last_seen_at TEXT
);
CREATE TABLE IF NOT EXISTS invitations (
  invitation_id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  administrator INTEGER NOT NULL DEFAULT 0,
  grants_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS grants (
  user_id TEXT NOT NULL REFERENCES users(user_id),
  deployment_id TEXT NOT NULL,
  role TEXT NOT NULL,
  granted_at TEXT NOT NULL,
  granted_by TEXT NOT NULL,
  PRIMARY KEY(user_id, deployment_id)
);
CREATE TABLE IF NOT EXISTS telegram_chats (
  chat_id INTEGER PRIMARY KEY,
  email TEXT NOT NULL,
  label TEXT,
  linked_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS telegram_links (
  code TEXT PRIMARY KEY,
  chat_id INTEGER NOT NULL,
  label TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS telegram_subscriptions (
  chat_id INTEGER NOT NULL REFERENCES telegram_chats(chat_id),
  scope TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(chat_id, scope)
);
CREATE TABLE IF NOT EXISTS telegram_outbox (
  message_id INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id INTEGER NOT NULL,
  text TEXT NOT NULL,
  created_at TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT NOT NULL,
  last_error TEXT
);
CREATE TABLE IF NOT EXISTS observed_deployments (
  observed_deployment_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  name TEXT NOT NULL,
  native_project TEXT NOT NULL UNIQUE,
  state TEXT NOT NULL CHECK(state IN ('running','degraded','stopped','failed')),
  health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','unknown')),
  source TEXT NOT NULL,
  evidence_json TEXT NOT NULL,
  observed_at TEXT NOT NULL,
  imported_at TEXT NOT NULL,
  UNIQUE(repository_id, native_project)
);
CREATE TABLE IF NOT EXISTS observed_containers (
  container_id TEXT PRIMARY KEY,
  observed_deployment_id TEXT NOT NULL REFERENCES observed_deployments(observed_deployment_id) ON DELETE CASCADE,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  name TEXT NOT NULL,
  image TEXT NOT NULL,
  compose_service TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('running','stopped','failed','starting','missing')),
  status TEXT NOT NULL,
  health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','starting','unknown','none')),
  observed_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS observed_containers_deployment ON observed_containers(observed_deployment_id);
CREATE INDEX IF NOT EXISTS observed_containers_repository ON observed_containers(repository_id);
CREATE TABLE IF NOT EXISTS observed_routes (
  domain TEXT PRIMARY KEY,
  observed_deployment_id TEXT NOT NULL REFERENCES observed_deployments(observed_deployment_id) ON DELETE CASCADE,
  component TEXT NOT NULL,
  port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
  public INTEGER NOT NULL CHECK(public IN (0,1)),
  evidence_json TEXT NOT NULL,
  observed_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS releases (
  release_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  seq INTEGER NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  note TEXT,
  requested_at TEXT,
  delivered_at TEXT,
  deployment_id TEXT,
  generation_number INTEGER,
  commit_hash TEXT,
  dirty INTEGER,
  fingerprint TEXT,
  url TEXT,
  port INTEGER,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(repository_id, seq)
);
CREATE TABLE IF NOT EXISTS tasks (
  task_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  parent_task_id TEXT REFERENCES tasks(task_id),
  release_id TEXT REFERENCES releases(release_id),
  seq INTEGER NOT NULL,
  position INTEGER NOT NULL,
  title TEXT NOT NULL,
  outcome TEXT NOT NULL,
  impact TEXT,
  unblock_condition TEXT,
  verification TEXT,
  technical_note TEXT,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  estimated_loc INTEGER,
  elaboration_needed INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(repository_id, seq)
);
CREATE INDEX IF NOT EXISTS tasks_repository_status ON tasks(repository_id, status);
CREATE INDEX IF NOT EXISTS tasks_release ON tasks(release_id);
CREATE INDEX IF NOT EXISTS tasks_parent ON tasks(parent_task_id);
CREATE TABLE IF NOT EXISTS runtime_configuration_events (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  action TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK(outcome IN ('prepared','activated','failed')),
  previous_revision TEXT NOT NULL,
  revision TEXT NOT NULL,
  actor TEXT NOT NULL,
  at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS plan_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  subject_kind TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  event TEXT NOT NULL,
  from_value TEXT,
  to_value TEXT,
  actor TEXT NOT NULL,
  at TEXT NOT NULL,
  note TEXT
);
CREATE INDEX IF NOT EXISTS plan_events_subject ON plan_events(subject_kind, subject_id);
CREATE TABLE IF NOT EXISTS decisions (
  decision_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  seq INTEGER NOT NULL,
  ref TEXT,
  aspect TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  technical_note TEXT,
  superseded_by TEXT REFERENCES decisions(decision_id),
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  UNIQUE(repository_id, seq)
);
CREATE INDEX IF NOT EXISTS decisions_repository_aspect ON decisions(repository_id, aspect);
CREATE TABLE IF NOT EXISTS review_records (
  record_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  window_start_ms INTEGER NOT NULL,
  window_end_ms INTEGER NOT NULL CHECK(window_end_ms > window_start_ms),
  record_json TEXT NOT NULL CHECK(length(record_json) <= 8192),
  actor TEXT NOT NULL,
  recorded_at_ms INTEGER NOT NULL,
  PRIMARY KEY(record_id, revision)
);
CREATE INDEX IF NOT EXISTS review_records_repository ON review_records(repository_id);
CREATE TRIGGER IF NOT EXISTS review_records_no_update BEFORE UPDATE ON review_records BEGIN
  SELECT RAISE(ABORT, 'review revisions are append-only');
END;
CREATE TRIGGER IF NOT EXISTS review_records_no_delete BEFORE DELETE ON review_records BEGIN
  SELECT RAISE(ABORT, 'review revisions are permanent');
END;
CREATE TABLE IF NOT EXISTS release_evidence (
  receipt_id TEXT PRIMARY KEY,
  release_id TEXT NOT NULL REFERENCES releases(release_id),
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  receipt_json TEXT NOT NULL CHECK(length(receipt_json) <= 8192)
);
CREATE INDEX IF NOT EXISTS release_evidence_release ON release_evidence(release_id);
CREATE TRIGGER IF NOT EXISTS release_evidence_no_update BEFORE UPDATE ON release_evidence BEGIN
  SELECT RAISE(ABORT, 'delivery receipts are immutable');
END;
CREATE TRIGGER IF NOT EXISTS release_evidence_no_delete BEFORE DELETE ON release_evidence BEGIN
  SELECT RAISE(ABORT, 'delivery receipts are permanent');
END;
CREATE UNIQUE INDEX IF NOT EXISTS decisions_ref ON decisions(repository_id, ref) WHERE ref IS NOT NULL;
CREATE TABLE IF NOT EXISTS decision_summaries (
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  covers_through_seq INTEGER NOT NULL,
  body TEXT NOT NULL,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  PRIMARY KEY(repository_id, covers_through_seq)
);
CREATE VIRTUAL TABLE IF NOT EXISTS decisions_fts USING fts5(
  title, body, technical_note, ref,
  content='decisions', content_rowid='rowid'
);
CREATE TRIGGER IF NOT EXISTS decisions_fts_insert AFTER INSERT ON decisions BEGIN
  INSERT INTO decisions_fts(rowid,title,body,technical_note,ref)
  VALUES(new.rowid,new.title,new.body,new.technical_note,new.ref);
END;
CREATE TABLE IF NOT EXISTS compose_completions (
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  component TEXT NOT NULL,
  service TEXT NOT NULL,
  generation INTEGER NOT NULL,
  container_id TEXT NOT NULL,
  image_id TEXT,
  exit_code INTEGER NOT NULL,
  started_at TEXT,
  finished_at TEXT,
  recorded_at TEXT NOT NULL,
  PRIMARY KEY(deployment_id, component, service, generation)
);
CREATE INDEX IF NOT EXISTS compose_completions_generation ON compose_completions(deployment_id,generation);
CREATE TABLE IF NOT EXISTS compose_service_desires (
  deployment_id TEXT NOT NULL REFERENCES deployments(deployment_id),
  component TEXT NOT NULL,
  service TEXT NOT NULL,
  desired_state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(deployment_id, component, service)
);
CREATE TABLE IF NOT EXISTS codex_usage_repository_links (
  source_uid INTEGER NOT NULL,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  codex_repository_id TEXT NOT NULL CHECK(length(codex_repository_id)=64),
  source_schema INTEGER NOT NULL,
  taxonomy_version INTEGER NOT NULL,
  resolved_at TEXT NOT NULL,
  PRIMARY KEY(source_uid, repository_id)
);
CREATE INDEX IF NOT EXISTS codex_usage_links_repository ON codex_usage_repository_links(repository_id);
CREATE TABLE IF NOT EXISTS repository_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  event TEXT NOT NULL CHECK(event IN ('archived','unarchived')),
  merged_into_repository_id TEXT,
  actor_uid INTEGER NOT NULL,
  at TEXT NOT NULL,
  note TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS repository_events_repository ON repository_events(repository_id,event_id);
CREATE TABLE IF NOT EXISTS test_capacity_state (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  learned_capacity INTEGER NOT NULL CHECK(learned_capacity>=1),
  cap INTEGER CHECK(cap IS NULL OR cap>=1),
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS test_capacity_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  at TEXT NOT NULL,
  actor TEXT NOT NULL,
  reason TEXT NOT NULL,
  previous_capacity INTEGER NOT NULL,
  new_capacity INTEGER NOT NULL,
  cap INTEGER,
  p95_cpu_percent REAL,
  p95_memory_percent REAL,
  saturation_fraction REAL,
  epoch_seconds REAL
);
CREATE INDEX IF NOT EXISTS test_capacity_events_time ON test_capacity_events(event_id);
CREATE TABLE IF NOT EXISTS test_log_retention_state (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  max_age_seconds INTEGER NOT NULL CHECK(max_age_seconds>=1),
  case_depth INTEGER NOT NULL CHECK(case_depth>=1),
  updated_at TEXT NOT NULL,
  updated_by TEXT NOT NULL,
  last_cleanup_at TEXT,
  last_cleanup_error_code TEXT
);
INSERT OR IGNORE INTO test_log_retention_state(singleton,max_age_seconds,case_depth,updated_at,updated_by)
VALUES(1,86400,3,strftime('%Y-%m-%dT%H:%M:%SZ','now'),'schema-default');
CREATE TABLE IF NOT EXISTS test_log_retention_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  at TEXT NOT NULL,
  actor TEXT NOT NULL,
  previous_max_age_seconds INTEGER NOT NULL,
  max_age_seconds INTEGER NOT NULL,
  previous_case_depth INTEGER NOT NULL,
  case_depth INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS test_log_retention_events_time ON test_log_retention_events(event_id);
CREATE TABLE IF NOT EXISTS visual_feedback (
  feedback_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id),
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  worktree_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  check_name TEXT NOT NULL,
  phase TEXT NOT NULL,
  case_id TEXT,
  formal_run_id TEXT NOT NULL,
  cell_id TEXT NOT NULL,
  review_cell_key TEXT,
  screenshot_kind TEXT NOT NULL,
  screenshot_sha256 TEXT NOT NULL,
  image_id TEXT NOT NULL,
  geometry_json TEXT NOT NULL,
  root_comment_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  deleted_by TEXT
);
CREATE INDEX IF NOT EXISTS visual_feedback_run ON visual_feedback(repository_id,worktree_id,run_id);
CREATE INDEX IF NOT EXISTS visual_feedback_image ON visual_feedback(image_id);
CREATE TABLE IF NOT EXISTS visual_feedback_comments (
  comment_id TEXT PRIMARY KEY,
  feedback_id TEXT NOT NULL REFERENCES visual_feedback(feedback_id),
  seq INTEGER NOT NULL,
  body TEXT NOT NULL,
  created_at TEXT NOT NULL,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  deleted_by TEXT,
  UNIQUE(feedback_id,seq)
);
CREATE INDEX IF NOT EXISTS visual_feedback_comments_thread ON visual_feedback_comments(feedback_id,created_at);
CREATE TABLE IF NOT EXISTS visual_feedback_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  feedback_id TEXT NOT NULL REFERENCES visual_feedback(feedback_id),
  event TEXT NOT NULL,
  comment_id TEXT,
  from_value TEXT,
  to_value TEXT,
  actor TEXT NOT NULL,
  at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS visual_feedback_events_thread ON visual_feedback_events(feedback_id,event_id);
CREATE TABLE IF NOT EXISTS owned_events (
  cursor INTEGER PRIMARY KEY AUTOINCREMENT,
  occurred_at TEXT NOT NULL,
  category TEXT NOT NULL CHECK(category IN ('test','deployment','planning','health','feedback','other')),
  kind TEXT NOT NULL,
  repository_id TEXT,
  deployment_id TEXT,
  payload_json TEXT NOT NULL CHECK(length(payload_json) <= 8192),
  dedupe_key TEXT UNIQUE
);
CREATE INDEX IF NOT EXISTS owned_events_repository_cursor ON owned_events(repository_id,cursor);
CREATE INDEX IF NOT EXISTS owned_events_deployment_cursor ON owned_events(deployment_id,cursor);
CREATE INDEX IF NOT EXISTS owned_events_category_cursor ON owned_events(category,cursor);
CREATE TABLE IF NOT EXISTS glossary_profiles (
    scope TEXT PRIMARY KEY,
    revision INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS glossary_revisions (
    scope TEXT NOT NULL REFERENCES glossary_profiles(scope),
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    body TEXT,
    summary TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(scope, revision)
);
CREATE INDEX IF NOT EXISTS glossary_subject_history
    ON glossary_revisions(scope, kind, subject_id, revision);
