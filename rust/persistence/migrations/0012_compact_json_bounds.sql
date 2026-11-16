-- PostgreSQL's JSONB display adds spaces and expands decimal exponents. The
-- authority limit applies to a compact, lossless UTF-8 JSON encoding instead.
-- Numbers use the shorter exact plain or integer-coefficient exponent form;
-- no floating-point conversion or precision loss is involved.
CREATE FUNCTION dc2_jsonb_compact_bytes(document JSONB) RETURNS BIGINT
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE
 kind TEXT=jsonb_typeof(document);
 result BIGINT;
 number_text TEXT;
 digits TEXT;
 fraction_length INTEGER;
 trailing_zeroes INTEGER;
 power INTEGER;
 sign_bytes INTEGER;
 plain_bytes BIGINT;
 exponent_bytes BIGINT;
BEGIN
 IF kind='object' THEN
  SELECT 2+coalesce(sum(octet_length(to_jsonb(entry.key)::text)+1+dc2_jsonb_compact_bytes(entry.value)),0)+greatest(count(*)-1,0)
   INTO result FROM jsonb_each(document) entry;
  RETURN result;
 ELSIF kind='array' THEN
  SELECT 2+coalesce(sum(dc2_jsonb_compact_bytes(entry.value)),0)+greatest(count(*)-1,0)
   INTO result FROM jsonb_array_elements(document) entry;
  RETURN result;
 ELSIF kind='number' THEN
  number_text=document::text;
  sign_bytes=CASE WHEN left(number_text,1)='-' THEN 1 ELSE 0 END;
  IF sign_bytes=1 THEN number_text=substring(number_text FROM 2); END IF;
  fraction_length=CASE WHEN strpos(number_text,'.')=0 THEN 0 ELSE length(number_text)-strpos(number_text,'.') END;
  digits=ltrim(replace(number_text,'.',''),'0');
  IF digits='' THEN RETURN 1; END IF;
  trailing_zeroes=length(digits)-length(rtrim(digits,'0'));
  digits=rtrim(digits,'0');
  power=trailing_zeroes-fraction_length;
  plain_bytes=CASE WHEN power>=0 THEN length(digits)+power
    WHEN length(digits)+power>0 THEN length(digits)+1
    ELSE 2-power END;
  exponent_bytes=length(digits)+CASE WHEN power=0 THEN 0 ELSE 1+length(power::text) END;
  RETURN sign_bytes+least(plain_bytes,exponent_bytes);
 END IF;
 RETURN octet_length(document::text);
END $$;

ALTER TABLE review_records DROP CONSTRAINT review_records_record_json_check;
ALTER TABLE review_records ADD CONSTRAINT review_records_record_json_check CHECK(dc2_jsonb_compact_bytes(record_json)<=8192);
ALTER TABLE release_evidence DROP CONSTRAINT release_evidence_receipt_json_check;
ALTER TABLE release_evidence ADD CONSTRAINT release_evidence_receipt_json_check CHECK(dc2_jsonb_compact_bytes(receipt_json)<=8192);
ALTER TABLE repository_retirement_receipts DROP CONSTRAINT repository_retirement_receipts_removed_worktree_ids_json_check;
ALTER TABLE repository_retirement_receipts ADD CONSTRAINT repository_retirement_receipts_removed_worktree_ids_json_check CHECK(dc2_jsonb_compact_bytes(removed_worktree_ids_json)<=8192);
ALTER TABLE owned_events DROP CONSTRAINT owned_events_payload_json_check;
ALTER TABLE owned_events ADD CONSTRAINT owned_events_payload_json_check CHECK(dc2_jsonb_compact_bytes(payload_json)<=8192);
