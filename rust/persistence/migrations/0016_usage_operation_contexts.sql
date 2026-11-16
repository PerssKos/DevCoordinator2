-- Content-free request metadata, never a mirror of collector measurements.
ALTER TABLE codex_usage_repository_links
    DROP CONSTRAINT codex_usage_repository_links_codex_repository_id_check;
ALTER TABLE codex_usage_repository_links
    ADD CONSTRAINT codex_usage_repository_links_codex_repository_id_check
    CHECK (codex_repository_id ~ '^[0-9a-fA-F]{64}$'
        OR (source_schema = 3 AND codex_repository_id ~ '^repo_[0-9a-f]{40}$'));

CREATE TABLE codex_usage_contexts (
    context_seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    context_json TEXT NOT NULL UNIQUE CHECK (octet_length(context_json) <= 4096)
);
CREATE TABLE codex_usage_operation_contexts (
    source_uid BIGINT NOT NULL CHECK (source_uid >= 0),
    operation_id TEXT NOT NULL CHECK (length(operation_id) BETWEEN 1 AND 256),
    repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
    captured_at_ms BIGINT NOT NULL CHECK (captured_at_ms >= 0),
    context_seq BIGINT NOT NULL REFERENCES codex_usage_contexts(context_seq),
    rejected BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (source_uid, operation_id)
);
CREATE INDEX codex_usage_operation_contexts_window
    ON codex_usage_operation_contexts(repository_id, source_uid, captured_at_ms, operation_id);
