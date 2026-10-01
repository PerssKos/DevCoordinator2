# Public feature requests

The new ticket feature currently supplies its English source catalog. Other
Console languages use the established English fallback for ticket-specific text;
their existing translated pages and the user's selected language are preserved.
The manifest reports this feature as source-only, not as translated coverage.

Open **Feature requests** in the Console menu. **Submitted** lists requests from
this installation at its selected upstream; **Received** lists requests owned by
this server. Every server can act as an upstream. Public readers can open
`/requests` on the Console host or the instance base domain without signing in.

The upstream setting defaults to `https://vr.ae`. Console administrators can
change it to another HTTPS server, or enter `local` to keep new requests on this
server. HTTP loopback origins are accepted for isolated development. Changing
the setting does not transfer existing tickets. The settings dialog retains
previous destinations so their requests remain accessible.

Tickets, comments and their attachments are public. Creating and changing them
requires either an admitted user of the originating Console, its local agent,
or an upstream administrator. A remote server proves its origin with a random
credential generated automatically and stored in its private authority database.
Each upstream receives a different credential. Server display names are labels,
not proof of domain ownership, and no local account or Console email is published.

The originating server can create, edit, remove, close, reopen and discuss its
requests. Upstream administrators can edit, remove, close, reopen and discuss
received requests. Public readers cannot change tickets. Removal hides the
ticket and its files while keeping its history and retry tombstone in the
owning authority database. Revision checks reject stale edits.

The initial request and **every comment** accept multiple attachments. A message
may contain up to 16 files, 16 MiB per file and 32 MiB total. Files are uploaded in
bounded, resumable chunks and verified by SHA-256 before the message is saved.
A comment and all its files commit together. Incomplete uploads remain private,
expire after 24 hours, and may be removed before posting. At most 64 pending
uploads and 128 MiB of declared upload data are retained per originating identity.

Images, PDF and plain-text files open in the attachment viewer. PDFs use a
locally retained Mozilla PDF.js 6.3.289 renderer with page navigation and
accessible text. The viewer renders pages, disables form annotations and XFA,
and does not load the scripting sandbox or execute document actions. No files
are sent to an external preview provider. DOCX and ODT
documents have a local text preview; the viewer never loads their external
relationships or executes their content. Other file formats are available as
original downloads. A download link is always available in a supported preview.

## Agents and transport

The existing `devcoordinator2 mcp` server exposes:

- `ticket_settings`: read the configured and previous upstreams.
- `ticket_configure`: change the upstream with its current revision.
- `ticket_request`: use the typed `action` variants `list`, `get`, `comments`,
  `create`, `edit`, `remove`, `close`, `comment`, `upload_start`, `upload_chunk`,
  `upload_remove`, and `file`.

Omit `target` to use the configured upstream. Use `target: "local"` for the
received inbox. A saved prior upstream can be addressed explicitly. Preserve
`request_key` when retrying a create, comment or upload start. `get` reports
the current revision and the caller's management/discussion authority.

Upload start takes the filename, byte size, SHA-256 and a request key. Upload
chunks contain at most 32 KiB decoded base64 and the expected byte offset. The
returned upload ID goes in a request or comment's `attachments` array. `file`
returns bounded base64 chunks with their exact ticket/comment association and
digest; follow `next_offset` until null. Credentials are never returned to agents.

Remote installations use POST `/.well-known/devcoordinator2/tickets` on the
upstream base or Console host. This endpoint accepts only the ticket action
contract. It does not forward arbitrary daemon operations, expose settings or
inherit the edge's local administrator authority. Anonymous reads omit the
credential. Redirects are refused, and network errors leave the local draft
available for retry.

## Verification

`console/verify-tickets.mjs` extends the existing isolated Console/control-plane
fixture into a two-server journey. It drives real HTTP edges, SQLite persistence,
the rendered Console, and actual MCP clients. The storage suite additionally
checks transactional file failures, retry identities, exact comment/file
association, database reopening and bounded discussion pages. Neither fixture
uses the installed daemon or live ticket data.

The pinned renderer comes from the official `pdfjs-dist` package under
Apache-2.0. `console/vendor/pdfjs/source.json` records every retained file's
digest; `scripts/vendor-pdfjs.mjs` reproduces the copies from the unpacked
versioned package. See https://github.com/mozilla/pdf.js/releases/tag/v6.3.289.
