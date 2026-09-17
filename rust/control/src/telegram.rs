//! Telegram notifications with durable, bounded delivery state.
//!
//! The authenticated HTTP adapter owns the bot token, so domain callers and
//! test transports never construct or receive token-bearing URLs. Database
//! rows contain only link, subscription, and bounded outbox state. Errors and
//! lifecycle status are deliberately content-free.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use devcoordinator2_api::params::TelegramSubscription as TelegramSubscriptionParams;
use devcoordinator2_api::results::{
    TelegramChat, TelegramLinked, TelegramList, TelegramSubscription as TelegramSubscriptionResult,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use reqwest::header::CONTENT_LENGTH;
use rusqlite::OptionalExtension;
use rustix::fs::{Mode, OFlags, fstat, open};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, format_description::FormatItem, macros::format_description};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock, HostRandom, RandomSource};

pub const OUTBOX_MAX_ROWS: u32 = 1_000;
pub const OUTBOX_MAX_ATTEMPTS: u32 = 10;
pub const OUTBOX_MAX_AGE_SECONDS: i64 = 24 * 60 * 60;
pub const LINK_CODE_TTL_SECONDS: i64 = 15 * 60;
pub const SCOPE_SERVER: &str = "server";

const OUTBOX_BATCH: u32 = 20;
const MESSAGE_MAX_CHARS: usize = 4_000;
const LABEL_MAX_CHARS: usize = 64;
const TOKEN_MAX_BYTES: u64 = 4_096;
const TOKEN_MAX_CHARS: usize = 512;
const HTTP_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
const POLL_TIMEOUT: StdDuration = StdDuration::from_secs(35);
const SEND_TIMEOUT: StdDuration = StdDuration::from_secs(15);
const LOOP_WAKE: StdDuration = StdDuration::from_millis(100);
const ISO_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

pub type TelegramHttpFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, TelegramHttpError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelegramMethod {
    GetUpdates,
    SendMessage,
}

impl TelegramMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::GetUpdates => "getUpdates",
            Self::SendMessage => "sendMessage",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelegramHttpError {
    Unavailable,
    Rejected,
    InvalidResponse,
}

impl std::fmt::Display for TelegramHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "telegram request unavailable",
            Self::Rejected => "telegram request rejected",
            Self::InvalidResponse => "telegram response invalid",
        })
    }
}

impl std::error::Error for TelegramHttpError {}

/// One already-authenticated Telegram transport.
///
/// Implementations receive only the method and bounded JSON payload. The
/// production implementation privately owns its token-bearing endpoint.
pub trait TelegramHttp: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        method: TelegramMethod,
        payload: Value,
        timeout: StdDuration,
    ) -> TelegramHttpFuture<'a>;
}

struct ReqwestTelegramHttp {
    client: reqwest::Client,
    authenticated_base: String,
}

impl ReqwestTelegramHttp {
    fn new(api_base: &str, token: String) -> Result<Self, TelegramHttpError> {
        if api_base.is_empty()
            || api_base.chars().any(char::is_control)
            || token.is_empty()
            || token.chars().any(char::is_control)
        {
            return Err(TelegramHttpError::Unavailable);
        }
        let authenticated_base = format!("{}/bot{}", api_base.trim_end_matches('/'), token);
        reqwest::Url::parse(&authenticated_base).map_err(|_| TelegramHttpError::Unavailable)?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| TelegramHttpError::Unavailable)?;
        Ok(Self {
            client,
            authenticated_base,
        })
    }
}

#[derive(Deserialize)]
struct TelegramApiEnvelope {
    ok: bool,
    #[serde(default)]
    result: Value,
}

