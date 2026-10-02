//! Project-scoped retained sketches and postmortem review state.
use crate::access::Caller;
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::ids;
use crate::platform::{Clock, HostClock};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use devcoordinator2_api::params::{self, SketchDecision};
use devcoordinator2_api::results;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHUNK: u32 = 180 * 1024;
const MAX_RECORD: u64 = 32 * 1024 * 1024;
const MANIFEST_VERSION: u8 = 2;

const SUMMARY_COLUMNS: &str = "sketches.sketch_id,sketches.repository_id,sketch_batches.sketch_set,sketch_batches.source_skill,sketches.title,sketches.sha256,sketches.byte_size,sketches.width,sketches.height,sketches.created_at,sketches.decision,sketches.decision_revision,sketches.surface_id,sketches.surface_title,sketches.element_ids_json,sketches.state_name,sketches.theme,sketches.viewport,sketches.description,sketches.journey,sketches.decisions,sketches.instructions,sketches.constraints,sketches.transition_note,sketches.manifest_version,sketches.legacy,sketches.description_revision";

struct PreparedSketch {
    id: String,
    title: String,
    bytes: Vec<u8>,
    digest: String,
    width: u32,
    height: u32,
    manifest: params::SketchManifest,
}

#[derive(Clone)]
pub struct SketchService {
    database: Database,
    state_dir: PathBuf,
    clock: Arc<dyn Clock>,
}
impl SketchService {
    pub fn new(config: &Config, database: Database) -> Self {
        Self {
            database,
            state_dir: config.state_dir.clone(),
            clock: Arc::new(HostClock),
        }
    }
    pub fn with_clock(config: &Config, database: Database, clock: Arc<dyn Clock>) -> Self {
        Self {
            database,
            state_dir: config.state_dir.clone(),
            clock,
        }
    }
    fn now(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format sketch timestamp")
                    .with_detail(e.to_string())
            })
    }
    pub fn publish(
        &self,
        p: params::SketchPublish,
        caller: &Caller,
    ) -> Result<results::SketchBatch, ProtocolError> {
        if !caller.is_local() {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "sketch publishing is available to local agents",
            ));
        }
        if p.images.is_empty() || p.images.len() > 64 {
            return Err(invalid("images must contain 1..64 entries"));
        }
        if p.manifest_version != MANIFEST_VERSION {
            return Err(invalid("manifest_version must be 2 for new sketches"));
        }
        let existing = {
            let key = p.idempotency_key.clone();
            let repo = p.repository_id.clone();
            self.database
                .call(move |c| {
                    c.query_row(
                        "SELECT batch_id FROM sketch_batches WHERE repository_id=?1 AND idempotency_key=?2",
                        rusqlite::params![repo, key],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
                })
                .map_err(db_error)?
        };
        if let Some(existing) = existing {
            return self.batch(&p.repository_id, &existing);
        }
        let surface_id = p
            .images
            .first()
            .map(|image| image.manifest.surface_id.clone())
            .ok_or_else(|| invalid("images must contain one manifest"))?;
        let mut prepared = Vec::with_capacity(p.images.len());
        let mut digests = HashSet::new();
        for input in &p.images {
            validate_manifest(&input.manifest)?;
            if input.manifest.surface_id != surface_id {
                return Err(invalid("one publish batch must target one surface"));
            }
            let bytes = read_file(&input.path, MAX_BYTES)?;
            let (width, height) =
                png_dimensions(&bytes).ok_or_else(|| invalid("sketch images must be PNG files"))?;
            let digest = sha(&bytes);
            if !digests.insert(digest.clone()) {
                return Err(invalid("a publish batch cannot repeat one image file"));
            }
            prepared.push(PreparedSketch {
                id: ids::sketch_id().map_err(id_error)?,
                title: input.title.clone(),
                bytes,
                digest,
                width,
                height,
                manifest: input.manifest.clone(),
            });
        }
        let root = self.state_dir.join("sketches").join(&p.repository_id);
        fs::create_dir_all(&root).map_err(io_error)?;
        let batch_id = ids::sketch_batch_id().map_err(id_error)?;
        let now = self.now()?;
        let record = read_file(&p.generation_record_path, MAX_RECORD)?;
        let record_sha = sha(&record);
        self.validate_publish_references(&p.repository_id, &prepared)?;
        let batch_dir = root.join(&batch_id);
        fs::create_dir_all(&batch_dir).map_err(io_error)?;
        let record_path = batch_dir.join("generation-record.bin");
        fs::write(&record_path, &record).map_err(io_error)?;
        for sketch in &prepared {
            let image_path = batch_dir.join(format!("{}.png", sketch.id));
            fs::write(&image_path, &sketch.bytes).map_err(io_error)?;
        }
        let repo = p.repository_id.clone();
        let set = p.sketch_set.clone();
        let skill = p.source_skill.clone();
        let idem = p.idempotency_key.clone();
        let gen_path = record_path.to_string_lossy().into_owned();
        let record_size = record.len() as i64;
        let batch_for_insert = batch_id.clone();
        let batch_for_rows = batch_id.clone();
        let now_for_insert = now.clone();
        let actor_for_insert = caller.actor();
        let repo_for_insert = repo.clone();
        let batch_dir_for_tx = batch_dir.clone();
        let transaction = self.database.transaction(move |tx| {
            tx.execute("INSERT INTO sketch_batches(batch_id,repository_id,sketch_set,source_skill,generation_record_path,generation_record_size,generation_record_sha256,manifest_version,idempotency_key,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    rusqlite::params![batch_for_insert,repo_for_insert,set,skill,gen_path,record_size,record_sha,MANIFEST_VERSION,idem,now_for_insert,actor_for_insert])?;
            for sketch in prepared {
                let image_path = batch_dir_for_tx.join(format!("{}.png", sketch.id));
                let element_ids = serde_json::to_string(&sketch.manifest.element_ids).map_err(|error| DatabaseError::Startup(error.to_string()))?;
                tx.execute("INSERT INTO sketches(sketch_id,batch_id,repository_id,title,file_path,byte_size,sha256,mime,width,height,decision,decision_revision,surface_id,surface_title,element_ids_json,state_name,theme,viewport,description,journey,decisions,instructions,constraints,transition_note,manifest_version,legacy,description_revision,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,'image/png',?8,?9,'undecided',0,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,2,0,0,?22,?23)",
                    rusqlite::params![sketch.id,batch_for_rows,repo,sketch.title,image_path.to_string_lossy().to_string(),sketch.bytes.len() as i64,sketch.digest,sketch.width,sketch.height,sketch.manifest.surface_id,sketch.manifest.surface_title,element_ids,sketch.manifest.state,sketch.manifest.theme,sketch.manifest.viewport,sketch.manifest.description,sketch.manifest.journey,sketch.manifest.decisions,sketch.manifest.instructions,sketch.manifest.constraints,sketch.manifest.transition_note,now,actor_for_insert])?;
                tx.execute("INSERT INTO sketch_description_history(sketch_id,revision,description,journey,decisions,instructions,constraints,rationale,actor,created_at) VALUES(?1,0,?2,?3,?4,?5,?6,?7,?8,?9)", rusqlite::params![sketch.id,sketch.manifest.description,sketch.manifest.journey,sketch.manifest.decisions,sketch.manifest.instructions,sketch.manifest.constraints,"Initial agent-authored mockup context",actor_for_insert,now])?;
                for parent in sketch.manifest.parent_relations {
                    let relation = lineage_relation_text(&parent.relation);
                    let lineage_id = ids::sketch_lineage_id().map_err(|error| DatabaseError::Startup(error.to_string()))?;
                    tx.execute("INSERT INTO sketch_lineage(lineage_id,repository_id,parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", rusqlite::params![lineage_id,repo, parent.parent_sketch_id,sketch.id,relation,sketch.manifest.transition_note,actor_for_insert,now])?;
                }
            }
            Ok(())
        });
        if let Err(error) = transaction {
            let _ = fs::remove_dir_all(&batch_dir);
            return Err(db_error(error));
        }
        self.refresh_search_index()?;
        self.batch(&p.repository_id, &batch_id)
    }
    fn validate_publish_references(
        &self,
        repository_id: &str,
        prepared: &[PreparedSketch],
    ) -> Result<(), ProtocolError> {
        let repo = repository_id.to_owned();
        let digests = prepared
            .iter()
            .map(|sketch| sketch.digest.clone())
            .collect::<Vec<_>>();
        let parents = prepared
            .iter()
            .flat_map(|sketch| {
                sketch
                    .manifest
                    .parent_relations
                    .iter()
                    .map(|parent| {
                        (
                            parent.parent_sketch_id.clone(),
                            sketch.manifest.surface_id.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        self.database
            .call(move |connection| {
                for digest in &digests {
                    let duplicate: Option<(Option<String>, bool)> = connection
                        .query_row(
                            "SELECT surface_id,legacy FROM sketches WHERE repository_id=?1 AND sha256=?2 LIMIT 1",
                            rusqlite::params![repo, digest],
                            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
                        )
                        .optional()?;
                    if duplicate.is_some() {
                        return Err(DatabaseError::Domain(invalid(
                            "image digest is already retained; publish a new image file",
                        )));
                    }
                }
                for (parent, surface) in &parents {
                    let parent_surface: Option<(Option<String>, bool)> = connection
                        .query_row(
                            "SELECT surface_id,legacy FROM sketches WHERE repository_id=?1 AND sketch_id=?2",
                            rusqlite::params![repo, parent],
                            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
                        )
                        .optional()?;
                    let Some((parent_surface, legacy)) = parent_surface else {
                        return Err(DatabaseError::Domain(invalid(
                            "lineage parent sketch was not found in this repository",
                        )));
                    };
                    if legacy || parent_surface.as_deref() != Some(surface.as_str()) {
                        return Err(DatabaseError::Domain(invalid(
                            "lineage parents must be manifest-complete sketches on the same surface",
                        )));
                    }
                }
                Ok(())
            })
            .map_err(db_error)
    }
    fn refresh_search_index(&self) -> Result<(), ProtocolError> {
        self.database
            .call(|connection| {
                connection.execute("DELETE FROM sketches_fts", [])?;
                let mut statement = connection.prepare(
                    "SELECT sketch_id,repository_id,COALESCE(surface_id,''),title,
                            COALESCE(surface_title,''),element_ids_json,
                            COALESCE(state_name,''),COALESCE(theme,''),COALESCE(viewport,''),
                            description,journey,decisions,instructions,constraints,transition_note
                     FROM sketches",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        let text = (3..15)
                            .map(|index| row.get::<_, String>(index))
                            .collect::<Result<Vec<_>, _>>()?
                            .join(" ");
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            text,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (sketch_id, repository_id, surface_id, text) in rows {
                    connection.execute(
                        "INSERT INTO sketches_fts(sketch_id,repository_id,surface_id,searchable) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![sketch_id, repository_id, surface_id, text],
                    )?;
                }
                Ok(())
            })
            .map_err(db_error)
    }
    fn batch(
        &self,
        repository_id: &str,
        batch_id: &str,
    ) -> Result<results::SketchBatch, ProtocolError> {
        let repo = repository_id.to_owned();
        let bid = batch_id.to_owned();
        self.database.call(move |c| {
            let b=c.query_row("SELECT batch_id,repository_id,sketch_set,source_skill,generation_record_size,generation_record_sha256,created_at FROM sketch_batches WHERE repository_id=?1 AND batch_id=?2",rusqlite::params![repo,bid],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)? as u64,r.get::<_,String>(5)?,r.get::<_,String>(6)?)))?;
            let mut st=c.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.batch_id=?1 ORDER BY sketches.rowid"))?;
            let sketches=st.query_map([&bid],summary_row).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            Ok(results::SketchBatch{batch_id:b.0,repository_id:b.1,sketch_set:b.2,source_skill:b.3,generation_record_size:b.4,generation_record_sha256:b.5,created_at:b.6,sketches})
        }).map_err(db_error)
    }
    pub fn list(&self, p: params::SketchList) -> Result<results::SketchListResult, ProtocolError> {
        let repo = p.repository_id.clone();
        let skill = p.source_skill.clone();
        let dec = p.decision.clone();
        let set = p.sketch_set.clone();
        let surface = p.surface_id.clone();
        let current_only = p.current_only;
        let include_legacy = p.include_legacy;
        let offset = p.offset;
        let limit = p.limit.clamp(1, 100);
        self.database.call(move|c|{
            let mut sql=format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1");
            let mut vals:Vec<rusqlite::types::Value>=vec![repo.clone().into()];
            if let Some(v)=skill { sql.push_str(" AND source_skill=?"); vals.push(v.into()); }
            if let Some(v)=set { sql.push_str(" AND sketch_set=?"); vals.push(v.into()); }
            if let Some(v)=dec { sql.push_str(" AND decision=?"); vals.push(serde_json::to_string(&v).unwrap().trim_matches('"').to_owned().into()); }
            if let Some(v)=surface { sql.push_str(" AND sketches.surface_id=?"); vals.push(v.into()); }
            if !include_legacy { sql.push_str(" AND sketches.legacy=0"); }
            if current_only {
                sql.push_str(" AND EXISTS (SELECT 1 FROM sketch_surface_activations a, json_each(a.sketch_ids_json) j WHERE a.repository_id=sketches.repository_id AND a.surface_id=sketches.surface_id AND j.value=sketches.sketch_id AND a.revision=(SELECT MAX(a2.revision) FROM sketch_surface_activations a2 WHERE a2.repository_id=a.repository_id AND a2.surface_id=a.surface_id))");
            }
            // Fetch one extra row so callers can continue without losing the
            // newest records when a repository has more than one page.
            sql.push_str(&format!(" ORDER BY sketches.created_at DESC, sketches.rowid DESC LIMIT {} OFFSET {}", u32::from(limit) + 1, offset));
            let mut st=c.prepare(&sql)?; let rows=st.query_map(rusqlite::params_from_iter(vals),summary_row)?.collect::<Result<Vec<_>,_>>()?;
            let has_more = rows.len() > usize::from(limit);
            let current_ids = repo.clone();
            let mut sketches = rows.into_iter().take(usize::from(limit)).collect::<Vec<_>>();
            for sketch in &mut sketches {
                sketch.current = if let Some(surface) = sketch.surface_id.as_deref() {
                    is_current_sketch(c, Some(surface), &sketch.sketch_id, &current_ids)?
                } else {
                    false
                };
            }
            Ok(results::SketchListResult{repository_id:repo,has_more,sketches})
        }).map_err(db_error)
    }
    pub fn search(
        &self,
        p: params::SketchSearch,
    ) -> Result<results::SketchListResult, ProtocolError> {
        let repo = p.repository_id.clone();
        let query = literal_fts_query(&p.query);
        let surface = p.surface_id.clone();
        let current_only = p.current_only;
        let include_legacy = p.include_legacy;
        let offset = p.offset;
        let limit = p.limit.clamp(1, 100);
        self.database.call(move |c| {
            let mut sql = format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) JOIN sketches_fts f ON f.sketch_id=sketches.sketch_id WHERE f.repository_id=?1 AND sketches_fts MATCH ?2");
            let mut vals: Vec<rusqlite::types::Value> = vec![repo.clone().into(), query.into()];
            if let Some(surface) = surface { sql.push_str(" AND sketches.surface_id=?"); vals.push(surface.into()); }
            if !include_legacy { sql.push_str(" AND sketches.legacy=0"); }
            if current_only { sql.push_str(" AND EXISTS (SELECT 1 FROM sketch_surface_activations a, json_each(a.sketch_ids_json) j WHERE a.repository_id=sketches.repository_id AND a.surface_id=sketches.surface_id AND j.value=sketches.sketch_id AND a.revision=(SELECT MAX(a2.revision) FROM sketch_surface_activations a2 WHERE a2.repository_id=a.repository_id AND a2.surface_id=a.surface_id))"); }
            sql.push_str(&format!(" ORDER BY sketches.created_at DESC, sketches.rowid DESC LIMIT {} OFFSET {}", u32::from(limit) + 1, offset));
            let mut statement = c.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(vals), summary_row)?.collect::<Result<Vec<_>, _>>()?;
            let has_more = rows.len() > usize::from(limit);
            let mut sketches = rows.into_iter().take(usize::from(limit)).collect::<Vec<_>>();
            for sketch in &mut sketches {
                sketch.current = if let Some(surface) = sketch.surface_id.as_deref() {
                    is_current_sketch(c, Some(surface), &sketch.sketch_id, &repo)?
                } else {
                    false
                };
            }
            Ok(results::SketchListResult { repository_id: repo, has_more, sketches })
        }).map_err(db_error)
    }
    pub fn story(
        &self,
        p: params::SketchStory,
    ) -> Result<results::SketchStoryResult, ProtocolError> {
        let repo = p.repository_id.clone();
        let surface = p.surface_id.clone();
        let limit = p.limit.clamp(1, 200);
        let include_legacy = p.include_legacy;
        self.database.call(move |c| {
            let mut sql = format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketches.surface_id=?2");
            if !include_legacy { sql.push_str(" AND sketches.legacy=0"); }
            sql.push_str(&format!(" ORDER BY sketches.created_at ASC, sketches.rowid ASC LIMIT {}", u32::from(limit) + 1));
            let mut statement = c.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params![repo, surface], summary_row)?.collect::<Result<Vec<_>, _>>()?;
            let has_more = rows.len() > usize::from(limit);
            let mut nodes = rows.into_iter().take(usize::from(limit)).collect::<Vec<_>>();
            let current_ids = current_ids(c, &repo, &surface)?;
            for node in &mut nodes { node.current = current_ids.iter().any(|id| id == &node.sketch_id); }
            let current = nodes.iter().filter(|node| node.current).cloned().collect::<Vec<_>>();
            let surface_title = nodes.iter().find_map(|node| node.surface_title.clone());
            let mut edge_stmt = c.prepare("SELECT parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at FROM sketch_lineage WHERE repository_id=?1 AND (parent_sketch_id IN (SELECT sketch_id FROM sketches WHERE repository_id=?1 AND surface_id=?2) OR child_sketch_id IN (SELECT sketch_id FROM sketches WHERE repository_id=?1 AND surface_id=?2)) ORDER BY created_at")?;
            let lineage = edge_stmt.query_map(rusqlite::params![repo, surface], |r| Ok(results::SketchLineageEvent { parent_sketch_id:r.get(0)?, child_sketch_id:r.get(1)?, relation:parse_lineage_relation(&r.get::<_,String>(2)?), rationale:r.get(3)?, actor:r.get(4)?, created_at:r.get(5)? })).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let mut activation_stmt = c.prepare("SELECT revision,action,sketch_ids_json,rationale,actor,created_at FROM sketch_surface_activations WHERE repository_id=?1 AND surface_id=?2 ORDER BY revision")?;
            let activations = activation_stmt.query_map(rusqlite::params![repo, surface], |r| {
                let ids = serde_json::from_str::<Vec<String>>(&r.get::<_,String>(2)?).unwrap_or_default();
                Ok(results::SketchActivationEvent { revision:r.get(0)?, action:parse_activation_action(&r.get::<_,String>(1)?), sketch_ids:ids, rationale:r.get(3)?, actor:r.get(4)?, created_at:r.get(5)? })
            }).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            Ok(results::SketchStoryResult { repository_id:repo, surface_id:surface, surface_title, current, nodes, lineage, activations, has_more })
        }).map_err(db_error)
    }
    pub fn resolve(
        &self,
        p: params::SketchResolve,
    ) -> Result<results::SketchResolveResult, ProtocolError> {
        let repo = p.repository_id.clone();
        let surface = p.surface_id.clone();
        self.database.call(move |c| {
            let ids = current_ids(c, &repo, &surface)?;
            let revision: u32 = c.query_row("SELECT COALESCE(MAX(revision),0) FROM sketch_surface_activations WHERE repository_id=?1 AND surface_id=?2", rusqlite::params![repo, surface], |r| r.get(0))?;
            let mut nodes = Vec::new();
            for id in &ids {
                let summary = c.query_row(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketches.sketch_id=?2"), rusqlite::params![repo, id], summary_row)?;
                if !summary.legacy { nodes.push(summary); }
            }
            let (status, rationale) = if !nodes.is_empty() && nodes.len() == ids.len() {
                if nodes.len() == 1 { (results::SketchResolveStatus::Resolved, "Explicit current selection is authoritative.".to_owned()) }
                else { (results::SketchResolveStatus::Resolved, "Multiple current heads were explicitly selected for continuation.".to_owned()) }
            } else {
                let total: i64 = c.query_row("SELECT COUNT(*) FROM sketches WHERE repository_id=?1 AND surface_id=?2", rusqlite::params![repo, surface], |r| r.get(0))?;
                if total > 0 { (results::SketchResolveStatus::LegacyOnly, "Only historical legacy sketches exist for this surface.".to_owned()) }
                else { (results::SketchResolveStatus::Unavailable, "No current selection exists for this surface.".to_owned()) }
            };
            Ok(results::SketchResolveResult { repository_id:repo, surface_id:surface, status, revision, rationale, current:nodes })
        }).map_err(db_error)
    }
    pub fn get(
        &self,
        p: params::SketchReference,
        actor: &str,
    ) -> Result<results::SketchDetail, ProtocolError> {
        let repo = p.repository_id.clone();
        let id = p.sketch_id.clone();
        let actor = actor.to_owned();
        self.database.call(move|c|{
            let row=c.query_row(&format!("SELECT {SUMMARY_COLUMNS},sketches.batch_id FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketches.sketch_id=?2"),rusqlite::params![repo,id],|r| Ok((summary_row(r)?,r.get::<_,String>(27)?)))?;
            let (summary,batch_id)=row;
            let (size,digest):(u64,String)=c.query_row("SELECT generation_record_size,generation_record_sha256 FROM sketch_batches WHERE batch_id=?1",[batch_id],|r|Ok((r.get::<_,i64>(0)? as u64,r.get(1)?)))?;
            let mut st=c.prepare("SELECT revision,decision,rationale,actor,created_at FROM sketch_decision_history WHERE sketch_id=?1 ORDER BY revision")?;
            let history=st.query_map([&summary.sketch_id],|r|Ok(results::SketchDecisionEvent{revision:r.get(0)?,decision:serde_json::from_str(&format!("\"{}\"",r.get::<_,String>(1)?)).unwrap_or(SketchDecision::Undecided),rationale:r.get(2)?,actor:r.get(3)?,created_at:r.get(4)?})).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let mut ann=c.prepare("SELECT annotation_id,sketch_id,body,marks_json,state,created_by,created_at,updated_at FROM sketch_annotations WHERE sketch_id=?1 AND state!='deleted' ORDER BY created_at")?;
            let annotations=ann.query_map([&summary.sketch_id],|r| annotation_row(r,&actor)).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let mut lineage_stmt=c.prepare("SELECT parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at FROM sketch_lineage WHERE repository_id=?1 AND (parent_sketch_id=?2 OR child_sketch_id=?2) ORDER BY created_at")?;
            let lineage=lineage_stmt.query_map(rusqlite::params![summary.repository_id,summary.sketch_id],|r| Ok(results::SketchLineageEvent{parent_sketch_id:r.get(0)?,child_sketch_id:r.get(1)?,relation:parse_lineage_relation(&r.get::<_,String>(2)?),rationale:r.get(3)?,actor:r.get(4)?,created_at:r.get(5)?})).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let mut descriptions=c.prepare("SELECT revision,description,journey,decisions,instructions,constraints,rationale,actor,created_at FROM sketch_description_history WHERE sketch_id=?1 ORDER BY revision")?;
            let description_history=descriptions.query_map([&summary.sketch_id],|r| Ok(results::SketchDescriptionRevision{revision:r.get(0)?,description:r.get(1)?,journey:r.get(2)?,decisions:r.get(3)?,instructions:r.get(4)?,constraints:r.get(5)?,rationale:r.get(6)?,actor:r.get(7)?,created_at:r.get(8)?})).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let current = is_current_sketch(c, summary.surface_id.as_deref(), &summary.sketch_id, &summary.repository_id)?;
            let mut sketch = summary;
            sketch.current = current;
            Ok(results::SketchDetail{sketch,generation_record_size:size,generation_record_sha256:digest,history,annotations,lineage,description_history})
        }).map_err(db_error)
    }
    pub fn image(&self, p: params::SketchImage) -> Result<results::SketchChunk, ProtocolError> {
        self.chunk(p.repository_id, p.sketch_id, false, p.offset, p.max_bytes)
    }
    pub fn record(
        &self,
        p: params::SketchReference,
    ) -> Result<results::SketchRecordChunk, ProtocolError> {
        self.record_chunk(p.repository_id, p.sketch_id, p.offset, p.max_bytes)
    }
    pub fn activate(
        &self,
        p: params::SketchActivate,
        caller: &Caller,
    ) -> Result<results::SketchActivationResult, ProtocolError> {
        if p.sketch_ids.is_empty() || p.sketch_ids.len() > 64 {
            return Err(invalid("sketch_ids must contain 1..64 entries"));
        }
        if p.sketch_ids.iter().collect::<HashSet<_>>().len() != p.sketch_ids.len() {
            return Err(invalid("sketch_ids must be unique"));
        }
        let now = self.now()?;
        let actor = caller.actor();
        let activation_id = ids::sketch_activation_id().map_err(id_error)?;
        let repo = p.repository_id.clone();
        let surface = p.surface_id.clone();
        let ids = p.sketch_ids.clone();
        let action = activation_action_text(&p.action).to_owned();
        let rationale = p.rationale.clone();
        let expected = p.expected_revision;
        let now_for_tx = now.clone();
        let actor_for_tx = actor.clone();
        let revision = self.database.transaction(move |tx| {
            let current: u32 = tx.query_row("SELECT COALESCE(MAX(revision),0) FROM sketch_surface_activations WHERE repository_id=?1 AND surface_id=?2", rusqlite::params![repo, surface], |r| r.get(0))?;
            if current != expected {
                return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::ConfigurationConflict, "surface selection changed; refresh and retry")));
            }
            for sketch_id in &ids {
                let row: Option<(Option<String>, bool)> = tx.query_row("SELECT surface_id,legacy FROM sketches WHERE repository_id=?1 AND sketch_id=?2", rusqlite::params![repo, sketch_id], |r| Ok((r.get(0)?,r.get::<_,i64>(1)? != 0))).optional()?;
                let Some((sketch_surface, legacy)) = row else { return Err(DatabaseError::Domain(invalid("selected sketch was not found"))); };
                if legacy || sketch_surface.as_deref() != Some(surface.as_str()) { return Err(DatabaseError::Domain(invalid("only manifest-complete sketches on this surface can become current"))); }
            }
            let ids_json = serde_json::to_string(&ids).map_err(|error| DatabaseError::Startup(error.to_string()))?;
            let next = current + 1;
            tx.execute("INSERT INTO sketch_surface_activations(activation_id,repository_id,surface_id,revision,action,sketch_ids_json,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", rusqlite::params![activation_id,repo,surface,next,action,ids_json,rationale,actor_for_tx,now_for_tx])?;
            Ok(next)
        }).map_err(db_error)?;
        self.add_message(
            &p.repository_id,
            "sketch.activation",
            &p.surface_id,
            "The current sketch continuation set changed",
        )?;
        let current = self.database.call({
            let repo = p.repository_id.clone();
            let ids = p.sketch_ids.clone();
            move |c| {
                let mut result = Vec::new();
                for id in ids {
                    let mut summary = c.query_row(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketches.sketch_id=?2"), rusqlite::params![repo, id], summary_row)?;
                    summary.current = true;
                    result.push(summary);
                }
                Ok(result)
            }
        }).map_err(db_error)?;
        Ok(results::SketchActivationResult {
            surface_id: p.surface_id,
            revision,
            current,
            event: results::SketchActivationEvent {
                revision,
                action: p.action,
                sketch_ids: p.sketch_ids,
                rationale: p.rationale,
                actor,
                created_at: now,
            },
        })
    }
    pub fn description(
        &self,
        p: params::SketchDescriptionChange,
        caller: &Caller,
    ) -> Result<results::SketchDescriptionResult, ProtocolError> {
        if p.description.trim().len() < 20 {
            return Err(invalid("description must contain at least 20 characters"));
        }
        let now = self.now()?;
        let actor = caller.actor();
        let repo = p.repository_id.clone();
        let sketch_id = p.sketch_id.clone();
        let description = p.description.clone();
        let journey = p.journey.clone();
        let decisions = p.decisions.clone();
        let instructions = p.instructions.clone();
        let constraints = p.constraints.clone();
        let rationale = p.rationale.clone();
        let expected = p.expected_revision;
        self.database.transaction(move |tx| {
            let (current, legacy): (u32, bool) = tx.query_row("SELECT description_revision,legacy FROM sketches WHERE repository_id=?1 AND sketch_id=?2", rusqlite::params![repo, sketch_id], |r| Ok((r.get(0)?, r.get::<_,i64>(1)? != 0)))?;
            if legacy { return Err(DatabaseError::Domain(invalid("legacy sketches remain historical and cannot be edited"))); }
            if current != expected { return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::ConfigurationConflict, "description changed; refresh and retry"))); }
            let next = current + 1;
            tx.execute("UPDATE sketches SET description=?1,journey=?2,decisions=?3,instructions=?4,constraints=?5,description_revision=?6 WHERE repository_id=?7 AND sketch_id=?8", rusqlite::params![description,journey,decisions,instructions,constraints,next,repo,sketch_id])?;
            tx.execute("INSERT INTO sketch_description_history(sketch_id,revision,description,journey,decisions,instructions,constraints,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", rusqlite::params![sketch_id,next,description,journey,decisions,instructions,constraints,rationale,actor,now])?;
            Ok(())
        }).map_err(db_error)?;
        self.refresh_search_index()?;
        let detail = self.get(
            params::SketchReference {
                repository_id: p.repository_id.clone(),
                sketch_id: p.sketch_id.clone(),
                offset: 0,
                max_bytes: 184320,
            },
            &caller.actor(),
        )?;
        let revision = detail.description_history.last().cloned().ok_or_else(|| {
            ProtocolError::new(ErrorCode::InternalError, "description history missing")
        })?;
        Ok(results::SketchDescriptionResult {
            sketch: detail.sketch,
            revision,
        })
    }
    fn record_chunk(
        &self,
        repo: String,
        id: String,
        offset: u32,
        max: u32,
    ) -> Result<results::SketchRecordChunk, ProtocolError> {
        self.chunk_inner(repo, id, true, offset, max)
            .map(|x| results::SketchRecordChunk {
                sketch_id: x.sketch_id,
                sha256: x.sha256,
                total_bytes: x.total_bytes,
                offset: x.offset,
                bytes: x.bytes,
                base64: x.base64,
                next_offset: x.next_offset,
            })
    }
    fn chunk(
        &self,
        repo: String,
        id: String,
        record: bool,
        offset: u32,
        max: u32,
    ) -> Result<results::SketchChunk, ProtocolError> {
        self.chunk_inner(repo, id, record, offset, max)
    }
    fn chunk_inner(
        &self,
        repo: String,
        id: String,
        record: bool,
        offset: u32,
        max: u32,
    ) -> Result<results::SketchChunk, ProtocolError> {
        if max == 0 || max > MAX_CHUNK {
            return Err(invalid("max_bytes is invalid"));
        }
        let (path,size,digest)=self.database.call({let repo=repo.clone();let id=id.clone();move|c|{
            let sql=if record{"SELECT generation_record_path,generation_record_size,generation_record_sha256 FROM sketch_batches JOIN sketches USING(batch_id) WHERE sketches.repository_id=?1 AND sketch_id=?2"}else{"SELECT file_path,byte_size,sha256 FROM sketches WHERE repository_id=?1 AND sketch_id=?2"};
            c.query_row(sql,rusqlite::params![repo,id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)? as u64,r.get::<_,String>(2)?))).map_err(DatabaseError::from)
        }}).map_err(db_error)?;
        let bytes = read_file(&path, MAX_RECORD)?;
        if bytes.len() as u64 != size || sha(&bytes) != digest {
            return Err(ProtocolError::new(
                ErrorCode::TestEvidenceTampered,
                "sketch file changed",
            ));
        }
        let off = offset as usize;
        if off > bytes.len() {
            return Err(invalid("offset is beyond sketch"));
        };
        let end = (off + max as usize).min(bytes.len());
        let block = &bytes[off..end];
        Ok(results::SketchChunk {
            sketch_id: id,
            sha256: digest,
            total_bytes: size,
            offset: offset as u64,
            bytes: block.len() as u32,
            base64: BASE64.encode(block),
            next_offset: (end < bytes.len()).then_some(end as u64),
        })
    }
    pub fn decision(
        &self,
        p: params::SketchDecisionChange,
        caller: &Caller,
    ) -> Result<results::SketchDecisionResult, ProtocolError> {
        let now = self.now()?;
        let actor = caller.actor();
        let repo = p.repository_id.clone();
        let id = p.sketch_id.clone();
        let expected = p.expected_revision;
        let decision = serde_json::to_string(&p.decision)
            .unwrap()
            .trim_matches('"')
            .to_owned();
        let rationale = p.rationale.clone();
        let decision_for_message = decision.clone();
        let id_for_message = id.clone();
        let repo_for_message = repo.clone();
        let fallback_task = ids::task_id().map_err(id_error)?;
        self.database.transaction(move|tx|{
            let current:(u32,String)=tx.query_row("SELECT decision_revision,decision FROM sketches WHERE repository_id=?1 AND sketch_id=?2",rusqlite::params![repo,id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if current.0!=expected{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::ConfigurationConflict,"sketch decision changed; refresh and retry")))}
            let next=current.0+1; tx.execute("UPDATE sketches SET decision=?1,decision_revision=?2 WHERE sketch_id=?3",rusqlite::params![decision,next,id])?;
            tx.execute("INSERT INTO sketch_decision_history(sketch_id,revision,decision,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![id,next,decision,rationale,actor,now])?;
            insert_fallback_task(tx,&fallback_task,&repo,"Review a changed sketch decision","The changed sketch decision must be reflected in the next agent review.",&actor,&now)?;
            Ok(())
        }).map_err(db_error)?;
        self.add_message(
            &repo_for_message,
            "sketch.decision",
            &id_for_message,
            &format!("Sketch decision changed to {}", decision_for_message),
        )?;
        let detail = self.get(
            params::SketchReference {
                repository_id: p.repository_id.clone(),
                sketch_id: p.sketch_id.clone(),
                offset: 0,
                max_bytes: 184320,
            },
            &caller.actor(),
        )?;
        let event = detail.history.last().cloned().unwrap();
        Ok(results::SketchDecisionResult {
            sketch: detail.sketch,
            event,
        })
    }
    pub fn annotation_create(
        &self,
        p: params::SketchAnnotationCreate,
        caller: &Caller,
    ) -> Result<results::SketchAnnotationMutation, ProtocolError> {
        let id = ids::sketch_annotation_id().map_err(id_error)?;
        let now = self.now()?;
        let marks = serde_json::to_string(&p.marks).map_err(|e| invalid(e.to_string()))?;
        let body = p.body.trim().to_owned();
        let actor = caller.actor();
        let repo = p.repository_id.clone();
        let repo_for_task = repo.clone();
        let sketch = p.sketch_id.clone();
        let id_for_lookup = id.clone();
        let fallback_task = ids::task_id().map_err(id_error)?;
        self.database.transaction(move|tx|{tx.execute("INSERT INTO sketch_annotations(annotation_id,sketch_id,repository_id,body,marks_json,state,created_by,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'open',?6,?7,?7)",rusqlite::params![id,sketch,repo,body,marks,actor,now])?;insert_fallback_task(tx,&fallback_task,&repo_for_task,"Review a sketch annotation","The marked sketch feedback must be handled by the next agent review.",&actor,&now)?;Ok(())}).map_err(db_error)?;
        self.add_message(
            &p.repository_id,
            "sketch.annotation",
            &p.sketch_id,
            "A sketch annotation was added",
        )?;
        let detail = self.get(
            params::SketchReference {
                repository_id: p.repository_id,
                sketch_id: p.sketch_id,
                offset: 0,
                max_bytes: 184320,
            },
            &caller.actor(),
        )?;
        let annotation = detail
            .annotations
            .into_iter()
            .find(|a| a.annotation_id == id_for_lookup)
            .unwrap();
        Ok(results::SketchAnnotationMutation { annotation })
    }

    pub fn message_poll(
        &self,
        p: params::AgentMessagePoll,
    ) -> Result<results::AgentMessageList, ProtocolError> {
        self.message_poll_kind(p, None)
    }
    pub(crate) fn message_poll_kind(
        &self,
        p: params::AgentMessagePoll,
        kind: Option<&str>,
    ) -> Result<results::AgentMessageList, ProtocolError> {
        let kind = kind.map(str::to_owned);
        let repo = p.repository_id;
        let limit = p.limit.clamp(1, 100);
        self.database.call(move|c|{ let mut st=c.prepare("SELECT message_id,repository_id,kind,subject_id,summary,created_at,claimed_by,acknowledged_at IS NOT NULL FROM agent_messages WHERE repository_id=?1 AND acknowledged_at IS NULL AND (?3 IS NULL OR kind=?3) AND NOT EXISTS (SELECT 1 FROM review_reminders r JOIN review_policies p USING(repository_id,workstream_key) WHERE r.message_id=agent_messages.message_id AND (r.resolved=1 OR p.active=0)) ORDER BY created_at LIMIT ?2")?; let rows=st.query_map(rusqlite::params![repo,limit,kind],message_row)?.collect::<Result<Vec<_>,_>>()?; Ok(results::AgentMessageList{has_more:rows.len()==usize::from(limit),messages:rows}) }).map_err(db_error)
    }
    fn add_message(
        &self,
        repo: &str,
        kind: &str,
        subject: &str,
        summary: &str,
    ) -> Result<(), ProtocolError> {
        let id = ids::agent_message_id().map_err(id_error)?;
        let now = self.now()?;
        let repo = repo.to_owned();
        let kind = kind.to_owned();
        let subject = subject.to_owned();
        let summary = summary.to_owned();
        self.database.call(move|c|{c.execute("INSERT INTO agent_messages(message_id,repository_id,kind,subject_id,summary,created_at) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![id,repo,kind,subject,summary,now])?;Ok(())}).map_err(db_error)
    }
    pub fn message_claim(
        &self,
        p: params::AgentMessageClaim,
        caller: &Caller,
    ) -> Result<results::AgentMessageMutation, ProtocolError> {
        let actor = caller.actor();
        let now = self.now()?;
        let until = (self.clock.now_utc() + time::Duration::minutes(10))
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format message lease")
                    .with_detail(e.to_string())
            })?;
        let repo = p.repository_id.clone();
        let id = p.message_id.clone();
        let actor2 = actor.clone();
        let until2 = until.clone();
        self.database.transaction(move|tx|{ let updated=tx.execute("UPDATE agent_messages SET claimed_by=?1,claimed_until=?2 WHERE repository_id=?3 AND message_id=?4 AND acknowledged_at IS NULL AND (claimed_until IS NULL OR claimed_until <= ?5)",rusqlite::params![actor2,until2,repo,id,now])?; if updated==0{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::ConfigurationConflict,"message is already claimed or acknowledged")))} Ok(()) }).map_err(db_error)?;
        self.message_by_id(&p.repository_id, &p.message_id)
    }
    pub fn message_ack(
        &self,
        p: params::AgentMessageAck,
        caller: &Caller,
    ) -> Result<results::AgentMessageMutation, ProtocolError> {
        let actor = caller.actor();
        let now = self.now()?;
        let repo = p.repository_id.clone();
        let id = p.message_id.clone();
        self.database.transaction(move|tx|{let updated=tx.execute("UPDATE agent_messages SET acknowledged_at=?4,acknowledged_by=?1 WHERE repository_id=?2 AND message_id=?3 AND acknowledged_at IS NULL AND claimed_by=?1 AND claimed_until>?4",rusqlite::params![actor,repo,id,now])?; if updated==0{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::PermissionDenied,"message is not claimed by this agent")))} Ok(())}).map_err(db_error)?;
        self.message_by_id(&p.repository_id, &p.message_id)
    }
    fn message_by_id(
        &self,
        repo: &str,
        id: &str,
    ) -> Result<results::AgentMessageMutation, ProtocolError> {
        let repo = repo.to_owned();
        let id = id.to_owned();
        self.database.call(move|c|{let row=c.query_row("SELECT message_id,repository_id,kind,subject_id,summary,created_at,claimed_by,acknowledged_at IS NOT NULL FROM agent_messages WHERE repository_id=?1 AND message_id=?2",rusqlite::params![repo,id],message_row)?;Ok(results::AgentMessageMutation{message:row})}).map_err(db_error)
    }
}
fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::SketchImageSummary> {
    let element_ids =
        serde_json::from_str::<Vec<String>>(&r.get::<_, String>(14)?).unwrap_or_default();
    Ok(results::SketchImageSummary {
        sketch_id: r.get(0)?,
        repository_id: r.get(1)?,
        sketch_set: r.get(2)?,
        source_skill: r.get(3)?,
        title: r.get(4)?,
        image_id: r.get(0)?,
        mime: "image/png".to_owned(),
        byte_size: r.get::<_, i64>(6)? as u64,
        sha256: r.get(5)?,
        width: r.get(7)?,
        height: r.get(8)?,
        created_at: r.get(9)?,
        decision: serde_json::from_str(&format!("\"{}\"", r.get::<_, String>(10)?))
            .unwrap_or(SketchDecision::Undecided),
        decision_revision: r.get(11)?,
        surface_id: r.get(12)?,
        surface_title: r.get(13)?,
        element_ids,
        state: r.get(15)?,
        theme: r.get(16)?,
        viewport: r.get(17)?,
        description: r.get(18)?,
        journey: r.get(19)?,
        decisions: r.get(20)?,
        instructions: r.get(21)?,
        constraints: r.get(22)?,
        transition_note: r.get(23)?,
        manifest_version: r.get(24)?,
        legacy: r.get::<_, i64>(25)? != 0,
        current: false,
        description_revision: r.get(26)?,
    })
}

