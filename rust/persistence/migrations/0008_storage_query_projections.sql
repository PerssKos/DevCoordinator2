ALTER TABLE storage_artifacts
 ADD COLUMN projection_resource_key TEXT GENERATED ALWAYS AS(record_json->>'resource_key') STORED,
 ADD COLUMN projection_ancestors JSONB GENERATED ALWAYS AS(coalesce(record_json->'ancestor_keys','[]'::jsonb)) STORED,
 ADD COLUMN projection_protected BOOLEAN GENERATED ALWAYS AS(coalesce((record_json#>>'{artifact,protected}')::boolean,false)) STORED,
 ADD COLUMN projection_dependencies JSONB GENERATED ALWAYS AS(coalesce(record_json#>'{artifact,dependencies}','[]'::jsonb)) STORED,
 ADD COLUMN projection_blockers JSONB GENERATED ALWAYS AS(coalesce(record_json->'blockers','[]'::jsonb)) STORED,
 ADD COLUMN projection_name TEXT GENERATED ALWAYS AS(record_json#>>'{artifact,name}') STORED,
 ADD COLUMN projection_repository_name TEXT GENERATED ALWAYS AS(record_json#>>'{artifact,repository_name}') STORED,
 ADD COLUMN projection_filesystem_id TEXT GENERATED ALWAYS AS(record_json#>>'{artifact,filesystem_id}') STORED,
 ADD COLUMN projection_verified_at_ms NUMERIC GENERATED ALWAYS AS((record_json#>>'{artifact,verified_at_ms}')::numeric) STORED,
 ADD COLUMN projection_disposal_approved BOOLEAN GENERATED ALWAYS AS(coalesce((record_json->>'disposal_approved')::boolean,false)) STORED,
 ADD COLUMN projection_effect TEXT GENERATED ALWAYS AS(record_json->>'discovered_effect') STORED;
ALTER TABLE storage_leases
 ADD COLUMN projection_artifact_ids JSONB GENERATED ALWAYS AS(coalesce(lease_json->'artifact_ids','[]'::jsonb)) STORED,
 ADD COLUMN projection_expires_at_ms NUMERIC GENERATED ALWAYS AS((lease_json->>'expires_at_ms')::numeric) STORED;
CREATE INDEX storage_artifacts_resource_live ON storage_artifacts(projection_resource_key) WHERE removed_at_ms IS NULL;
CREATE INDEX storage_artifacts_ancestors_live ON storage_artifacts USING GIN(projection_ancestors) WHERE removed_at_ms IS NULL;
CREATE INDEX storage_leases_artifacts ON storage_leases USING GIN(projection_artifact_ids);
CREATE INDEX storage_artifacts_name_seek ON storage_artifacts(projection_repository_name COLLATE "C" NULLS FIRST,projection_name COLLATE "C",artifact_id COLLATE "C");
CREATE INDEX storage_artifacts_live_seek ON storage_artifacts(artifact_id) WHERE removed_at_ms IS NULL;
CREATE INDEX storage_artifacts_repository_live_seek ON storage_artifacts(repository_id,artifact_id) WHERE removed_at_ms IS NULL;
CREATE INDEX storage_plans_claims ON storage_plans USING GIN(plan_json jsonb_path_ops);
