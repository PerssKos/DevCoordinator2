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
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHUNK: u32 = 180 * 1024;
const MAX_RECORD: u64 = 32 * 1024 * 1024;

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
        let root = self.state_dir.join("sketches").join(&p.repository_id);
        fs::create_dir_all(&root).map_err(io_error)?;
        let batch_id = ids::sketch_batch_id().map_err(id_error)?;
        let now = self.now()?;
        let record = read_file(&p.generation_record_path, MAX_RECORD)?;
        let record_sha = sha(&record);
        let existing = {
            let key = p.idempotency_key.clone();
            let repo = p.repository_id.clone();
            self.database.call(move |c| c.query_row(
                "SELECT batch_id FROM sketch_batches WHERE repository_id=?1 AND idempotency_key=?2",
                rusqlite::params![repo, key], |r| r.get::<_,String>(0)).optional().map_err(DatabaseError::from)
            ).map_err(db_error)?
        };
        if let Some(existing) = existing {
            return self.batch(&p.repository_id, &existing);
        }
        let batch_dir = root.join(&batch_id);
        fs::create_dir_all(&batch_dir).map_err(io_error)?;
        let record_path = batch_dir.join("generation-record.bin");
        fs::write(&record_path, &record).map_err(io_error)?;
        let mut metas = Vec::new();
        for (index, input) in p.images.iter().enumerate() {
            let bytes = read_file(&input.path, MAX_BYTES)?;
            let (width, height) =
                png_dimensions(&bytes).ok_or_else(|| invalid("sketch images must be PNG files"))?;
            let id = ids::sketch_id().map_err(id_error)?;
            let image_path = batch_dir.join(format!("{index}-{id}.png"));
            fs::write(&image_path, &bytes).map_err(io_error)?;
            metas.push((
                id,
                input.title.clone(),
                image_path,
                bytes.len() as u64,
                sha(&bytes),
                width,
                height,
            ));
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
        self.database.transaction(move |tx| {
            tx.execute("INSERT INTO sketch_batches(batch_id,repository_id,sketch_set,source_skill,generation_record_path,generation_record_size,generation_record_sha256,idempotency_key,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![batch_for_insert,repo_for_insert,set,skill,gen_path,record_size,record_sha,idem,now_for_insert,actor_for_insert])?;
            for (id,title,path,size,digest,width,height) in metas {
                tx.execute("INSERT INTO sketches(sketch_id,batch_id,repository_id,title,file_path,byte_size,sha256,mime,width,height,decision,decision_revision,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,'image/png',?8,?9,'undecided',0,?10,?11)",
                    rusqlite::params![id,batch_for_rows,repo,title,path.to_string_lossy().to_string(),size as i64,digest,width,height,now,actor_for_insert])?;
            }
            Ok(())
        }).map_err(db_error)?;
        self.batch(&p.repository_id, &batch_id)
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
            let mut st=c.prepare("SELECT sketches.sketch_id,sketches.repository_id,sketch_batches.sketch_set,sketch_batches.source_skill,sketches.title,sketches.sha256,sketches.byte_size,sketches.width,sketches.height,sketches.created_at,sketches.decision,sketches.decision_revision FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.batch_id=?1 ORDER BY sketches.rowid")?;
            let sketches=st.query_map([&bid],|r| summary_row(r)).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            Ok(results::SketchBatch{batch_id:b.0,repository_id:b.1,sketch_set:b.2,source_skill:b.3,generation_record_size:b.4,generation_record_sha256:b.5,created_at:b.6,sketches})
        }).map_err(db_error)
    }
    pub fn list(&self, p: params::SketchList) -> Result<results::SketchListResult, ProtocolError> {
        let repo = p.repository_id.clone();
        let skill = p.source_skill.clone();
        let dec = p.decision.clone();
        let set = p.sketch_set.clone();
        let limit = p.limit.clamp(1, 100);
        self.database.call(move|c|{
            let mut sql="SELECT sketches.sketch_id,sketches.repository_id,sketch_batches.sketch_set,sketch_batches.source_skill,sketches.title,sketches.sha256,sketches.byte_size,sketches.width,sketches.height,sketches.created_at,sketches.decision,sketches.decision_revision FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1".to_owned();
            let mut vals:Vec<rusqlite::types::Value>=vec![repo.clone().into()];
            if let Some(v)=skill { sql.push_str(" AND source_skill=?"); vals.push(v.into()); }
            if let Some(v)=set { sql.push_str(" AND sketch_set=?"); vals.push(v.into()); }
            if let Some(v)=dec { sql.push_str(" AND decision=?"); vals.push(serde_json::to_string(&v).unwrap().trim_matches('"').to_owned().into()); }
            sql.push_str(&format!(" ORDER BY sketches.created_at DESC LIMIT {}",limit));
            let mut st=c.prepare(&sql)?; let rows=st.query_map(rusqlite::params_from_iter(vals),summary_row)?.collect::<Result<Vec<_>,_>>()?;
            Ok(results::SketchListResult{repository_id:repo,has_more:rows.len()==usize::from(limit),sketches:rows})
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
            let row=c.query_row("SELECT sketches.sketch_id,sketches.repository_id,sketch_batches.sketch_set,sketch_batches.source_skill,sketches.title,sketches.sha256,sketches.byte_size,sketches.width,sketches.height,sketches.created_at,sketches.decision,sketches.decision_revision,sketches.batch_id FROM sketches JOIN sketch_batches USING(batch_id) WHERE sketches.repository_id=?1 AND sketches.sketch_id=?2",rusqlite::params![repo,id],|r| Ok((summary_row(r)?,r.get::<_,String>(12)?)))?;
            let (summary,batch_id)=row;
            let (size,digest):(u64,String)=c.query_row("SELECT generation_record_size,generation_record_sha256 FROM sketch_batches WHERE batch_id=?1",[batch_id],|r|Ok((r.get::<_,i64>(0)? as u64,r.get(1)?)))?;
            let mut st=c.prepare("SELECT revision,decision,rationale,actor,created_at FROM sketch_decision_history WHERE sketch_id=?1 ORDER BY revision")?;
            let history=st.query_map([&summary.sketch_id],|r|Ok(results::SketchDecisionEvent{revision:r.get(0)?,decision:serde_json::from_str(&format!("\"{}\"",r.get::<_,String>(1)?)).unwrap_or(SketchDecision::Undecided),rationale:r.get(2)?,actor:r.get(3)?,created_at:r.get(4)?})).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            let mut ann=c.prepare("SELECT annotation_id,sketch_id,body,marks_json,state,created_by,created_at,updated_at FROM sketch_annotations WHERE sketch_id=?1 AND state!='deleted' ORDER BY created_at")?;
            let annotations=ann.query_map([&summary.sketch_id],|r| annotation_row(r,&actor)).map_err(DatabaseError::from)?.collect::<Result<Vec<_>,_>>()?;
            Ok(results::SketchDetail{sketch:summary,generation_record_size:size,generation_record_sha256:digest,history,annotations})
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
        let repo = p.repository_id;
        let limit = p.limit.clamp(1, 100);
        self.database.call(move|c|{ let mut st=c.prepare("SELECT message_id,repository_id,kind,subject_id,summary,created_at,claimed_by,acknowledged_at IS NOT NULL FROM agent_messages WHERE repository_id=?1 AND acknowledged_at IS NULL ORDER BY created_at LIMIT ?2")?; let rows=st.query_map(rusqlite::params![repo,limit],message_row)?.collect::<Result<Vec<_>,_>>()?; Ok(results::AgentMessageList{has_more:rows.len()==usize::from(limit),messages:rows}) }).map_err(db_error)
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
        self.database.transaction(move|tx|{ let updated=tx.execute("UPDATE agent_messages SET claimed_by=?1,claimed_until=?2 WHERE repository_id=?3 AND message_id=?4 AND acknowledged_at IS NULL AND (claimed_until IS NULL OR claimed_until < ?5)",rusqlite::params![actor2,until2,repo,id,now])?; if updated==0{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::ConfigurationConflict,"message is already claimed or acknowledged")))} Ok(()) }).map_err(db_error)?;
        self.message_by_id(&p.repository_id, &p.message_id)
    }
    pub fn message_ack(
        &self,
        p: params::AgentMessageAck,
        caller: &Caller,
    ) -> Result<results::AgentMessageMutation, ProtocolError> {
        let actor = caller.actor();
        let repo = p.repository_id.clone();
        let id = p.message_id.clone();
        self.database.transaction(move|tx|{let updated=tx.execute("UPDATE agent_messages SET acknowledged_at=strftime('%Y-%m-%dT%H:%M:%SZ','now'),acknowledged_by=?1 WHERE repository_id=?2 AND message_id=?3 AND acknowledged_at IS NULL AND claimed_by=?1",rusqlite::params![actor,repo,id])?; if updated==0{return Err(DatabaseError::Domain(ProtocolError::new(ErrorCode::PermissionDenied,"message is not claimed by this agent")))} Ok(())}).map_err(db_error)?;
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
    })
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
        let record = root.join("record.json");
        fs::write(&record, b"{\"prompt\":\"fixture\"}").unwrap();
        let service = SketchService::new(&config(root), db);
        let caller = Caller::from_client(1, 1000, 1000, ClientContext::default(), None).unwrap();
        let batch = service
            .publish(
                params::SketchPublish {
                    repository_id: "r1111111111111111".into(),
                    sketch_set: "set".into(),
                    source_skill: "Image Gen".into(),
                    generation_record_path: record.to_string_lossy().into_owned(),
                    images: vec![params::SketchImageInput {
                        title: "First".into(),
                        path: image.to_string_lossy().into_owned(),
                    }],
                    idempotency_key: "fixture-1".into(),
                },
                &caller,
            )
            .unwrap();
        assert_eq!(batch.sketches.len(), 1);
        let sketch = &batch.sketches[0];
        let list = service
            .list(params::SketchList {
                repository_id: batch.repository_id.clone(),
                source_skill: None,
                decision: None,
                sketch_set: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(list.sketches.len(), 1);
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
    }
}
