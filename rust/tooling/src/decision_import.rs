//! Import the established `DecisionHistory.md` format through protocol v2.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use devcoordinator2_api::{
    ClientContext, ClientKind, ErrorCode, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
    RequestEnvelope, ResponseEnvelope,
};
use regex::Regex;
use rustix::fs::{Mode, OFlags, open as unix_open};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BODY_MAX: usize = 4_000;
pub const TITLE_MAX: usize = 120;

const ASPECT_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "ui",
        &[
            "console", "button", "page", "view", "ux", "pop-up", "dialog",
        ],
    ),
    (
        "deployment",
        &[
            "deploy",
            "edge",
            "route",
            "domain",
            "proxy",
            "canary",
            "cutover",
            "install",
            "docker",
            "container",
        ],
    ),
    (
        "data",
        &[
            "database", "schema", "sqlite", "table", "ledger", "import", "export", "storage",
        ],
    ),
    (
        "security",
        &[
            "access",
            "permission",
            "grant",
            "secret",
            "auth",
            "socket",
            "trust",
            "acl",
        ],
    ),
    (
        "testing",
        &["test", "verification", "playwright", "acceptance"],
    ),
    (
        "process",
        &["workflow", "phase", "owner decision", "handover", "skill"],
    ),
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionEntry {
    pub r#ref: String,
    pub title: String,
    pub aspect: String,
    pub body: String,
    pub technical_note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub repository: PathBuf,
    pub source: PathBuf,
    pub socket: PathBuf,
    pub dry_run: bool,
    pub aspect_overrides: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportCounts {
    pub imported: u32,
    pub skipped: u32,
    pub failed: u32,
}

pub fn guess_aspect(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut best = "architecture";
    let mut hits = 0usize;
    for (aspect, words) in ASPECT_KEYWORDS {
        let count = words.iter().map(|word| lowered.matches(word).count()).sum();
        if count > hits {
            best = aspect;
            hits = count;
        }
    }
    best.to_owned()
}

pub fn parse(text: &str) -> Vec<DecisionEntry> {
    let section = Regex::new(r"(?m)^## +").expect("constant section regex");
    let heading = Regex::new(r"^(\S+) +— +(.+)").expect("constant heading regex");
    section
        .split(text)
        .skip(1)
        .filter_map(|section| {
            let (head, rest) = section.split_once('\n').unwrap_or((section, ""));
            let captures = heading.captures(head.trim())?;
            let reference = captures.get(1)?.as_str().to_owned();
            let mut title = captures.get(2)?.as_str().trim().to_owned();
            if title.chars().count() > TITLE_MAX {
                title = format!("{}…", truncate_chars(&title, TITLE_MAX - 1).trim_end());
            }
            let mut body = plain(rest);
            let mut technical_note = None;
            if body.chars().count() > BODY_MAX {
                let suffix = "\n\n(continued in the technical note)";
                let maximum = BODY_MAX - suffix.chars().count();
                let prefix = truncate_chars(&body, maximum);
                let cut = prefix
                    .rfind("\n\n")
                    .map(|byte| prefix[..byte].chars().count())
                    .filter(|cut| *cut > 0)
                    .unwrap_or(maximum);
                let split = byte_after_chars(&body, cut);
                let overflow = body[split..].trim().to_owned();
                body = format!("{}{}", body[..split].trim_end(), suffix);
                technical_note = Some(overflow);
            }
            Some(DecisionEntry {
                r#ref: reference,
                title,
                aspect: guess_aspect(section),
                body,
                technical_note,
            })
        })
        .collect()
}

pub fn apply_overrides(entries: &mut [DecisionEntry], overrides: &[String]) -> Result<(), String> {
    let mut parsed = HashMap::new();
    for override_value in overrides {
        let (reference, aspect) = override_value.split_once('=').ok_or_else(|| {
            format!("invalid aspect override {override_value:?}; expected REF=aspect")
        })?;
        if reference.is_empty() || aspect.is_empty() {
            return Err(format!(
                "invalid aspect override {override_value:?}; expected REF=aspect"
            ));
        }
        parsed.insert(reference, aspect);
    }
    for entry in entries {
        if let Some(aspect) = parsed.get(entry.r#ref.as_str()) {
            entry.aspect = (*aspect).to_owned();
        }
    }
    Ok(())
}

pub fn load(options: &ImportOptions) -> Result<Vec<DecisionEntry>, String> {
    let mut text = String::new();
    unix_open(
        &options.source,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open {}: {error}", options.source.display()))?
    .take(16 * 1024 * 1024 + 1)
    .read_to_string(&mut text)
    .map_err(|error| format!("cannot read {}: {error}", options.source.display()))?;
    if text.len() > 16 * 1024 * 1024 {
        return Err("decision history exceeds 16 MiB".to_owned());
    }
    let mut entries = parse(&text);
    apply_overrides(&mut entries, &options.aspect_overrides)?;
    Ok(entries)
}

pub fn import_with<F>(
    entries: &[DecisionEntry],
    repository: &Path,
    mut record: F,
) -> Result<ImportCounts, String>
where
    F: FnMut(Value) -> Result<ResponseEnvelope, String>,
{
    let repository = repository
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository path: {error}"))?;
    let repository = repository
        .into_os_string()
        .into_string()
        .map_err(|_| "repository path must be valid UTF-8".to_owned())?;
    let mut counts = ImportCounts::default();
    for entry in entries {
        let mut params = serde_json::json!({
            "path": repository,
            "aspect": entry.aspect,
            "title": entry.title,
            "body": entry.body,
            "ref": entry.r#ref,
        });
        if let Some(note) = &entry.technical_note {
            params["technical_note"] = Value::String(note.clone());
        }
        match record(params)? {
            ResponseEnvelope::Success { .. } => counts.imported += 1,
            ResponseEnvelope::Failure { error, .. } if error.message.contains("already used") => {
                counts.skipped += 1;
            }
            ResponseEnvelope::Failure { .. } => counts.failed += 1,
        }
    }
    Ok(counts)
}

pub fn call(socket: &Path, params: Value) -> Result<ResponseEnvelope, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|error| format!("daemon unavailable: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("cannot set daemon read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("cannot set daemon write timeout: {error}"))?;
    let request = RequestEnvelope {
        protocol: PROTOCOL_VERSION,
        id: request_id(),
        operation: "decision.record".to_owned(),
        params,
        client: ClientContext {
            kind: ClientKind::Other,
            session: Some("decision-import".to_owned()),
            identity: None,
            ..ClientContext::default()
        },
    };
    let mut encoded = serde_json::to_vec(&request)
        .map_err(|error| format!("cannot encode daemon request: {error}"))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err("decision import request exceeds 64 KiB".to_owned());
    }
    stream
        .write_all(&encoded)
        .map_err(|error| format!("cannot write daemon request: {error}"))?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| format!("cannot finish daemon request: {error}"))?;
    let mut response = Vec::new();
    stream
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)
        .map_err(|error| format!("cannot read daemon response: {error}"))?;
    if response.len() > MAX_RESPONSE_BYTES {
        return Err("daemon response exceeds 256 KiB".to_owned());
    }
    serde_json::from_slice(&response)
        .map_err(|error| format!("daemon returned invalid protocol v2: {error}"))
}

