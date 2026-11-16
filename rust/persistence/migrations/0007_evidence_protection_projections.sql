-- Exact-run protection lookups must not rescan retained authority history.
CREATE TABLE dc2_delivery_protection_heads (
  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('target','release')),
  owner_id TEXT NOT NULL, target TEXT NOT NULL,
  receipt_id TEXT NOT NULL REFERENCES release_evidence(receipt_id),
  worktree_id TEXT NOT NULL, run_id TEXT NOT NULL,
  qualified BIGINT NOT NULL CHECK(qualified IN (0,1)),
  checked_at_ms BIGINT NOT NULL, source_rowid BIGINT NOT NULL,
  PRIMARY KEY(scope_kind,owner_id,target)
);
CREATE INDEX dc2_delivery_protection_run ON dc2_delivery_protection_heads(worktree_id,run_id);
CREATE FUNCTION dc2_index_delivery_protection() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE qualified_value BIGINT; checked BIGINT;
BEGIN
  qualified_value=CASE WHEN NEW.receipt_json->'qualified'='true'::jsonb THEN 1 ELSE 0 END;
  checked=(NEW.receipt_json->>'checked_at_ms')::bigint;
  INSERT INTO dc2_delivery_protection_heads VALUES('release',NEW.release_id,'',NEW.receipt_id,NEW.receipt_json->>'worktree_id',NEW.receipt_json->>'run_id',qualified_value,checked,NEW.rowid)
  ON CONFLICT(scope_kind,owner_id,target) DO UPDATE SET receipt_id=excluded.receipt_id,worktree_id=excluded.worktree_id,run_id=excluded.run_id,qualified=excluded.qualified,checked_at_ms=excluded.checked_at_ms,source_rowid=excluded.source_rowid
  WHERE (excluded.checked_at_ms,excluded.source_rowid)>(dc2_delivery_protection_heads.checked_at_ms,dc2_delivery_protection_heads.source_rowid);
  IF qualified_value=1 THEN
    INSERT INTO dc2_delivery_protection_heads VALUES('target',NEW.repository_id,NEW.receipt_json->>'target',NEW.receipt_id,NEW.receipt_json->>'worktree_id',NEW.receipt_json->>'run_id',1,checked,NEW.rowid)
    ON CONFLICT(scope_kind,owner_id,target) DO UPDATE SET receipt_id=excluded.receipt_id,worktree_id=excluded.worktree_id,run_id=excluded.run_id,qualified=excluded.qualified,checked_at_ms=excluded.checked_at_ms,source_rowid=excluded.source_rowid
    WHERE (excluded.checked_at_ms,excluded.source_rowid)>(dc2_delivery_protection_heads.checked_at_ms,dc2_delivery_protection_heads.source_rowid);
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER dc2_delivery_protection_insert AFTER INSERT ON release_evidence FOR EACH ROW EXECUTE FUNCTION dc2_index_delivery_protection();
INSERT INTO dc2_delivery_protection_heads
 SELECT DISTINCT ON(release_id) 'release',release_id,'',receipt_id,receipt_json->>'worktree_id',receipt_json->>'run_id',CASE WHEN receipt_json->'qualified'='true'::jsonb THEN 1 ELSE 0 END,(receipt_json->>'checked_at_ms')::bigint,rowid
 FROM release_evidence ORDER BY release_id,(receipt_json->>'checked_at_ms')::bigint DESC,rowid DESC;
INSERT INTO dc2_delivery_protection_heads
 SELECT DISTINCT ON(repository_id,receipt_json->>'target') 'target',repository_id,receipt_json->>'target',receipt_id,receipt_json->>'worktree_id',receipt_json->>'run_id',1,(receipt_json->>'checked_at_ms')::bigint,rowid
 FROM release_evidence WHERE receipt_json->'qualified'='true'::jsonb ORDER BY repository_id,receipt_json->>'target',(receipt_json->>'checked_at_ms')::bigint DESC,rowid DESC;

