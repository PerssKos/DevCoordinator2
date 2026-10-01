-- Authority schema 29: public ticket content and private federation credentials.
CREATE TABLE IF NOT EXISTS ticket_settings (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  upstream TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>0)
);
INSERT OR IGNORE INTO ticket_settings(singleton,upstream,revision)
  VALUES(1,'https://vr.ae',1);
CREATE TABLE IF NOT EXISTS feature_tickets (
  ticket_id TEXT PRIMARY KEY,
  upstream TEXT NOT NULL,
  origin TEXT NOT NULL,
  author_name TEXT NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1)),
  deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN (0,1)),
  revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0),
  request_key TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(origin,request_key)
);
CREATE INDEX IF NOT EXISTS feature_tickets_inbox
  ON feature_tickets(deleted,updated_at DESC,ticket_id DESC);
CREATE INDEX IF NOT EXISTS feature_tickets_origin
  ON feature_tickets(origin,deleted,updated_at DESC,ticket_id DESC);
CREATE TABLE IF NOT EXISTS ticket_comments (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  comment_id TEXT NOT NULL UNIQUE,
  ticket_id TEXT NOT NULL REFERENCES feature_tickets(ticket_id),
  server TEXT NOT NULL,
  author_name TEXT NOT NULL,
  body TEXT NOT NULL,
  request_key TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(ticket_id,server,request_key),
  UNIQUE(ticket_id,comment_id)
);
CREATE INDEX IF NOT EXISTS ticket_comments_thread ON ticket_comments(ticket_id,sequence);
CREATE TABLE IF NOT EXISTS ticket_attachments (
  attachment_id TEXT PRIMARY KEY,
  ticket_id TEXT NOT NULL REFERENCES feature_tickets(ticket_id),
  comment_id TEXT,
  position INTEGER NOT NULL,
  name TEXT NOT NULL,
  content_type TEXT NOT NULL,
  byte_size INTEGER NOT NULL CHECK(byte_size>0 AND byte_size<=16777216),
  sha256 TEXT NOT NULL,
  content BLOB NOT NULL CHECK(length(content)=byte_size),
  FOREIGN KEY(ticket_id,comment_id) REFERENCES ticket_comments(ticket_id,comment_id)
);
CREATE INDEX IF NOT EXISTS ticket_attachments_message
  ON ticket_attachments(ticket_id,comment_id,position);
CREATE TABLE IF NOT EXISTS ticket_history (
  ticket_id TEXT NOT NULL REFERENCES feature_tickets(ticket_id),
  revision INTEGER NOT NULL CHECK(revision>0),
  action TEXT NOT NULL CHECK(action IN ('created','edited','closed','reopened','removed','commented')),
  server TEXT NOT NULL,
  author_name TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  PRIMARY KEY(ticket_id,revision)
);
CREATE TABLE IF NOT EXISTS ticket_peers (
  upstream TEXT PRIMARY KEY,
  credential TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ticket_uploads (
  upload_id TEXT PRIMARY KEY,
  owner TEXT NOT NULL,
  request_key TEXT NOT NULL,
  name TEXT NOT NULL,
  byte_size INTEGER NOT NULL CHECK(byte_size>0 AND byte_size<=16777216),
  sha256 TEXT NOT NULL,
  content BLOB NOT NULL,
  expires_at INTEGER NOT NULL,
  UNIQUE(owner,request_key)
);
