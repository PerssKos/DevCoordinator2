//! Public ticket workflow shared by Console, federation and MCP adapters.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Omit for configured upstream; use "local" to manage received tickets.
    pub target: Option<String>,
    pub action: Action,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteRequest {
    pub action: Action,
    /// Transport credential, never returned or logged. Omit for public reads.
    pub credential: Option<String>,
    pub server_label: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    List {
        #[serde(default)]
        offset: u32,
        #[serde(default = "page_size")]
        limit: u32,
        closed: Option<bool>,
        #[serde(default)]
        mine: bool,
    },
    Get {
        ticket_id: String,
    },
    Comments {
        ticket_id: String,
        #[serde(default)]
        offset: u32,
        #[serde(default = "page_size")]
        limit: u32,
    },
    Create {
        request_key: String,
        title: String,
        body: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    Edit {
        ticket_id: String,
        expected_revision: u64,
        title: String,
        body: String,
    },
    Remove {
        ticket_id: String,
        expected_revision: u64,
    },
    Close {
        ticket_id: String,
        expected_revision: u64,
        closed: bool,
    },
    Comment {
        ticket_id: String,
        request_key: String,
        body: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    UploadStart {
        request_key: String,
        name: String,
        byte_size: u32,
        sha256: String,
    },
    UploadChunk {
        upload_id: String,
        offset: u32,
        data_base64: String,
    },
    UploadRemove {
        upload_id: String,
    },
    File {
        ticket_id: String,
        comment_id: Option<String>,
        file_id: String,
        #[serde(default)]
        offset: u64,
    },
}

impl Action {
    pub fn is_read(&self) -> bool {
        matches!(
            self,
            Self::List { .. } | Self::Get { .. } | Self::Comments { .. } | Self::File { .. }
        )
    }
}
fn page_size() -> u32 {
    20
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configure {
    pub upstream: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Settings {
    pub upstream: String,
    pub revision: u64,
    pub local: String,
    pub previous_upstreams: Vec<String>,
    pub can_configure: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Author {
    pub server: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ticket {
    #[serde(flatten)]
    pub summary: TicketSummary,
    pub author: Author,
    pub body: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Comment {
    pub id: String,
    pub ticket_id: String,
    pub author: Author,
    pub body: String,
    pub created_at: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Tickets {
        items: Vec<TicketSummary>,
        next_offset: Option<u32>,
    },
    Ticket {
        ticket: Ticket,
        can_manage: bool,
        can_comment: bool,
    },
    Comments {
        items: Vec<Comment>,
        next_offset: Option<u32>,
    },
    Comment {
        comment: Comment,
    },
    Upload {
        upload_id: String,
        offset: u32,
    },
    File {
        attachment: Attachment,
        offset: u64,
        data_base64: String,
        next_offset: Option<u64>,
    },
    Removed {
        removed: bool,
    },
}
