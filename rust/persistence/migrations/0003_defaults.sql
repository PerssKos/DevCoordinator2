-- Idempotent fresh-database defaults; existing historical values always win.
INSERT INTO meta(key,value) VALUES('usage_rate_card_revision','1') ON CONFLICT DO NOTHING;

INSERT INTO usage_rate_cards(
  card_id,version,provider,model_pattern,processing_tier,context_tier,effective_from_ms,
  effective_to_ms,input_usd_micros_per_million,cached_input_usd_micros_per_million,
  cache_write_usd_micros_per_million,output_usd_micros_per_million,source_ref,active,
  created_at,created_by
) VALUES
  ('openai-standard-gpt-6-astra',1,'openai','gpt-6-astra','standard','short',0,NULL,10000000,1000000,12500000,50000000,'https://developers.openai.com/api/docs/pricing',1,'2026-09-01T00:00:00Z','system-rate-card'),
  ('openai-standard-gpt-6.1-sol',1,'openai','gpt-6.1-sol','standard','short',0,NULL,2000000,100000,2500000,10000000,'https://developers.openai.com/api/docs/pricing',1,'2026-10-01T00:00:00Z','system-rate-card'),
  ('openai-standard-gpt-6-sol',1,'openai','gpt-6-sol','standard','short',0,NULL,2000000,200000,2500000,10000000,'https://developers.openai.com/api/docs/pricing',1,'2026-09-01T00:00:00Z','system-rate-card'),
  ('openai-standard-gpt-6-luna',1,'openai','gpt-6-luna','standard','short',0,NULL,100000,10000,125000,500000,'https://developers.openai.com/api/docs/pricing',1,'2026-09-01T00:00:00Z','system-rate-card'),
  ('openai-standard-gpt-5.6-sol',1,'openai','gpt-5.6-sol','standard','short',0,NULL,4000000,400000,5000000,20000000,'https://developers.openai.com/api/docs/pricing',1,'2026-09-01T00:00:00Z','system-rate-card') ON CONFLICT DO NOTHING;

INSERT INTO test_log_retention_state(singleton,max_age_seconds,case_depth,updated_at,updated_by)
VALUES(1,86400,3,to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS"Z"'),'schema-default') ON CONFLICT DO NOTHING;

INSERT INTO usage_rate_cards(card_id,version,provider,model_pattern,processing_tier,context_tier,effective_from_ms,effective_to_ms,input_usd_micros_per_million,cached_input_usd_micros_per_million,cache_write_usd_micros_per_million,output_usd_micros_per_million,source_ref,active,created_at,created_by)
SELECT card_id||'-long',version,provider,model_pattern,processing_tier,'long',effective_from_ms,
 effective_to_ms,input_usd_micros_per_million*2,cached_input_usd_micros_per_million*2,
 cache_write_usd_micros_per_million*2,output_usd_micros_per_million*3/2,source_ref,active,
 created_at,created_by FROM usage_rate_cards WHERE created_by='system-rate-card' AND context_tier='short' ON CONFLICT DO NOTHING;

INSERT INTO ticket_settings(singleton,upstream,revision)
  VALUES(1,'https://vr.ae',1) ON CONFLICT DO NOTHING;

INSERT INTO storage_scan_state VALUES(1,1,NULL,'[]','["not_scanned"]') ON CONFLICT DO NOTHING;

INSERT INTO sketch_surfaces(repository_id,surface_id,title,revision)
 SELECT repository_id,surface_id,MAX(surface_title),0 FROM sketches WHERE legacy=0 GROUP BY repository_id,surface_id ON CONFLICT DO NOTHING;
INSERT INTO meta(key,value) VALUES('schema_version','35') ON CONFLICT(key) DO UPDATE SET value=excluded.value;
CREATE TABLE metric_minutes_default PARTITION OF metric_minutes DEFAULT;
CREATE TABLE dc2_metric_partitions(table_name TEXT PRIMARY KEY,range_start TEXT NOT NULL,range_end TEXT NOT NULL);
