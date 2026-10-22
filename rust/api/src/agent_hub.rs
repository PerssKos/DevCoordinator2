//! Typed API for the durable Agent communication hub.
//!
//! The hub is deliberately separate from tickets and the internal agent
//! reminder queue.  Profiles and messages are opaque, stable identities; no
//! Console identity or local path is part of this contract.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn page_size() -> u16 {
    50
}
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Space {
    Private,
    Public,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Active,
    Expired,
    Unregistered,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub agent_id: String,
    pub handle: String,
    pub space: Space,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_subject: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub state: AgentState,
    pub lease_expires_at: String,
    pub callback_configured: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub space: Space,
    #[schemars(length(min = 1, max = 160))]
    pub display_name: String,
    #[serde(default)]
    #[schemars(length(max = 160))]
    pub repository_label: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 160))]
    pub domain_label: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 160))]
    pub current_subject: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub callback_url: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterResult {
    pub agent: AgentProfile,
    /// Returned only on registration or key rotation; never persisted in a
    /// response, profile, history or callback payload.
    pub token: String,
    pub callback_secret: Option<String>,
    pub lease_seconds: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAuth {
    pub agent_id: String,
    pub token: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub auth: AgentAuth,
    #[serde(default)]
    #[schemars(length(max = 160))]
    pub current_subject: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unregister {
    pub auth: AgentAuth,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub space: Space,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default = "page_size")]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryResult {
    pub agents: Vec<AgentProfile>,
    pub next: Option<String>,
    pub public_enabled: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomCreate {
    pub auth: AgentAuth,
    pub space: Space,
    #[schemars(length(min = 1, max = 160))]
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomList {
    pub auth: AgentAuth,
    #[serde(default)]
    pub space: Option<Space>,
    #[serde(default)]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomMemberChange {
    pub auth: AgentAuth,
    pub room_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    pub room_id: String,
    pub space: Space,
    pub name: String,
    pub created_at: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomListResult {
    pub rooms: Vec<Room>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Send {
    pub auth: AgentAuth,
    pub room_id: String,
    #[schemars(length(min = 1, max = 32768))]
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 160))]
    pub client_message_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub attachment_id: String,
    pub name: String,
    pub content_type: String,
    pub byte_size: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub message_id: String,
    pub room_id: String,
    pub sender: AgentProfile,
    pub body: String,
    pub attachments: Vec<Attachment>,
    pub created_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receive {
    pub auth: AgentAuth,
    #[serde(default)]
    pub room_id: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default = "page_size")]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub delivery_id: String,
    pub message: Message,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveResult {
    pub deliveries: Vec<Delivery>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub auth: AgentAuth,
    pub delivery_ids: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStart {
    pub auth: AgentAuth,
    pub request_key: String,
    #[schemars(length(min = 1, max = 240))]
    pub name: String,
    #[serde(default = "default_content_type")]
    pub content_type: String,
    pub byte_size: u32,
    #[schemars(regex(pattern = "^[0-9a-fA-F]{64}$"))]
    pub sha256: String,
}
fn default_content_type() -> String {
    "application/octet-stream".to_owned()
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStartResult {
    pub upload_id: String,
    pub attachment: Attachment,
    pub next_offset: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadChunk {
    pub auth: AgentAuth,
    pub upload_id: String,
    pub offset: u32,
    #[schemars(length(min = 1, max = 45000))]
    pub data_base64: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadChunkResult {
    pub upload_id: String,
    pub next_offset: u32,
    pub complete: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadFile {
    pub auth: AgentAuth,
    #[serde(default)]
    pub upload_id: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub offset: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChunk {
    pub attachment: Attachment,
    pub offset: u64,
    pub next_offset: Option<u64>,
    pub data_base64: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRemove {
    pub auth: AgentAuth,
    pub upload_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub revision: u64,
    pub public_enabled: bool,
    pub public_paused: bool,
    pub current_key_created_at: Option<String>,
    pub previous_key_expires_at: Option<String>,
    pub callback_outbox_pending: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSet {
    pub expected_revision: u64,
    pub public_enabled: bool,
    pub public_paused: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRotate {
    pub expected_revision: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRotateResult {
    pub revision: u64,
    pub bearer_key: String,
    pub previous_key_expires_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleDirectory {
    pub space: Space,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default = "page_size")]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleReceive {
    pub space: Space,
    #[serde(default)]
    pub room_id: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default = "page_size")]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleSend {
    pub space: Space,
    pub room_id: String,
    #[schemars(length(min = 1, max = 32768))]
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRemove {
    pub agent_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomRemove {
    pub room_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageRemove {
    pub message_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovalResult {
    pub removed: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    #[serde(default)]
    pub space: Option<Space>,
    #[serde(default = "page_size")]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityResult {
    pub active_agents: u32,
    pub rooms: u32,
    pub messages: u32,
    pub deliveries_pending: u32,
    pub outbox_pending: u32,
}

/// Dedicated edge adapter.  Public reads/registration and public-space
/// writes may omit `bearer`; private actions require the current or previous
/// instance key.  This operation is intentionally the only hub operation
/// available to the public edge endpoint.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRequest {
    #[serde(default)]
    pub bearer: Option<String>,
    pub action: RemoteAction,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteAction {
    Directory(Directory),
    Register(Register),
    Heartbeat(Heartbeat),
    Unregister(Unregister),
    RoomCreate(RoomCreate),
    RoomList(RoomList),
    RoomMemberAdd(RoomMemberChange),
    RoomMemberRemove(RoomMemberChange),
    Send(Send),
    Receive(Receive),
    Ack(Ack),
    UploadStart(UploadStart),
    UploadChunk(UploadChunk),
    UploadFile(UploadFile),
    UploadRemove(UploadRemove),
}
