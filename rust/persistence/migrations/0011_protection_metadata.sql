-- Historical release evidence may predate typed delivery receipts. Preserve
-- those rows and mark unavailable ownership in the derived projection only.
ALTER TABLE dc2_delivery_protection_heads ALTER COLUMN worktree_id DROP NOT NULL;
ALTER TABLE dc2_delivery_protection_heads ALTER COLUMN run_id DROP NOT NULL;
ALTER TABLE dc2_delivery_protection_heads ALTER COLUMN checked_at_ms DROP NOT NULL;
ALTER TABLE dc2_delivery_protection_heads ALTER COLUMN checked_at_ms TYPE NUMERIC;
CREATE INDEX dc2_delivery_protection_invalid ON dc2_delivery_protection_heads(source_rowid,scope_kind,owner_id,target)
 WHERE worktree_id IS NULL OR run_id IS NULL OR checked_at_ms IS NULL;
CREATE INDEX dc2_active_review_invalid_seek ON dc2_active_review_runs(repository_id,workstream_key,ordinal) WHERE valid=0;
CREATE FUNCTION dc2_refresh_delivery_head(receipt_value TEXT,release_value TEXT,repo_value TEXT,document JSONB,source_id BIGINT) RETURNS void LANGUAGE plpgsql AS $$
DECLARE qualified_value BIGINT; checked NUMERIC; owner_value TEXT; run_value TEXT;
BEGIN
 qualified_value=CASE WHEN document->'qualified'='true'::jsonb THEN 1 ELSE 0 END;
 owner_value=CASE WHEN jsonb_typeof(document->'worktree_id')='string' THEN document->>'worktree_id' END;
 run_value=CASE WHEN jsonb_typeof(document->'run_id')='string' THEN document->>'run_id' END;
 IF jsonb_typeof(document->'checked_at_ms')='number' THEN
  checked=(document->>'checked_at_ms')::numeric;
  IF checked<0 OR checked>18446744073709551615 OR trunc(checked)<>checked THEN checked=NULL; END IF;
 END IF;
 IF jsonb_typeof(document->'qualified') IS DISTINCT FROM 'boolean'
  OR (qualified_value=1 AND jsonb_typeof(document->'target') IS DISTINCT FROM 'string') THEN owner_value=NULL; END IF;
 INSERT INTO dc2_delivery_protection_heads VALUES('release',release_value,'',receipt_value,owner_value,run_value,qualified_value,checked,source_id)
 ON CONFLICT(scope_kind,owner_id,target) DO UPDATE SET receipt_id=excluded.receipt_id,worktree_id=excluded.worktree_id,run_id=excluded.run_id,qualified=excluded.qualified,checked_at_ms=excluded.checked_at_ms,source_rowid=excluded.source_rowid
 WHERE (coalesce(excluded.checked_at_ms,'-Infinity'::numeric),excluded.source_rowid)>(coalesce(dc2_delivery_protection_heads.checked_at_ms,'-Infinity'::numeric),dc2_delivery_protection_heads.source_rowid);
 IF qualified_value=1 AND jsonb_typeof(document->'target')='string' THEN
  INSERT INTO dc2_delivery_protection_heads VALUES('target',repo_value,document->>'target',receipt_value,owner_value,run_value,1,checked,source_id)
  ON CONFLICT(scope_kind,owner_id,target) DO UPDATE SET receipt_id=excluded.receipt_id,worktree_id=excluded.worktree_id,run_id=excluded.run_id,qualified=excluded.qualified,checked_at_ms=excluded.checked_at_ms,source_rowid=excluded.source_rowid
  WHERE (coalesce(excluded.checked_at_ms,'-Infinity'::numeric),excluded.source_rowid)>(coalesce(dc2_delivery_protection_heads.checked_at_ms,'-Infinity'::numeric),dc2_delivery_protection_heads.source_rowid);
 END IF;
END $$;
CREATE OR REPLACE FUNCTION dc2_index_delivery_protection() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
 PERFORM dc2_refresh_delivery_head(NEW.receipt_id,NEW.release_id,NEW.repository_id,NEW.receipt_json,NEW.rowid);
 RETURN NEW;
END $$;
TRUNCATE dc2_delivery_protection_heads;
DO $$ DECLARE source_row RECORD; BEGIN
 FOR source_row IN SELECT * FROM release_evidence ORDER BY rowid LOOP
  PERFORM dc2_refresh_delivery_head(source_row.receipt_id,source_row.release_id,source_row.repository_id,source_row.receipt_json,source_row.rowid);
 END LOOP;
END $$;

-- Review DTOs serialize their evidence fields in camelCase.
CREATE OR REPLACE FUNCTION dc2_extract_review_run_refs(id TEXT,rev BIGINT,document JSONB) RETURNS void LANGUAGE plpgsql AS $$
DECLARE reference JSONB; value TEXT; position_value INTEGER; ordinal_value BIGINT=0;
BEGIN
  DELETE FROM dc2_review_run_refs WHERE record_id=id AND revision=rev;
  IF jsonb_typeof(document#>'{experiment,evidenceRefs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,resultEvidenceRefs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,baseline,evidenceRefs}') IS DISTINCT FROM 'array'
    OR jsonb_typeof(document#>'{experiment,observations}') IS DISTINCT FROM 'array' THEN
    INSERT INTO dc2_review_run_refs VALUES(id,rev,0,NULL,NULL,0);
    RETURN;
  END IF;
  FOR reference IN
    SELECT * FROM jsonb_path_query(document,'$.experiment.evidenceRefs[*] ? (@.kind == "run")')
    UNION ALL SELECT * FROM jsonb_path_query(document,'$.experiment.resultEvidenceRefs[*] ? (@.kind == "run")')
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
DO $$ DECLARE source_row RECORD; BEGIN
 FOR source_row IN SELECT record_id,revision,record_json FROM review_records ORDER BY rowid LOOP
  PERFORM dc2_extract_review_run_refs(source_row.record_id,source_row.revision,source_row.record_json);
 END LOOP;
 FOR source_row IN SELECT repository_id,workstream_key FROM review_policies LOOP
  PERFORM dc2_refresh_review_policy(source_row.repository_id,source_row.workstream_key);
 END LOOP;
END $$;
