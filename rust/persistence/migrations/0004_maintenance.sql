-- Durable jobs are independent of request connections and lease atomically.
CREATE TABLE coordinator_maintenance (
  job_key TEXT PRIMARY KEY,
  cursor JSONB NOT NULL DEFAULT 'null',
  due_at_ms BIGINT NOT NULL,
  lease_until_ms BIGINT,
  attempts BIGINT NOT NULL DEFAULT 0 CHECK(attempts >= 0),
  priority BIGINT NOT NULL DEFAULT 0,
  last_status TEXT NOT NULL DEFAULT 'pending',
  last_rows BIGINT NOT NULL DEFAULT 0 CHECK(last_rows >= 0),
  last_bytes BIGINT NOT NULL DEFAULT 0 CHECK(last_bytes >= 0),
  last_error_code TEXT CHECK(last_error_code IS NULL OR (length(last_error_code) <= 96 AND last_error_code ~ '^[a-z0-9_]+$'))
);
CREATE INDEX coordinator_maintenance_due ON coordinator_maintenance(priority DESC,due_at_ms,job_key);

-- Exact bootstrap state proves the import target has never acquired data.
CREATE TABLE dc2_bootstrap_defaults(table_name TEXT PRIMARY KEY,rows_json JSONB NOT NULL);
INSERT INTO dc2_bootstrap_defaults
  SELECT 'meta',jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM meta t UNION ALL
  SELECT 'usage_rate_cards',jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM usage_rate_cards t UNION ALL
  SELECT 'ticket_settings',jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM ticket_settings t UNION ALL
  SELECT 'storage_scan_state',jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM storage_scan_state t UNION ALL
  SELECT 'test_log_retention_state',jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM test_log_retention_state t;
CREATE TRIGGER dc2_bootstrap_defaults_immutable BEFORE UPDATE OR DELETE ON dc2_bootstrap_defaults FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();
