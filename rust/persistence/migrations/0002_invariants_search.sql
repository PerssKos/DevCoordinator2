-- PostgreSQL enforces permanent evidence and native text/record search.
CREATE OR REPLACE FUNCTION dc2_reject_history_mutation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'permanent history is append-only' USING ERRCODE='23514'; END $$;

CREATE TRIGGER health_incident_history_no_update BEFORE UPDATE ON health_incident_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER health_incident_history_no_delete BEFORE DELETE ON health_incident_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER review_records_no_update BEFORE UPDATE ON review_records FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER review_records_no_delete BEFORE DELETE ON review_records FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER release_evidence_no_update BEFORE UPDATE ON release_evidence FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER release_evidence_no_delete BEFORE DELETE ON release_evidence FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER repository_retirement_receipts_no_update BEFORE UPDATE ON repository_retirement_receipts FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER repository_retirement_receipts_no_delete BEFORE DELETE ON repository_retirement_receipts FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER planning_recoveries_no_update BEFORE UPDATE ON planning_recoveries FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER planning_recoveries_no_delete BEFORE DELETE ON planning_recoveries FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER planning_recovery_records_no_update BEFORE UPDATE ON planning_recovery_records FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER planning_recovery_records_no_delete BEFORE DELETE ON planning_recovery_records FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER deployment_recoveries_no_update BEFORE UPDATE ON deployment_recoveries FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER deployment_recoveries_no_delete BEFORE DELETE ON deployment_recoveries FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER usage_rate_cards_no_update BEFORE UPDATE ON usage_rate_cards FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER usage_rate_cards_no_delete BEFORE DELETE ON usage_rate_cards FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER review_clock_history_no_update BEFORE UPDATE ON review_clock_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER review_clock_history_no_delete BEFORE DELETE ON review_clock_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_identity_immutable BEFORE UPDATE OF batch_id,repository_id,file_path,sha256,byte_size,width,height,surface_id,display_order,legacy,manifest_version ON sketches FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_no_delete BEFORE DELETE ON sketches FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_lineage_no_update BEFORE UPDATE ON sketch_lineage FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_lineage_no_delete BEFORE DELETE ON sketch_lineage FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_description_history_no_update BEFORE UPDATE ON sketch_description_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_description_history_no_delete BEFORE DELETE ON sketch_description_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_surface_activations_no_update BEFORE UPDATE ON sketch_surface_activations FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_surface_activations_no_delete BEFORE DELETE ON sketch_surface_activations FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_decision_history_no_update BEFORE UPDATE ON sketch_decision_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE TRIGGER sketch_decision_history_no_delete BEFORE DELETE ON sketch_decision_history FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation();

CREATE OR REPLACE FUNCTION dc2_immutable_port_lease() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.lease_id IS DISTINCT FROM OLD.lease_id OR NEW.port IS DISTINCT FROM OLD.port OR NEW.deployment_id IS DISTINCT FROM OLD.deployment_id OR NEW.component IS DISTINCT FROM OLD.component THEN RAISE EXCEPTION 'immutable port lease' USING ERRCODE='23514'; END IF; RETURN NEW; END $$;

CREATE TRIGGER immutable_port_lease BEFORE UPDATE OF lease_id,port,deployment_id,component ON port_assignments FOR EACH ROW EXECUTE FUNCTION dc2_immutable_port_lease();

CREATE UNIQUE INDEX port_lease_owner ON port_assignments(deployment_id,component,generation);

ALTER TABLE decisions ADD COLUMN search_vector TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple',coalesce(title,'')||' '||coalesce(body,'')||' '||coalesce(technical_note,'')||' '||coalesce(ref,''))) STORED;

CREATE INDEX decisions_search ON decisions USING GIN(search_vector);

ALTER TABLE sketches ADD COLUMN searchable TEXT NOT NULL DEFAULT '';

ALTER TABLE sketches ADD COLUMN search_vector TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple',searchable)) STORED;

CREATE INDEX sketches_search ON sketches USING GIN(search_vector);

CREATE INDEX tasks_search ON tasks USING GIN (to_tsvector('simple',coalesce(title,'')||' '||coalesce(outcome,'')||' '||coalesce(technical_note,'')));

CREATE INDEX deployments_spec_json ON deployments USING GIN(spec_json jsonb_path_ops);

CREATE INDEX storage_artifacts_record_json ON storage_artifacts USING GIN(record_json jsonb_path_ops);

CREATE INDEX storage_jobs_job_json ON storage_jobs USING GIN(job_json jsonb_path_ops);

CREATE INDEX review_records_record_json ON review_records USING GIN(record_json jsonb_path_ops);

CREATE INDEX metric_minutes_expiry_high ON metric_minutes(minute_utc,rowid)
  WHERE subject_kind IN ('component','container','test');
CREATE INDEX metric_minutes_expiry_low ON metric_minutes(minute_utc,rowid)
  WHERE subject_kind NOT IN ('component','container','test');

CREATE INDEX plan_events_latest_kind ON plan_events(subject_kind,subject_id,event,event_id DESC);
CREATE INDEX metric_minutes_rowid_order ON metric_minutes(rowid);
CREATE INDEX agent_hub_deliveries_recipient_cursor ON agent_hub_deliveries(recipient_agent_id,created_at,delivery_id);
CREATE INDEX agent_hub_messages_cursor ON agent_hub_messages(created_at,message_id);
