//! Ticket federation uses a deliberately narrow public endpoint, never a remote
//! control-plane bridge. Credentials stay inside the originating authority DB.
use std::io::Read;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use devcoordinator2_api::{ErrorCode, ProtocolError, tickets::*};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::database::{Database, DatabaseError};
use crate::tickets::{
    AddComment, CreateTicket, FileInput, TicketError, TicketStore, normalize_upstream,
};

const ENDPOINT: &str = "/.well-known/devcoordinator2/tickets";
const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_BYTES: i64 = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct TicketService {
    database: Database,
    store: TicketStore,
    local: String,
    label: String,
}

impl TicketService {
    pub fn new(database: Database, base_domain: &str) -> Result<Self, ProtocolError> {
        let local = normalize_upstream(if base_domain.trim().is_empty() {
            "local"
        } else {
            base_domain
        })
        .map_err(storage_error)?;
        let store = TicketStore::new(database.clone(), &local).map_err(storage_error)?;
        Ok(Self {
            database,
            store,
            label: if base_domain.trim().is_empty() {
                "This server".into()
            } else {
                base_domain.to_owned()
            },
            local,
        })
    }

    pub fn settings(&self, administrator: bool) -> Result<Settings, ProtocolError> {
        let setting = self.store.settings().map_err(storage_error)?;
        let previous_upstreams = self
            .database
            .call(|db| {
                let mut query =
                    db.prepare("SELECT upstream FROM ticket_peers ORDER BY upstream")?;
                Ok(query
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        Ok(Settings {
            upstream: setting.upstream,
            revision: setting.revision,
            local: self.local.clone(),
            previous_upstreams,
            can_configure: administrator,
        })
    }

    pub fn configure(&self, input: Configure) -> Result<Settings, ProtocolError> {
        let target = if input.upstream.trim() == "local" {
            self.local.clone()
        } else {
            transport_origin(&input.upstream)?
        };
        self.store
            .set_upstream(&target, input.expected_revision)
            .map_err(storage_error)?;
        self.settings(true)
    }

    pub fn request(&self, request: Request, administrator: bool) -> Result<Reply, ProtocolError> {
        let settings = self.store.settings().map_err(storage_error)?;
        let selected = request.target.as_deref().unwrap_or(&settings.upstream);
        let local_requested = selected == "local";
        let target = if local_requested {
            self.local.clone()
        } else {
            transport_origin(selected)?
        };
        if target != settings.upstream && target != self.local {
            let lookup = target.clone();
            let known = self
                .database
                .call(move |db| {
                    Ok(db
                        .query_row(
                            "SELECT 1 FROM ticket_peers WHERE upstream=?1",
                            [lookup],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some())
                })
                .map_err(database_error)?;
            if !known {
                return Err(invalid(
                    "choose the configured upstream or an existing ticket destination",
                ));
            }
        }
        let credential = self.credential(&target)?;
        if target == self.local {
            return self.execute(
                request.action,
                Some(server_id(&credential)),
                &self.label,
                administrator && local_requested,
            );
        }
        // Only the local settings author can choose a new network destination.
        // No redirects: never disclose this upstream's credential to another host.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(7))
            .build()
            .map_err(|_| unavailable())?;
        let remote = RemoteRequest {
            action: request.action,
            credential: Some(credential),
            server_label: Some(self.label.clone()),
        };
        let response = client
            .post(format!("{target}{ENDPOINT}"))
            .json(&remote)
            .send()
            .map_err(|_| unavailable())?;
        if response.status().is_redirection() {
            return Err(invalid(
                "the upstream redirected; configure its exact server address",
            ));
        }
        let mut bytes = Vec::new();
        response
            .take((devcoordinator2_api::MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > devcoordinator2_api::MAX_RESPONSE_BYTES {
            return Err(unavailable());
        }
        let envelope: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if envelope.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            // Do not promote arbitrary remote error text into local diagnostics.
            let code = envelope
                .pointer("/error/code")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(match code {
                "permission_denied" => denied(),
                "configuration_conflict" => ProtocolError::new(
                    ErrorCode::ConfigurationConflict,
                    "the ticket changed; refresh before saving",
                ),
                "task_not_found" => {
                    ProtocolError::new(ErrorCode::TaskNotFound, "the ticket or file is unavailable")
                }
                "params_invalid" => invalid(
                    "the upstream rejected the request; check its fields and attachment sizes",
                ),
                _ => unavailable(),
            });
        }
        serde_json::from_value(envelope["data"].clone()).map_err(|_| unavailable())
    }

    /// This path never consults local/administrator authority. Even if the edge
    /// transports the call as a trusted local process, only the bearer identity
    /// below can mutate the origin's own content.
    pub fn remote(&self, request: RemoteRequest) -> Result<Reply, ProtocolError> {
        let identity = match request.credential {
            Some(value) if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some(server_id(&value))
            }
            Some(_) => return Err(denied()),
            None => None,
        };
        let label = request
            .server_label
            .unwrap_or_else(|| "Remote server".into());
        if label.is_empty() || label.len() > 160 || label.chars().any(char::is_control) {
            return Err(invalid("server label must be short plain text"));
        }
        self.execute(request.action, identity, &label, false)
    }

    fn execute(
        &self,
        action: Action,
        identity: Option<String>,
        label: &str,
        administrator: bool,
    ) -> Result<Reply, ProtocolError> {
        if !action.is_read() && identity.is_none() {
            return Err(denied());
        }
        let actor = identity.as_deref().unwrap_or("");
        let author = Author {
            server: actor.to_owned(),
            name: label.to_owned(),
        };
        let at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| unavailable())?;
        let get = |id: &str| self.store.get(id).map_err(storage_error);
        let can_manage = |ticket: &Ticket| {
            administrator || (!actor.is_empty() && ticket.summary.origin == actor)
        };
        let allowed = |id: &str| {
            let ticket = get(id)?;
            if can_manage(&ticket) {
                Ok(ticket)
            } else {
                Err(denied())
            }
        };
        let ticket_reply = |ticket: Ticket| {
            let permission = can_manage(&ticket);
            Reply::Ticket {
                ticket,
                can_manage: permission,
                can_comment: permission,
            }
        };
        Ok(match action {
            Action::List {
                offset,
                limit,
                closed,
                mine,
            } => {
                if mine && identity.is_none() {
                    return Err(denied());
                }
                let page = self
                    .store
                    .list(mine.then_some(actor), closed, offset, limit)
                    .map_err(storage_error)?;
                Reply::Tickets {
                    items: page.items,
                    next_offset: page.next_offset,
                }
            }
            Action::Get { ticket_id } => ticket_reply(get(&ticket_id)?),
            Action::Comments {
                ticket_id,
                offset,
                limit,
            } => {
                let page = self
                    .store
                    .comments(&ticket_id, offset, limit)
                    .map_err(storage_error)?;
                Reply::Comments {
                    items: page.items,
                    next_offset: page.next_offset,
                }
            }
            Action::Create {
                request_key,
                title,
                body,
                attachments,
            } => {
                let files = self.files(actor, attachments)?;
                ticket_reply(
                    self.store
                        .create(
                            CreateTicket {
                                request_key,
                                title,
                                body,
                                author,
                                files,
                            },
                            &at,
                        )
                        .map_err(storage_error)?,
                )
            }
            Action::Edit {
                ticket_id,
                expected_revision,
                title,
                body,
            } => {
                allowed(&ticket_id)?;
                ticket_reply(
                    self.store
                        .edit(&ticket_id, &title, &body, expected_revision, author, &at)
                        .map_err(storage_error)?,
                )
            }
            Action::Remove {
                ticket_id,
                expected_revision,
            } => {
                allowed(&ticket_id)?;
                self.store
                    .remove(&ticket_id, expected_revision, author, &at)
                    .map_err(storage_error)?;
                Reply::Removed { removed: true }
            }
            Action::Close {
                ticket_id,
                expected_revision,
                closed,
            } => {
                allowed(&ticket_id)?;
                ticket_reply(
                    self.store
                        .set_closed(&ticket_id, closed, expected_revision, author, &at)
                        .map_err(storage_error)?,
                )
            }
            Action::Comment {
                ticket_id,
                request_key,
                body,
                attachments,
            } => {
                allowed(&ticket_id)?;
                let files = self.files(actor, attachments)?;
                Reply::Comment {
                    comment: self
                        .store
                        .add_comment(
                            AddComment {
                                ticket_id,
                                request_key,
                                body,
                                author,
                                files,
                            },
                            &at,
                        )
                        .map_err(storage_error)?,
                }
            }
            Action::UploadStart {
                request_key,
                name,
                byte_size,
                sha256,
            } => self.upload_start(actor, request_key, name, byte_size, sha256)?,
            Action::UploadChunk {
                upload_id,
                offset,
                data_base64,
            } => self.upload_chunk(actor, upload_id, offset, data_base64)?,
            Action::UploadRemove { upload_id } => {
                let actor = actor.to_owned();
                let removed = self
                    .database
                    .call(move |db| {
                        Ok(db.execute(
                            "DELETE FROM ticket_uploads WHERE upload_id=?1 AND owner=?2",
                            params![upload_id, actor],
                        )? > 0)
                    })
                    .map_err(database_error)?;
                Reply::Removed { removed }
            }
            Action::File {
                ticket_id,
                comment_id,
                file_id,
                offset,
            } => {
                let file = self
                    .store
                    .file(
                        &ticket_id,
                        comment_id.as_deref(),
                        &file_id,
                        offset,
                        crate::tickets::MAX_FILE_CHUNK,
                    )
                    .map_err(storage_error)?;
                Reply::File {
                    attachment: file.attachment,
                    offset: file.offset,
                    data_base64: STANDARD.encode(file.bytes),
                    next_offset: file.next_offset,
                }
            }
        })
    }

    fn credential(&self, upstream: &str) -> Result<String, ProtocolError> {
        let upstream = upstream.to_owned();
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| unavailable())?;
        let credential = hex(&bytes);
        self.database
            .transaction(move |db| {
                db.execute(
                    "INSERT OR IGNORE INTO ticket_peers(upstream,credential) VALUES(?1,?2)",
                    params![upstream, credential],
                )?;
                Ok(db.query_row(
                    "SELECT credential FROM ticket_peers WHERE upstream=?1",
                    [upstream],
                    |row| row.get(0),
                )?)
            })
            .map_err(database_error)
    }

    fn upload_start(
        &self,
        owner: &str,
        key: String,
        name: String,
        size: u32,
        digest: String,
    ) -> Result<Reply, ProtocolError> {
        if key.is_empty()
            || key.len() > 256
            || name.trim().is_empty()
            || name.len() > 240
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
            || matches!(name.as_str(), "." | "..")
            || size == 0
            || size as usize > MAX_UPLOAD_BYTES
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid(
                "invalid attachment name, size, digest or request key",
            ));
        }
        let digest = digest.to_ascii_lowercase();
        let owner = owner.to_owned();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| unavailable())?;
        let id = format!("upload-{}", hex(&random));
        self.database.transaction(move |db|{
            db.execute("DELETE FROM ticket_uploads WHERE expires_at<?1",[now])?;
            let previous:Option<(String,String,u32,String,u32)>=db.query_row("SELECT upload_id,name,byte_size,sha256,length(content) FROM ticket_uploads WHERE owner=?1 AND request_key=?2",params![owner,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            if let Some((id,old_name,old_size,old_digest,offset))=previous {
                if old_name!=name || old_size!=size || old_digest!=digest { return Err(invalid("attachment request key was reused for different content").into()); }
                return Ok(Reply::Upload{upload_id:id,offset});
            }
            let (count,total):(i64,i64)=db.query_row("SELECT COUNT(*),COALESCE(SUM(byte_size),0) FROM ticket_uploads WHERE owner=?1",[&owner],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if count>=64 || total+i64::from(size)>MAX_PENDING_BYTES { return Err(invalid("too many pending attachments; remove unused uploads first").into()); }
            db.execute("INSERT INTO ticket_uploads(upload_id,owner,request_key,name,byte_size,sha256,content,expires_at) VALUES(?1,?2,?3,?4,?5,?6,X'',?7)",params![id,owner,key,name,size,digest,now+86400])?;
            Ok(Reply::Upload{upload_id:id,offset:0})
        }).map_err(database_error)
    }

    fn upload_chunk(
        &self,
        owner: &str,
        id: String,
        offset: u32,
        encoded: String,
    ) -> Result<Reply, ProtocolError> {
        if encoded.len() > 44_000 {
            return Err(invalid("attachment chunk is too large"));
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| invalid("invalid base64 attachment chunk"))?;
        if bytes.is_empty() || bytes.len() > 32_768 {
            return Err(invalid("attachment chunk must contain 1..32768 bytes"));
        }
        let owner = owner.to_owned();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.database.transaction(move |db|{
            let (size,mut content):(u32,Vec<u8>)=db.query_row("SELECT byte_size,content FROM ticket_uploads WHERE upload_id=?1 AND owner=?2 AND expires_at>=?3",params![id,owner,now],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(||invalid("attachment upload is unavailable"))?;
            let end=offset as usize+bytes.len();
            if (offset as usize)<content.len() && content.get(offset as usize..end)==Some(bytes.as_slice()) { return Ok(Reply::Upload{upload_id:id,offset:content.len() as u32}); }
            if offset as usize!=content.len() || end>size as usize { return Err(invalid("attachment offset or length does not match").into()); }
            content.extend(bytes);
            db.execute("UPDATE ticket_uploads SET content=?2 WHERE upload_id=?1",params![id,content])?;
            Ok(Reply::Upload{upload_id:id,offset:end as u32})
        }).map_err(database_error)
    }

    fn files(&self, owner: &str, ids: Vec<String>) -> Result<Vec<FileInput>, ProtocolError> {
        if ids.len() > crate::tickets::MAX_ATTACHMENTS {
            return Err(invalid("at most 16 attachments per message"));
        }
        let owner = owner.to_owned();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.database.call(move |db|{
            let mut files=Vec::new();let mut total=0;
            for id in ids {
                let (name,size,digest,bytes):(String,u32,String,Vec<u8>)=db.query_row("SELECT name,byte_size,sha256,content FROM ticket_uploads WHERE upload_id=?1 AND owner=?2 AND expires_at>=?3",params![id,owner,now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or_else(||invalid("attachment upload is unavailable"))?;
                total+=bytes.len();
                if total>crate::tickets::MAX_MESSAGE_BYTES || bytes.len()!=size as usize || hex(Sha256::digest(&bytes).as_slice())!=digest { return Err(invalid("attachment is incomplete or does not match its digest").into()); }
                files.push(FileInput{name,bytes});
            }
            Ok(files)
        }).map_err(database_error)
    }
}

fn transport_origin(value: &str) -> Result<String, ProtocolError> {
    let origin = normalize_upstream(value).map_err(storage_error)?;
    let url = reqwest::Url::parse(&origin).map_err(|_| invalid("invalid upstream"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !loopback {
        return Err(invalid(
            "upstreams require HTTPS; HTTP is only available for loopback development",
        ));
    }
    Ok(origin)
}
fn server_id(credential: &str) -> String {
    format!(
        "server-{}",
        hex(Sha256::digest(credential.as_bytes()).as_slice())
    )
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}
fn denied() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        "only the originating server and upstream maintainers may change this ticket",
    )
}
fn unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DaemonUnavailable,
        "the upstream ticket server is unavailable; keep the draft and retry",
    )
}
fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        _ => ProtocolError::new(ErrorCode::InternalError, "ticket storage is unavailable"),
    }
}
fn storage_error(error: TicketError) -> ProtocolError {
    match error {
        TicketError::Invalid(message) => invalid(&message),
        TicketError::NotFound => ProtocolError::new(
            ErrorCode::TaskNotFound,
            "the ticket or attachment is unavailable",
        ),
        TicketError::Conflict | TicketError::IdempotencyConflict => {
            ProtocolError::new(ErrorCode::ConfigurationConflict, error.to_string())
        }
        _ => ProtocolError::new(ErrorCode::InternalError, "ticket storage is unavailable"),
    }
}
