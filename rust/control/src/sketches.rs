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

const SUMMARY_COLUMNS: &str = "sketches.sketch_id,sketches.repository_id,sketch_batches.sketch_set,sketch_batches.source_skill,sketches.title,sketches.sha256,sketches.byte_size,sketches.width,sketches.height,sketches.created_at,sketches.decision,sketches.decision_revision,sketches.surface_id,sketches.surface_title,sketches.element_ids_json,sketches.state_name,sketches.theme,sketches.viewport,sketches.description,sketches.journey,sketches.decisions,sketches.instructions,sketches.constraints,sketches.transition_note,sketches.manifest_version,sketches.legacy,sketches.description_revision,sketches.batch_id,sketches.display_order";

struct PreparedSketch {
    id: String,
    title: String,
    bytes: Vec<u8>,
    digest: String,
    width: u32,
    height: u32,
    manifest: params::SketchManifest,
    display_order: u16,
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
        validate_id(&p.repository_id, 'r')?;
        text_field("set", &p.sketch_set, 1, 160)?;
        text_field("source skill", &p.source_skill, 1, 80)?;
        text_field("idempotency key", &p.idempotency_key, 1, 256)?;
        if p.images.is_empty() || p.images.len() > 64 || p.manifest_version != MANIFEST_VERSION {
            return Err(invalid(
                "publish requires manifest_version 2 and 1..64 images",
            ));
        }
        let request_sha = sha(&serde_json::to_vec(&p).map_err(|_| invalid("invalid manifest"))?);
        let repo = p.repository_id.clone();
        let key = p.idempotency_key.clone();
        let existing = self.database.call(move |c| Ok(c.query_row("SELECT batch_id,request_sha256 FROM sketch_batches WHERE repository_id=?1 AND idempotency_key=?2", rusqlite::params![repo,key], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).optional()?)).map_err(db_error)?;
        if let Some((id, digest)) = existing {
            if digest.as_deref() != Some(&request_sha) {
                return Err(conflict(
                    "idempotency key belongs to a different publication",
                ));
            }
            return self.batch(&p.repository_id, &id);
        }
        let mut positions = HashSet::new();
        let mut hashes = HashSet::new();
        let mut prepared = Vec::new();
        let mut total = 0;
        let surface = p.images[0].manifest.surface_id.clone();
        for input in &p.images {
            text_field("title", &input.title, 1, 120)?;
            validate_manifest(&input.manifest)?;
            if input.manifest.surface_id != surface {
                return Err(invalid("one generation batch must target one surface"));
            }
            if input.display_order == 0
                || usize::from(input.display_order) > p.images.len()
                || !positions.insert(input.display_order)
            {
                return Err(invalid(
                    "display_order must identify the actual displayed positions 1..image count without duplicates",
                ));
            }
            let bytes = read_file(&input.path, MAX_BYTES)?;
            total += bytes.len();
            if total > 64 * 1024 * 1024 {
                return Err(invalid("image batch exceeds 64 MiB"));
            }
            let (width, height) = png_dimensions(&bytes)
                .filter(|(w, h)| *w > 0 && *h > 0)
                .ok_or_else(|| invalid("mockup images must be nonempty PNG files"))?;
            let digest = sha(&bytes);
            if !hashes.insert(digest.clone()) {
                return Err(invalid("each option requires its own image file"));
            }
            prepared.push(PreparedSketch {
                id: ids::sketch_id().map_err(id_error)?,
                title: input.title.clone(),
                bytes,
                digest,
                width,
                height,
                display_order: input.display_order,
                manifest: input.manifest.clone(),
            });
        }
        let batch_id = ids::sketch_batch_id().map_err(id_error)?;
        let now = self.now()?;
        let record = read_file(&p.generation_record_path, MAX_RECORD)?;
        let record_sha = sha(&record);
        let record_size = record.len();
        let dir = self
            .state_dir
            .join("sketches")
            .join(&p.repository_id)
            .join(&batch_id);
        fs::create_dir_all(&dir).map_err(io_error)?;
        let files = (|| -> Result<(), ProtocolError> {
            write_new(&dir.join("generation-record.bin"), &record)?;
            for item in &prepared {
                write_new(&dir.join(format!("{}.png", item.id)), &item.bytes)?;
            }
            Ok(())
        })();
        if let Err(error) = files {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
        let repo = p.repository_id.clone();
        let batch = batch_id.clone();
        let actor = caller.actor();
        let path = dir.clone();
        let result = self.database.transaction(move |tx| {
            // Validate inside the same transaction as insertion. Parents must already
            // exist: immutable edges into fresh child IDs cannot introduce a cycle.
            for item in &prepared {
                let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sketches WHERE repository_id=?1 AND sha256=?2)",rusqlite::params![repo,item.digest],|r|r.get(0))?;
                if duplicate { return Err(invalid("image digest is already retained; generate a distinct image").into()); }
                for parent in &item.manifest.parent_relations {
                    let owner: Option<(Option<String>,bool)> = tx.query_row("SELECT surface_id,legacy FROM sketches WHERE repository_id=?1 AND sketch_id=?2",rusqlite::params![repo,parent.parent_sketch_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    if !owner.is_some_and(|(scope,legacy)| !legacy && scope.as_deref()==Some(surface.as_str())) { return Err(invalid("lineage parent must be an existing nonlegacy image on the same repository and surface").into()); }
                }
            }
            tx.execute("INSERT INTO sketch_surfaces(repository_id,surface_id,title,revision) VALUES(?1,?2,?3,0) ON CONFLICT DO NOTHING",rusqlite::params![repo,surface,prepared[0].manifest.surface_title])?;
            tx.execute("INSERT INTO sketch_batches(batch_id,repository_id,sketch_set,source_skill,generation_record_path,generation_record_size,generation_record_sha256,manifest_version,request_sha256,idempotency_key,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,2,?8,?9,?10,?11)",rusqlite::params![batch,repo,p.sketch_set,p.source_skill,path.join("generation-record.bin").to_string_lossy(),record_size as i64,record_sha,request_sha,p.idempotency_key,now,actor])?;
            for item in prepared {
                let m=&item.manifest;
                let elements=json_text(&m.element_ids)?;
                tx.execute("INSERT INTO sketches(sketch_id,batch_id,repository_id,title,file_path,byte_size,sha256,mime,width,height,decision,decision_revision,surface_id,surface_title,display_order,element_ids_json,state_name,theme,viewport,description,journey,decisions,instructions,constraints,transition_note,manifest_version,legacy,description_revision,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,'image/png',?8,?9,'undecided',0,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,2,0,0,?23,?24)",rusqlite::params![item.id,batch,repo,item.title,path.join(format!("{}.png",item.id)).to_string_lossy(),item.bytes.len() as i64,item.digest,item.width,item.height,m.surface_id,m.surface_title,item.display_order,elements,m.state,m.theme,m.viewport,m.description,m.journey,m.decisions,m.instructions,m.constraints,m.transition_note,now,actor])?;
                tx.execute("INSERT INTO sketch_description_history(sketch_id,revision,description,journey,decisions,instructions,constraints,rationale,actor,created_at) VALUES(?1,0,?2,?3,?4,?5,?6,'Initial agent-authored context',?7,?8)",rusqlite::params![item.id,m.description,m.journey,m.decisions,m.instructions,m.constraints,actor,now])?;
                for parent in &m.parent_relations {
                    tx.execute("INSERT INTO sketch_lineage(lineage_id,repository_id,parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",rusqlite::params![ids::sketch_lineage_id().map_err(id_error)?,repo,parent.parent_sketch_id,item.id,lineage_relation_text(&parent.relation),m.transition_note,actor,now])?;
                }
                reindex_sketch(tx,&item.id)?;
            }
            bump_surface(tx,&repo,&surface)?;
            Ok(())
        });
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&dir);
            return Err(db_error(error));
        }
        self.batch(&p.repository_id, &batch_id)
    }
    fn batch(
        &self,
        repository_id: &str,
        batch_id: &str,
    ) -> Result<results::SketchBatch, ProtocolError> {
        let repo = repository_id.to_owned();
        let batch = batch_id.to_owned();
        self.database.call(move |c| {
            let b=c.query_row("SELECT sketch_set,source_skill,generation_record_size,generation_record_sha256,created_at FROM sketch_batches WHERE repository_id=?1 AND batch_id=?2",rusqlite::params![repo,batch],|r|Ok((r.get(0)?,r.get(1)?,r.get::<_,i64>(2)? as u64,r.get(3)?,r.get(4)?)))?;
            let sketches=query_summaries(c,&format!("sketches.repository_id=? AND sketches.batch_id=? ORDER BY sketches.display_order,sketches.rowid"),vec![repo.clone().into(),batch.clone().into()])?;
            Ok(results::SketchBatch {batch_id:batch,repository_id:repo,sketch_set:b.0,source_skill:b.1,generation_record_size:b.2,generation_record_sha256:b.3,created_at:b.4,sketches})
        }).map_err(db_error)
    }
    pub fn list(&self, p: params::SketchList) -> Result<results::SketchListResult, ProtocolError> {
        self.query(
            p.repository_id,
            None,
            p.source_skill,
            p.decision,
            p.sketch_set,
            p.surface_id,
            p.batch_id,
            p.state,
            p.theme,
            p.current_only,
            p.include_legacy,
            p.offset,
            p.limit,
        )
    }
    pub fn search(
        &self,
        p: params::SketchSearch,
    ) -> Result<results::SketchListResult, ProtocolError> {
        text_field("query", &p.query, 1, 256)?;
        self.query(
            p.repository_id,
            Some(p.query),
            None,
            None,
            None,
            p.surface_id,
            None,
            p.state,
            p.theme,
            p.current_only,
            p.include_legacy,
            p.offset,
            p.limit,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn query(
        &self,
        repo: String,
        query: Option<String>,
        skill: Option<String>,
        decision: Option<SketchDecision>,
        set: Option<String>,
        surface: Option<String>,
        batch: Option<String>,
        state: Option<String>,
        theme: Option<String>,
        current: bool,
        legacy: bool,
        offset: u32,
        limit: u16,
    ) -> Result<results::SketchListResult, ProtocolError> {
        if !(1..=100).contains(&limit) || offset > 4_000_000 {
            return Err(invalid("invalid page bounds"));
        }
        self.database.call(move |c| {
            let mut where_sql="sketches.repository_id=?".to_owned();let mut values=vec![repo.clone().into()];
            for (column,value) in [("sketch_batches.source_skill",skill),("sketches.decision",decision.map(|d|decision_text(&d).to_owned())),("sketch_batches.sketch_set",set),("sketches.surface_id",surface),("sketches.batch_id",batch),("sketches.state_name",state),("sketches.theme",theme)] {
                if let Some(value)=value {where_sql.push_str(&format!(" AND {column}=?"));values.push(value.into());}
            }
            if !legacy {where_sql.push_str(" AND sketches.legacy=0");}
            if current {where_sql.push_str(&format!(" AND {CURRENT_SQL}"));}
            if let Some(query)=query {where_sql.push_str(" AND sketches.sketch_id IN (SELECT sketch_id FROM sketches_fts WHERE repository_id=? AND sketches_fts MATCH ?)");values.push(repo.clone().into());values.push(literal_fts_query(&query).into());}
            where_sql.push_str(&format!(" ORDER BY sketch_batches.rowid DESC,sketches.display_order ASC,sketches.rowid LIMIT {} OFFSET {offset}",u32::from(limit)+1));
            let rows=query_summaries(c,&where_sql,values)?;
            let (sketches,has_more)=bounded_summaries(rows,usize::from(limit));
            Ok(results::SketchListResult{repository_id:repo,sketches,has_more})
        }).map_err(db_error)
    }
    pub fn story(
        &self,
        p: params::SketchStory,
    ) -> Result<results::SketchStoryResult, ProtocolError> {
        self.database.call(move |c| {
            let current=resolve_current(c,&p.repository_id,&p.surface_id)?;
            let rows=query_summaries(c,&format!("sketches.repository_id=? AND sketches.surface_id=? ORDER BY sketch_batches.rowid DESC,sketches.display_order ASC,sketches.rowid LIMIT {} OFFSET {}",p.limit.clamp(1,100)+1,p.offset),vec![p.repository_id.clone().into(),p.surface_id.clone().into()])?;
            let (nodes,has_more)=bounded_summaries(rows,usize::from(p.limit.clamp(1,100)));
            let mut lineage=Vec::new();
            for node in &nodes {let mut q=c.prepare("SELECT parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at FROM sketch_lineage WHERE repository_id=?1 AND child_sketch_id=?2 ORDER BY rowid")?;lineage.extend(q.query_map(rusqlite::params![p.repository_id,node.sketch_id],lineage_row)?.collect::<Result<Vec<_>,_>>()?);}
            let mut q=c.prepare("SELECT revision,action,sketch_ids_json,rationale,actor,created_at FROM sketch_surface_activations WHERE repository_id=?1 AND surface_id=?2 ORDER BY revision DESC LIMIT 21 OFFSET ?3")?;
            let mut activations=q.query_map(rusqlite::params![p.repository_id,p.surface_id,p.activation_offset],activation_row)?.collect::<Result<Vec<_>,_>>()?;
            let next_activation_offset=(activations.len()>20).then_some(p.activation_offset+20);activations.truncate(20);
            let title=c.query_row("SELECT title FROM sketch_surfaces WHERE repository_id=?1 AND surface_id=?2",rusqlite::params![p.repository_id,p.surface_id],|r|r.get(0)).optional()?;
            let next_offset=has_more.then_some(p.offset+nodes.len() as u32);
            bound_story(results::SketchStoryResult{repository_id:p.repository_id,surface_id:p.surface_id,surface_title:title,current:current.current,nodes,lineage,activations,has_more,next_offset,revision:current.revision,next_activation_offset},p.offset,p.activation_offset)
        }).map_err(db_error)
    }
    pub fn resolve(
        &self,
        p: params::SketchResolve,
    ) -> Result<results::SketchResolveResult, ProtocolError> {
        self.database
            .call(move |c| resolve_current(c, &p.repository_id, &p.surface_id))
            .map_err(db_error)
    }
    pub fn get(
        &self,
        p: params::SketchReference,
        actor: &str,
    ) -> Result<results::SketchDetail, ProtocolError> {
        let actor = actor.to_owned();
        self.database.call(move |c| {
            let mut sketch=c.query_row(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketch_id=?2"),rusqlite::params![p.repository_id,p.sketch_id],summary_row)?;
            sketch.current=is_current_sketch(c,sketch.surface_id.as_deref(),&p.sketch_id,&p.repository_id)?;
            let (size,digest)=c.query_row("SELECT generation_record_size,generation_record_sha256 FROM sketch_batches WHERE batch_id=?1",[&sketch.batch_id],|r|Ok((r.get::<_,i64>(0)? as u64,r.get(1)?)))?;
            let mut q=c.prepare("SELECT revision,decision,rationale,actor,created_at FROM sketch_decision_history WHERE sketch_id=?1 ORDER BY revision DESC LIMIT 20")?;
            let history=q.query_map([&p.sketch_id],|r|Ok(results::SketchDecisionEvent{revision:r.get(0)?,decision:parse_decision(&r.get::<_,String>(1)?),rationale:r.get(2)?,actor:r.get(3)?,created_at:r.get(4)?}))?.collect::<Result<Vec<_>,_>>()?;
            let mut q=c.prepare("SELECT annotation_id,sketch_id,body,marks_json,state,created_by,created_at,updated_at FROM sketch_annotations WHERE repository_id=?1 AND sketch_id=?2 AND state!='deleted' ORDER BY created_at,rowid")?;
            let annotations=q.query_map(rusqlite::params![p.repository_id,p.sketch_id],|r|annotation_row(r,&actor))?.collect::<Result<Vec<_>,_>>()?;
            let mut q=c.prepare("SELECT parent_sketch_id,child_sketch_id,relation,rationale,actor,created_at FROM sketch_lineage WHERE repository_id=?1 AND child_sketch_id=?2 ORDER BY rowid")?;
            let lineage=q.query_map(rusqlite::params![p.repository_id,p.sketch_id],lineage_row)?.collect::<Result<Vec<_>,_>>()?;
            let initial_context=c.query_row(&format!("SELECT {CONTEXT_COLUMNS} FROM sketch_description_history WHERE sketch_id=?1 AND revision=0"),[&p.sketch_id],context_row).optional()?;
            let mut q=c.prepare(&format!("SELECT {CONTEXT_COLUMNS} FROM sketch_description_history WHERE sketch_id=?1 AND (?2 IS NULL OR revision<?2) ORDER BY revision DESC LIMIT 4"))?;
            let mut description_history=q.query_map(rusqlite::params![p.sketch_id,p.context_before_revision],context_row)?.collect::<Result<Vec<_>,_>>()?;
            let context_history_has_more=description_history.len()>3;description_history.truncate(3);
            Ok(results::SketchDetail{sketch,generation_record_size:size,generation_record_sha256:digest,history,annotations,lineage,description_history,initial_context,context_history_has_more})
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
        text_field("rationale", &p.rationale, 1, 2000)?;
        if p.sketch_ids.len() > 64
            || p.sketch_ids.iter().collect::<HashSet<_>>().len() != p.sketch_ids.len()
        {
            return Err(invalid(
                "continuation IDs must be unique and contain at most 64 options",
            ));
        }
        let actor = caller.actor();
        let now = self.now()?;
        let request = p.clone();
        let actor_tx = actor.clone();
        let time_tx = now.clone();
        let revision=self.database.transaction(move |tx|{
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sketch_surfaces WHERE repository_id=?1 AND surface_id=?2)",rusqlite::params![request.repository_id,request.surface_id],|r|r.get(0))?;
            if !exists {return Err(invalid("surface has no manifest-complete mockups").into());}
            let current=surface_revision(tx,&request.repository_id,&request.surface_id)?;
            if current!=request.expected_revision {return Err(conflict("mockup history changed; refresh before saving this selection").into());}
            for id in &request.sketch_ids {
                validate_id(id,'s')?;
                let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sketches WHERE repository_id=?1 AND surface_id=?2 AND sketch_id=?3 AND legacy=0)",rusqlite::params![request.repository_id,request.surface_id,id],|r|r.get(0))?;
                if !valid{return Err(invalid("only nonlegacy options from this surface and repository can become current").into());}
            }
            let next=bump_surface(tx,&request.repository_id,&request.surface_id)?;
            tx.execute("INSERT INTO sketch_surface_activations(activation_id,repository_id,surface_id,revision,action,sketch_ids_json,rationale,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",rusqlite::params![ids::sketch_activation_id().map_err(id_error)?,request.repository_id,request.surface_id,next,activation_action_text(&request.action),json_text(&request.sketch_ids)?,request.rationale,actor_tx,time_tx])?;
            for id in &request.sketch_ids {
                let (revision,decision):(u32,String)=tx.query_row("SELECT decision_revision,decision FROM sketches WHERE sketch_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                if decision!="keep" {
                    tx.execute("UPDATE sketches SET decision='keep',decision_revision=?2 WHERE sketch_id=?1",rusqlite::params![id,revision+1])?;
                    tx.execute("INSERT INTO sketch_decision_history VALUES(?1,?2,'keep',?3,?4,?5)",rusqlite::params![id,revision+1,request.rationale,actor_tx,time_tx])?;
                }
                reindex_sketch(tx,id)?;
            }
            Ok(next)
        }).map_err(db_error)?;
        self.add_message(
            &p.repository_id,
            "sketch.activation",
            &p.surface_id,
            "The current mockup selection changed",
        )?;
        let current = self
            .resolve(params::SketchResolve {
                repository_id: p.repository_id,
                surface_id: p.surface_id.clone(),
            })?
            .current;
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
        for (name, value, min, max) in [
            ("description", p.description.as_str(), 20, 8000),
            ("journey", &p.journey, 1, 4000),
            ("decisions", &p.decisions, 1, 4000),
            ("instructions", &p.instructions, 1, 4000),
            ("constraints", &p.constraints, 1, 4000),
            ("rationale", &p.rationale, 1, 2000),
        ] {
            text_field(name, value, min, max)?;
        }
        let actor = caller.actor();
        let now = self.now()?;
        let q = p.clone();
        self.database.transaction(move|tx|{
            let (revision,legacy,surface):(u32,bool,Option<String>)=tx.query_row("SELECT description_revision,legacy,surface_id FROM sketches WHERE repository_id=?1 AND sketch_id=?2",rusqlite::params![q.repository_id,q.sketch_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            if legacy{return Err(invalid("legacy context remains historical").into());}
            if revision!=q.expected_revision{return Err(conflict("description changed; refresh before saving").into());}
            tx.execute("INSERT INTO sketch_description_history VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",rusqlite::params![q.sketch_id,revision+1,q.description,q.journey,q.decisions,q.instructions,q.constraints,q.rationale,actor,now])?;
            tx.execute("UPDATE sketches SET description=?3,journey=?4,decisions=?5,instructions=?6,constraints=?7,description_revision=?8 WHERE repository_id=?1 AND sketch_id=?2",rusqlite::params![q.repository_id,q.sketch_id,q.description,q.journey,q.decisions,q.instructions,q.constraints,revision+1])?;
            bump_surface(tx,&q.repository_id,surface.as_deref().unwrap_or_default())?;reindex_sketch(tx,&q.sketch_id)?;Ok(())
        }).map_err(db_error)?;
        self.add_message(
            &p.repository_id,
            "sketch.description",
            &p.sketch_id,
            "The mockup context was revised",
        )?;
        let detail = self.get(
            params::SketchReference {
                repository_id: p.repository_id,
                sketch_id: p.sketch_id,
                offset: 0,
                max_bytes: 184320,
                context_before_revision: None,
            },
            &caller.actor(),
        )?;
        let revision = detail
            .description_history
            .first()
            .cloned()
            .ok_or_else(|| invalid("context revision is unavailable"))?;
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
        text_field("rationale", &p.rationale, 1, 2000)?;
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
            let surface:Option<String>=tx.query_row("SELECT surface_id FROM sketches WHERE sketch_id=?1",[&id],|r|r.get(0))?;
            if let Some(surface)=surface {
                let next_surface=bump_surface(tx,&repo,&surface)?;
                let mut heads=current_ids(tx,&repo,&surface)?;
                if decision!="keep" && heads.contains(&id) {
                    heads.retain(|head|head!=&id);
                    tx.execute("INSERT INTO sketch_surface_activations VALUES(?1,?2,?3,?4,'supersede',?5,?6,?7,?8)",rusqlite::params![ids::sketch_activation_id().map_err(id_error)?,repo,surface,next_surface,json_text(&heads)?,rationale,actor,now])?;
                }
            }
            reindex_sketch(tx,&id)?;
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
                context_before_revision: None,
                max_bytes: 184320,
            },
            &caller.actor(),
        )?;
        let event = detail.history.first().cloned().unwrap();
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
        self.database.transaction(move|tx|{let surface:Option<String>=tx.query_row("SELECT surface_id FROM sketches WHERE repository_id=?1 AND sketch_id=?2",rusqlite::params![repo,sketch],|r|r.get(0))?;if let Some(surface)=surface{bump_surface(tx,&repo,&surface)?;}tx.execute("INSERT INTO sketch_annotations(annotation_id,sketch_id,repository_id,body,marks_json,state,created_by,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'open',?6,?7,?7)",rusqlite::params![id,sketch,repo,body,marks,actor,now])?;reindex_sketch(tx,&sketch)?;insert_fallback_task(tx,&fallback_task,&repo_for_task,"Review a sketch annotation","The marked sketch feedback must be handled by the next agent review.",&actor,&now)?;Ok(())}).map_err(db_error)?;
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
                context_before_revision: None,
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

const CONTEXT_COLUMNS: &str =
    "revision,description,journey,decisions,instructions,constraints,rationale,actor,created_at";
const CURRENT_SQL: &str = "EXISTS(SELECT 1 FROM sketch_surface_activations a,json_each(a.sketch_ids_json) j WHERE a.repository_id=sketches.repository_id AND a.surface_id=sketches.surface_id AND j.value=sketches.sketch_id AND a.revision=(SELECT MAX(revision) FROM sketch_surface_activations WHERE repository_id=a.repository_id AND surface_id=a.surface_id))";
fn text_field(name: &str, value: &str, min: usize, max: usize) -> Result<(), ProtocolError> {
    if value.trim().len() < min
        || value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(invalid(format!(
            "{name} must contain {min}..{max} bytes of plain text"
        )));
    }
    Ok(())
}
fn validate_id(value: &str, prefix: char) -> Result<(), ProtocolError> {
    if value.len() != 17
        || !value.starts_with(prefix)
        || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(invalid("invalid record identity"));
    }
    Ok(())
}
fn conflict(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ConfigurationConflict, message)
}
fn json_text(value: &impl serde::Serialize) -> Result<String, DatabaseError> {
    serde_json::to_string(value)
        .map_err(|_| DatabaseError::Domain(invalid("cannot encode mockup context")))
}
fn surface_revision(
    c: &rusqlite::Connection,
    repo: &str,
    surface: &str,
) -> Result<u32, DatabaseError> {
    Ok(c.query_row(
        "SELECT revision FROM sketch_surfaces WHERE repository_id=?1 AND surface_id=?2",
        rusqlite::params![repo, surface],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(0))
}
fn bump_surface(c: &rusqlite::Connection, repo: &str, surface: &str) -> Result<u32, DatabaseError> {
    c.execute(
        "UPDATE sketch_surfaces SET revision=revision+1 WHERE repository_id=?1 AND surface_id=?2",
        rusqlite::params![repo, surface],
    )?;
    surface_revision(c, repo, surface)
}
fn decision_text(value: &SketchDecision) -> &'static str {
    match value {
        SketchDecision::Keep => "keep",
        SketchDecision::Reject => "reject",
        SketchDecision::Undecided => "undecided",
    }
}
fn parse_decision(value: &str) -> SketchDecision {
    match value {
        "keep" => SketchDecision::Keep,
        "reject" => SketchDecision::Reject,
        _ => SketchDecision::Undecided,
    }
}
fn context_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::SketchDescriptionRevision> {
    Ok(results::SketchDescriptionRevision {
        revision: r.get(0)?,
        description: r.get(1)?,
        journey: r.get(2)?,
        decisions: r.get(3)?,
        instructions: r.get(4)?,
        constraints: r.get(5)?,
        rationale: r.get(6)?,
        actor: r.get(7)?,
        created_at: r.get(8)?,
    })
}
fn lineage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::SketchLineageEvent> {
    Ok(results::SketchLineageEvent {
        parent_sketch_id: r.get(0)?,
        child_sketch_id: r.get(1)?,
        relation: parse_lineage_relation(&r.get::<_, String>(2)?),
        rationale: r.get(3)?,
        actor: r.get(4)?,
        created_at: r.get(5)?,
    })
}
fn activation_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::SketchActivationEvent> {
    let json: String = r.get(2)?;
    let ids = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(results::SketchActivationEvent {
        revision: r.get(0)?,
        action: parse_activation_action(&r.get::<_, String>(1)?),
        sketch_ids: ids,
        rationale: r.get(3)?,
        actor: r.get(4)?,
        created_at: r.get(5)?,
    })
}
fn compact_summary(mut row: results::SketchImageSummary) -> results::SketchImageSummary {
    row.description = row.description.map(|s| s.chars().take(240).collect());
    row.journey = None;
    row.decisions = None;
    row.instructions = None;
    row.constraints = None;
    row.element_ids.clear();
    row.transition_note = None;
    row
}
fn query_summaries(
    c: &rusqlite::Connection,
    condition: &str,
    values: Vec<rusqlite::types::Value>,
) -> Result<Vec<results::SketchImageSummary>, DatabaseError> {
    let mut q=c.prepare(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE {condition}"))?;
    let rows = q
        .query_map(rusqlite::params_from_iter(values), summary_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|row| {
            let mut row = compact_summary(row);
            row.current = is_current_sketch(
                c,
                row.surface_id.as_deref(),
                &row.sketch_id,
                &row.repository_id,
            )?;
            Ok(row)
        })
        .collect()
}
fn bounded_summaries(
    rows: Vec<results::SketchImageSummary>,
    limit: usize,
) -> (Vec<results::SketchImageSummary>, bool) {
    let mut result = Vec::new();
    let count = rows.len();
    let mut bytes = 0;
    for row in rows {
        let size = serde_json::to_vec(&row)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if result.len() >= limit || bytes + size > 96 * 1024 {
            break;
        }
        bytes += size;
        result.push(row);
    }
    let more = result.len() < count;
    (result, more)
}
fn bound_story(
    mut story: results::SketchStoryResult,
    offset: u32,
    event_offset: u32,
) -> Result<results::SketchStoryResult, DatabaseError> {
    while serde_json::to_vec(&story)
        .map_err(|_| invalid("cannot encode story"))?
        .len()
        > 240 * 1024
    {
        if !story.activations.is_empty() {
            story.activations.pop();
            story.next_activation_offset = Some(event_offset + story.activations.len() as u32);
        } else if story.nodes.len() > 1 {
            let dropped = story.nodes.pop().unwrap();
            story
                .lineage
                .retain(|edge| edge.child_sketch_id != dropped.sketch_id);
            story.has_more = true;
            story.next_offset = Some(offset + story.nodes.len() as u32);
        } else {
            return Err(invalid("story context exceeds the response budget").into());
        }
    }
    Ok(story)
}
fn resolve_current(
    c: &rusqlite::Connection,
    repo: &str,
    surface: &str,
) -> Result<results::SketchResolveResult, DatabaseError> {
    let ids = current_ids(c, repo, surface)?;
    let revision = surface_revision(c, repo, surface)?;
    let mut nodes = Vec::new();
    for id in &ids {
        let row=c.query_row(&format!("SELECT {SUMMARY_COLUMNS} FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND surface_id=?2 AND sketch_id=?3 AND legacy=0"),rusqlite::params![repo,surface,id],summary_row).optional()?;
        if let Some(mut row) = row {
            row.current = true;
            nodes.push(compact_summary(row));
        }
    }
    let (status, rationale) = if !nodes.is_empty() && nodes.len() == ids.len() {
        (
            results::SketchResolveStatus::Resolved,
            "Use the explicitly selected continuation set.",
        )
    } else if !ids.is_empty() {
        (
            results::SketchResolveStatus::Ambiguous,
            "The saved selection has an unavailable member; resolve it before implementation.",
        )
    } else if let Some(batch) = surface.strip_prefix("legacy:") {
        let legacy:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sketches WHERE repository_id=?1 AND batch_id=?2 AND legacy=1)",rusqlite::params![repo,batch],|r|r.get(0))?;
        if legacy {
            (
                results::SketchResolveStatus::LegacyOnly,
                "Historical sketches cannot become current.",
            )
        } else {
            (
                results::SketchResolveStatus::Unavailable,
                "No current selection exists.",
            )
        }
    } else {
        (
            results::SketchResolveStatus::Unavailable,
            "No current selection exists; generation and Keep flags do not select a source.",
        )
    };
    Ok(results::SketchResolveResult {
        repository_id: repo.into(),
        surface_id: surface.into(),
        status,
        revision,
        rationale: rationale.into(),
        current: nodes,
    })
}
pub(crate) fn reindex_sketch(c: &rusqlite::Connection, id: &str) -> Result<(), DatabaseError> {
    let (repo,surface,base):(String,String,String)=c.query_row("SELECT repository_id,COALESCE(surface_id,''),title||' '||COALESCE(surface_title,'')||' '||COALESCE(surface_id,'')||' '||element_ids_json||' '||COALESCE(state_name,'')||' '||COALESCE(theme,'')||' '||COALESCE(viewport,'')||' '||transition_note FROM sketches WHERE sketch_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let context:String=c.query_row("SELECT COALESCE(group_concat(description||' '||journey||' '||decisions||' '||instructions||' '||constraints||' '||rationale,' '),'') FROM sketch_description_history WHERE sketch_id=?1",[id],|r|r.get(0))?;
    let decisions:String=c.query_row("SELECT COALESCE(group_concat(rationale,' '),'') FROM sketch_decision_history WHERE sketch_id=?1",[id],|r|r.get(0))?;
    let comments:String=c.query_row("SELECT COALESCE(group_concat(body,' '),'') FROM sketch_annotations WHERE sketch_id=?1 AND state!='deleted'",[id],|r|r.get(0))?;
    let activations:String=c.query_row("SELECT COALESCE(group_concat(rationale,' '),'') FROM sketch_surface_activations a WHERE repository_id=?1 AND EXISTS(SELECT 1 FROM json_each(a.sketch_ids_json) WHERE value=?2)",rusqlite::params![repo,id],|r|r.get(0))?;
    c.execute("DELETE FROM sketches_fts WHERE sketch_id=?1", [id])?;
    c.execute("INSERT INTO sketches_fts(sketch_id,repository_id,surface_id,searchable) VALUES(?1,?2,?3,?4)",rusqlite::params![id,repo,surface,format!("{base} {context} {decisions} {comments} {activations}")])?;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), ProtocolError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)
}
fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<results::SketchImageSummary> {
    let element_ids =
        serde_json::from_str::<Vec<String>>(&r.get::<_, String>(14)?).unwrap_or_default();
    Ok(results::SketchImageSummary {
        sketch_id: r.get(0)?,
        repository_id: r.get(1)?,
        sketch_set: r.get(2)?,
        batch_id: r.get(27)?,
        display_order: r.get(28)?,
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
    match value {
        Some(value) => serde_json::from_str(&value)
            .map_err(|_| DatabaseError::Domain(invalid("invalid saved continuation set"))),
        None => Ok(Vec::new()),
    }
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

fn validate_manifest(m: &params::SketchManifest) -> Result<(), ProtocolError> {
    if m.window_count != 1 {
        return Err(invalid(
            "each mockup file must show exactly one window, state, theme and viewport",
        ));
    }
    for (name, value, min, max) in [
        ("surface", m.surface_id.as_str(), 1, 160),
        ("surface title", &m.surface_title, 1, 200),
        ("state", &m.state, 1, 120),
        ("theme", &m.theme, 1, 40),
        ("viewport", &m.viewport, 1, 80),
        ("description", &m.description, 20, 8000),
        ("journey", &m.journey, 1, 4000),
        ("decisions", &m.decisions, 1, 4000),
        ("instructions", &m.instructions, 1, 4000),
        ("constraints", &m.constraints, 1, 4000),
    ] {
        text_field(name, value, min, max)?;
    }
    if m.surface_id.starts_with("legacy:")
        || !m
            .surface_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
    {
        return Err(invalid(
            "surface identity must be a stable nonlegacy identifier",
        ));
    }
    if m.element_ids.len() > 64
        || m.element_ids.iter().collect::<HashSet<_>>().len() != m.element_ids.len()
    {
        return Err(invalid(
            "element identities must be unique and contain at most 64 entries",
        ));
    }
    for id in &m.element_ids {
        text_field("element identity", id, 1, 160)?;
    }
    if m.parent_relations.len() > 16 {
        return Err(invalid("at most 16 parents are supported"));
    }
    let mut parents = HashSet::new();
    for parent in &m.parent_relations {
        validate_id(&parent.parent_sketch_id, 's')?;
        if !parents.insert((
            parent.parent_sketch_id.clone(),
            lineage_relation_text(&parent.relation),
        )) {
            return Err(invalid("duplicate lineage edge"));
        }
    }
    text_field(
        "transition rationale",
        &m.transition_note,
        if m.parent_relations.is_empty() { 0 } else { 1 },
        2000,
    )?;
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
                        display_order: 1,
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
                        display_order: 1,
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
                    expected_revision: 2,
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
                offset: 0,
                activation_offset: 0,
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
                state: None,
                theme: None,
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
                state: None,
                theme: None,
                batch_id: None,
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
                state: None,
                theme: None,
                batch_id: None,
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
                    expected_revision: 1,
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