CREATE TABLE dc2_storage_lease_artifacts (
  lease_id TEXT NOT NULL REFERENCES storage_leases(lease_id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL, expires_at_ms BIGINT NOT NULL,
  PRIMARY KEY(lease_id,artifact_id)
);
CREATE INDEX dc2_storage_lease_artifact_expiry ON dc2_storage_lease_artifacts(artifact_id,expires_at_ms DESC);
CREATE FUNCTION dc2_index_storage_lease() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  DELETE FROM dc2_storage_lease_artifacts WHERE lease_id=NEW.lease_id;
  INSERT INTO dc2_storage_lease_artifacts SELECT NEW.lease_id,value,NEW.expires_at_ms FROM jsonb_array_elements_text(NEW.lease_json->'artifact_ids') ON CONFLICT DO NOTHING;
  RETURN NEW;
END $$;
CREATE TRIGGER dc2_storage_lease_change AFTER INSERT OR UPDATE OF lease_json,expires_at_ms ON storage_leases FOR EACH ROW EXECUTE FUNCTION dc2_index_storage_lease();
INSERT INTO dc2_storage_lease_artifacts SELECT l.lease_id,a.value,l.expires_at_ms FROM storage_leases l CROSS JOIN LATERAL jsonb_array_elements_text(l.lease_json->'artifact_ids') a ON CONFLICT DO NOTHING;

ALTER TABLE review_records ADD COLUMN revision_ref TEXT GENERATED ALWAYS AS (record_id||'@'||revision::text) STORED;
CREATE UNIQUE INDEX dc2_review_revision_ref ON review_records(revision_ref);
CREATE INDEX dc2_review_policy_receipt ON review_policies(last_receipt);
CREATE TABLE dc2_review_run_refs (
  record_id TEXT NOT NULL, revision BIGINT NOT NULL, ordinal BIGINT NOT NULL,
  worktree_id TEXT, run_id TEXT, valid BIGINT NOT NULL CHECK(valid IN (0,1)),
  PRIMARY KEY(record_id,revision,ordinal),
  FOREIGN KEY(record_id,revision) REFERENCES review_records(record_id,revision)
);
CREATE TABLE dc2_active_review_runs (
  repository_id TEXT NOT NULL, workstream_key TEXT NOT NULL, ordinal BIGINT NOT NULL,
  worktree_id TEXT, run_id TEXT, valid BIGINT NOT NULL CHECK(valid IN (0,1)),
  PRIMARY KEY(repository_id,workstream_key,ordinal),
  FOREIGN KEY(repository_id,workstream_key) REFERENCES review_policies(repository_id,workstream_key) ON DELETE CASCADE
);
CREATE INDEX dc2_active_review_run ON dc2_active_review_runs(worktree_id,run_id) WHERE valid=1;
CREATE INDEX dc2_active_review_invalid ON dc2_active_review_runs(valid) WHERE valid=0;
CREATE FUNCTION dc2_extract_review_run_refs(id TEXT,rev BIGINT,document JSONB) RETURNS void LANGUAGE plpgsql AS $$
DECLARE reference JSONB; value TEXT; position_value INTEGER; ordinal_value BIGINT=0;
BEGIN
  DELETE FROM dc2_review_run_refs WHERE record_id=id AND revision=rev;
  IF jsonb_typeof(document#>'{experiment,evidence_refs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,result_evidence_refs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,baseline,evidenceRefs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,observations}') IS DISTINCT FROM 'array' THEN
    INSERT INTO dc2_review_run_refs VALUES(id,rev,0,NULL,NULL,0);
    RETURN;
  END IF;
  FOR reference IN
    SELECT * FROM jsonb_path_query(document,'$.experiment.evidence_refs[*] ? (@.kind == "run")')
    UNION ALL SELECT * FROM jsonb_path_query(document,'$.experiment.result_evidence_refs[*] ? (@.kind == "run")')
    UNION ALL SELECT * FROM jsonb_path_query(document,'$.experiment.baseline.evidenceRefs[*] ? (@.kind == "run")')
    UNION ALL SELECT * FROM jsonb_path_query(document,'$.experiment.observations[*].evidenceRefs[*] ? (@.kind == "run")')
  LOOP
    ordinal_value=ordinal_value+1;
    value=reference->>'reference'; position_value=strpos(value,'/');
    IF jsonb_typeof(reference->'reference') IS DISTINCT FROM 'string' OR position_value IS NULL OR position_value=0 THEN
      INSERT INTO dc2_review_run_refs VALUES(id,rev,ordinal_value,NULL,NULL,0);
    ELSE
      INSERT INTO dc2_review_run_refs VALUES(id,rev,ordinal_value,substring(value FROM 1 FOR position_value-1),substring(value FROM position_value+1),1);
    END IF;
  END LOOP;
END $$;
CREATE FUNCTION dc2_refresh_review_policy(repo TEXT,scope TEXT) RETURNS void LANGUAGE SQL AS $$
  DELETE FROM dc2_active_review_runs WHERE repository_id=repo AND workstream_key=scope;
  INSERT INTO dc2_active_review_runs
    SELECT p.repository_id,p.workstream_key,x.ordinal,x.worktree_id,x.run_id,x.valid
    FROM review_policies p JOIN review_records r ON r.revision_ref=p.last_receipt
    JOIN dc2_review_run_refs x USING(record_id,revision)
    WHERE p.repository_id=repo AND p.workstream_key=scope AND p.active=1;
$$;
CREATE FUNCTION dc2_index_review_record() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE policy RECORD;
BEGIN
  PERFORM dc2_extract_review_run_refs(NEW.record_id,NEW.revision,NEW.record_json);
  FOR policy IN SELECT repository_id,workstream_key FROM review_policies WHERE last_receipt=NEW.revision_ref LOOP
    PERFORM dc2_refresh_review_policy(policy.repository_id,policy.workstream_key);
  END LOOP;
  RETURN NEW;
END $$;
CREATE TRIGGER dc2_review_record_insert AFTER INSERT ON review_records FOR EACH ROW EXECUTE FUNCTION dc2_index_review_record();
CREATE FUNCTION dc2_review_policy_changed() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM dc2_refresh_review_policy(NEW.repository_id,NEW.workstream_key); RETURN NEW; END $$;
CREATE TRIGGER dc2_review_policy_change AFTER INSERT OR UPDATE OF active,last_receipt ON review_policies FOR EACH ROW EXECUTE FUNCTION dc2_review_policy_changed();
DO $$ DECLARE row RECORD; BEGIN
  FOR row IN SELECT record_id,revision,record_json FROM review_records LOOP PERFORM dc2_extract_review_run_refs(row.record_id,row.revision,row.record_json); END LOOP;
  FOR row IN SELECT repository_id,workstream_key FROM review_policies LOOP PERFORM dc2_refresh_review_policy(row.repository_id,row.workstream_key); END LOOP;
END $$;
CREATE INDEX dc2_evidence_run ON storage_artifacts((record_json#>>'{locator,worktree}'),(record_json#>>'{locator,run_id}')) WHERE kind='evidence' AND removed_at_ms IS NULL;
CREATE INDEX dc2_feedback_run ON visual_feedback(worktree_id,run_id) WHERE deleted_at IS NULL;