impl TelegramHttp for ReqwestTelegramHttp {
    fn call<'a>(
        &'a self,
        method: TelegramMethod,
        payload: Value,
        timeout: StdDuration,
    ) -> TelegramHttpFuture<'a> {
        Box::pin(async move {
            let endpoint = format!("{}/{}", self.authenticated_base, method.as_str());
            let mut response = self
                .client
                .post(endpoint)
                .timeout(timeout)
                .json(&payload)
                .send()
                .await
                .map_err(|_| TelegramHttpError::Unavailable)?;
            if !response.status().is_success() {
                return Err(TelegramHttpError::Rejected);
            }
            if response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > HTTP_RESPONSE_MAX_BYTES)
            {
                return Err(TelegramHttpError::InvalidResponse);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| TelegramHttpError::Unavailable)?
            {
                if body.len().saturating_add(chunk.len()) > HTTP_RESPONSE_MAX_BYTES {
                    return Err(TelegramHttpError::InvalidResponse);
                }
                body.extend_from_slice(&chunk);
            }
            let envelope: TelegramApiEnvelope =
                serde_json::from_slice(&body).map_err(|_| TelegramHttpError::InvalidResponse)?;
            if !envelope.ok {
                return Err(TelegramHttpError::Rejected);
            }
            Ok(envelope.result)
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelegramScope {
    Server,
    Deployment(String),
    Repository(String),
}

impl TelegramScope {
    pub fn as_text(&self) -> String {
        match self {
            Self::Server => SCOPE_SERVER.into(),
            Self::Deployment(identifier) => format!("deployment:{identifier}"),
            Self::Repository(identifier) => format!("repository:{identifier}"),
        }
    }
}

pub fn parse_scope(scope: &str) -> Result<TelegramScope, ProtocolError> {
    if scope == SCOPE_SERVER {
        return Ok(TelegramScope::Server);
    }
    if let Some(identifier) = scope.strip_prefix("deployment:")
        && !identifier.is_empty()
    {
        return Ok(TelegramScope::Deployment(identifier.into()));
    }
    if let Some(identifier) = scope.strip_prefix("repository:")
        && !identifier.is_empty()
    {
        return Ok(TelegramScope::Repository(identifier.into()));
    }
    Err(ProtocolError::new(
        ErrorCode::ParamsInvalid,
        "scope must be server, deployment:<id>, or repository:<id>",
    ))
}

/// Runtime-neutral event envelope consumed by notification routing.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramEvent {
    pub kind: String,
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

impl TelegramEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            fields: BTreeMap::new(),
        }
    }

    pub fn with(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(name.into(), value.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedTelegramEvent {
    pub scopes: Vec<String>,
    pub text: Option<String>,
}

struct RuntimeStatus {
    last_poll_at: Option<String>,
    last_error: Option<String>,
}

struct TelegramInner {
    database: Database,
    http: Option<Arc<dyn TelegramHttp>>,
    clock: Arc<dyn Clock>,
    random: Arc<dyn RandomSource>,
    update_offset: AtomicI64,
    status: Mutex<RuntimeStatus>,
    delivery_wake: Notify,
}

#[derive(Clone)]
pub struct TelegramService {
    inner: Arc<TelegramInner>,
}

impl TelegramService {
    pub fn from_config(config: &Config, database: Database) -> Self {
        let http = config
            .telegram_token_file
            .as_deref()
            .and_then(read_private_token)
            .and_then(|token| ReqwestTelegramHttp::new(&config.telegram_api, token).ok())
            .map(|http| Arc::new(http) as Arc<dyn TelegramHttp>);
        Self::with_dependencies(database, http, Arc::new(HostClock), Arc::new(HostRandom))
    }

    pub fn with_dependencies(
        database: Database,
        http: Option<Arc<dyn TelegramHttp>>,
        clock: Arc<dyn Clock>,
        random: Arc<dyn RandomSource>,
    ) -> Self {
        Self {
            inner: Arc::new(TelegramInner {
                database,
                http,
                clock,
                random,
                update_offset: AtomicI64::new(0),
                status: Mutex::new(RuntimeStatus {
                    last_poll_at: None,
                    last_error: None,
                }),
                delivery_wake: Notify::new(),
            }),
        }
    }

    pub fn configured(&self) -> bool {
        self.inner.http.is_some()
    }

    pub fn enqueue(&self, chat_id: i64, text: &str) -> Result<(), ProtocolError> {
        let now = timestamp(self.inner.clock.now_utc())?;
        let bounded = text.chars().take(MESSAGE_MAX_CHARS).collect::<String>();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO telegram_outbox(chat_id,text,created_at,attempts,next_attempt_at) \
                     VALUES(?1,?2,?3,0,?3)",
                    rusqlite::params![chat_id, bounded, now],
                )?;
                let count =
                    transaction.query_row("SELECT count(*) FROM telegram_outbox", [], |row| {
                        row.get::<_, u32>(0)
                    })?;
                if count > OUTBOX_MAX_ROWS {
                    transaction.execute(
                        "DELETE FROM telegram_outbox WHERE message_id IN (\
                           SELECT message_id FROM telegram_outbox ORDER BY message_id LIMIT ?1\
                         )",
                        [count - OUTBOX_MAX_ROWS],
                    )?;
                }
                Ok(())
            })
            .map_err(database_error)?;
        self.inner.delivery_wake.notify_one();
        Ok(())
    }

    pub fn issue_link_code(&self, chat_id: i64, label: &str) -> Result<String, ProtocolError> {
        let mut random = [0u8; 3];
        self.inner.random.fill(&mut random).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "Telegram link code generation failed.",
            )
        })?;
        let code = upper_hex(&random);
        let now_value = self.inner.clock.now_utc();
        let now = timestamp(now_value)?;
        let expires = timestamp(now_value + Duration::seconds(LINK_CODE_TTL_SECONDS))?;
        let label = label.chars().take(LABEL_MAX_CHARS).collect::<String>();
        let stored_code = code.clone();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "DELETE FROM telegram_links WHERE chat_id=?1 OR expires_at<?2",
                    rusqlite::params![chat_id, now],
                )?;
                transaction.execute(
                    "INSERT INTO telegram_links(code,chat_id,label,created_at,expires_at) \
                     VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![stored_code, chat_id, label, now, expires],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        Ok(code)
    }

    pub fn link(&self, code: &str, email: &str) -> Result<TelegramLinked, ProtocolError> {
        if email.is_empty()
            || email.len() > 320
            || !email.contains('@')
            || email.chars().any(char::is_control)
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "'email' is required",
            ));
        }
        let code = code.to_uppercase();
        let email = email.to_lowercase();
        let now = timestamp(self.inner.clock.now_utc())?;
        let transaction_email = email.clone();
        let row = self
            .inner
            .database
            .transaction(move |transaction| {
                let link = transaction
                    .query_row(
                        "SELECT chat_id,label FROM telegram_links \
                         WHERE code=?1 AND expires_at>=?2",
                        rusqlite::params![code, now],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .optional()?;
                let Some((chat_id, label)) = link else {
                    return Err(DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "unknown or expired link code",
                    )));
                };
                let consumed =
                    transaction.execute("DELETE FROM telegram_links WHERE code=?1", [&code])?;
                if consumed != 1 {
                    return Err(DatabaseError::Domain(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "unknown or expired link code",
                    )));
                }
                transaction.execute(
                    "INSERT OR REPLACE INTO telegram_chats(chat_id,email,label,linked_at) \
                     VALUES(?1,?2,?3,?4)",
                    rusqlite::params![chat_id, transaction_email, label, now],
                )?;
                Ok(chat_id)
            })
            .map_err(database_error)?;
        self.enqueue(row, &format!("DevCoordinator2: linked to {email}."))?;
        Ok(TelegramLinked {
            chat_id: row,
            email,
        })
    }

    pub fn subscribe(
        &self,
        params: TelegramSubscriptionParams,
    ) -> Result<TelegramSubscriptionResult, ProtocolError> {
        let scope = parse_scope(&params.scope)?.as_text();
        let chat_id = params.chat_id;
        let exists = self
            .inner
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT 1 FROM telegram_chats WHERE chat_id=?1",
                        [chat_id],
                        |_| Ok(()),
                    )
                    .optional()
                    .map(|value| value.is_some())
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        if !exists {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "chat is not linked",
            ));
        }
        let now = timestamp(self.inner.clock.now_utc())?;
        let stored_scope = scope.clone();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT OR IGNORE INTO telegram_subscriptions(chat_id,scope,created_at) \
                     VALUES(?1,?2,?3)",
                    rusqlite::params![chat_id, stored_scope, now],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        Ok(TelegramSubscriptionResult {
            chat_id,
            scope,
            removed: None,
        })
    }

    pub fn unsubscribe(
        &self,
        params: TelegramSubscriptionParams,
    ) -> Result<TelegramSubscriptionResult, ProtocolError> {
        let chat_id = params.chat_id;
        let scope = params.scope;
        let transaction_scope = scope.clone();
        let removed = self
            .inner
            .database
            .transaction(move |transaction| {
                Ok(transaction.execute(
                    "DELETE FROM telegram_subscriptions WHERE chat_id=?1 AND scope=?2",
                    rusqlite::params![chat_id, transaction_scope],
                )? > 0)
            })
            .map_err(database_error)?;
        Ok(TelegramSubscriptionResult {
            chat_id,
            scope,
            removed: Some(removed),
        })
    }

    pub fn list(&self, email: Option<&str>) -> Result<TelegramList, ProtocolError> {
        let email = email.map(str::to_owned);
        let (mut chats, subscriptions, outbox_pending) = self
            .inner
            .database
            .call(move |connection| {
                let chats = {
                    let mut statement = if email.is_some() {
                        connection.prepare(
                            "SELECT chat_id,email,label,linked_at FROM telegram_chats \
                             WHERE email=?1 ORDER BY email,chat_id",
                        )?
                    } else {
                        connection.prepare(
                            "SELECT chat_id,email,label,linked_at FROM telegram_chats \
                             ORDER BY email,chat_id",
                        )?
                    };
                    let rows = if let Some(email) = email.as_deref() {
                        statement.query_map([email], telegram_chat_row)?
                    } else {
                        statement.query_map([], telegram_chat_row)?
                    };
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                let subscriptions = {
                    let mut statement = connection.prepare(
                        "SELECT chat_id,scope FROM telegram_subscriptions ORDER BY scope",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let outbox =
                    connection.query_row("SELECT count(*) FROM telegram_outbox", [], |row| {
                        row.get::<_, u32>(0)
                    })?;
                Ok((chats, subscriptions, outbox))
            })
            .map_err(database_error)?;
        for chat in &mut chats {
            chat.subscriptions = subscriptions
                .iter()
                .filter(|(chat_id, _)| *chat_id == chat.chat_id)
                .map(|(_, scope)| scope.clone())
                .collect();
        }
        let status = self
            .inner
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(TelegramList {
            configured: self.configured(),
            chats,
            outbox_pending,
            last_poll_at: status.last_poll_at.clone(),
            last_error: status.last_error.clone(),
        })
    }

    pub fn chat_email(&self, chat_id: i64) -> Result<Option<String>, ProtocolError> {
        self.inner
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT email FROM telegram_chats WHERE chat_id=?1",
                        [chat_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    pub async fn poll_once(&self) -> Result<u32, ProtocolError> {
        let Some(http) = self.inner.http.as_ref() else {
            return Ok(0);
        };
        let offset = self.inner.update_offset.load(Ordering::Acquire);
        let updates = http
            .call(
                TelegramMethod::GetUpdates,
                json!({
                    "timeout": 25,
                    "offset": offset,
                    "allowed_updates": ["message"],
                }),
                POLL_TIMEOUT,
            )
            .await
            .map_err(|_| telegram_unavailable("Telegram polling is unavailable."))?;
        let polled_at = timestamp(self.inner.clock.now_utc())?;
        self.inner
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_poll_at = Some(polled_at);
        let updates = updates
            .as_array()
            .ok_or_else(|| telegram_unavailable("Telegram polling returned an invalid result."))?;
        let mut handled = 0u32;
        for update in updates {
            let Some(update) = update.as_object() else {
                continue;
            };
            let update_id = update
                .get("update_id")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            self.inner
                .update_offset
                .fetch_max(update_id.saturating_add(1), Ordering::AcqRel);
            let Some(message) = update.get("message").and_then(Value::as_object) else {
                continue;
            };
            let Some(chat) = message.get("chat").and_then(Value::as_object) else {
                continue;
            };
            let chat_id = chat.get("id").and_then(Value::as_i64).unwrap_or_default();
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if chat_id == 0 || text.is_empty() {
                continue;
            }
            let label = ["first_name", "username"]
                .iter()
                .filter_map(|name| chat.get(*name).and_then(Value::as_str))
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            self.handle_message(chat_id, text, &label)?;
            handled = handled.saturating_add(1);
        }
        Ok(handled)
    }

    fn handle_message(&self, chat_id: i64, text: &str, label: &str) -> Result<(), ProtocolError> {
        if text.starts_with("/start") {
            let code = self.issue_link_code(chat_id, label)?;
            self.enqueue(
                chat_id,
                &format!(
                    "DevCoordinator2: link code {code} (valid 15 minutes). An administrator links it to your account."
                ),
            )
        } else if text.starts_with("/stop") {
            self.inner
                .database
                .transaction(move |transaction| {
                    transaction.execute(
                        "DELETE FROM telegram_subscriptions WHERE chat_id=?1",
                        [chat_id],
                    )?;
                    transaction
                        .execute("DELETE FROM telegram_chats WHERE chat_id=?1", [chat_id])?;
                    Ok(())
                })
                .map_err(database_error)?;
            self.enqueue(chat_id, "DevCoordinator2: unlinked; no more notifications.")
        } else {
            self.enqueue(
                chat_id,
                "DevCoordinator2 notifies only. /start to link, /stop to unlink.",
            )
        }
    }

    pub async fn deliver_once(&self) -> Result<u32, ProtocolError> {
        let now_value = self.inner.clock.now_utc();
        let now = timestamp(now_value)?;
        let cutoff = timestamp(now_value - Duration::seconds(OUTBOX_MAX_AGE_SECONDS))?;
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "DELETE FROM telegram_outbox WHERE created_at<?1 OR attempts>=?2",
                    rusqlite::params![cutoff, OUTBOX_MAX_ATTEMPTS],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        let rows = self
            .inner
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT message_id,chat_id,text,attempts FROM telegram_outbox \
                     WHERE next_attempt_at<=?1 ORDER BY message_id LIMIT ?2",
                )?;
                Ok(statement
                    .query_map(rusqlite::params![now, OUTBOX_BATCH], |row| {
                        Ok(OutboxRow {
                            message_id: row.get(0)?,
                            chat_id: row.get(1)?,
                            text: row.get(2)?,
                            attempts: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        let Some(http) = self.inner.http.as_ref() else {
            return Ok(0);
        };
        let mut delivered = 0u32;
        for row in rows {
            let result = http
                .call(
                    TelegramMethod::SendMessage,
                    json!({"chat_id": row.chat_id, "text": row.text}),
                    SEND_TIMEOUT,
                )
                .await;
            if result.is_ok() {
                let message_id = row.message_id;
                self.inner
                    .database
                    .transaction(move |transaction| {
                        transaction.execute(
                            "DELETE FROM telegram_outbox WHERE message_id=?1",
                            [message_id],
                        )?;
                        Ok(())
                    })
                    .map_err(database_error)?;
                delivered = delivered.saturating_add(1);
            } else {
                let attempts = row.attempts.saturating_add(1);
                let shift = attempts.min(31);
                let delay = (1i64 << shift).min(300);
                let retry_at = timestamp(now_value + Duration::seconds(delay))?;
                let message_id = row.message_id;
                self.inner
                    .database
                    .transaction(move |transaction| {
                        transaction.execute(
                            "UPDATE telegram_outbox SET attempts=?1,next_attempt_at=?2,\
                             last_error='telegram request failed' WHERE message_id=?3",
                            rusqlite::params![attempts, retry_at, message_id],
                        )?;
                        Ok(())
                    })
                    .map_err(database_error)?;
            }
        }
        Ok(delivered)
    }

    pub fn enqueue_event(&self, event: &TelegramEvent) -> Result<u32, ProtocolError> {
        if !self.configured() {
            return Ok(0);
        }
        let routed = route_event(event);
        let Some(text) = routed.text else {
            return Ok(0);
        };
        if routed.scopes.is_empty() {
            return Ok(0);
        }
        let scopes = routed.scopes.into_iter().collect::<HashSet<_>>();
        let chats = self
            .inner
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT DISTINCT chat_id,scope FROM telegram_subscriptions ORDER BY chat_id",
                )?;
                let mut chats = Vec::new();
                for row in statement.query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (chat_id, scope) = row?;
                    if scopes.contains(&scope) && chats.last() != Some(&chat_id) {
                        chats.push(chat_id);
                    }
                }
                Ok(chats)
            })
            .map_err(database_error)?;
        for chat_id in &chats {
            self.enqueue(*chat_id, &text)?;
        }
        Ok(u32::try_from(chats.len()).unwrap_or(u32::MAX))
    }

    pub fn start(&self) -> TelegramRuntime {
        let (stop, receiver) = watch::channel(false);
        let mut tasks = Vec::new();
        if self.configured() {
            let poll_service = self.clone();
            let poll_receiver = receiver.clone();
            tasks.push(tokio::spawn(async move {
                poll_service.poll_loop(poll_receiver).await;
            }));
            let delivery_service = self.clone();
            tasks.push(tokio::spawn(async move {
                delivery_service.delivery_loop(receiver).await;
            }));
        }
        TelegramRuntime { stop, tasks }
    }

    async fn poll_loop(&self, mut stop: watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            let result = tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                    continue;
                }
                result = self.poll_once() => result,
            };
            if result.is_err() {
                self.record_runtime_error("telegram polling unavailable");
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                }
                () = tokio::time::sleep(LOOP_WAKE) => {}
            }
        }
    }

    async fn delivery_loop(&self, mut stop: watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break; }
                    continue;
                }
                () = self.inner.delivery_wake.notified() => {}
                () = tokio::time::sleep(LOOP_WAKE) => {}
            }
            if let Err(_error) = self.deliver_once().await {
                self.record_runtime_error("telegram delivery unavailable");
            }
        }
    }

    fn record_runtime_error(&self, message: &'static str) {
        self.inner
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_error = Some(message.into());
    }
}