fn plain(text: &str) -> String {
    let bold = Regex::new(r"(?s)\*\*(.+?)\*\*").expect("constant bold regex");
    let spaces = Regex::new(r"[ \t]+").expect("constant whitespace regex");
    let gaps = Regex::new(r"\n{3,}").expect("constant gap regex");
    let text = bold.replace_all(text, "$1");
    let text = spaces.replace_all(&text, " ");
    gaps.replace_all(&text, "\n\n").trim().to_owned()
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn byte_after_chars(value: &str, count: usize) -> usize {
    value
        .char_indices()
        .nth(count)
        .map_or(value.len(), |(index, _)| index)
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut random = [0u8; 4];
    let _ = getrandom::fill(&mut random);
    format!("{nanos:x}{:08x}", u32::from_be_bytes(random))
        .chars()
        .take(24)
        .collect()
}

pub fn failure_code(response: &ResponseEnvelope) -> Option<ErrorCode> {
    match response {
        ResponseEnvelope::Success { .. } => None,
        ResponseEnvelope::Failure { error, .. } => Some(error.code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::ProtocolError;
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    const SAMPLE: &str = r#"# Decision History

Compact record of owner-level decisions.

## XX-2026-01-01-CONSOLE-COLORS — Buttons use one accent color

**Decision.** Every primary button on the console page uses the accent color
so actions are easy to spot.

**Alternatives.** Per-view colors were rejected as noise.

**Owner context.** The owner saw both variants in a preview.

## XX-2026-01-02-EDGE-ROUTES — The edge proxies by route document

**Decision.** The edge serves domains from the last valid route document and
proxies each deployment by its leased port.

**Alternatives.** Restarting the edge per change was rejected.
"#;

    #[test]
    fn parser_preserves_refs_content_order_and_aspect_guesses() {
        let entries = parse(SAMPLE);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].r#ref, "XX-2026-01-01-CONSOLE-COLORS");
        assert_eq!(entries[0].title, "Buttons use one accent color");
        assert!(!entries[0].body.contains("**"));
        assert!(entries[0].body.contains("Alternatives"));
        assert_eq!(entries[0].aspect, "ui");
        assert_eq!(entries[1].aspect, "deployment");
        assert!(entries[0].technical_note.is_none());
    }

    #[test]
    fn parser_clips_unicode_safely_and_moves_body_overflow() {
        let title = "T".repeat(200);
        let body = "word ".repeat(1_200);
        let text = format!("## XX-LONG — {title}\n\n**Decision.** {body}\n");
        let entries = parse(&text);
        assert_eq!(entries[0].title.chars().count(), TITLE_MAX);
        assert!(entries[0].body.chars().count() <= BODY_MAX);
        assert!(
            entries[0]
                .body
                .ends_with("(continued in the technical note)")
        );
        assert!(
            entries[0]
                .technical_note
                .as_deref()
                .is_some_and(|note| note.ends_with("word"))
        );
    }

    #[test]
    fn overrides_and_idempotent_import_counts_are_exact() {
        let mut entries = parse(SAMPLE);
        apply_overrides(
            &mut entries,
            &["XX-2026-01-02-EDGE-ROUTES=process".to_owned()],
        )
        .unwrap();
        assert_eq!(entries[1].aspect, "process");
        let repository = tempfile::tempdir().unwrap();
        let mut seen = HashSet::new();
        let counts = import_with(&entries, repository.path(), |params| {
            let reference = params["ref"].as_str().unwrap().to_owned();
            if !seen.insert(reference.clone()) {
                return Ok(ResponseEnvelope::failure(
                    "fixture",
                    ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        format!("ref {reference:?} is already used"),
                    ),
                ));
            }
            ResponseEnvelope::success("fixture", serde_json::json!({"seq":seen.len()}))
                .map_err(|error| error.to_string())
        })
        .unwrap();
        assert_eq!(
            counts,
            ImportCounts {
                imported: 2,
                skipped: 0,
                failed: 0
            }
        );
        let counts = import_with(&entries, repository.path(), |params| {
            Ok(ResponseEnvelope::failure(
                "fixture",
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!("ref {:?} is already used", params["ref"]),
                ),
            ))
        })
        .unwrap();
        assert_eq!(counts.skipped, 2);
    }

    #[test]
    fn recorder_uses_one_bounded_protocol_two_socket_exchange() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: RequestEnvelope = serde_json::from_str(&line).unwrap();
            assert_eq!(request.protocol, 2);
            assert_eq!(request.operation, "decision.record");
            assert_eq!(request.params["ref"], "XX-1");
            let response = ResponseEnvelope::success(
                request.id,
                serde_json::json!({"decision_id":"n1","seq":1}),
            )
            .unwrap();
            stream
                .write_all(&devcoordinator2_api::encode_response(&response))
                .unwrap();
        });
        let response = call(
            &socket,
            serde_json::json!({
                "path":"/repo",
                "aspect":"architecture",
                "title":"One decision",
                "body":"Keep one exact decision.",
                "ref":"XX-1"
            }),
        )
        .unwrap();
        assert!(matches!(response, ResponseEnvelope::Success { .. }));
        server.join().unwrap();
    }
}
