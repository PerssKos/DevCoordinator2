-- The serialized record is authoritative; older writers could leave the
-- original repository_id scalar stale. Preserve it as migration evidence.
ALTER TABLE storage_artifacts ADD COLUMN projection_repository_id TEXT GENERATED ALWAYS AS(record_json#>>'{artifact,repository_id}') STORED;
CREATE INDEX storage_artifacts_record_repository_live_seek ON storage_artifacts(projection_repository_id,artifact_id) WHERE removed_at_ms IS NULL;