pub struct TelegramRuntime {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl TelegramRuntime {
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for TelegramRuntime {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct OutboxRow {
    message_id: i64,
    chat_id: i64,
    text: String,
    attempts: u32,
}

pub fn route_event(event: &TelegramEvent) -> RoutedTelegramEvent {
    let mut scopes = Vec::new();
    if let Some(deployment) = truthy_text(event.fields.get("deployment_id")) {
        scopes.push(format!("deployment:{deployment}"));
    }
    if let Some(repository) = truthy_text(event.fields.get("repository_id")) {
        scopes.push(format!("repository:{repository}"));
    }
    let name = format!(
        "{}@{}",
        display_default(event.fields.get("name"), ""),
        display_default(event.fields.get("source"), "")
    )
    .trim_matches('@')
    .to_owned();
    let routed = match event.kind.as_str() {
        "deployment.applied" => Some(format!(
            "deployed {name} generation {}",
            display_default(event.fields.get("generation"), "None")
        )),
        "deployment.failed" => Some(format!(
            "deployment {name} FAILED: {}",
            display_default(event.fields.get("message"), "")
        )),
        "deployment.rolled_back" => Some(format!(
            "rolled back {name} to generation {}",
            display_default(event.fields.get("generation"), "None")
        )),
        "deployment.stop" | "deployment.start" | "deployment.restart" => {
            let action = event.kind.split('.').nth(1).unwrap_or_default();
            let component = truthy_text(event.fields.get("component"))
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            Some(format!(
                "{action} {name}{component}: now {}",
                display_default(event.fields.get("state"), "None")
            ))
        }
        "deployment.removed" => Some(format!(
            "removed {name} (data deleted: {})",
            display_default(event.fields.get("data_deleted"), "None")
        )),
        "component.failed" => Some(format!(
            "component {} failed: {}",
            display_default(event.fields.get("component"), "None"),
            display_default(event.fields.get("message"), "")
        )),
        "preview.expired" => Some(format!("preview {name} expired and was stopped")),
        "release.requested" => {
            scopes.insert(0, SCOPE_SERVER.into());
            Some(format!(
                "preview requested for {}: {}",
                display_default(event.fields.get("repository_name"), ""),
                display_default(event.fields.get("name"), "")
            ))
        }
        "release.delivered" => {
            scopes.insert(0, SCOPE_SERVER.into());
            let where_text = truthy_text(event.fields.get("url"))
                .or_else(|| {
                    truthy_text(event.fields.get("port")).map(|port| format!("server port {port}"))
                })
                .unwrap_or_else(|| "no route yet".into());
            let draft = if is_truthy(event.fields.get("dirty")) {
                " (work in progress)"
            } else {
                ""
            };
            Some(format!(
                "preview delivered{draft}: {} — {where_text}",
                display_default(event.fields.get("name"), "")
            ))
        }
        "test.finished" => {
            let status = display_default(event.fields.get("status"), "None");
            if matches!(
                status.as_str(),
                "failed" | "timed-out" | "superseded" | "interrupted"
            ) {
                Some(format!(
                    "test {} {status} (exit {})",
                    display_default(event.fields.get("test"), "None"),
                    display_default(event.fields.get("exit_code"), "None")
                ))
            } else {
                scopes.clear();
                None
            }
        }
        "test.cleanup_failed" => Some(format!(
            "test cleanup failed: {}",
            display_default(event.fields.get("message"), "")
        )),
        "alert.opened" | "alert.recovered" => {
            let subject_kind = display_default(event.fields.get("subject_kind"), "None");
            let subject_id = display_default(event.fields.get("subject_id"), "");
            scopes = if subject_kind == "component" && subject_id.contains('/') {
                vec![format!(
                    "deployment:{}",
                    subject_id.split('/').next().unwrap_or_default()
                )]
            } else {
                vec![SCOPE_SERVER.into()]
            };
            let prefix = if event.kind == "alert.opened" {
                "ALERT"
            } else {
                "recovered"
            };
            Some(format!(
                "{prefix} [{}]: {}",
                display_default(event.fields.get("alert_kind"), "None"),
                display_default(event.fields.get("message"), "")
            ))
        }
        "container.unmanaged_seen" | "container.orphaned_seen" => {
            scopes = vec![SCOPE_SERVER.into()];
            let class = event
                .kind
                .split('.')
                .nth(1)
                .unwrap_or_default()
                .replace("_seen", "");
            Some(format!(
                "new {class} container {} ({})",
                display_default(event.fields.get("name"), "None"),
                display_default(event.fields.get("image"), "None")
            ))
        }
        "coordinator.started" => {
            scopes = vec![SCOPE_SERVER.into()];
            Some("DevCoordinator2 daemon started".into())
        }
        "bug.opened" | "bug.closed" => {
            scopes.insert(0, SCOPE_SERVER.into());
            let action = event.kind.split('.').nth(1).unwrap_or_default();
            Some(format!(
                "bug {action}: [{}] {}",
                display_default(event.fields.get("component"), "None"),
                display_default(event.fields.get("summary"), "None")
            ))
        }
        "user.invited" | "user.removed" | "grant.set" | "grant.removed" => {
            scopes = vec![SCOPE_SERVER.into()];
            Some(format!(
                "{}: {}",
                event.kind,
                display_default(event.fields.get("email"), "None")
            ))
        }
        _ => {
            scopes.clear();
            None
        }
    };
    RoutedTelegramEvent {
        scopes,
        text: routed,
    }
}

fn display_default(value: Option<&Value>, absent: &str) -> String {
    match value {
        None => absent.into(),
        Some(Value::Null) => "None".into(),
        Some(Value::Bool(value)) => {
            if *value {
                "True".into()
            } else {
                "False".into()
            }
        }
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(value) => value.to_string(),
    }
}

fn truthy_text(value: Option<&Value>) -> Option<String> {
    is_truthy(value).then(|| display_default(value, ""))
}

fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn read_private_token(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let descriptor = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let before = fstat(&descriptor).ok()?;
    let effective_uid = rustix::process::geteuid().as_raw();
    if before.st_mode & libc::S_IFMT != libc::S_IFREG
        || before.st_size <= 0
        || before.st_size as u64 > TOKEN_MAX_BYTES
        || before.st_nlink != 1
        || before.st_mode & 0o777 != 0o600
        || (before.st_uid != 0 && before.st_uid != effective_uid)
    {
        return None;
    }
    let mut file = std::fs::File::from(descriptor);
    let mut bytes = Vec::with_capacity(before.st_size as usize);
    (&mut file)
        .take(TOKEN_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let after = file.metadata().ok()?;
    // rustix uses the native dev_t/nanosecond types; MetadataExt normalizes them.
    let before_device: u64 = before.st_dev as _;
    let before_modified_nanoseconds: u64 = before.st_mtime_nsec as _;
    let before_changed_nanoseconds: u64 = before.st_ctime_nsec as _;
    if after.dev() != before_device
        || after.ino() != before.st_ino
        || after.size() != before.st_size as u64
        || after.mtime() != before.st_mtime
        || u64::try_from(after.mtime_nsec()).ok() != Some(before_modified_nanoseconds)
        || after.ctime() != before.st_ctime
        || u64::try_from(after.ctime_nsec()).ok() != Some(before_changed_nanoseconds)
        || bytes.len() as u64 != before.st_size as u64
    {
        return None;
    }
    let token = std::str::from_utf8(&bytes).ok()?.trim();
    if token.is_empty()
        || token.chars().count() > TOKEN_MAX_CHARS
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return None;
    }
    Some(token.into())
}

fn telegram_chat_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TelegramChat> {
    Ok(TelegramChat {
        chat_id: row.get(0)?,
        email: row.get(1)?,
        label: row.get(2)?,
        linked_at: row.get(3)?,
        subscriptions: Vec::new(),
    })
}

fn timestamp(value: OffsetDateTime) -> Result<String, ProtocolError> {
    value.format(ISO_FORMAT).map_err(|_| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "Telegram timestamp generation failed.",
        )
    })
}

fn upper_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

fn telegram_unavailable(message: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, message)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        _ => ProtocolError::new(ErrorCode::InternalError, "Telegram state is unavailable."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;
    use time::macros::datetime;

    #[derive(Default)]
    struct FakeHttpState {
        updates: Vec<Value>,
        sent: Vec<Value>,
        fail_sends: u32,
        fail_polls: u32,
        poll_offsets: Vec<i64>,
    }

    #[derive(Default)]
    struct FakeHttp {
        state: Mutex<FakeHttpState>,
    }

    impl FakeHttp {
        fn push_update(&self, update: Value) {
            self.state
                .lock()
                .expect("fake HTTP state")
                .updates
                .push(update);
        }

        fn fail_sends(&self, count: u32) {
            self.state.lock().expect("fake HTTP state").fail_sends = count;
        }

        fn sent(&self) -> Vec<Value> {
            self.state.lock().expect("fake HTTP state").sent.clone()
        }
    }

    impl TelegramHttp for FakeHttp {
        fn call<'a>(
            &'a self,
            method: TelegramMethod,
            payload: Value,
            _timeout: StdDuration,
        ) -> TelegramHttpFuture<'a> {
            Box::pin(async move {
                let mut state = self.state.lock().expect("fake HTTP state");
                match method {
                    TelegramMethod::GetUpdates => {
                        if state.fail_polls > 0 {
                            state.fail_polls -= 1;
                            return Err(TelegramHttpError::Unavailable);
                        }
                        let offset = payload
                            .get("offset")
                            .and_then(Value::as_i64)
                            .unwrap_or_default();
                        state.poll_offsets.push(offset);
                        Ok(Value::Array(
                            state
                                .updates
                                .iter()
                                .filter(|update| {
                                    update
                                        .get("update_id")
                                        .and_then(Value::as_i64)
                                        .is_some_and(|id| id >= offset)
                                })
                                .cloned()
                                .collect(),
                        ))
                    }
                    TelegramMethod::SendMessage => {
                        if state.fail_sends > 0 {
                            state.fail_sends -= 1;
                            Err(TelegramHttpError::Rejected)
                        } else {
                            state.sent.push(payload);
                            Ok(json!({"message_id": state.sent.len()}))
                        }
                    }
                }
            })
        }
    }

    struct TestClock(Mutex<OffsetDateTime>);

    impl TestClock {
        fn new() -> Self {
            Self(Mutex::new(datetime!(2026-09-03 12:00 UTC)))
        }

        fn advance(&self, seconds: i64) {
            let mut now = self.0.lock().expect("test clock");
            *now += Duration::seconds(seconds);
        }
    }

    impl Clock for TestClock {
        fn now_utc(&self) -> OffsetDateTime {
            *self.0.lock().expect("test clock")
        }
    }

    struct TestRandom;

    impl RandomSource for TestRandom {
        fn fill(&self, destination: &mut [u8]) -> Result<(), getrandom::Error> {
            for (target, value) in destination.iter_mut().zip([0xab, 0xcd, 0xef].into_iter()) {
                *target = value;
            }
            Ok(())
        }
    }

    struct World {
        _temporary: TempDir,
        database: Database,
        clock: Arc<TestClock>,
        http: Arc<FakeHttp>,
        service: TelegramService,
    }

    impl World {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let database =
                Database::open(temporary.path().join("authority.sqlite3")).expect("database");
            let clock = Arc::new(TestClock::new());
            let http = Arc::new(FakeHttp::default());
            let service = TelegramService::with_dependencies(
                database.clone(),
                Some(http.clone()),
                clock.clone(),
                Arc::new(TestRandom),
            );
            Self {
                _temporary: temporary,
                database,
                clock,
                http,
                service,
            }
        }

        fn subscription(chat_id: i64, scope: &str) -> TelegramSubscriptionParams {
            TelegramSubscriptionParams {
                chat_id,
                scope: scope.into(),
            }
        }

        fn outbox_count(&self) -> u32 {
            self.database
                .call(|connection| {
                    Ok(connection
                        .query_row("SELECT count(*) FROM telegram_outbox", [], |row| row.get(0))?)
                })
                .expect("outbox count")
        }
    }

    #[test]
    fn private_token_requires_exact_regular_owner_and_mode() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let token = temporary.path().join("telegram.token");
        fs::write(&token, "123:SECRET-TOKEN\n").expect("token");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("token mode");
        assert_eq!(
            read_private_token(&token).as_deref(),
            Some("123:SECRET-TOKEN")
        );

        fs::set_permissions(&token, fs::Permissions::from_mode(0o640)).expect("unsafe mode");
        assert_eq!(read_private_token(&token), None);
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("restore mode");
        let alias = temporary.path().join("telegram-link.token");
        symlink(&token, &alias).expect("token symlink");
        assert_eq!(read_private_token(&alias), None);

        fs::write(&token, "123:bad/token\n").expect("unsafe token bytes");
        assert_eq!(read_private_token(&token), None);
    }

    #[tokio::test]
    async fn poll_link_subscribe_route_and_delivery_preserve_offset_and_dedup() {
        let world = World::new();
        world.http.push_update(json!({
            "update_id": 7,
            "message": {
                "chat": {"id": 4242, "first_name": "Dev"},
                "text": "/start"
            }
        }));
        assert_eq!(world.service.poll_once().await.expect("poll"), 1);
        assert_eq!(world.service.poll_once().await.expect("repeat poll"), 0);
        assert_eq!(world.service.deliver_once().await.expect("deliver"), 1);
        let sent = world.http.sent();
        let start_text = sent[0]["text"].as_str().expect("start response");
        assert!(start_text.contains("link code ABCDEF"));

        let linked = world
            .service
            .link("abcdef", "Dev@Example.Test")
            .expect("link");
        assert_eq!(linked.chat_id, 4242);
        assert_eq!(linked.email, "dev@example.test");
        let reused = world
            .service
            .link("ABCDEF", "dev@example.test")
            .expect_err("single use code");
        assert_eq!(reused.code, ErrorCode::ParamsInvalid);

        world
            .service
            .subscribe(World::subscription(4242, "deployment:d1"))
            .expect("subscribe");
        let routed = world
            .service
            .enqueue_event(
                &TelegramEvent::new("deployment.failed")
                    .with("deployment_id", "d1")
                    .with("name", "web")
                    .with("source", "worktree")
                    .with("message", "boom"),
            )
            .expect("event");
        assert_eq!(routed, 1);
        assert_eq!(
            world
                .service
                .enqueue_event(
                    &TelegramEvent::new("deployment.failed")
                        .with("deployment_id", "d2")
                        .with("name", "other")
                        .with("source", "worktree")
                        .with("message", "boom"),
                )
                .expect("unrelated event"),
            0
        );
        assert_eq!(
            world
                .service
                .enqueue_event(
                    &TelegramEvent::new("test.finished")
                        .with("status", "passed")
                        .with("repository_id", "r1")
                        .with("test", "unit"),
                )
                .expect("passing test"),
            0
        );
        assert_eq!(world.service.deliver_once().await.expect("deliver"), 2);
        let texts = world
            .http
            .sent()
            .into_iter()
            .filter_map(|message| message["text"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("linked to dev@example.test"))
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("deployment web@worktree FAILED: boom"))
        );
        assert!(!texts.iter().any(|text| text.contains("other")));
        assert!(!texts.iter().any(|text| text.contains("passed")));
        assert_eq!(
            world.http.state.lock().expect("HTTP state").poll_offsets,
            vec![0, 8]
        );
    }

    #[tokio::test]
    async fn outbox_retries_expires_and_drops_oldest_rows_with_safe_errors() {
        let world = World::new();
        world.http.fail_sends(1);
        world.service.enqueue(4242, "retry me").expect("enqueue");
        assert_eq!(world.service.deliver_once().await.expect("attempt"), 0);
        let row = world
            .database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT attempts,next_attempt_at,last_error FROM telegram_outbox",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, u32>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?)
            })
            .expect("outbox row");
        assert_eq!(row.0, 1);
        assert_eq!(row.2, "telegram request failed");
        assert!(!row.2.contains("retry me"));
        world.clock.advance(2);
        assert_eq!(world.service.deliver_once().await.expect("retry"), 1);

        for index in 0..(OUTBOX_MAX_ROWS + 5) {
            world
                .service
                .enqueue(1, &format!("message-{index}"))
                .expect("bounded enqueue");
        }
        assert_eq!(world.outbox_count(), OUTBOX_MAX_ROWS);
        world
            .database
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE telegram_outbox SET attempts=?1",
                    [OUTBOX_MAX_ATTEMPTS],
                )?;
                Ok(())
            })
            .expect("exhaust attempts");
        assert_eq!(world.service.deliver_once().await.expect("cleanup"), 0);
        assert_eq!(world.outbox_count(), 0);

        world.service.enqueue(1, "old").expect("old enqueue");
        world
            .database
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE telegram_outbox SET created_at='2020-01-01T00:00:00Z'",
                    [],
                )?;
                Ok(())
            })
            .expect("age row");
        world.service.deliver_once().await.expect("age cleanup");
        assert_eq!(world.outbox_count(), 0);
    }

    #[test]
    fn expiring_codes_subscriptions_listing_and_chat_identity_are_typed() {
        let world = World::new();
        let code = world
            .service
            .issue_link_code(7, "A label that is retained")
            .expect("code");
        world.clock.advance(LINK_CODE_TTL_SECONDS + 1);
        assert_eq!(
            world
                .service
                .link(&code, "person@example.test")
                .expect_err("expired code")
                .code,
            ErrorCode::ParamsInvalid
        );

        let code = world
            .service
            .issue_link_code(7, "Person")
            .expect("new code");
        world
            .service
            .link(&code, "person@example.test")
            .expect("link");
        assert_eq!(
            world.service.chat_email(7).expect("chat email").as_deref(),
            Some("person@example.test")
        );
        assert!(
            world
                .service
                .subscribe(World::subscription(7, "repository:r1"))
                .is_ok()
        );
        assert_eq!(
            world
                .service
                .subscribe(World::subscription(99, SCOPE_SERVER))
                .expect_err("unlinked chat")
                .code,
            ErrorCode::ParamsInvalid
        );
        assert_eq!(
            world
                .service
                .subscribe(World::subscription(7, "weird"))
                .expect_err("invalid scope")
                .code,
            ErrorCode::ParamsInvalid
        );
        let listing = world
            .service
            .list(Some("person@example.test"))
            .expect("listing");
        assert!(listing.configured);
        assert_eq!(listing.chats.len(), 1);
        assert_eq!(listing.chats[0].subscriptions, vec!["repository:r1"]);
        assert!(
            !serde_json::to_string(&listing)
                .expect("listing JSON")
                .contains("SECRET")
        );
        assert_eq!(
            world
                .service
                .unsubscribe(World::subscription(7, "repository:r1"))
                .expect("unsubscribe")
                .removed,
            Some(true)
        );
        assert_eq!(
            world
                .service
                .unsubscribe(World::subscription(7, "repository:r1"))
                .expect("idempotent unsubscribe")
                .removed,
            Some(false)
        );
    }

    #[test]
    fn route_table_covers_all_established_event_classes() {
        let cases = [
            TelegramEvent::new("deployment.applied")
                .with("deployment_id", "d1")
                .with("name", "web")
                .with("source", "worktree")
                .with("generation", 3),
            TelegramEvent::new("deployment.rolled_back")
                .with("deployment_id", "d1")
                .with("generation", 2),
            TelegramEvent::new("component.failed")
                .with("deployment_id", "d1")
                .with("component", "api")
                .with("message", "boom"),
            TelegramEvent::new("preview.expired")
                .with("deployment_id", "d1")
                .with("name", "p"),
            TelegramEvent::new("release.requested").with("repository_name", "repo"),
            TelegramEvent::new("release.delivered")
                .with("name", "preview")
                .with("port", 20000),
            TelegramEvent::new("test.finished")
                .with("status", "timed-out")
                .with("repository_id", "r1")
                .with("test", "unit"),
            TelegramEvent::new("test.cleanup_failed")
                .with("repository_id", "r1")
                .with("message", "cleanup"),
            TelegramEvent::new("alert.opened")
                .with("subject_kind", "component")
                .with("subject_id", "d1/api")
                .with("alert_kind", "x")
                .with("message", "m"),
            TelegramEvent::new("alert.recovered")
                .with("subject_kind", "host")
                .with("alert_kind", "host_cpu"),
            TelegramEvent::new("container.unmanaged_seen")
                .with("name", "n")
                .with("image", "i"),
            TelegramEvent::new("coordinator.started"),
            TelegramEvent::new("bug.opened")
                .with("component", "c")
                .with("summary", "s"),
            TelegramEvent::new("user.invited").with("email", "u@example.test"),
        ];
        for event in cases {
            let routed = route_event(&event);
            assert!(!routed.scopes.is_empty(), "{}", event.kind);
            assert!(routed.text.is_some(), "{}", event.kind);
        }
        assert_eq!(
            route_event(
                &TelegramEvent::new("alert.opened")
                    .with("subject_kind", "component")
                    .with("subject_id", "d1/api")
            )
            .scopes,
            vec!["deployment:d1"]
        );
        assert_eq!(
            route_event(&TelegramEvent::new("test.finished").with("status", "passed")),
            RoutedTelegramEvent {
                scopes: Vec::new(),
                text: None,
            }
        );
        assert_eq!(
            parse_scope("deployment:d1").expect("deployment scope"),
            TelegramScope::Deployment("d1".into())
        );
    }

    #[tokio::test]
    async fn asynchronous_lifecycle_delivers_and_shuts_down_cleanly() {
        let world = World::new();
        let runtime = world.service.start();
        world.service.enqueue(88, "lifecycle").expect("enqueue");
        tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                if world
                    .http
                    .sent()
                    .iter()
                    .any(|message| message["text"] == "lifecycle")
                {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("delivery deadline");
        tokio::time::timeout(StdDuration::from_secs(1), runtime.shutdown())
            .await
            .expect("clean shutdown");
        assert!(
            world
                .service
                .list(None)
                .expect("status")
                .last_poll_at
                .is_some()
        );
    }
}
