//! Durable ticket records at the owning upstream.
//!
//! This storage boundary does not authenticate callers. The Console/MCP and
//! federation adapters must authorize a request before passing its attributed
//! server and actor here. It is not registered as a remotely callable operation.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::database::{Database, DatabaseError};

const SCHEMA: &str = include_str!("tickets.sql");
pub const DEFAULT_UPSTREAM: &str = "https://vr.ae";
pub const MAX_ATTACHMENTS: usize = 16;
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_FILE_CHUNK: usize = 32 * 1024;
const MAX_PAGE_BYTES: usize = 160 * 1024;

#[derive(Debug, Error)]
pub enum TicketError {
    #[error("{0}")]
    Invalid(String),
    #[error("the requested ticket or attachment is unavailable")]
    NotFound,
    #[error("the record changed; reload before saving")]
    Conflict,
    #[error("this request key was already used for different content")]
    IdempotencyConflict,
    #[error("ticket storage is unavailable")]
    Database(#[from] DatabaseError),
    #[error("ticket storage could not read or save a record")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not allocate a ticket identity")]
    Identity,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub upstream: String,
    pub revision: u64,
}

/// Attribution supplied by an authenticated adapter, never a public identity assertion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Author {
    pub server: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct FileInput {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct CreateTicket {
    pub request_key: String,
    pub author: Author,
    pub title: String,
    pub body: String,
    pub files: Vec<FileInput>,
}

#[derive(Clone, Debug)]
pub struct AddComment {
    pub request_key: String,
    pub author: Author,
    pub ticket_id: String,
    pub body: String,
    pub files: Vec<FileInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketSummary {
    pub id: String,
    pub upstream: String,
    pub origin: String,
    pub title: String,
    pub closed: bool,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
    pub comment_count: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    #[serde(flatten)]
    pub summary: TicketSummary,
    pub author: Author,
    pub body: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub ticket_id: String,
    pub author: Author,
    pub body: String,
    pub created_at: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChunk {
    pub attachment: Attachment,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}

#[derive(Clone)]
pub struct TicketStore {
    database: Database,
    upstream: String,
}

impl TicketStore {
    /// Initialize the ticket component using the existing private authority DB.
    /// This constructor adds no network listener or public API.
    pub fn new(database: Database, upstream: &str) -> Result<Self, TicketError> {
        let upstream = normalize_upstream(upstream)?;
        database.call(|db| {
            db.execute_batch(SCHEMA)?;
            Ok(())
        })?;
        Ok(Self { database, upstream })
    }

    pub fn settings(&self) -> Result<Settings, TicketError> {
        self.read(|db| {
            Ok(db.query_row(
                "SELECT upstream,revision FROM ticket_settings WHERE singleton=1",
                [],
                |row| {
                    Ok(Settings {
                        upstream: row.get(0)?,
                        revision: unsigned(row, 1)?,
                    })
                },
            )?)
        })
    }

    pub fn set_upstream(
        &self,
        upstream: &str,
        expected_revision: u64,
    ) -> Result<Settings, TicketError> {
        let upstream = normalize_upstream(upstream)?;
        self.write(move |db| {
            let changed = db.execute(
                "UPDATE ticket_settings SET upstream=?1,revision=revision+1 WHERE singleton=1 AND revision=?2",
                params![upstream, sql_integer(expected_revision)?],
            )?;
            if changed != 1 { return Err(TicketError::Conflict); }
            Ok(Settings { upstream, revision: expected_revision + 1 })
        })
    }

    pub fn create(&self, input: CreateTicket, at: &str) -> Result<Ticket, TicketError> {
        validate_text(&input.request_key, 256, false, "request key")?;
        validate_author(&input.author)?;
        validate_text(&input.title, 240, false, "title")?;
        validate_text(&input.body, 16_384, false, "description")?;
        validate_timestamp(at)?;
        let files = prepare_files(input.files)?;
        let fingerprint = fingerprint(&input.title, &input.body, &input.author, &files);
        let id = new_id("ticket")?;
        let at = at.to_owned();
        let upstream = self.upstream.clone();
        self.write(move |db| {
            let prior: Option<(String,String)> = db.query_row(
                "SELECT ticket_id,fingerprint FROM feature_tickets WHERE origin=?1 AND request_key=?2",
                params![input.author.server,input.request_key], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            if let Some((prior_id, prior_fingerprint)) = prior {
                if prior_fingerprint != fingerprint { return Err(TicketError::IdempotencyConflict); }
                return ticket(db, &prior_id);
            }
            db.execute(
                "INSERT INTO feature_tickets(ticket_id,upstream,origin,author_name,title,body,request_key,fingerprint,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                params![id,upstream,input.author.server,input.author.name,input.title,input.body,input.request_key,fingerprint,at],
            )?;
            insert_files(db, &id, None, files)?;
            history(db, &id, "created", &input.author, &at, 1)?;
            ticket(db, &id)
        })
    }

    pub fn get(&self, id: &str) -> Result<Ticket, TicketError> {
        let id = id.to_owned();
        self.read(move |db| ticket(db, &id))
    }

    /// Callers apply their access policy before choosing an origin filter.
    /// No filter is the owning upstream's complete inbox.
    pub fn list(
        &self,
        origin: Option<&str>,
        closed: Option<bool>,
        offset: u32,
        limit: u32,
    ) -> Result<Page<TicketSummary>, TicketError> {
        validate_page(offset, limit)?;
        let origin = origin.map(str::to_owned);
        self.read(move |db| {
            let mut query = db.prepare(
                "SELECT ticket_id,upstream,origin,title,closed,revision,created_at,updated_at,(SELECT COUNT(*) FROM ticket_comments c WHERE c.ticket_id=t.ticket_id) FROM feature_tickets t WHERE deleted=0 AND (?1 IS NULL OR origin=?1) AND (?2 IS NULL OR closed=?2) ORDER BY updated_at DESC,ticket_id DESC LIMIT ?3 OFFSET ?4",
            )?;
            let rows = query.query_map(params![origin,closed,limit+1,offset], summary_row)?;
            let items = rows.collect::<Result<Vec<_>,_>>()?;
            page(items, offset, limit)
        })
    }

    pub fn edit(
        &self,
        id: &str,
        title: &str,
        body: &str,
        expected_revision: u64,
        author: Author,
        at: &str,
    ) -> Result<Ticket, TicketError> {
        validate_text(title, 240, false, "title")?;
        validate_text(body, 16_384, false, "description")?;
        validate_author(&author)?;
        validate_timestamp(at)?;
        let (id, title, body, at) = (
            id.to_owned(),
            title.to_owned(),
            body.to_owned(),
            at.to_owned(),
        );
        self.write(move |db| {
            check_revision(db, &id, expected_revision)?;
            db.execute("UPDATE feature_tickets SET title=?2,body=?3,revision=revision+1,updated_at=?4 WHERE ticket_id=?1", params![id,title,body,at])?;
            history(db, &id, "edited", &author, &at, expected_revision + 1)?;
            ticket(db, &id)
        })
    }

    pub fn set_closed(
        &self,
        id: &str,
        closed: bool,
        expected_revision: u64,
        author: Author,
        at: &str,
    ) -> Result<Ticket, TicketError> {
        validate_author(&author)?;
        validate_timestamp(at)?;
        let (id, at) = (id.to_owned(), at.to_owned());
        self.write(move |db| {
            check_revision(db, &id, expected_revision)?;
            db.execute("UPDATE feature_tickets SET closed=?2,revision=revision+1,updated_at=?3 WHERE ticket_id=?1", params![id,closed,at])?;
            history(db, &id, if closed { "closed" } else { "reopened" }, &author, &at, expected_revision + 1)?;
            ticket(db, &id)
        })
    }

    /// Retain a tombstone and history, preventing a delayed retry from recreating
    /// a removed request. All ordinary reads, including file reads, exclude it.
    pub fn remove(
        &self,
        id: &str,
        expected_revision: u64,
        author: Author,
        at: &str,
    ) -> Result<(), TicketError> {
        validate_author(&author)?;
        validate_timestamp(at)?;
        let (id, at) = (id.to_owned(), at.to_owned());
        self.write(move |db| {
            check_revision(db, &id, expected_revision)?;
            db.execute("UPDATE feature_tickets SET deleted=1,revision=revision+1,updated_at=?2 WHERE ticket_id=?1", params![id,at])?;
            history(db, &id, "removed", &author, &at, expected_revision + 1)
        })
    }

    pub fn add_comment(&self, input: AddComment, at: &str) -> Result<Comment, TicketError> {
        validate_text(&input.request_key, 256, false, "request key")?;
        validate_author(&input.author)?;
        validate_text(&input.body, 8192, !input.files.is_empty(), "comment")?;
        validate_timestamp(at)?;
        let files = prepare_files(input.files)?;
        let fingerprint = fingerprint("", &input.body, &input.author, &files);
        let id = new_id("comment")?;
        let at = at.to_owned();
        self.write(move |db| {
            let parent = ticket(db, &input.ticket_id)?;
            let prior: Option<(String,String)> = db.query_row(
                "SELECT comment_id,fingerprint FROM ticket_comments WHERE ticket_id=?1 AND server=?2 AND request_key=?3",
                params![input.ticket_id,input.author.server,input.request_key], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            if let Some((prior_id, prior_fingerprint)) = prior {
                if prior_fingerprint != fingerprint { return Err(TicketError::IdempotencyConflict); }
                return comment(db, &prior_id);
            }
            db.execute("INSERT INTO ticket_comments(comment_id,ticket_id,server,author_name,body,request_key,fingerprint,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id,input.ticket_id,input.author.server,input.author.name,input.body,input.request_key,fingerprint,at])?;
            insert_files(db, &input.ticket_id, Some(&id), files)?;
            db.execute("UPDATE feature_tickets SET revision=revision+1,updated_at=?2 WHERE ticket_id=?1", params![input.ticket_id,at])?;
            history(db, &input.ticket_id, "commented", &input.author, &at, parent.summary.revision + 1)?;
            comment(db, &id)
        })
    }

    pub fn comments(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Comment>, TicketError> {
        validate_page(offset, limit)?;
        let id = id.to_owned();
        self.read(move |db| {
            ticket(db, &id)?;
            let mut query = db.prepare("SELECT comment_id FROM ticket_comments WHERE ticket_id=?1 ORDER BY sequence LIMIT ?2 OFFSET ?3")?;
            let ids = query.query_map(params![id,limit+1,offset], |r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            let items = ids.iter().map(|id| comment(db,id)).collect::<Result<Vec<_>,_>>()?;
            page(items, offset, limit)
        })
    }

    /// Read a bounded chunk through its exact ticket and comment association.
    /// A file belonging to another message is never returned by a guessed ID.
    pub fn file(
        &self,
        ticket_id: &str,
        comment_id: Option<&str>,
        file_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<FileChunk, TicketError> {
        if max_bytes == 0 || max_bytes > MAX_FILE_CHUNK {
            return Err(invalid("file chunk must contain 1..32768 bytes"));
        }
        let (ticket_id, comment_id, file_id) = (
            ticket_id.to_owned(),
            comment_id.map(str::to_owned),
            file_id.to_owned(),
        );
        self.read(move |db| {
            ticket(db, &ticket_id)?;
            let attachment = db.query_row("SELECT attachment_id,name,content_type,byte_size,sha256 FROM ticket_attachments WHERE attachment_id=?1 AND ticket_id=?2 AND comment_id IS ?3", params![file_id,ticket_id,comment_id], attachment_row).optional()?.ok_or(TicketError::NotFound)?;
            if offset > attachment.byte_size { return Err(invalid("file offset exceeds its size")); }
            let bytes: Vec<u8> = db.query_row("SELECT substr(content,?2,?3) FROM ticket_attachments WHERE attachment_id=?1", params![file_id,sql_integer(offset+1)?,max_bytes as i64], |row| row.get(0))?;
            let end = offset + bytes.len() as u64;
            Ok(FileChunk { next_offset: (end < attachment.byte_size).then_some(end), attachment, offset, bytes })
        })
    }

    fn read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, TicketError> + Send + 'static,
    ) -> Result<T, TicketError> {
        self.database.call(move |db| Ok(work(db)))?
    }

    fn write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, TicketError> + Send + 'static,
    ) -> Result<T, TicketError> {
        self.database.call(move |db| {
            let transaction = db.transaction()?;
            let result = work(&transaction);
            if result.is_ok() {
                transaction.commit()?;
            }
            Ok(result)
        })?
    }
}

pub fn normalize_upstream(value: &str) -> Result<String, TicketError> {
    let value = value.trim();
    let input = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let url =
        reqwest::Url::parse(&input).map_err(|_| invalid("enter an upstream server address"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "use a server origin without credentials, path, query or fragment",
        ));
    }
    // Transport policy (including allowed development origins) belongs to the
    // authenticated adapter. Normalization never grants network access.
    Ok(url.origin().ascii_serialization())
}

struct PreparedFile {
    metadata: Attachment,
    bytes: Vec<u8>,
}

fn prepare_files(files: Vec<FileInput>) -> Result<Vec<PreparedFile>, TicketError> {
    if files.len() > MAX_ATTACHMENTS {
        return Err(invalid("a message may contain at most 16 files"));
    }
    let mut total = 0usize;
    let mut names = HashSet::new();
    files
        .into_iter()
        .map(|file| {
            validate_text(&file.name, 240, false, "file name")?;
            if file.name.contains(['/', '\\'])
                || file.name.chars().any(char::is_control)
                || matches!(file.name.as_str(), "." | "..")
                || !names.insert(file.name.clone())
            {
                return Err(invalid(
                    "use distinct file names without directory components",
                ));
            }
            total = total
                .checked_add(file.bytes.len())
                .ok_or_else(|| invalid("files are too large"))?;
            if file.bytes.is_empty()
                || file.bytes.len() > MAX_FILE_BYTES
                || total > MAX_MESSAGE_BYTES
            {
                return Err(invalid(
                    "files must be nonempty, at most 16 MiB each and 32 MiB per message",
                ));
            }
            Ok(PreparedFile {
                metadata: Attachment {
                    id: new_id("file")?,
                    name: file.name,
                    content_type: content_type(&file.bytes).to_owned(),
                    byte_size: file.bytes.len() as u64,
                    sha256: sha(&file.bytes),
                },
                bytes: file.bytes,
            })
        })
        .collect()
}

fn content_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

fn fingerprint(title: &str, body: &str, author: &Author, files: &[PreparedFile]) -> String {
    let mut hash = Sha256::new();
    for part in [title, body, &author.server, &author.name]
        .into_iter()
        .chain(
            files
                .iter()
                .flat_map(|f| [f.metadata.name.as_str(), f.metadata.sha256.as_str()]),
        )
    {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex(hash.finalize().as_slice())
}

fn sha(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn new_id(prefix: &str) -> Result<String, TicketError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| TicketError::Identity)?;
    Ok(format!("{prefix}-{}", hex(&bytes)))
}

fn invalid(message: &str) -> TicketError {
    TicketError::Invalid(message.to_owned())
}

fn validate_text(value: &str, max: usize, empty: bool, label: &str) -> Result<(), TicketError> {
    if (!empty && value.trim().is_empty())
        || value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(invalid(&format!(
            "{label} must contain valid text within {max} bytes"
        )));
    }
    Ok(())
}

fn validate_author(author: &Author) -> Result<(), TicketError> {
    validate_text(&author.server, 256, false, "origin server")?;
    validate_text(&author.name, 160, false, "author")
}

fn validate_timestamp(at: &str) -> Result<(), TicketError> {
    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
        .map_err(|_| invalid("timestamp must be RFC 3339"))?;
    Ok(())
}

fn validate_page(offset: u32, limit: u32) -> Result<(), TicketError> {
    if !(1..=25).contains(&limit) || offset > 1_000_000 {
        return Err(invalid(
            "page size must be 1..25 and offset at most 1000000",
        ));
    }
    Ok(())
}

fn page<T: Serialize>(items: Vec<T>, offset: u32, limit: u32) -> Result<Page<T>, TicketError> {
    let available = items.len();
    let mut result = Vec::new();
    let mut bytes = 0;
    for item in items.into_iter().take(limit as usize) {
        let size = serde_json::to_vec(&item)
            .map_err(|_| invalid("could not encode ticket page"))?
            .len();
        if bytes + size > MAX_PAGE_BYTES {
            break;
        }
        bytes += size;
        result.push(item);
    }
    if available > 0 && result.is_empty() {
        return Err(invalid("ticket item exceeds the response limit"));
    }
    let next_offset = (available > result.len()).then_some(offset + result.len() as u32);
    Ok(Page {
        items: result,
        next_offset,
    })
}

fn sql_integer(value: u64) -> Result<i64, TicketError> {
    value
        .try_into()
        .map_err(|_| invalid("integer exceeds the storage range"))
}

fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    row.get::<_, i64>(index)?.try_into().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TicketSummary> {
    Ok(TicketSummary {
        id: row.get(0)?,
        upstream: row.get(1)?,
        origin: row.get(2)?,
        title: row.get(3)?,
        closed: row.get(4)?,
        revision: unsigned(row, 5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        comment_count: unsigned(row, 8)?,
    })
}

fn ticket(db: &Connection, id: &str) -> Result<Ticket, TicketError> {
    let mut result = db.query_row(
        "SELECT ticket_id,upstream,origin,title,closed,revision,created_at,updated_at,(SELECT COUNT(*) FROM ticket_comments c WHERE c.ticket_id=t.ticket_id),author_name,body FROM feature_tickets t WHERE ticket_id=?1 AND deleted=0",
        [id], |row| { let summary = summary_row(row)?; Ok(Ticket { author: Author { server: summary.origin.clone(), name: row.get(9)? }, summary, body: row.get(10)?, attachments: vec![] }) },
    ).optional()?.ok_or(TicketError::NotFound)?;
    result.attachments = attachments(db, id, None)?;
    Ok(result)
}

fn comment(db: &Connection, id: &str) -> Result<Comment, TicketError> {
    let mut result = db.query_row(
        "SELECT comment_id,ticket_id,server,author_name,body,created_at FROM ticket_comments WHERE comment_id=?1",
        [id], |row| Ok(Comment { id: row.get(0)?, ticket_id: row.get(1)?, author: Author {server:row.get(2)?,name:row.get(3)?}, body: row.get(4)?, created_at: row.get(5)?, attachments:vec![] }),
    ).optional()?.ok_or(TicketError::NotFound)?;
    result.attachments = attachments(db, &result.ticket_id, Some(id))?;
    Ok(result)
}

fn attachment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: row.get(0)?,
        name: row.get(1)?,
        content_type: row.get(2)?,
        byte_size: unsigned(row, 3)?,
        sha256: row.get(4)?,
    })
}

fn attachments(
    db: &Connection,
    ticket_id: &str,
    comment_id: Option<&str>,
) -> Result<Vec<Attachment>, TicketError> {
    let mut query = db.prepare("SELECT attachment_id,name,content_type,byte_size,sha256 FROM ticket_attachments WHERE ticket_id=?1 AND comment_id IS ?2 ORDER BY position")?;
    Ok(query
        .query_map(params![ticket_id, comment_id], attachment_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn insert_files(
    db: &Connection,
    ticket_id: &str,
    comment_id: Option<&str>,
    files: Vec<PreparedFile>,
) -> Result<(), TicketError> {
    for (position, file) in files.into_iter().enumerate() {
        let m = file.metadata;
        db.execute("INSERT INTO ticket_attachments(attachment_id,ticket_id,comment_id,position,name,content_type,byte_size,sha256,content) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![m.id,ticket_id,comment_id,position as i64,m.name,m.content_type,sql_integer(m.byte_size)?,m.sha256,file.bytes])?;
    }
    Ok(())
}

fn check_revision(db: &Connection, id: &str, expected: u64) -> Result<(), TicketError> {
    let actual: Option<u64> = db
        .query_row(
            "SELECT revision FROM feature_tickets WHERE ticket_id=?1 AND deleted=0",
            [id],
            |row| unsigned(row, 0),
        )
        .optional()?;
    match actual {
        None => Err(TicketError::NotFound),
        Some(value) if value != expected => Err(TicketError::Conflict),
        Some(_) => Ok(()),
    }
}

fn history(
    db: &Connection,
    id: &str,
    action: &str,
    author: &Author,
    at: &str,
    revision: u64,
) -> Result<(), TicketError> {
    db.execute("INSERT INTO ticket_history(ticket_id,revision,action,server,author_name,occurred_at) VALUES(?1,?2,?3,?4,?5,?6)",params![id,sql_integer(revision)?,action,author.server,author.name,at])?;
    Ok(())
}
