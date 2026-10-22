//! Durable Agent communication hub service.
//!
//! This module intentionally has no dependency on tickets or the internal
//! reminder `agent_messages` table.  Private and opt-in public spaces are
//! separated at every query boundary; remote calls use the dedicated edge
//! adapter and instance bearer key.
use std::time::Duration;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use devcoordinator2_api::agent_hub as api;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use reqwest::blocking::Client;
use rusqlite::{OptionalExtension, Row};
use sha2::{Digest, Sha256};
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::access::Caller;
use crate::database::{Database, DatabaseError};
use crate::ids;

const LEASE_SECONDS: i64 = 90;
const KEY_OVERLAP_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_ATTACHMENTS: usize = 16;
const MAX_FILE_BYTES: u32 = 16 * 1024 * 1024;
const MAX_MESSAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 32 * 1024;
const MAX_PENDING_UPLOADS: i64 = 64;
const MAX_PENDING_BYTES: i64 = 128 * 1024 * 1024;
const MAX_OUTBOX_ROWS: i64 = 1_000;
const MAX_OUTBOX_ATTEMPTS: i64 = 10;
const MAX_OUTBOX_AGE_SECONDS: i64 = 24 * 60 * 60;
const OUTBOX_BATCH: i64 = 20;

#[derive(Clone)]
pub struct AgentHubService {
    database: Database,
}

#[derive(Clone)]
struct AgentRow {
    agent_id: String,
    handle: String,
    space: api::Space,
    display_name: String,
    repository_label: Option<String>,
    domain_label: Option<String>,
    current_subject: Option<String>,
    capabilities: Vec<String>,
    token_hash: String,
    callback_url: Option<String>,
    state: api::AgentState,
    lease_expires_at: String,
}