fn current_ids(
    connection: &rusqlite::Connection,
    repository_id: &str,
    surface_id: &str,
) -> Result<Vec<String>, DatabaseError> {
    let value: Option<String> = connection
        .query_row(
            "SELECT sketch_ids_json FROM sketch_surface_activations WHERE repository_id=?1 AND surface_id=?2 ORDER BY revision DESC LIMIT 1",
            rusqlite::params![repository_id, surface_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .unwrap_or_default())
}

fn is_current_sketch(
    connection: &rusqlite::Connection,
    surface_id: Option<&str>,
    sketch_id: &str,
    repository_id: &str,
) -> Result<bool, DatabaseError> {
    let Some(surface_id) = surface_id else {
        return Ok(false);
    };
    Ok(current_ids(connection, repository_id, surface_id)?
        .iter()
        .any(|id| id == sketch_id))
}

fn validate_manifest(manifest: &params::SketchManifest) -> Result<(), ProtocolError> {
    if manifest.window_count != 1 {
        return Err(invalid("a mockup file must declare exactly one window"));
    }
    for (name, value, min, max) in [
        ("surface_id", manifest.surface_id.as_str(), 1, 160),
        ("surface_title", manifest.surface_title.as_str(), 1, 200),
        ("state", manifest.state.as_str(), 1, 120),
        ("theme", manifest.theme.as_str(), 1, 40),
        ("viewport", manifest.viewport.as_str(), 1, 80),
        ("description", manifest.description.as_str(), 20, 8000),
        ("journey", manifest.journey.as_str(), 1, 4000),
        ("decisions", manifest.decisions.as_str(), 1, 4000),
        ("instructions", manifest.instructions.as_str(), 1, 4000),
        ("constraints", manifest.constraints.as_str(), 1, 4000),
    ] {
        let trimmed = value.trim();
        if trimmed.len() < min || trimmed.len() > max || value.contains(['\r', '\n']) {
            return Err(invalid(format!(
                "{name} must be a plain text value of {min}..{max} characters"
            )));
        }
    }
    if manifest.element_ids.len() > 64
        || manifest.element_ids.iter().any(|id| {
            let trimmed = id.trim();
            trimmed.is_empty() || trimmed.len() > 160 || id.contains(['\r', '\n'])
        })
    {
        return Err(invalid(
            "element_ids must contain at most 64 plain identifiers",
        ));
    }
    let mut element_ids = HashSet::new();
    if manifest
        .element_ids
        .iter()
        .any(|element| !element_ids.insert(element.trim().to_owned()))
    {
        return Err(invalid("element_ids must be unique within one mockup"));
    }
    if manifest.parent_relations.len() > 64 {
        return Err(invalid("parent_relations must contain at most 64 entries"));
    }
    if manifest.parent_relations.is_empty() && !manifest.transition_note.trim().is_empty() {
        return Err(invalid(
            "transition_note requires at least one parent relation",
        ));
    }
    if !manifest.parent_relations.is_empty() && manifest.transition_note.trim().is_empty() {
        return Err(invalid(
            "parent relations require an agent-authored transition_note",
        ));
    }
    if manifest.parent_relations.iter().any(|parent| {
        parent.parent_sketch_id.len() != 17
            || !parent.parent_sketch_id.starts_with('s')
            || !parent.parent_sketch_id[1..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(invalid("parent sketch identities are invalid"));
    }
    Ok(())
}

fn lineage_relation_text(relation: &params::SketchLineageRelation) -> &'static str {
    match relation {
        params::SketchLineageRelation::DerivedFrom => "derived_from",
        params::SketchLineageRelation::AdjustedFrom => "adjusted_from",
        params::SketchLineageRelation::Supersedes => "supersedes",
        params::SketchLineageRelation::ReintroducedFrom => "reintroduced_from",
    }
}

fn parse_lineage_relation(value: &str) -> params::SketchLineageRelation {
    match value {
        "adjusted_from" => params::SketchLineageRelation::AdjustedFrom,
        "supersedes" => params::SketchLineageRelation::Supersedes,
        "reintroduced_from" => params::SketchLineageRelation::ReintroducedFrom,
        _ => params::SketchLineageRelation::DerivedFrom,
    }
}

fn activation_action_text(action: &params::SketchActivationAction) -> &'static str {
    match action {
        params::SketchActivationAction::Select => "select",
        params::SketchActivationAction::Restore => "restore",
        params::SketchActivationAction::Supersede => "supersede",
    }
}

fn parse_activation_action(value: &str) -> params::SketchActivationAction {
    match value {
        "restore" => params::SketchActivationAction::Restore,
        "supersede" => params::SketchActivationAction::Supersede,
        _ => params::SketchActivationAction::Select,
    }
}

fn literal_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}
fn annotation_row(
    r: &rusqlite::Row<'_>,
    actor: &str,
) -> rusqlite::Result<results::SketchAnnotation> {
    Ok(results::SketchAnnotation {
        annotation_id: r.get(0)?,
        sketch_id: r.get(1)?,
        body: r.get(2)?,
        marks: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        state: r.get(4)?,
        author: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        can_delete: r.get::<_, String>(5)? == actor,
    })
}
fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::AgentMessage> {
    Ok(results::AgentMessage {
        message_id: r.get(0)?,
        repository_id: r.get(1)?,
        kind: r.get(2)?,
        subject_id: r.get(3)?,
        summary: r.get(4)?,
        created_at: r.get(5)?,
        claimed_by: r.get(6)?,
        acknowledged: r.get(7)?,
    })
}
fn read_file(path: &str, max: u64) -> Result<Vec<u8>, ProtocolError> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(invalid("path must be absolute"));
    };
    let m = fs::metadata(p).map_err(io_error)?;
    if !m.is_file() || m.len() == 0 || m.len() > max {
        return Err(invalid("file is unavailable or too large"));
    };
    let mut f = File::open(p).map_err(io_error)?;
    let mut b = Vec::with_capacity(m.len() as usize);
    f.read_to_end(&mut b).map_err(io_error)?;
    Ok(b)
}
fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || !b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    Some((w, h))
}
fn invalid(m: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, m)
}
fn io_error(e: std::io::Error) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestEvidenceNotFound, "sketch file unavailable")
        .with_detail(e.to_string())
}
fn id_error(e: ids::IdError) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, "cannot allocate sketch identity")
        .with_detail(e.to_string())
}
fn db_error(e: DatabaseError) -> ProtocolError {
    match e {
        DatabaseError::Domain(x) => x,
        other => ProtocolError::new(ErrorCode::InternalError, "sketch storage failed")
            .with_detail(other.to_string()),
    }
}
fn insert_fallback_task(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    repo: &str,
    title: &str,
    outcome: &str,
    actor: &str,
    now: &str,
) -> Result<(), DatabaseError> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM tasks WHERE repository_id=?1",
        [repo],
        |r| r.get(0),
    )?;
    tx.execute("INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,impact,verification,kind,status,created_at,created_by,updated_at) VALUES(?1,?2,?3,1,?4,?5,?6,?7,'user_feedback','planned',?8,?9,?8)",rusqlite::params![task_id,repo,seq,title,outcome,"A retained sketch decision or annotation needs agent follow-up.","Inspect the linked sketch and record the resulting change or resolution.",now,actor])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::ClientContext;
    use tempfile::tempdir;

    fn config(root: &Path) -> Config {
        Config {
            socket_path: root.join("daemon.sock"),
            sandbox_bridge_dir: root.join("bridge"),
            state_dir: root.join("state"),
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "fixture".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: vec![],
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: root.join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: Default::default(),
            codex_usage_sources_file: None,
            codex_usage_sources: vec![],
        }
    }
    fn manifest(description: &str) -> params::SketchManifest {
        params::SketchManifest {
            surface_id: "controller-device".into(),
            surface_title: "Controller device".into(),
            element_ids: vec!["diagram".into(), "properties".into()],
            state: "default".into(),
            theme: "light".into(),
            viewport: "desktop-1440x1024".into(),
            description: description.into(),
            journey: "Inspect one controller surface and choose a direction.".into(),
            decisions: "Keep the diagram readable and preserve the property panel.".into(),
            instructions: "Use one window and keep the annotated component central.".into(),
            constraints: "Do not add a second window or merge unrelated states.".into(),
            parent_relations: vec![],
            transition_note: "".into(),
            window_count: 1,
        }
    }
    #[test]
    fn publish_list_decide_and_read_chunks() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let db = Database::open(root.join("authority.sqlite3")).unwrap();
        let root_path = root.to_string_lossy().to_string();
        db.transaction(move |tx| { tx.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',?1,'fixture','t',1000,'t')",[root_path])?; Ok(()) }).unwrap();
        let png=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk/x8AAusB9Y9Z4rUAAAAASUVORK5CYII=").unwrap();
        let image = root.join("sketch.png");
        fs::write(&image, &png).unwrap();
        let image_second = root.join("sketch-second.png");
        let mut png_second = png.clone();
        png_second.push(2);
        fs::write(&image_second, &png_second).unwrap();
        let record = root.join("record.json");
        fs::write(&record, b"{\"prompt\":\"fixture\"}").unwrap();
        let service = SketchService::with_clock(
            &config(root),
            db,
            Arc::new(crate::platform::FixedClock(
                time::macros::datetime!(2026-09-26 00:00 UTC),
            )),
        );
        let caller = Caller::from_client(1, 1000, 1000, ClientContext::default(), None).unwrap();
        let batch = service
            .publish(
                params::SketchPublish {
                    repository_id: "r1111111111111111".into(),
                    sketch_set: "set".into(),
                    source_skill: "Image Gen".into(),
                    manifest_version: 2,
                    generation_record_path: record.to_string_lossy().into_owned(),
                    images: vec![params::SketchImageInput {
                        title: "First".into(),
                        path: image.to_string_lossy().into_owned(),
                        manifest: manifest("Agent-authored first controller surface description with the initial layout and review context."),
                    }],
                    idempotency_key: "fixture-1".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(batch.sketches.len(), 1);
        let sketch = &batch.sketches[0];
        let mut second_manifest = manifest(
            "Agent-authored second controller surface description with a revised layout and review context.",
        );
        second_manifest.parent_relations = vec![params::SketchLineageInput {
            parent_sketch_id: sketch.sketch_id.clone(),
            relation: params::SketchLineageRelation::AdjustedFrom,
        }];
        second_manifest.transition_note = "Adjusted the first direction after review.".into();
        let second_batch = service
            .publish(
                params::SketchPublish {
                    repository_id: batch.repository_id.clone(),
                    sketch_set: "set".into(),
                    source_skill: "Image Gen".into(),
                    manifest_version: 2,
                    generation_record_path: record.to_string_lossy().into_owned(),
                    images: vec![params::SketchImageInput {
                        title: "Second".into(),
                        path: image_second.to_string_lossy().into_owned(),
                        manifest: second_manifest,
                    }],
                    idempotency_key: "fixture-2".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(second_batch.sketches.len(), 1);
        let activation = service
            .activate(
                params::SketchActivate {
                    repository_id: batch.repository_id.clone(),
                    surface_id: "controller-device".into(),
                    sketch_ids: vec![
                        sketch.sketch_id.clone(),
                        second_batch.sketches[0].sketch_id.clone(),
                    ],
                    expected_revision: 0,
                    action: params::SketchActivationAction::Select,
                    rationale: "Keep both directions for the next comparison.".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(activation.current.len(), 2);
        let resolved = service
            .resolve(params::SketchResolve {
                repository_id: batch.repository_id.clone(),
                surface_id: "controller-device".into(),
            })
            .unwrap();
        assert!(matches!(
            resolved.status,
            results::SketchResolveStatus::Resolved
        ));
        let story = service
            .story(params::SketchStory {
                repository_id: batch.repository_id.clone(),
                surface_id: "controller-device".into(),
                limit: 100,
                include_legacy: true,
            })
            .unwrap();
        assert_eq!(story.nodes.len(), 2);
        assert_eq!(story.lineage.len(), 1);
        let described = service
            .description(
                params::SketchDescriptionChange {
                    repository_id: batch.repository_id.clone(),
                    sketch_id: second_batch.sketches[0].sketch_id.clone(),
                    expected_revision: 0,
                    description: "Owner context: keep the revised controller direction and compare both selected paths before implementation.".into(),
                    journey: "Compare selected controller directions before implementation.".into(),
                    decisions: "Preserve the property panel and readable signal flow.".into(),
                    instructions: "Use the selected options as the next generation parents.".into(),
                    constraints: "Do not merge multiple windows into one mockup.".into(),
                    rationale: "The owner added a follow-up direction.".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(described.revision.revision, 1);
        let search = service
            .search(params::SketchSearch {
                repository_id: batch.repository_id.clone(),
                query: "Owner context".into(),
                surface_id: None,
                current_only: false,
                include_legacy: true,
                offset: 0,
                limit: 10,
            })
            .unwrap();
        assert_eq!(search.sketches.len(), 1);
        let list = service
            .list(params::SketchList {
                repository_id: batch.repository_id.clone(),
                source_skill: None,
                decision: None,
                sketch_set: None,
                surface_id: None,
                current_only: false,
                include_legacy: true,
                offset: 0,
                limit: 1,
            })
            .unwrap();
        assert_eq!(list.sketches.len(), 1);
        assert_eq!(list.sketches[0].title, "Second");
        assert!(list.has_more);
        let next = service
            .list(params::SketchList {
                repository_id: batch.repository_id.clone(),
                source_skill: None,
                decision: None,
                sketch_set: None,
                surface_id: None,
                current_only: false,
                include_legacy: true,
                offset: 1,
                limit: 1,
            })
            .unwrap();
        assert_eq!(next.sketches.len(), 1);
        assert_eq!(next.sketches[0].title, "First");
        assert!(!next.has_more);
        let chunk = service
            .image(params::SketchImage {
                repository_id: batch.repository_id.clone(),
                sketch_id: sketch.sketch_id.clone(),
                offset: 0,
                max_bytes: 184320,
            })
            .unwrap();
        assert_eq!(chunk.sha256, sketch.sha256);
        let changed = service
            .decision(
                params::SketchDecisionChange {
                    repository_id: batch.repository_id.clone(),
                    sketch_id: sketch.sketch_id.clone(),
                    expected_revision: 0,
                    decision: SketchDecision::Keep,
                    rationale: "fixture review".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(changed.sketch.decision, SketchDecision::Keep);
        service
            .add_message(
                &batch.repository_id,
                "performance_review.reminder",
                "review-fixture",
                "Review the stated window",
            )
            .unwrap();
        let messages = service
            .message_poll(params::AgentMessagePoll {
                repository_id: batch.repository_id.clone(),
                after_id: None,
                limit: 10,
            })
            .unwrap();
        let reminder = messages
            .messages
            .into_iter()
            .find(|message| message.kind == "performance_review.reminder")
            .unwrap();
        let claim = params::AgentMessageClaim {
            repository_id: batch.repository_id.clone(),
            message_id: reminder.message_id.clone(),
        };
        let ack = params::AgentMessageAck {
            repository_id: batch.repository_id.clone(),
            message_id: reminder.message_id,
        };
        service.message_claim(claim.clone(), &caller).unwrap();
        let other = Caller::from_client(2, 2000, 2000, ClientContext::default(), None).unwrap();
        assert!(service.message_ack(ack.clone(), &other).is_err());
        service
            .database
            .call(|connection| {
                connection.execute(
                    "UPDATE agent_messages SET claimed_until='2026-09-26T00:00:00Z'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(service.message_ack(ack.clone(), &caller).is_err());
        service.message_claim(claim, &caller).unwrap();
        assert!(
            service
                .message_ack(ack, &caller)
                .unwrap()
                .message
                .acknowledged
        );
    }
}
