-- pg_restore clears the caller's search_path while loading table data. Keep
-- recursive compact-size checks resolvable under that restored session too.
ALTER FUNCTION public.dc2_jsonb_compact_bytes(JSONB)
 SET search_path TO pg_catalog,public;