impl AgentHubService {
    pub fn new(database: Database) -> Result<Self, ProtocolError> {
        let service = Self { database };
        service.database.call(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO agent_hub_settings(singleton,revision,public_enabled,public_paused) VALUES(1,0,0,0)",
                [],
            )?;
            Ok(())
        }).map_err(database_error)?;
        Ok(service)
    }

    pub fn register(
        &self,
        request: api::Register,
        _caller: &Caller,
    ) -> Result<api::RegisterResult, ProtocolError> {
        validate_name(&request.display_name)?;
        validate_optional_label(request.repository_label.as_deref())?;
        validate_optional_label(request.domain_label.as_deref())?;
        validate_optional_label(request.current_subject.as_deref())?;
        validate_capabilities(&request.capabilities)?;
        let callback_url = request
            .callback_url
            .as_deref()
            .map(validate_callback_url)
            .transpose()?;
        if request.space == api::Space::Public && !self.public_available()? {
            return Err(hub_disabled());
        }
        let agent_id = ids::hub_agent_id().map_err(id_error)?;
        let token = secret()?;
        let callback_secret = callback_url.as_ref().map(|_| secret()).transpose()?;
        let now = now_text()?;
        let expiry = timestamp_after(LEASE_SECONDS)?;
        let handle = format!("agent-{}", &agent_id[1..]);
        let hash = hash_secret(&token);
        let row = AgentRow {
            agent_id: agent_id.clone(),
            handle: handle.clone(),
            space: request.space,
            display_name: request.display_name.clone(),
            repository_label: request.repository_label.clone(),
            domain_label: request.domain_label.clone(),
            current_subject: request.current_subject.clone(),
            capabilities: request.capabilities.clone(),
            token_hash: hash.clone(),
            callback_url: callback_url.clone(),
            state: api::AgentState::Active,
            lease_expires_at: expiry.clone(),
        };
        let capabilities =
            serde_json::to_string(&request.capabilities).map_err(|e| internal(e.to_string()))?;
        let callback_secret_for_insert = callback_secret.clone();
        self.database.transaction(move |tx| {
            tx.execute(
                "INSERT INTO agent_hub_agents(agent_id,handle,space,display_name,repository_label,domain_label,current_subject,capabilities_json,token_hash,callback_url,callback_secret,state,lease_expires_at,created_at,last_seen_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'active',?12,?13,?13)",
                rusqlite::params![agent_id,handle,space_text(request.space),request.display_name,request.repository_label,request.domain_label,request.current_subject,capabilities,hash,callback_url,callback_secret_for_insert,expiry,now],
            )?;
            Ok(())
        }).map_err(database_error)?;
        Ok(api::RegisterResult {
            agent: row.to_profile(),
            token,
            callback_secret,
            lease_seconds: LEASE_SECONDS as u16,
        })
    }

    pub fn heartbeat(
        &self,
        request: api::Heartbeat,
        caller: &Caller,
    ) -> Result<api::AgentProfile, ProtocolError> {
        validate_optional_label(request.current_subject.as_deref())?;
        let now = now_text()?;
        let expiry = timestamp_after(LEASE_SECONDS)?;
        let agent = self.authenticate(&request.auth, caller, None)?;
        let id = agent.agent_id.clone();
        let subject = request.current_subject;
        self.database.call(move |db| {
            let changed = db.execute("UPDATE agent_hub_agents SET current_subject=COALESCE(?1,current_subject),lease_expires_at=?2,last_seen_at=?3,state='active' WHERE agent_id=?4 AND state!='unregistered'", rusqlite::params![subject,expiry,now,id])?;
            if changed == 0 { return Err(DatabaseError::Domain(hub_not_found())); }
            Ok(())
        }).map_err(database_error)?;
        self.agent_profile(&agent.agent_id)
    }

    pub fn unregister(
        &self,
        request: api::Unregister,
        caller: &Caller,
    ) -> Result<api::AgentProfile, ProtocolError> {
        let agent = self.authenticate(&request.auth, caller, None)?;
        let id = agent.agent_id.clone();
        let now = now_text()?;
        self.database.transaction(move |tx| {
            tx.execute("UPDATE agent_hub_agents SET state='unregistered',unregistered_at=?1,last_seen_at=?1 WHERE agent_id=?2", rusqlite::params![now,id])?;
            tx.execute("DELETE FROM agent_hub_room_members WHERE agent_id=?1", [id])?;
            Ok(())
        }).map_err(database_error)?;
        self.agent_profile(&agent.agent_id)
    }

    pub fn directory(
        &self,
        request: api::Directory,
    ) -> Result<api::DirectoryResult, ProtocolError> {
        self.expire_leases()?;
        if request.space == api::Space::Public && !self.public_available()? {
            return Err(hub_disabled());
        }
        let limit = bounded_page(request.limit);
        let after = request.after;
        let space = space_text(request.space);
        let rows = self.database.call(move |db| {
            let mut statement = db.prepare("SELECT agent_id,handle,space,display_name,repository_label,domain_label,current_subject,capabilities_json,token_hash,callback_url,callback_secret,state,lease_expires_at,created_at,last_seen_at FROM agent_hub_agents WHERE space=?1 AND state!='unregistered' AND (?2 IS NULL OR handle>?2) ORDER BY handle LIMIT ?3")?;
            let iter = statement.query_map(rusqlite::params![space,after,limit as i64], agent_row)?;
            iter.collect::<Result<Vec<_>,_>>().map_err(DatabaseError::from)
        }).map_err(database_error)?;
        let next = (rows.len() == limit as usize)
            .then(|| rows.last().map(|r| r.handle.clone()))
            .flatten();
        let profiles = rows.into_iter().map(|r| r.to_profile()).collect();
        Ok(api::DirectoryResult {
            agents: profiles,
            next,
            public_enabled: self.public_enabled()?,
        })
    }

    pub fn room_create(
        &self,
        request: api::RoomCreate,
        caller: &Caller,
    ) -> Result<api::Room, ProtocolError> {
        let creator = self.authenticate(&request.auth, caller, Some(request.space))?;
        self.ensure_public(request.space)?;
        validate_name(&request.name)?;
        let room_id = ids::hub_room_id().map_err(id_error)?;
        let room_id_for_insert = room_id.clone();
        let now = now_text()?;
        let creator_id = creator.agent_id.clone();
        let requested_members = request.members.clone();
        let name = request.name.clone();
        let space = request.space;
        self.database.transaction(move |tx| {
            tx.execute("INSERT INTO agent_hub_rooms(room_id,space,name,created_by,created_at) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![room_id_for_insert.clone(),space_text(space),name,creator_id,now])?;
            tx.execute("INSERT INTO agent_hub_room_members(room_id,agent_id,joined_at) VALUES(?1,?2,?3)", rusqlite::params![room_id_for_insert.clone(),creator_id,now])?;
            for member in requested_members {
                let member_space: String = tx.query_row("SELECT space FROM agent_hub_agents WHERE agent_id=?1 AND state='active'", [&member], |r| r.get(0)).optional()?.ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
                if member_space != space_text(space) { return Err(DatabaseError::Domain(invalid("room members must use one hub space"))); }
                tx.execute("INSERT OR IGNORE INTO agent_hub_room_members(room_id,agent_id,joined_at) VALUES(?1,?2,?3)", rusqlite::params![room_id_for_insert.clone(),member,now])?;
            }
            Ok(())
        }).map_err(database_error)?;
        self.room(&room_id)
    }

    pub fn room_list(
        &self,
        request: api::RoomList,
        caller: &Caller,
    ) -> Result<api::RoomListResult, ProtocolError> {
        let agent = self.authenticate(&request.auth, caller, request.space)?;
        let limit = bounded_page(request.limit);
        let id = agent.agent_id;
        let space = request.space.map(space_text);
        let rooms = self.database.call(move |db| {
            let mut statement = db.prepare("SELECT r.room_id,r.space,r.name,r.created_at FROM agent_hub_rooms r JOIN agent_hub_room_members m ON m.room_id=r.room_id WHERE m.agent_id=?1 AND (?2 IS NULL OR r.space=?2) AND r.archived_at IS NULL ORDER BY r.created_at,r.room_id LIMIT ?3")?;
            let mut rows = Vec::new();
            for row in statement.query_map(rusqlite::params![id,space,limit as i64], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
                let (room_id,space,name,created_at) = row?;
                let members = room_members(db, &room_id)?;
                rows.push(api::Room {room_id,space:parse_space(&space)?,name,created_at,members});
            }
            Ok(rows)
        }).map_err(database_error)?;
        Ok(api::RoomListResult { rooms })
    }

    pub fn room_member_add(
        &self,
        request: api::RoomMemberChange,
        caller: &Caller,
    ) -> Result<api::Room, ProtocolError> {
        self.room_member_change(request, caller, true)
    }
    pub fn room_member_remove(
        &self,
        request: api::RoomMemberChange,
        caller: &Caller,
    ) -> Result<api::Room, ProtocolError> {
        self.room_member_change(request, caller, false)
    }
    fn room_member_change(
        &self,
        request: api::RoomMemberChange,
        caller: &Caller,
        add: bool,
    ) -> Result<api::Room, ProtocolError> {
        let actor = self.authenticate(&request.auth, caller, None)?;
        let room_id = request.room_id.clone();
        let member = request.agent_id.clone();
        let actor_id = actor.agent_id.clone();
        let now = now_text()?;
        self.database.transaction(move |tx| {
            let space: String = tx.query_row("SELECT space FROM agent_hub_rooms WHERE room_id=?1 AND archived_at IS NULL", [&room_id], |r| r.get(0)).optional()?.ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
            let actor_member: i64 = tx.query_row("SELECT count(*) FROM agent_hub_room_members WHERE room_id=?1 AND agent_id=?2", rusqlite::params![room_id.clone(),actor_id], |r| r.get(0))?;
            if actor_member == 0 { return Err(DatabaseError::Domain(denied())); }
            let member_space: Option<String> = tx.query_row("SELECT space FROM agent_hub_agents WHERE agent_id=?1 AND state='active'", [&member], |r| r.get(0)).optional()?;
            if add {
                if member_space.as_deref() != Some(&space) { return Err(DatabaseError::Domain(hub_not_found())); }
                tx.execute("INSERT OR IGNORE INTO agent_hub_room_members(room_id,agent_id,joined_at) VALUES(?1,?2,?3)", rusqlite::params![room_id.clone(),member,now])?;
            } else { tx.execute("DELETE FROM agent_hub_room_members WHERE room_id=?1 AND agent_id=?2", rusqlite::params![room_id.clone(),member])?; }
            Ok(())
        }).map_err(database_error)?;
        self.room(&request.room_id)
    }

    pub fn send(&self, request: api::Send, caller: &Caller) -> Result<api::Message, ProtocolError> {
        let sender = self.authenticate(&request.auth, caller, None)?;
        self.send_inner(
            Some(&sender),
            Some(request.auth.clone()),
            &request.room_id,
            &request.body,
            &request.attachments,
            request.client_message_id.as_deref(),
        )
    }

    pub fn receive(
        &self,
        request: api::Receive,
        caller: &Caller,
    ) -> Result<api::ReceiveResult, ProtocolError> {
        let recipient = self.authenticate(&request.auth, caller, None)?;
        self.receive_for(
            &recipient.agent_id,
            request.room_id.as_deref(),
            request.after.as_deref(),
            request.limit,
        )
    }

    pub fn ack(
        &self,
        request: api::Ack,
        caller: &Caller,
    ) -> Result<api::ReceiveResult, ProtocolError> {
        let recipient = self.authenticate(&request.auth, caller, None)?;
        let ids = request.delivery_ids;
        if ids.len() > 100 {
            return Err(invalid("at most 100 deliveries may be acknowledged"));
        }
        let now = now_text()?;
        let recipient_id = recipient.agent_id.clone();
        self.database.transaction(move |tx| {
            for id in ids {
                tx.execute(
                    "UPDATE agent_hub_deliveries SET acknowledged_at=?1 WHERE delivery_id=?2 AND recipient_agent_id=?3",
                    rusqlite::params![now, id, recipient_id],
                )?;
                tx.execute("DELETE FROM agent_hub_outbox WHERE delivery_id=?1", [&id])?;
            }
            Ok(())
        }).map_err(database_error)?;
        Ok(api::ReceiveResult {
            deliveries: Vec::new(),
            next: None,
        })
    }

    pub fn upload_start(
        &self,
        request: api::UploadStart,
        caller: &Caller,
    ) -> Result<api::UploadStartResult, ProtocolError> {
        let owner = self.authenticate(&request.auth, caller, None)?;
        if request.byte_size > MAX_FILE_BYTES || request.byte_size == 0 {
            return Err(hub_attachment_invalid("file must be 1..16777216 bytes"));
        }
        validate_name(&request.name)?;
        validate_digest(&request.sha256)?;
        let now = now_text()?;
        let expiry = timestamp_after(24 * 60 * 60)?;
        let upload_id = ids::hub_upload_id().map_err(id_error)?;
        let owner_id = owner.agent_id.clone();
        let request_key = request.request_key.clone();
        let name = request.name.clone();
        let ctype = request.content_type.clone();
        let size = request.byte_size;
        let digest = request.sha256.to_ascii_lowercase();
        let result = self.database.transaction(move |tx| {
            tx.execute("DELETE FROM agent_hub_uploads WHERE expires_at<?1", [&now])?;
            if let Some(existing) = tx.query_row("SELECT upload_id,name,content_type,byte_size,sha256,length(content) FROM agent_hub_uploads WHERE owner_agent_id=?1 AND request_key=?2", rusqlite::params![owner_id,request_key], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u32>(3)?,r.get::<_,String>(4)?,r.get::<_,u32>(5)?))).optional()? {
                if existing.1 != name || existing.3 != size || existing.4 != digest { return Err(DatabaseError::Domain(invalid("upload request key was reused for different content"))); }
                return Ok(api::UploadStartResult {upload_id:existing.0,attachment:api::Attachment{attachment_id:String::new(),name:existing.1,content_type:existing.2,byte_size:existing.3,sha256:existing.4},next_offset:existing.5});
            }
            let count: i64 = tx.query_row("SELECT count(*) FROM agent_hub_uploads WHERE owner_agent_id=?1", [&owner_id], |r| r.get(0))?;
            let total: i64 = tx.query_row("SELECT coalesce(sum(byte_size),0) FROM agent_hub_uploads WHERE owner_agent_id=?1", [&owner_id], |r| r.get(0))?;
            if count >= MAX_PENDING_UPLOADS || total + i64::from(size) > MAX_PENDING_BYTES { return Err(DatabaseError::Domain(hub_attachment_invalid("pending upload quota exceeded"))); }
            tx.execute("INSERT INTO agent_hub_uploads(upload_id,owner_agent_id,request_key,name,content_type,byte_size,sha256,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", rusqlite::params![upload_id,owner_id,request_key,name,ctype,size,digest,now,expiry])?;
            Ok(api::UploadStartResult {upload_id:upload_id.clone(),attachment:api::Attachment{attachment_id:String::new(),name,content_type:ctype,byte_size:size,sha256:digest},next_offset:0})
        }).map_err(database_error)?;
        Ok(result)
    }

    pub fn upload_chunk(
        &self,
        request: api::UploadChunk,
        caller: &Caller,
    ) -> Result<api::UploadChunkResult, ProtocolError> {
        let owner = self.authenticate(&request.auth, caller, None)?;
        let bytes = STANDARD
            .decode(request.data_base64.as_bytes())
            .map_err(|_| hub_attachment_invalid("attachment chunk is not valid base64"))?;
        if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
            return Err(hub_attachment_invalid(
                "attachment chunk must contain 1..32768 bytes",
            ));
        }
        let id = request.upload_id.clone();
        let owner_id = owner.agent_id.clone();
        let offset = request.offset;
        self.database.transaction(move |tx| {
            let (size,mut content):(u32,Vec<u8>)=tx.query_row("SELECT byte_size,content FROM agent_hub_uploads WHERE upload_id=?1 AND owner_agent_id=?2 AND expires_at>=?3", rusqlite::params![id,owner_id,now_text()?], |r| Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
            if content.len() as u32 != offset || content.len()+bytes.len() > size as usize { return Err(DatabaseError::Domain(hub_attachment_invalid("attachment offset or length does not match"))); }
            content.extend_from_slice(&bytes);
            tx.execute("UPDATE agent_hub_uploads SET content=?1 WHERE upload_id=?2 AND owner_agent_id=?3", rusqlite::params![content,id,owner_id])?;
            Ok(api::UploadChunkResult {upload_id:id,next_offset:offset + bytes.len() as u32,complete:offset + bytes.len() as u32 == size})
        }).map_err(database_error)
    }

    pub fn upload_file(
        &self,
        request: api::UploadFile,
        caller: &Caller,
    ) -> Result<api::FileChunk, ProtocolError> {
        let owner = self.authenticate(&request.auth, caller, None)?;
        let owner_id = owner.agent_id.clone();
        let offset = request.offset;
        let upload_id = request.upload_id.clone();
        let file_id = request.file_id.clone();
        self.database.call(move |db| {
            if let Some(id) = upload_id {
                let (name,ctype,size,digest,content):(String,String,u32,String,Vec<u8>)=db.query_row("SELECT name,content_type,byte_size,sha256,content FROM agent_hub_uploads WHERE upload_id=?1 AND owner_agent_id=?2", rusqlite::params![id,owner_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
                return file_chunk(api::Attachment{attachment_id:id,name,content_type:ctype,byte_size:size,sha256:digest},content,offset);
            }
            let id=file_id.ok_or_else(|| DatabaseError::Domain(hub_attachment_invalid("upload_id or file_id is required")))?;
            let (attachment,message_room,name,ctype,size,digest,content):(String,String,String,String,u32,String,Vec<u8>)=db.query_row("SELECT a.attachment_id,m.room_id,a.name,a.content_type,a.byte_size,a.sha256,a.content FROM agent_hub_attachments a JOIN agent_hub_messages m ON m.message_id=a.message_id JOIN agent_hub_room_members rm ON rm.room_id=m.room_id WHERE a.attachment_id=?1 AND rm.agent_id=?2", rusqlite::params![id,owner_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?.ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
            let _ = (message_room, attachment);
            file_chunk(api::Attachment{attachment_id:id,name,content_type:ctype,byte_size:size,sha256:digest},content,offset)
        }).map_err(database_error)
    }

    pub fn upload_remove(
        &self,
        request: api::UploadRemove,
        caller: &Caller,
    ) -> Result<api::Settings, ProtocolError> {
        let owner = self.authenticate(&request.auth, caller, None)?;
        let id = request.upload_id;
        self.database
            .call(move |db| {
                db.execute(
                    "DELETE FROM agent_hub_uploads WHERE upload_id=?1 AND owner_agent_id=?2",
                    rusqlite::params![id, owner.agent_id],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        self.settings()
    }

    pub fn settings(&self) -> Result<api::Settings, ProtocolError> {
        self.database.call(|db| {
            let (revision,enabled,paused,current,previous):(u64,bool,bool,Option<String>,Option<String>)=db.query_row("SELECT revision,public_enabled,public_paused,current_key_created_at,previous_key_expires_at FROM agent_hub_settings WHERE singleton=1", [], |r| Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? != 0,r.get::<_,i64>(2)? != 0,r.get(3)?,r.get(4)?)))?;
            let pending:u32=db.query_row("SELECT count(*) FROM agent_hub_outbox", [], |r| r.get(0))?;
            Ok(api::Settings{revision,public_enabled:enabled,public_paused:paused,current_key_created_at:current,previous_key_expires_at:previous,callback_outbox_pending:pending})
        }).map_err(database_error)
    }

    pub fn settings_set(&self, request: api::SettingsSet) -> Result<api::Settings, ProtocolError> {
        self.database.transaction(move |tx| {
            let changed=tx.execute("UPDATE agent_hub_settings SET revision=revision+1,public_enabled=?1,public_paused=?2 WHERE singleton=1 AND revision=?3", rusqlite::params![i64::from(request.public_enabled),i64::from(request.public_paused),request.expected_revision as i64])?;
            if changed==0{return Err(DatabaseError::Domain(conflict("hub settings changed; refresh before saving")));} Ok(())
        }).map_err(database_error)?;
        self.settings()
    }

    pub fn key_rotate(
        &self,
        request: api::KeyRotate,
    ) -> Result<api::KeyRotateResult, ProtocolError> {
        let key = secret()?;
        let now = now_text()?;
        let expiry = timestamp_after(KEY_OVERLAP_SECONDS)?;
        let hash = hash_secret(&key);
        let expiry_for_store = expiry.clone();
        let revision=self.database.transaction(move |tx| {
            let row: (u64,Option<String>)=tx.query_row("SELECT revision,current_key_hash FROM agent_hub_settings WHERE singleton=1", [], |r| Ok((r.get::<_,i64>(0)? as u64,r.get(1)?)))?;
            if row.0 != request.expected_revision { return Err(DatabaseError::Domain(conflict("hub key changed; refresh before rotating"))); }
            tx.execute("UPDATE agent_hub_settings SET revision=revision+1,previous_key_hash=current_key_hash,previous_key_expires_at=?1,current_key_hash=?2,current_key_created_at=?3 WHERE singleton=1", rusqlite::params![expiry_for_store,hash,now])?;
            Ok(row.0+1)
        }).map_err(database_error)?;
        Ok(api::KeyRotateResult {
            revision,
            bearer_key: key,
            previous_key_expires_at: expiry,
        })
    }

    pub fn activity(&self, request: api::Activity) -> Result<api::ActivityResult, ProtocolError> {
        self.expire_leases()?;
        let space = request.space.map(space_text);
        self.database.call(move |db| {
            let where_space = if space.is_some() { " AND space=?1" } else { "" };
            let active: u32 = db.query_row(&format!("SELECT count(*) FROM agent_hub_agents WHERE state='active'{}",where_space), rusqlite::params_from_iter(space.iter()), |r| r.get(0))?;
            let rooms: u32 = db.query_row(&format!("SELECT count(*) FROM agent_hub_rooms WHERE archived_at IS NULL{}",where_space), rusqlite::params_from_iter(space.iter()), |r| r.get(0))?;
            let messages: u32 = db.query_row(&format!("SELECT count(*) FROM agent_hub_messages m JOIN agent_hub_rooms r ON r.room_id=m.room_id WHERE 1=1{}",space.as_ref().map(|_| " AND r.space=?1").unwrap_or("")), rusqlite::params_from_iter(space.iter()), |r| r.get(0))?;
            let deliveries: u32 = db.query_row("SELECT count(*) FROM agent_hub_deliveries WHERE acknowledged_at IS NULL", [], |r| r.get(0))?;
            let outbox: u32 = db.query_row("SELECT count(*) FROM agent_hub_outbox", [], |r| r.get(0))?;
            Ok(api::ActivityResult{active_agents:active,rooms,messages,deliveries_pending:deliveries,outbox_pending:outbox})
        }).map_err(database_error)
    }

    pub fn console_directory(
        &self,
        request: api::ConsoleDirectory,
    ) -> Result<api::DirectoryResult, ProtocolError> {
        self.directory(api::Directory {
            space: request.space,
            after: request.after,
            limit: request.limit,
        })
    }

    pub fn console_receive(
        &self,
        request: api::ConsoleReceive,
    ) -> Result<api::ReceiveResult, ProtocolError> {
        if request.space == api::Space::Public && !self.public_available()? {
            return Err(hub_disabled());
        }
        let limit = bounded_page(request.limit);
        let room = request.room_id;
        let after = request.after;
        let space = space_text(request.space);
        let messages = self.database.call(move |db| {
            let mut statement = db.prepare("SELECT m.message_id,m.room_id,m.body,m.created_at FROM agent_hub_messages m JOIN agent_hub_rooms r ON r.room_id=m.room_id WHERE r.space=?1 AND (?2 IS NULL OR m.room_id=?2) AND (?3 IS NULL OR m.message_id>?3) ORDER BY m.created_at,m.message_id LIMIT ?4")?;
            let mut result = Vec::new();
            for row in statement.query_map(rusqlite::params![space,room,after,limit as i64], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
                let (message_id,room_id,body,created_at) = row?;
                result.push(api::Delivery { delivery_id: format!("console:{message_id}"), message: message_from_parts(db,&message_id,&room_id,&body,&created_at)?, acknowledged: false });
            }
            Ok(result)
        }).map_err(database_error)?;
        let next = messages
            .last()
            .map(|d| d.delivery_id.trim_start_matches("console:").to_owned());
        Ok(api::ReceiveResult {
            deliveries: messages,
            next,
        })
    }

    pub fn console_send(&self, request: api::ConsoleSend) -> Result<api::Message, ProtocolError> {
        if !request.attachments.is_empty() {
            return Err(hub_attachment_invalid(
                "Console messages cannot stage agent uploads",
            ));
        }
        let room = self.room(&request.room_id)?;
        self.ensure_public(room.space)?;
        self.send_inner(None, None, &request.room_id, &request.body, &[], None)
    }

    pub fn admin_remove_agent(
        &self,
        request: api::AgentRemove,
    ) -> Result<api::RemovalResult, ProtocolError> {
        let agent_id = request.agent_id;
        let now = now_text()?;
        let removed = self
            .database
            .transaction(move |tx| {
                let exists: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM agent_hub_agents WHERE agent_id=?1 AND state!='unregistered'",
                        [&agent_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Ok(false);
                }
                tx.execute(
                    "DELETE FROM agent_hub_outbox WHERE delivery_id IN (SELECT delivery_id FROM agent_hub_deliveries WHERE recipient_agent_id=?1)",
                    [&agent_id],
                )?;
                tx.execute(
                    "DELETE FROM agent_hub_deliveries WHERE recipient_agent_id=?1",
                    [&agent_id],
                )?;
                tx.execute(
                    "DELETE FROM agent_hub_room_members WHERE agent_id=?1",
                    [&agent_id],
                )?;
                tx.execute(
                    "UPDATE agent_hub_agents SET state='unregistered',unregistered_at=?1,last_seen_at=?1 WHERE agent_id=?2",
                    rusqlite::params![now, agent_id],
                )?;
                Ok(true)
            })
            .map_err(database_error)?;
        Ok(api::RemovalResult { removed })
    }

    pub fn admin_remove_message(
        &self,
        request: api::MessageRemove,
    ) -> Result<api::RemovalResult, ProtocolError> {
        let message_id = request.message_id;
        let removed = self
            .database
            .transaction(move |tx| {
                let changed = tx.execute(
                    "DELETE FROM agent_hub_messages WHERE message_id=?1",
                    [&message_id],
                )?;
                Ok(changed != 0)
            })
            .map_err(database_error)?;
        Ok(api::RemovalResult { removed })
    }

    pub fn admin_remove_room(
        &self,
        request: api::RoomRemove,
    ) -> Result<api::RemovalResult, ProtocolError> {
        let room_id = request.room_id;
        let removed = self
            .database
            .transaction(move |tx| {
                let exists: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM agent_hub_rooms WHERE room_id=?1",
                        [&room_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Ok(false);
                }
                tx.execute(
                    "DELETE FROM agent_hub_outbox WHERE delivery_id IN (SELECT d.delivery_id FROM agent_hub_deliveries d JOIN agent_hub_messages m ON m.message_id=d.message_id WHERE m.room_id=?1)",
                    [&room_id],
                )?;
                tx.execute(
                    "DELETE FROM agent_hub_messages WHERE room_id=?1",
                    [&room_id],
                )?;
                tx.execute(
                    "DELETE FROM agent_hub_room_members WHERE room_id=?1",
                    [&room_id],
                )?;
                tx.execute("DELETE FROM agent_hub_rooms WHERE room_id=?1", [&room_id])?;
                Ok(true)
            })
            .map_err(database_error)?;
        Ok(api::RemovalResult { removed })
    }

    pub fn remote(
        &self,
        request: api::RemoteRequest,
        caller: &Caller,
    ) -> Result<serde_json::Value, ProtocolError> {
        let bearer = request.bearer.as_deref();
        match &request.action {
            api::RemoteAction::Directory(p) => {
                self.authorize_remote_space(p.space, bearer)?;
                return serde_json::to_value(self.directory(p.clone())?)
                    .map_err(|e| internal(e.to_string()));
            }
            api::RemoteAction::Register(p) => {
                self.authorize_remote_space(p.space, bearer)?;
                return serde_json::to_value(self.register(p.clone(), caller)?)
                    .map_err(|e| internal(e.to_string()));
            }
            _ => {}
        }
        let auth = remote_action_auth(&request.action);
        let agent = self.authenticate_remote(auth, bearer)?;
        self.authorize_remote_space(agent.space, bearer)?;
        let value = match request.action {
            api::RemoteAction::Heartbeat(p) => serde_json::to_value(self.heartbeat(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::Unregister(p) => serde_json::to_value(self.unregister(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::RoomCreate(p) => serde_json::to_value(self.room_create(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::RoomList(p) => serde_json::to_value(self.room_list(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::RoomMemberAdd(p) => {
                serde_json::to_value(self.room_member_add(p, caller)?)
                    .map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::RoomMemberRemove(p) => {
                serde_json::to_value(self.room_member_remove(p, caller)?)
                    .map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::Send(p) => {
                serde_json::to_value(self.send(p, caller)?).map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::Receive(p) => serde_json::to_value(self.receive(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::Ack(p) => {
                serde_json::to_value(self.ack(p, caller)?).map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::UploadStart(p) => {
                serde_json::to_value(self.upload_start(p, caller)?)
                    .map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::UploadChunk(p) => {
                serde_json::to_value(self.upload_chunk(p, caller)?)
                    .map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::UploadFile(p) => serde_json::to_value(self.upload_file(p, caller)?)
                .map_err(|e| internal(e.to_string()))?,
            api::RemoteAction::UploadRemove(p) => {
                serde_json::to_value(self.upload_remove(p, caller)?)
                    .map_err(|e| internal(e.to_string()))?
            }
            api::RemoteAction::Directory(_) | api::RemoteAction::Register(_) => unreachable!(),
        };
        Ok(value)
    }

    pub fn deliver_once(&self) -> Result<u32, ProtocolError> {
        let now = OffsetDateTime::now_utc();
        let now_text = now.format(&Rfc3339).map_err(|e| internal(e.to_string()))?;
        let cutoff = (now - TimeDuration::seconds(MAX_OUTBOX_AGE_SECONDS))
            .format(&Rfc3339)
            .map_err(|e| internal(e.to_string()))?;
        self.database
            .transaction(move |tx| {
                tx.execute(
                    "DELETE FROM agent_hub_outbox WHERE created_at<?1 OR attempts>=?2",
                    rusqlite::params![cutoff, MAX_OUTBOX_ATTEMPTS],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        let rows=self.database.call(move |db| { let mut q=db.prepare("SELECT outbox_id,callback_url,payload_json,signature,attempts FROM agent_hub_outbox WHERE next_attempt_at<=?1 ORDER BY outbox_id LIMIT ?2")?; q.query_map(rusqlite::params![now_text,OUTBOX_BATCH], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?))).and_then(|i| i.collect::<Result<Vec<_>,_>>()).map_err(DatabaseError::from) }).map_err(database_error)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(7))
            .build()
            .map_err(|_| unavailable())?;
        let mut delivered = 0;
        for (id, url, payload, signature, attempts) in rows {
            let result = client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-devcoordinator2-signature", signature)
                .body(payload)
                .send()
                .and_then(|r| r.error_for_status());
            if result.is_ok() {
                self.database
                    .call(move |db| {
                        db.execute("DELETE FROM agent_hub_outbox WHERE outbox_id=?1", [id])?;
                        Ok(())
                    })
                    .map_err(database_error)?;
                delivered += 1;
            } else {
                let next_attempt = attempts.saturating_add(1);
                let delay = (1_i64 << next_attempt.min(8)).min(300);
                let retry = (OffsetDateTime::now_utc() + TimeDuration::seconds(delay))
                    .format(&Rfc3339)
                    .map_err(|e| internal(e.to_string()))?;
                self.database.call(move |db| { db.execute("UPDATE agent_hub_outbox SET attempts=?1,next_attempt_at=?2,last_error='callback delivery failed' WHERE outbox_id=?3",rusqlite::params![next_attempt,retry,id])?; Ok(()) }).map_err(database_error)?;
            }
        }
        Ok(delivered)
    }

    fn send_inner(
        &self,
        sender: Option<&AgentRow>,
        _auth: Option<api::AgentAuth>,
        room_id: &str,
        body: &str,
        attachment_ids: &[String],
        client_message_id: Option<&str>,
    ) -> Result<api::Message, ProtocolError> {
        if body.is_empty() || body.len() > 32768 {
            return Err(invalid("message body must contain 1..32768 bytes"));
        }
        if attachment_ids.len() > MAX_ATTACHMENTS {
            return Err(hub_attachment_invalid("at most 16 attachments may be sent"));
        }
        let sender_id = sender.map(|a| a.agent_id.clone());
        let room_id_owned = room_id.to_owned();
        let body_owned = body.to_owned();
        let client_id = client_message_id.map(str::to_owned);
        let ids = attachment_ids.to_vec();
        let now = now_text()?;
        let message_id = ids::agent_message_id().map_err(id_error)?;
        let inserted=self.database.transaction(move |tx| {
            let (space,): (String,)=tx.query_row("SELECT space FROM agent_hub_rooms WHERE room_id=?1 AND archived_at IS NULL", [&room_id_owned], |r| Ok((r.get(0)?,))).optional()?.ok_or_else(||DatabaseError::Domain(hub_not_found()))?;
            if let Some(ref id)=sender_id { let count:i64=tx.query_row("SELECT count(*) FROM agent_hub_room_members WHERE room_id=?1 AND agent_id=?2",rusqlite::params![room_id_owned,id],|r|r.get(0))?; if count==0{return Err(DatabaseError::Domain(denied()));} }
            if let Some(ref existing)=client_id && let Some(found)=tx.query_row("SELECT message_id FROM agent_hub_messages WHERE sender_agent_id IS ?1 AND client_message_id=?2",rusqlite::params![sender_id,existing],|r|r.get::<_,String>(0)).optional()? { return Ok(found); }
            if !ids.is_empty() { let total:i64=tx.query_row("SELECT coalesce(sum(byte_size),0) FROM agent_hub_uploads WHERE owner_agent_id IS ?1 AND upload_id IN (SELECT value FROM json_each(?2))",rusqlite::params![sender_id,serde_json::to_string(&ids).unwrap_or_default()],|r|r.get(0)).unwrap_or(0); if total as u64 > MAX_MESSAGE_BYTES{return Err(DatabaseError::Domain(hub_attachment_invalid("message attachment total exceeds 32 MiB")));} }
            tx.execute("INSERT INTO agent_hub_messages(message_id,room_id,sender_agent_id,body,client_message_id,created_at) VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![message_id,room_id_owned,sender_id,body_owned,client_id,now])?;
            for (position,upload_id) in ids.iter().enumerate() {
                let (name,ctype,size,digest,content):(String,String,u32,String,Vec<u8>)=tx.query_row("SELECT name,content_type,byte_size,sha256,content FROM agent_hub_uploads WHERE upload_id=?1 AND owner_agent_id IS ?2 AND length(content)=byte_size",rusqlite::params![upload_id,sender_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?.ok_or_else(||DatabaseError::Domain(hub_attachment_invalid("attachment upload is incomplete or unavailable")))?;
                let actual=hex_digest(&content); if actual != digest {return Err(DatabaseError::Domain(hub_attachment_invalid("attachment digest mismatch")));}
                let attachment_id=ids::hub_attachment_id().map_err(id_error)?;
                tx.execute("INSERT INTO agent_hub_attachments(attachment_id,message_id,position,name,content_type,byte_size,sha256,content) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",rusqlite::params![attachment_id,message_id,position as i64,name,ctype,size,digest,content])?;
                tx.execute("DELETE FROM agent_hub_uploads WHERE upload_id=?1",[upload_id])?;
            }
            let attachment_meta = attachments_for(tx, &message_id)?;
            let mut recipients=Vec::new(); let mut q=tx.prepare("SELECT a.agent_id,a.callback_url,a.callback_secret FROM agent_hub_room_members m JOIN agent_hub_agents a ON a.agent_id=m.agent_id WHERE m.room_id=?1 AND a.state='active' AND a.agent_id IS NOT ?2")?;
            for row in q.query_map(rusqlite::params![room_id_owned,sender_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?)))? { recipients.push(row?); }
            for (recipient,callback,secret) in recipients { let delivery=ids::hub_delivery_id().map_err(id_error)?; let callback_nonce=ids::hub_delivery_id().map_err(id_error)?; tx.execute("INSERT INTO agent_hub_deliveries(delivery_id,message_id,recipient_agent_id,created_at,next_attempt_at) VALUES(?1,?2,?3,?4,?4)",rusqlite::params![delivery,message_id,recipient,now])?; if let (Some(url),Some(secret))=(callback,secret) { let payload=serde_json::json!({"delivery_id":delivery,"message_id":message_id,"room_id":room_id_owned,"body":body_owned,"attachments":attachment_meta,"created_at":now,"timestamp":now,"nonce":callback_nonce}); let payload_json=serde_json::to_string(&payload).map_err(|e|DatabaseError::Domain(internal(e.to_string())))?; let signature=hmac_signature(&secret,payload_json.as_bytes()); tx.execute("INSERT INTO agent_hub_outbox(delivery_id,callback_url,payload_json,signature,callback_nonce,signed_at,next_attempt_at,created_at) VALUES(?1,?2,?3,?4,?5,?6,?6,?6)",rusqlite::params![delivery,url,payload_json,signature,callback_nonce,now])?; } }
            tx.execute("DELETE FROM agent_hub_outbox WHERE outbox_id IN (SELECT outbox_id FROM agent_hub_outbox ORDER BY outbox_id DESC LIMIT -1 OFFSET ?1)",[MAX_OUTBOX_ROWS])?;
            let _=space;
            Ok(message_id)
        }).map_err(database_error)?;
        self.message(&inserted)
    }

    fn receive_for(
        &self,
        recipient: &str,
        room_id: Option<&str>,
        after: Option<&str>,
        limit: u16,
    ) -> Result<api::ReceiveResult, ProtocolError> {
        let recipient = recipient.to_owned();
        let room = room_id.map(str::to_owned);
        let after = after.map(str::to_owned);
        let limit = bounded_page(limit);
        let deliveries=self.database.call(move |db| { let mut q=db.prepare("SELECT d.delivery_id,d.message_id,d.acknowledged_at,m.room_id,m.body,m.created_at FROM agent_hub_deliveries d JOIN agent_hub_messages m ON m.message_id=d.message_id WHERE d.recipient_agent_id=?1 AND (?2 IS NULL OR m.room_id=?2) AND (?3 IS NULL OR d.delivery_id>?3) ORDER BY d.created_at,d.delivery_id LIMIT ?4")?; let mut out=Vec::new(); for row in q.query_map(rusqlite::params![recipient,room,after,limit as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))? { let (delivery_id,message_id,acked,room_id,body,created)=row?; out.push(api::Delivery{delivery_id,message:message_from_parts(db,&message_id,&room_id,&body,&created)?,acknowledged:acked.is_some()}); } Ok(out) }).map_err(database_error)?;
        let next = deliveries.last().map(|d| d.delivery_id.clone());
        Ok(api::ReceiveResult { deliveries, next })
    }

    fn message(&self, id: &str) -> Result<api::Message, ProtocolError> {
        let id = id.to_owned();
        self.database
            .call(move |db| {
                let (room, body, created): (String, String, String) = db
                .query_row(
                    "SELECT room_id,body,created_at FROM agent_hub_messages WHERE message_id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
                message_from_parts(db, &id, &room, &body, &created)
            })
            .map_err(database_error)
    }

    fn room(&self, id: &str) -> Result<api::Room, ProtocolError> {
        let id = id.to_owned();
        self.database
            .call(move |db| {
                let (space, name, created): (String, String, String) = db
                    .query_row(
                        "SELECT space,name,created_at FROM agent_hub_rooms WHERE room_id=?1",
                        [id.clone()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?
                    .ok_or_else(|| DatabaseError::Domain(hub_not_found()))?;
                Ok(api::Room {
                    room_id: id.clone(),
                    space: parse_space(&space)?,
                    name,
                    created_at: created,
                    members: room_members(db, &id)?,
                })
            })
            .map_err(database_error)
    }

    fn agent_profile(&self, id: &str) -> Result<api::AgentProfile, ProtocolError> {
        let id = id.to_owned();
        self.database
            .call(move |db| Ok(agent_row_from_id(db, &id)?.to_profile()))
            .map_err(database_error)
    }

    fn authenticate(
        &self,
        auth: &api::AgentAuth,
        caller: &Caller,
        expected: Option<api::Space>,
    ) -> Result<AgentRow, ProtocolError> {
        let id = auth.agent_id.clone();
        let token = auth.token.clone();
        let local = caller.is_local();
        let row = self
            .database
            .call(move |db| agent_row_from_id(db, &id))
            .map_err(database_error)?;
        if expected.is_some_and(|space| space != row.space) {
            return Err(hub_not_found());
        }
        if row.state == api::AgentState::Unregistered {
            return Err(hub_not_found());
        }
        if row.lease_expires_at <= now_text()? && !local {
            return Err(hub_lease_expired());
        }
        if !local && hash_secret(&token) != row.token_hash {
            return Err(denied());
        }
        Ok(row)
    }
    fn authenticate_remote(
        &self,
        auth: &api::AgentAuth,
        bearer: Option<&str>,
    ) -> Result<AgentRow, ProtocolError> {
        let id = auth.agent_id.clone();
        let token = auth.token.clone();
        let row = self
            .database
            .call(move |db| agent_row_from_id(db, &id))
            .map_err(database_error)?;
        if hash_secret(&token) != row.token_hash {
            return Err(denied());
        }
        if row.state == api::AgentState::Unregistered {
            return Err(hub_not_found());
        }
        if row.lease_expires_at <= now_text()? {
            return Err(hub_lease_expired());
        }
        self.authorize_remote_space(row.space, bearer)?;
        Ok(row)
    }
    fn authorize_remote_space(
        &self,
        space: api::Space,
        bearer: Option<&str>,
    ) -> Result<(), ProtocolError> {
        if space == api::Space::Public && !self.public_available()? {
            return Err(hub_disabled());
        }
        if space == api::Space::Private && !self.valid_bearer(bearer)? {
            return Err(denied());
        }
        Ok(())
    }
    fn valid_bearer(&self, bearer: Option<&str>) -> Result<bool, ProtocolError> {
        let Some(value) = bearer.filter(|v| !v.is_empty()) else {
            return Ok(false);
        };
        let hash = hash_secret(value);
        let now = now_text()?;
        self.database.call(move |db| { let (current,previous,expires):(Option<String>,Option<String>,Option<String>)=db.query_row("SELECT current_key_hash,previous_key_hash,previous_key_expires_at FROM agent_hub_settings WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?; Ok(current.as_deref()==Some(&hash) || (previous.as_deref()==Some(&hash) && expires.is_some_and(|e|e>=now))) }).map_err(database_error)
    }
    fn public_enabled(&self) -> Result<bool, ProtocolError> {
        self.database
            .call(|db| {
                Ok(db.query_row(
                    "SELECT public_enabled FROM agent_hub_settings WHERE singleton=1",
                    [],
                    |r| r.get::<_, i64>(0),
                )? != 0)
            })
            .map_err(database_error)
    }

    fn expire_leases(&self) -> Result<(), ProtocolError> {
        let now = now_text()?;
        self.database
            .call(move |db| {
                db.execute(
                    "UPDATE agent_hub_agents SET state='expired' WHERE state='active' AND lease_expires_at<=?1",
                    [&now],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }
    fn public_available(&self) -> Result<bool, ProtocolError> {
        self.database.call(|db|Ok(db.query_row("SELECT public_enabled=1 AND public_paused=0 FROM agent_hub_settings WHERE singleton=1",[],|r|r.get::<_,bool>(0))?)).map_err(database_error)
    }
    fn ensure_public(&self, space: api::Space) -> Result<(), ProtocolError> {
        if space == api::Space::Public && !self.public_available()? {
            Err(hub_disabled())
        } else {
            Ok(())
        }
    }
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "agent hub storage is unavailable")
            .with_detail(other.to_string()),
    }
}
fn id_error(error: ids::IdError) -> ProtocolError {
    internal(error.to_string())
}
fn internal(detail: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, "agent hub operation failed")
        .with_detail(detail.into())
}
fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}
fn denied() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        "agent hub authorization is invalid",
    )
}
fn conflict(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ConfigurationConflict, message)
}
fn hub_not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::HubNotFound, "agent hub resource is unavailable")
}
fn hub_disabled() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::HubDisabled,
        "the public agent hub is disabled or paused",
    )
}
fn hub_lease_expired() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::HubLeaseExpired,
        "agent lease expired; register again",
    )
}
fn hub_attachment_invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::HubAttachmentInvalid, message)
}
fn unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DaemonUnavailable,
        "agent callback delivery is unavailable",
    )
}

fn now_text() -> Result<String, ProtocolError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| internal(e.to_string()))
}
fn timestamp_after(seconds: i64) -> Result<String, ProtocolError> {
    (OffsetDateTime::now_utc() + TimeDuration::seconds(seconds))
        .format(&Rfc3339)
        .map_err(|e| internal(e.to_string()))
}
fn bounded_page(value: u16) -> u16 {
    value.clamp(1, 100)
}
fn space_text(space: api::Space) -> &'static str {
    match space {
        api::Space::Private => "private",
        api::Space::Public => "public",
    }
}
fn parse_space(value: &str) -> Result<api::Space, DatabaseError> {
    match value {
        "private" => Ok(api::Space::Private),
        "public" => Ok(api::Space::Public),
        _ => Err(DatabaseError::Domain(internal("invalid hub space"))),
    }
}
fn parse_state(value: &str) -> Result<api::AgentState, DatabaseError> {
    match value {
        "active" => Ok(api::AgentState::Active),
        "expired" => Ok(api::AgentState::Expired),
        "unregistered" => Ok(api::AgentState::Unregistered),
        _ => Err(DatabaseError::Domain(internal("invalid hub agent state"))),
    }
}

fn validate_name(value: &str) -> Result<(), ProtocolError> {
    if value.trim().is_empty() || value.len() > 240 || value.chars().any(|c| c.is_control()) {
        Err(invalid("hub name is invalid"))
    } else {
        Ok(())
    }
}
fn validate_optional_label(value: Option<&str>) -> Result<(), ProtocolError> {
    if value.is_some_and(|v| v.len() > 160 || v.chars().any(|c| c.is_control())) {
        Err(invalid("hub profile label is invalid"))
    } else {
        Ok(())
    }
}
fn validate_capabilities(values: &[String]) -> Result<(), ProtocolError> {
    if values.len() > 32
        || values
            .iter()
            .any(|v| v.is_empty() || v.len() > 80 || v.chars().any(|c| c.is_control()))
    {
        Err(invalid("hub capabilities are invalid"))
    } else {
        Ok(())
    }
}
fn validate_digest(value: &str) -> Result<(), ProtocolError> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Err(hub_attachment_invalid("attachment SHA-256 is invalid"))
    } else {
        Ok(())
    }
}
fn validate_callback_url(value: &str) -> Result<String, ProtocolError> {
    let url = reqwest::Url::parse(value).map_err(|_| invalid("callback URL is invalid"))?;
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or_default();
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1") || host.ends_with(".localhost");
    if scheme != "https" && !(scheme == "http" && local) {
        return Err(invalid(
            "callback URL must use HTTPS (HTTP loopback is allowed for tests)",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("callback URL cannot contain credentials"));
    }
    Ok(url.to_string())
}
fn secret() -> Result<String, ProtocolError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| internal(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn hash_secret(value: &str) -> String {
    let mut h = Sha256::new();
    h.update(value.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
fn hex_digest(value: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(value);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
fn hmac_signature(secret: &str, payload: &[u8]) -> String {
    let mut key = [0u8; 64];
    let digest = Sha256::digest(secret.as_bytes());
    key[..digest.len()].copy_from_slice(&digest);
    let mut inner = Sha256::new();
    for b in &mut key {
        *b ^= 0x36;
    }
    inner.update(key);
    inner.update(payload);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    for b in &mut key {
        *b ^= 0x36 ^ 0x5c;
    }
    outer.update(key);
    outer.update(inner);
    format!(
        "sha256={}",
        outer
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn agent_row(row: &Row<'_>) -> rusqlite::Result<AgentRow> {
    let capabilities_json: String = row.get(7)?;
    Ok(AgentRow {
        agent_id: row.get(0)?,
        handle: row.get(1)?,
        space: parse_space(&row.get::<_, String>(2)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
        display_name: row.get(3)?,
        repository_label: row.get(4)?,
        domain_label: row.get(5)?,
        current_subject: row.get(6)?,
        capabilities: serde_json::from_str(&capabilities_json).unwrap_or_default(),
        token_hash: row.get(8)?,
        callback_url: row.get(9)?,
        state: parse_state(&row.get::<_, String>(11)?)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        lease_expires_at: row.get(12)?,
    })
}
fn agent_row_from_id(db: &rusqlite::Connection, id: &str) -> Result<AgentRow, DatabaseError> {
    db.query_row("SELECT agent_id,handle,space,display_name,repository_label,domain_label,current_subject,capabilities_json,token_hash,callback_url,callback_secret,state,lease_expires_at,created_at,last_seen_at FROM agent_hub_agents WHERE agent_id=?1",[id],agent_row).optional()?.ok_or_else(||DatabaseError::Domain(hub_not_found()))
}
impl AgentRow {
    fn to_profile(&self) -> api::AgentProfile {
        api::AgentProfile {
            agent_id: self.agent_id.clone(),
            handle: self.handle.clone(),
            space: self.space,
            display_name: self.display_name.clone(),
            repository_label: self.repository_label.clone(),
            domain_label: self.domain_label.clone(),
            current_subject: self.current_subject.clone(),
            capabilities: self.capabilities.clone(),
            state: self.state,
            lease_expires_at: self.lease_expires_at.clone(),
            callback_configured: self.callback_url.is_some(),
        }
    }
}
fn room_members(db: &rusqlite::Connection, room_id: &str) -> Result<Vec<String>, DatabaseError> {
    let mut q = db.prepare(
        "SELECT agent_id FROM agent_hub_room_members WHERE room_id=?1 ORDER BY agent_id",
    )?;
    q.query_map([room_id], |r| r.get(0))
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(DatabaseError::from)
}
fn attachments_for(
    db: &rusqlite::Connection,
    message_id: &str,
) -> Result<Vec<api::Attachment>, DatabaseError> {
    let mut q=db.prepare("SELECT attachment_id,name,content_type,byte_size,sha256 FROM agent_hub_attachments WHERE message_id=?1 ORDER BY position")?;
    q.query_map([message_id], |r| {
        Ok(api::Attachment {
            attachment_id: r.get(0)?,
            name: r.get(1)?,
            content_type: r.get(2)?,
            byte_size: r.get(3)?,
            sha256: r.get(4)?,
        })
    })
    .and_then(|i| i.collect::<Result<Vec<_>, _>>())
    .map_err(DatabaseError::from)
}
fn file_chunk(
    attachment: api::Attachment,
    content: Vec<u8>,
    offset: u64,
) -> Result<api::FileChunk, DatabaseError> {
    if offset > content.len() as u64 {
        return Err(DatabaseError::Domain(hub_attachment_invalid(
            "file offset exceeds its size",
        )));
    }
    let end = (offset as usize + MAX_CHUNK_BYTES).min(content.len());
    Ok(api::FileChunk {
        attachment,
        offset,
        next_offset: (end < content.len()).then_some(end as u64),
        data_base64: STANDARD.encode(&content[offset as usize..end]),
    })
}
fn console_profile() -> api::AgentProfile {
    api::AgentProfile {
        agent_id: "console".into(),
        handle: "console".into(),
        space: api::Space::Private,
        display_name: "Console".into(),
        repository_label: None,
        domain_label: None,
        current_subject: None,
        capabilities: Vec::new(),
        state: api::AgentState::Active,
        lease_expires_at: String::new(),
        callback_configured: false,
    }
}
fn message_from_parts(
    db: &rusqlite::Connection,
    message_id: &str,
    room_id: &str,
    body: &str,
    created_at: &str,
) -> Result<api::Message, DatabaseError> {
    let sender: Option<String> = db.query_row(
        "SELECT sender_agent_id FROM agent_hub_messages WHERE message_id=?1",
        [message_id],
        |r| r.get(0),
    )?;
    let profile = sender
        .and_then(|id| agent_row_from_id(db, &id).ok())
        .map(|a| a.to_profile())
        .unwrap_or_else(console_profile);
    Ok(api::Message {
        message_id: message_id.into(),
        room_id: room_id.into(),
        sender: profile,
        body: body.into(),
        attachments: attachments_for(db, message_id)?,
        created_at: created_at.into(),
    })
}
fn remote_action_auth(action: &api::RemoteAction) -> &api::AgentAuth {
    match action {
        api::RemoteAction::Heartbeat(p) => &p.auth,
        api::RemoteAction::Unregister(p) => &p.auth,
        api::RemoteAction::RoomCreate(p) => &p.auth,
        api::RemoteAction::RoomList(p) => &p.auth,
        api::RemoteAction::RoomMemberAdd(p) => &p.auth,
        api::RemoteAction::RoomMemberRemove(p) => &p.auth,
        api::RemoteAction::Send(p) => &p.auth,
        api::RemoteAction::Receive(p) => &p.auth,
        api::RemoteAction::Ack(p) => &p.auth,
        api::RemoteAction::UploadStart(p) => &p.auth,
        api::RemoteAction::UploadChunk(p) => &p.auth,
        api::RemoteAction::UploadFile(p) => &p.auth,
        api::RemoteAction::UploadRemove(p) => &p.auth,
        api::RemoteAction::Directory(_) | api::RemoteAction::Register(_) => {
            panic!("remote action does not carry agent auth")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::{ClientKind, agent_hub as api};
    use tempfile::TempDir;

    fn local() -> Caller {
        Caller {
            via_edge: false,
            pid: 1,
            uid: 1000,
            gid: 1000,
            client_kind: ClientKind::Other,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: None,
        }
    }
    fn edge() -> Caller {
        Caller {
            via_edge: true,
            pid: 2,
            uid: 1001,
            gid: 1001,
            client_kind: ClientKind::Edge,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: None,
        }
    }
    fn world() -> (TempDir, AgentHubService) {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(temp.path().join("authority.sqlite3")).unwrap();
        let service = AgentHubService::new(db).unwrap();
        (temp, service)
    }
    fn register(service: &AgentHubService, name: &str) -> api::RegisterResult {
        service
            .register(
                api::Register {
                    space: api::Space::Private,
                    display_name: name.into(),
                    repository_label: Some("repo".into()),
                    domain_label: Some("example.test".into()),
                    current_subject: Some("subject".into()),
                    capabilities: vec!["chat".into()],
                    callback_url: None,
                },
                &local(),
            )
            .unwrap()
    }
    fn auth(result: &api::RegisterResult) -> api::AgentAuth {
        api::AgentAuth {
            agent_id: result.agent.agent_id.clone(),
            token: result.token.clone(), // public-artifact-guard: allow text-secret
        }
    }

    #[test]
    fn public_registration_is_disabled_until_enabled_and_profiles_are_opaque() {
        let (_temp, service) = world();
        let error = service
            .register(
                api::Register {
                    space: api::Space::Public,
                    display_name: "public".into(),
                    repository_label: None,
                    domain_label: None,
                    current_subject: None,
                    capabilities: vec![],
                    callback_url: None,
                },
                &edge(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::HubDisabled);
        service
            .settings_set(api::SettingsSet {
                expected_revision: 0,
                public_enabled: true,
                public_paused: false,
            })
            .unwrap();
        let created = service
            .register(
                api::Register {
                    space: api::Space::Public,
                    display_name: "public".into(),
                    repository_label: Some("repository".into()),
                    domain_label: Some("host.example".into()),
                    current_subject: Some("topic".into()),
                    capabilities: vec!["stream".into()],
                    callback_url: None,
                },
                &edge(),
            )
            .unwrap();
        assert!(created.agent.handle.starts_with("agent-"));
        let listed = service
            .directory(api::Directory {
                space: api::Space::Public,
                after: None,
                limit: 20,
            })
            .unwrap();
        assert_eq!(listed.agents.len(), 1);
        assert_eq!(
            listed.agents[0].domain_label.as_deref(),
            Some("host.example")
        );
        assert!(listed.agents[0].callback_configured == false);
    }

    #[test]
    fn missed_heartbeats_mark_directory_presence_expired() {
        let (_temp, service) = world();
        let agent = register(&service, "offline");
        service
            .database
            .call(move |db| {
                db.execute(
                    "UPDATE agent_hub_agents SET lease_expires_at='1970-01-01T00:00:00Z' WHERE agent_id=?1",
                    [&agent.agent.agent_id],
                )?;
                Ok(())
            })
            .unwrap();
        let listed = service
            .directory(api::Directory {
                space: api::Space::Private,
                after: None,
                limit: 20,
            })
            .unwrap();
        assert_eq!(listed.agents[0].state, api::AgentState::Expired);
    }

    #[test]
    fn private_room_delivery_is_durable_and_acknowledgeable() {
        let (_temp, service) = world();
        let first = register(&service, "first");
        let second = register(&service, "second");
        let updated = service
            .heartbeat(
                api::Heartbeat {
                    auth: auth(&first),
                    current_subject: Some("updated subject".into()),
                },
                &local(),
            )
            .unwrap();
        assert_eq!(updated.current_subject.as_deref(), Some("updated subject"));
        let room = service
            .room_create(
                api::RoomCreate {
                    auth: auth(&first),
                    space: api::Space::Private,
                    name: "direct".into(),
                    members: vec![second.agent.agent_id.clone()],
                },
                &local(),
            )
            .unwrap();
        assert_eq!(room.members.len(), 2);
        let message = service
            .send(
                api::Send {
                    auth: auth(&first),
                    room_id: room.room_id.clone(),
                    body: "hello".into(),
                    attachments: vec![],
                    client_message_id: Some("msg-1".into()),
                },
                &local(),
            )
            .unwrap();
        let duplicate = service
            .send(
                api::Send {
                    auth: auth(&first),
                    room_id: room.room_id.clone(),
                    body: "hello".into(),
                    attachments: vec![],
                    client_message_id: Some("msg-1".into()),
                },
                &local(),
            )
            .unwrap();
        assert_eq!(message.message_id, duplicate.message_id);
        let received = service
            .receive(
                api::Receive {
                    auth: auth(&second),
                    room_id: Some(room.room_id.clone()),
                    after: None,
                    limit: 20,
                },
                &local(),
            )
            .unwrap();
        assert_eq!(received.deliveries.len(), 1);
        assert_eq!(received.deliveries[0].message.body, "hello");
        service
            .ack(
                api::Ack {
                    auth: auth(&second),
                    delivery_ids: vec![received.deliveries[0].delivery_id.clone()],
                },
                &local(),
            )
            .unwrap();
        let after = service
            .receive(
                api::Receive {
                    auth: auth(&second),
                    room_id: Some(room.room_id),
                    after: None,
                    limit: 20,
                },
                &local(),
            )
            .unwrap();
        assert!(after.deliveries[0].acknowledged);
    }

    #[test]
    fn callback_outbox_signs_bounded_metadata_without_file_bytes() {
        let (_temp, service) = world();
        let first = register(&service, "sender");
        let second = service
            .register(
                api::Register {
                    space: api::Space::Private,
                    display_name: "callback".into(),
                    repository_label: None,
                    domain_label: None,
                    current_subject: None,
                    capabilities: vec![],
                    callback_url: Some("http://127.0.0.1:1234/callback".into()),
                },
                &local(),
            )
            .unwrap();
        let room = service
            .room_create(
                api::RoomCreate {
                    auth: auth(&first),
                    space: api::Space::Private,
                    name: "callback room".into(),
                    members: vec![second.agent.agent_id.clone()],
                },
                &local(),
            )
            .unwrap();
        let bytes = b"callback file";
        let upload = service
            .upload_start(
                api::UploadStart {
                    auth: auth(&first),
                    request_key: "callback-file".into(),
                    name: "note.txt".into(),
                    content_type: "text/plain".into(),
                    byte_size: bytes.len() as u32,
                    sha256: hex_digest(bytes),
                },
                &local(),
            )
            .unwrap();
        service
            .upload_chunk(
                api::UploadChunk {
                    auth: auth(&first),
                    upload_id: upload.upload_id.clone(),
                    offset: 0,
                    data_base64: STANDARD.encode(bytes),
                },
                &local(),
            )
            .unwrap();
        service
            .send(
                api::Send {
                    auth: auth(&first),
                    room_id: room.room_id,
                    body: "callback body".into(),
                    attachments: vec![upload.upload_id],
                    client_message_id: None,
                },
                &local(),
            )
            .unwrap();
        let payload: serde_json::Value = service
            .database
            .call(|db| {
                Ok(db.query_row(
                    "SELECT payload_json FROM agent_hub_outbox LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(payload["body"], "callback body");
        assert!(payload["timestamp"].as_str().is_some());
        assert!(payload["nonce"].as_str().is_some());
        assert_eq!(payload["attachments"][0]["name"], "note.txt");
        assert!(payload["attachments"][0].get("content").is_none());
        let received = service
            .receive(
                api::Receive {
                    auth: auth(&second),
                    room_id: None,
                    after: None,
                    limit: 20,
                },
                &local(),
            )
            .unwrap();
        service
            .ack(
                api::Ack {
                    auth: auth(&second),
                    delivery_ids: vec![received.deliveries[0].delivery_id.clone()],
                },
                &local(),
            )
            .unwrap();
        let pending: u32 = service
            .database
            .call(|db| {
                Ok(db.query_row("SELECT count(*) FROM agent_hub_outbox", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn remote_private_actions_require_current_instance_key() {
        let (_temp, service) = world();
        let rotated = service
            .key_rotate(api::KeyRotate {
                expected_revision: 0,
            })
            .unwrap();
        let created = service
            .remote(
                api::RemoteRequest {
                    bearer: Some(rotated.bearer_key.clone()),
                    action: api::RemoteAction::Register(api::Register {
                        space: api::Space::Private,
                        display_name: "remote".into(),
                        repository_label: None,
                        domain_label: None,
                        current_subject: None,
                        capabilities: vec![],
                        callback_url: None,
                    }),
                },
                &edge(),
            )
            .unwrap();
        assert!(created.get("token").is_some());
        let denied = service
            .remote(
                api::RemoteRequest {
                    bearer: Some("wrong".into()),
                    action: api::RemoteAction::Directory(api::Directory {
                        space: api::Space::Private,
                        after: None,
                        limit: 10,
                    }),
                },
                &edge(),
            )
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn remote_private_and_public_agents_exchange_messages_through_the_adapter() {
        let (_temp, service) = world();
        let rotated = service
            .key_rotate(api::KeyRotate { expected_revision: 0 })
            .unwrap();
        let register_remote = |bearer: Option<String>, name: &str, space: api::Space| {
            let value = service
                .remote(
                    api::RemoteRequest {
                        bearer,
                        action: api::RemoteAction::Register(api::Register {
                            space,
                            display_name: name.into(),
                            repository_label: Some("remote-repo".into()),
                            domain_label: Some("remote.example".into()),
                            current_subject: Some("remote subject".into()),
                            capabilities: vec!["chat".into()],
                            callback_url: None,
                        }),
                    },
                    &edge(),
                )
                .unwrap();
            serde_json::from_value::<api::RegisterResult>(value).unwrap()
        };
        let first = register_remote(Some(rotated.bearer_key.clone()), "private-a", api::Space::Private);
        let second = register_remote(Some(rotated.bearer_key.clone()), "private-b", api::Space::Private);
        let room = service
            .remote(
                api::RemoteRequest {
                    bearer: Some(rotated.bearer_key.clone()),
                    action: api::RemoteAction::RoomCreate(api::RoomCreate {
                        auth: auth(&first),
                        space: api::Space::Private,
                        name: "remote private".into(),
                        members: vec![second.agent.agent_id.clone()],
                    }),
                },
                &edge(),
            )
            .unwrap();
        let room: api::Room = serde_json::from_value(room).unwrap();
        service
            .remote(
                api::RemoteRequest {
                    bearer: Some(rotated.bearer_key.clone()),
                    action: api::RemoteAction::Send(api::Send {
                        auth: auth(&first),
                        room_id: room.room_id.clone(),
                        body: "private remote message".into(),
                        attachments: vec![],
                        client_message_id: None,
                    }),
                },
                &edge(),
            )
            .unwrap();
        let received = service
            .remote(
                api::RemoteRequest {
                    bearer: Some(rotated.bearer_key.clone()),
                    action: api::RemoteAction::Receive(api::Receive {
                        auth: auth(&second),
                        room_id: Some(room.room_id),
                        after: None,
                        limit: 20,
                    }),
                },
                &edge(),
            )
            .unwrap();
        assert_eq!(received["deliveries"][0]["message"]["body"], "private remote message");

        service
            .settings_set(api::SettingsSet {
                expected_revision: 1,
                public_enabled: true,
                public_paused: false,
            })
            .unwrap();
        let public_a = register_remote(None, "public-a", api::Space::Public);
        let public_b = register_remote(None, "public-b", api::Space::Public);
        let public_room = service
            .remote(
                api::RemoteRequest {
                    bearer: None,
                    action: api::RemoteAction::RoomCreate(api::RoomCreate {
                        auth: auth(&public_a),
                        space: api::Space::Public,
                        name: "public board".into(),
                        members: vec![public_b.agent.agent_id.clone()],
                    }),
                },
                &edge(),
            )
            .unwrap();
        let public_room: api::Room = serde_json::from_value(public_room).unwrap();
        service
            .remote(
                api::RemoteRequest {
                    bearer: None,
                    action: api::RemoteAction::Send(api::Send {
                        auth: auth(&public_a),
                        room_id: public_room.room_id.clone(),
                        body: "public message".into(),
                        attachments: vec![],
                        client_message_id: None,
                    }),
                },
                &edge(),
            )
            .unwrap();
        let public_received = service
            .remote(
                api::RemoteRequest {
                    bearer: None,
                    action: api::RemoteAction::Receive(api::Receive {
                        auth: auth(&public_b),
                        room_id: Some(public_room.room_id),
                        after: None,
                        limit: 20,
                    }),
                },
                &edge(),
            )
            .unwrap();
        assert_eq!(public_received["deliveries"][0]["message"]["body"], "public message");
    }

    #[test]
    fn upload_rejects_oversize_and_chunks_at_exact_offsets() {
        let (_temp, service) = world();
        let owner = register(&service, "uploader");
        let too_large = service
            .upload_start(
                api::UploadStart {
                    auth: auth(&owner),
                    request_key: "large".into(),
                    name: "big.bin".into(),
                    content_type: "application/octet-stream".into(),
                    byte_size: MAX_FILE_BYTES + 1,
                    sha256: "0".repeat(64),
                },
                &local(),
            )
            .unwrap_err();
        assert_eq!(too_large.code, ErrorCode::HubAttachmentInvalid);
        let bytes = b"hello";
        let digest = hex_digest(bytes);
        let upload = service
            .upload_start(
                api::UploadStart {
                    auth: auth(&owner),
                    request_key: "ok".into(),
                    name: "hello.txt".into(),
                    content_type: "text/plain".into(),
                    byte_size: bytes.len() as u32,
                    sha256: digest,
                },
                &local(),
            )
            .unwrap();
        let chunk = service
            .upload_chunk(
                api::UploadChunk {
                    auth: auth(&owner),
                    upload_id: upload.upload_id.clone(),
                    offset: 0,
                    data_base64: STANDARD.encode(bytes),
                },
                &local(),
            )
            .unwrap();
        assert!(chunk.complete);
        let file = service
            .upload_file(
                api::UploadFile {
                    auth: auth(&owner),
                    upload_id: Some(upload.upload_id),
                    file_id: None,
                    offset: 0,
                },
                &local(),
            )
            .unwrap();
        assert_eq!(STANDARD.decode(file.data_base64).unwrap(), bytes);
    }

    #[test]
    fn administrator_removals_delete_messages_and_memberships() {
        let (_temp, service) = world();
        let first = register(&service, "first");
        let second = register(&service, "second");
        let room = service
            .room_create(
                api::RoomCreate {
                    auth: auth(&first),
                    space: api::Space::Private,
                    name: "room".into(),
                    members: vec![second.agent.agent_id.clone()],
                },
                &local(),
            )
            .unwrap();
        let message = service
            .send(
                api::Send {
                    auth: auth(&first),
                    room_id: room.room_id.clone(),
                    body: "remove me".into(),
                    attachments: vec![],
                    client_message_id: None,
                },
                &local(),
            )
            .unwrap();
        assert!(service
            .admin_remove_message(api::MessageRemove {
                message_id: message.message_id,
            })
            .unwrap()
            .removed);
        assert!(service
            .admin_remove_room(api::RoomRemove {
                room_id: room.room_id,
            })
            .unwrap()
            .removed);
        assert!(service
            .admin_remove_agent(api::AgentRemove {
                agent_id: second.agent.agent_id,
            })
            .unwrap()
            .removed);
    }
}
