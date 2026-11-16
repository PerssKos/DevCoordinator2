-- These exact-key projections serve short authority requests. Repeated probes
-- must not revisit a GIN pending list accumulated by unrelated catalog writes.
ALTER INDEX public.storage_artifacts_ancestors_live SET (fastupdate = off);
ALTER INDEX public.storage_leases_artifacts SET (fastupdate = off);
SELECT pg_catalog.gin_clean_pending_list('public.storage_artifacts_ancestors_live'::regclass);
SELECT pg_catalog.gin_clean_pending_list('public.storage_leases_artifacts'::regclass);

-- A scoped TestList cursor must not filter unrelated worktrees to find the
-- next page. The global cursor continues to use the worktree primary key.
CREATE INDEX worktrees_repository_seek ON public.worktrees(repository_id,worktree_id);
