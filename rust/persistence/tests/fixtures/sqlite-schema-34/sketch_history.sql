CREATE TABLE IF NOT EXISTS sketch_lineage (
  lineage_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  parent_sketch_id TEXT NOT NULL REFERENCES sketches(sketch_id),
  child_sketch_id TEXT NOT NULL REFERENCES sketches(sketch_id),
  relation TEXT NOT NULL CHECK(relation IN ('derived_from','adjusted_from','supersedes','reintroduced_from')),
  rationale TEXT NOT NULL,
  actor TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(parent_sketch_id,child_sketch_id,relation)
);
CREATE INDEX IF NOT EXISTS sketch_lineage_parent ON sketch_lineage(repository_id,parent_sketch_id,created_at);
CREATE INDEX IF NOT EXISTS sketch_lineage_child ON sketch_lineage(repository_id,child_sketch_id,created_at);
CREATE TABLE IF NOT EXISTS sketch_description_history (
  sketch_id TEXT NOT NULL REFERENCES sketches(sketch_id),
  revision INTEGER NOT NULL,
  description TEXT NOT NULL,
  journey TEXT NOT NULL,
  decisions TEXT NOT NULL,
  instructions TEXT NOT NULL,
  constraints TEXT NOT NULL,
  rationale TEXT NOT NULL,
  actor TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(sketch_id,revision)
);
CREATE INDEX IF NOT EXISTS sketch_description_history_sketch ON sketch_description_history(sketch_id,revision);
CREATE TABLE IF NOT EXISTS sketch_surface_activations (
  activation_id TEXT PRIMARY KEY,
  repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
  surface_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  action TEXT NOT NULL CHECK(action IN ('select','restore','supersede')),
  sketch_ids_json TEXT NOT NULL,
  rationale TEXT NOT NULL,
  actor TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(repository_id,surface_id,revision)
);
CREATE INDEX IF NOT EXISTS sketch_surface_activations_current ON sketch_surface_activations(repository_id,surface_id,revision DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS sketches_fts USING fts5(
  sketch_id UNINDEXED,
  repository_id UNINDEXED,
  surface_id UNINDEXED,
  searchable
);

CREATE INDEX IF NOT EXISTS sketches_surface ON sketches(repository_id,surface_id,created_at);
CREATE INDEX IF NOT EXISTS sketches_legacy ON sketches(repository_id,legacy);
CREATE TABLE IF NOT EXISTS sketch_surfaces (
 repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
 surface_id TEXT NOT NULL, title TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(repository_id,surface_id)
);
INSERT OR IGNORE INTO sketch_surfaces(repository_id,surface_id,title,revision)
 SELECT repository_id,surface_id,MAX(surface_title),0 FROM sketches WHERE legacy=0 GROUP BY repository_id,surface_id;
CREATE TRIGGER IF NOT EXISTS sketch_identity_immutable BEFORE UPDATE OF batch_id,repository_id,file_path,sha256,byte_size,width,height,surface_id,display_order,legacy,manifest_version ON sketches
 BEGIN SELECT RAISE(ABORT,'mockup identity is immutable'); END;
CREATE TRIGGER IF NOT EXISTS sketch_no_delete BEFORE DELETE ON sketches BEGIN SELECT RAISE(ABORT,'mockup history is permanent'); END;
CREATE TRIGGER IF NOT EXISTS sketch_lineage_no_update BEFORE UPDATE ON sketch_lineage BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_lineage_no_delete BEFORE DELETE ON sketch_lineage BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_description_history_no_update BEFORE UPDATE ON sketch_description_history BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_description_history_no_delete BEFORE DELETE ON sketch_description_history BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_surface_activations_no_update BEFORE UPDATE ON sketch_surface_activations BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_surface_activations_no_delete BEFORE DELETE ON sketch_surface_activations BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_decision_history_no_update BEFORE UPDATE ON sketch_decision_history BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
CREATE TRIGGER IF NOT EXISTS sketch_decision_history_no_delete BEFORE DELETE ON sketch_decision_history BEGIN SELECT RAISE(ABORT,'mockup history is append-only'); END;
