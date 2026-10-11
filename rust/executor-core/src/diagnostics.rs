//! Deterministic, content-safe normalization of declared test diagnostics.
//!
//! Raw framework messages, rendered compiler diagnostics, stdout, stderr and
//! stacks deliberately never enter the values returned by this module.  They
//! remain in the caller-owned cold report and complete indexed log streams.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use devcoordinator2_executor_protocol::{
    CoordinatorDiagnosticClass, CoordinatorDiagnosticReport, DiagnosticEvent, DiagnosticExit,
    DiagnosticOrigin, DiagnosticReportFormat, DiagnosticReportSource, DiagnosticValue,
    DiagnosticValueType, ErrorCategory, FailureIndexEntry, LeafStatus, LogPhase, LogRef, LogStream,
    MAX_DIAGNOSTIC_EVENTS, MAX_DIAGNOSTIC_NAME_BYTES, MAX_DIAGNOSTIC_PREVIEW_BYTES,
    MAX_FAILURE_INDEX, SourceLocation, TerminationReason,
};
use quick_xml::Reader;
use quick_xml::encoding::Decoder;
use quick_xml::events::{BytesStart, Event};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_XML_DEPTH: usize = 64;
const MAX_XML_NODES: usize = 65_536;
const MAX_XML_ATTRIBUTE_BYTES: usize = 4096;
const MAX_RUST_RECORD_BYTES: usize = 1024 * 1024;
const MAX_RUST_RECORDS: usize = 65_536;
const MAX_COORDINATOR_REPORT_BYTES: usize = 256 * 1024;

/// Identity supplied by the executor, never inferred from report content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticContext {
    pub run_id: String,
    pub check: String,
    pub case: Option<String>,
    pub phase: LogPhase,
}

impl DiagnosticContext {
    pub fn validate(&self) -> Result<(), DiagnosticParseError> {
        for stream in [LogStream::Stdout, LogStream::Stderr] {
            LogRef {
                run_id: self.run_id.clone(),
                check: Some(self.check.clone()),
                phase: self.phase,
                case: self.case.clone(),
                stream,
            }
            .validate()
            .map_err(|_| DiagnosticParseError::InvalidContext)?;
        }
        if self.phase == LogPhase::Executor {
            return Err(DiagnosticParseError::InvalidContext);
        }
        Ok(())
    }

    fn log_refs(&self) -> Vec<LogRef> {
        [LogStream::Stdout, LogStream::Stderr]
            .into_iter()
            .map(|stream| LogRef {
                run_id: self.run_id.clone(),
                check: Some(self.check.clone()),
                phase: self.phase,
                case: self.case.clone(),
                stream,
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticParseError {
    InvalidContext,
    InvalidReport,
    InvalidEvent,
    LimitExceeded,
}

impl fmt::Display for DiagnosticParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidContext => "diagnostic execution context is invalid",
            Self::InvalidReport => "declared structured diagnostic report is invalid",
            Self::InvalidEvent => "structured diagnostic event is invalid",
            Self::LimitExceeded => "structured diagnostic evidence exceeds a safety bound",
        })
    }
}

impl std::error::Error for DiagnosticParseError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedDiagnostics {
    pub entries: Vec<FailureIndexEntry>,
    pub total_unique: usize,
    pub truncated: bool,
}

pub fn parse_declared_report(
    source: &DiagnosticReportSource,
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<NormalizedDiagnostics, DiagnosticParseError> {
    context.validate()?;
    let entries = match source.format {
        DiagnosticReportFormat::Junit => parse_junit(input, context)?,
        DiagnosticReportFormat::PlaywrightJson => parse_playwright(input, context)?,
        DiagnosticReportFormat::RustJson => parse_rust_json(input, context)?,
        DiagnosticReportFormat::CoordinatorJson => parse_coordinator_json(input, context)?,
    };
    normalize_diagnostics(entries)
}

fn parse_coordinator_json(
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<Vec<FailureIndexEntry>, DiagnosticParseError> {
    if input.len() > MAX_COORDINATOR_REPORT_BYTES {
        return Err(DiagnosticParseError::LimitExceeded);
    }
    let report: CoordinatorDiagnosticReport =
        serde_json::from_slice(input).map_err(|_| DiagnosticParseError::InvalidReport)?;
    report
        .validate(
            &context.run_id,
            &context.check,
            context.case.as_deref(),
            context.phase,
        )
        .map_err(|_| DiagnosticParseError::InvalidReport)?;
    Ok(report
        .diagnostics
        .into_iter()
        .map(|diagnostic| {
            let category = match diagnostic.class {
                CoordinatorDiagnosticClass::Solver => ErrorCategory::ProcessExit,
                CoordinatorDiagnosticClass::ContainerStart => ErrorCategory::Dependency,
                CoordinatorDiagnosticClass::ContainerCleanup => ErrorCategory::Artifact,
                CoordinatorDiagnosticClass::CoordinatorInfrastructure => ErrorCategory::Internal,
            };
            let mut entry = new_entry(
                context,
                None,
                diagnostic.status,
                diagnostic.exit,
                None,
                None,
                category,
                None,
                None,
                DiagnosticOrigin::ExplicitEvent,
            );
            entry.coordinator = Some(diagnostic);
            entry
        })
        .collect())
}

pub fn parse_diagnostic_event(
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<FailureIndexEntry, DiagnosticParseError> {
    context.validate()?;
    let event = DiagnosticEvent::from_json(
        input,
        &context.run_id,
        &context.check,
        context.case.as_deref(),
        context.phase,
    )
    .map_err(|_| DiagnosticParseError::InvalidEvent)?;
    let mut log_refs = event.log_refs;
    if log_refs.is_empty() {
        log_refs = context.log_refs();
    }
    let mut entry = FailureIndexEntry {
        check: Some(event.check),
        case: event.case,
        status: event.status,
        exit: event.exit,
        termination_reason: event.termination_reason,
        source: event.source,
        error_category: event.error_category,
        expected: event.expected,
        actual: event.actual,
        fingerprint: String::new(),
        occurrences: 1,
        log_refs,
        origin: DiagnosticOrigin::ExplicitEvent,
        coordinator: None,
    };
    entry.fingerprint = diagnostic_fingerprint(&entry);
    entry
        .validate()
        .map_err(|_| DiagnosticParseError::InvalidEvent)?;
    Ok(entry)
}

/// Collapse identical failures and return the bounded, deterministically
/// ranked completion projection.  The complete normalized source remains in
/// the cold diagnostics artifact owned by the caller.
pub fn normalize_diagnostics(
    mut entries: Vec<FailureIndexEntry>,
) -> Result<NormalizedDiagnostics, DiagnosticParseError> {
    if entries.len() > MAX_DIAGNOSTIC_EVENTS {
        return Err(DiagnosticParseError::LimitExceeded);
    }
    for entry in &mut entries {
        entry.fingerprint = diagnostic_fingerprint(entry);
        entry
            .validate()
            .map_err(|_| DiagnosticParseError::InvalidReport)?;
        entry.log_refs.sort();
    }
    entries.sort_by(stable_diagnostic_order);

    let mut grouped = BTreeMap::<String, FailureIndexEntry>::new();
    for entry in entries {
        if let Some(existing) = grouped.get_mut(&entry.fingerprint) {
            existing.occurrences = existing.occurrences.saturating_add(entry.occurrences);
            for log_ref in entry.log_refs {
                if !existing.log_refs.contains(&log_ref) {
                    existing.log_refs.push(log_ref);
                }
            }
            existing.log_refs.sort();
        } else {
            grouped.insert(entry.fingerprint.clone(), entry);
        }
    }
    let total_unique = grouped.len();
    let mut entries: Vec<_> = grouped.into_values().collect();
    entries.sort_by(stable_diagnostic_order);
    entries.truncate(MAX_FAILURE_INDEX);
    Ok(NormalizedDiagnostics {
        truncated: total_unique > entries.len(),
        total_unique,
        entries,
    })
}

pub fn diagnostic_rank(entry: &FailureIndexEntry) -> u8 {
    if entry.origin == DiagnosticOrigin::ExplicitEvent {
        return 0;
    }
    match entry.error_category {
        ErrorCategory::Assertion => 1,
        ErrorCategory::Compiler => 2,
        ErrorCategory::Panic | ErrorCategory::Exception => 3,
        ErrorCategory::StackFrame => 4,
        ErrorCategory::Timeout | ErrorCategory::Cancellation | ErrorCategory::ProcessExit => 5,
        ErrorCategory::BrowserConsole | ErrorCategory::NetworkRequest => 6,
        ErrorCategory::StructuredEvidenceInvalid
        | ErrorCategory::LogStorage
        | ErrorCategory::SourceChanged
        | ErrorCategory::Artifact
        | ErrorCategory::Dependency
        | ErrorCategory::Internal => 7,
    }
}

pub fn diagnostic_fingerprint(entry: &FailureIndexEntry) -> String {
    let mut hasher = Sha256::new();
    for field in [
        "devcoordinator-diagnostic-v1".to_owned(),
        status_name(entry.status).to_owned(),
        category_name(entry.error_category).to_owned(),
        entry.check.clone().unwrap_or_default(),
        entry.case.clone().unwrap_or_default(),
        entry
            .source
            .as_ref()
            .map(|source| source.file.clone())
            .unwrap_or_default(),
        entry
            .source
            .as_ref()
            .map(|source| source.line.to_string())
            .unwrap_or_default(),
        entry
            .source
            .as_ref()
            .and_then(|source| source.column)
            .map(|column| column.to_string())
            .unwrap_or_default(),
        entry
            .expected
            .as_ref()
            .map(|value| value.sha256.clone())
            .unwrap_or_default(),
        entry
            .exit
            .code
            .map(|code| code.to_string())
            .unwrap_or_default(),
        entry
            .exit
            .signal
            .map(|signal| signal.to_string())
            .unwrap_or_default(),
        entry
            .termination_reason
            .map(termination_name)
            .unwrap_or_default()
            .to_owned(),
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    if let Some(coordinator) = &entry.coordinator {
        for field in [
            format!("{:?}", coordinator.class),
            format!("{:?}", coordinator.command_phase),
            format!("{:?}", coordinator.cleanup_status),
            coordinator.container_id.clone().unwrap_or_default(),
            coordinator.container_name.clone().unwrap_or_default(),
            coordinator.native_execution_started.to_string(),
            coordinator.retryable.to_string(),
            format!("{:?}", coordinator.next_action),
            coordinator.stdout_bytes.to_string(),
            coordinator.stderr_bytes.to_string(),
            coordinator.cleanup_stderr_bytes.to_string(),
        ] {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field.as_bytes());
        }
        for (key, value) in &coordinator.container_labels {
            hasher.update((key.len() as u64).to_be_bytes());
            hasher.update(key.as_bytes());
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    format!("sha256:{}", lower_hex(&hasher.finalize()))
}

pub fn bounded_diagnostic_value(value_type: DiagnosticValueType, raw: &str) -> DiagnosticValue {
    let sha256 = lower_hex(&Sha256::digest(raw.as_bytes()));
    let byte_count = u64::try_from(raw.len()).unwrap_or(u64::MAX);
    if looks_sensitive(raw) {
        return DiagnosticValue {
            value_type,
            preview: None,
            sha256,
            byte_count,
            truncated: raw.len() > MAX_DIAGNOSTIC_PREVIEW_BYTES,
            redacted: true,
        };
    }
    if raw.chars().any(char::is_control) {
        return DiagnosticValue {
            value_type,
            preview: None,
            sha256,
            byte_count,
            truncated: true,
            redacted: false,
        };
    }
    let (preview, truncated) = truncate_utf8(raw, MAX_DIAGNOSTIC_PREVIEW_BYTES);
    DiagnosticValue {
        value_type,
        preview: Some(preview),
        sha256,
        byte_count,
        truncated,
        redacted: false,
    }
}

fn stable_diagnostic_order(left: &FailureIndexEntry, right: &FailureIndexEntry) -> Ordering {
    diagnostic_rank(left)
        .cmp(&diagnostic_rank(right))
        .then_with(|| left.check.cmp(&right.check))
        .then_with(|| left.case.cmp(&right.case))
        .then_with(|| left.source.cmp(&right.source))
        .then_with(|| left.fingerprint.cmp(&right.fingerprint))
        .then_with(|| left.actual.cmp(&right.actual))
        .then_with(|| left.log_refs.cmp(&right.log_refs))
}

fn looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("password=")
        || lower.contains("password:")
        || lower.contains("token=")
        || lower.contains("token:")
        || lower.contains("secret=")
        || lower.contains("secret:")
        || lower.contains("authorization: bearer")
        || lower.contains("-----begin private key-----")
        || value.contains("AKIA")
        || value.contains("ghp_")
        || value.starts_with("sk-")
        || value.contains(" sk-")
        || value.contains("\"sk-")
        || value.contains("'sk-")
}

fn truncate_utf8(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn status_name(status: LeafStatus) -> &'static str {
    match status {
        LeafStatus::Pending => "pending",
        LeafStatus::Running => "running",
        LeafStatus::Reused => "reused",
        LeafStatus::Passed => "passed",
        LeafStatus::Failed => "failed",
        LeafStatus::TimedOut => "timed_out",
        LeafStatus::Invalidated => "invalidated",
        LeafStatus::NotMeaningful => "not_meaningful",
        LeafStatus::Cancelled => "cancelled",
        LeafStatus::Unsafe => "unsafe",
    }
}

fn category_name(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Assertion => "assertion",
        ErrorCategory::Compiler => "compiler",
        ErrorCategory::Panic => "panic",
        ErrorCategory::Exception => "exception",
        ErrorCategory::StackFrame => "stack_frame",
        ErrorCategory::Timeout => "timeout",
        ErrorCategory::Cancellation => "cancellation",
        ErrorCategory::ProcessExit => "process_exit",
        ErrorCategory::BrowserConsole => "browser_console",
        ErrorCategory::NetworkRequest => "network_request",
        ErrorCategory::StructuredEvidenceInvalid => "structured_evidence_invalid",
        ErrorCategory::LogStorage => "log_storage",
        ErrorCategory::SourceChanged => "source_changed",
        ErrorCategory::Artifact => "artifact",
        ErrorCategory::Dependency => "dependency",
        ErrorCategory::Internal => "internal",
    }
}

fn termination_name(reason: TerminationReason) -> &'static str {
    match reason {
        TerminationReason::DeadlineExceeded => "deadline_exceeded",
        TerminationReason::UserCancelled => "user_cancelled",
        TerminationReason::Superseded => "superseded",
        TerminationReason::RunCancelled => "run_cancelled",
        TerminationReason::DaemonInterrupted => "daemon_interrupted",
        TerminationReason::UnsafeStop => "unsafe_stop",
    }
}

#[allow(clippy::too_many_arguments)]
fn new_entry(
    context: &DiagnosticContext,
    framework_case: Option<&str>,
    status: LeafStatus,
    exit: DiagnosticExit,
    termination_reason: Option<TerminationReason>,
    source: Option<SourceLocation>,
    error_category: ErrorCategory,
    expected: Option<DiagnosticValue>,
    actual: Option<DiagnosticValue>,
    origin: DiagnosticOrigin,
) -> FailureIndexEntry {
    let mut entry = FailureIndexEntry {
        check: Some(context.check.clone()),
        case: combined_case(context.case.as_deref(), framework_case),
        status,
        exit,
        termination_reason,
        source,
        error_category,
        expected,
        actual,
        fingerprint: String::new(),
        occurrences: 1,
        log_refs: context.log_refs(),
        origin,
        coordinator: None,
    };
    entry.fingerprint = diagnostic_fingerprint(&entry);
    entry
}

fn combined_case(executor_case: Option<&str>, framework_case: Option<&str>) -> Option<String> {
    match (executor_case, framework_case.and_then(bounded_name)) {
        (Some(executor), Some(framework)) if executor != framework => {
            bounded_name(&format!("{executor} :: {framework}"))
        }
        (Some(executor), _) => Some(executor.to_owned()),
        (None, framework) => framework,
    }
}

fn bounded_name(value: &str) -> Option<String> {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return None;
    }
    if normalized.len() <= MAX_DIAGNOSTIC_NAME_BYTES {
        return Some(normalized.to_owned());
    }
    let digest = lower_hex(&Sha256::digest(normalized.as_bytes()));
    let suffix = format!("~{}", &digest[..16]);
    let limit = MAX_DIAGNOSTIC_NAME_BYTES - suffix.len();
    let (prefix, _) = truncate_utf8(normalized, limit);
    Some(format!("{prefix}{suffix}"))
}

fn source_location(
    file: Option<&str>,
    line: Option<u64>,
    column: Option<u64>,
) -> Option<SourceLocation> {
    let mut file = file?.trim();
    while let Some(stripped) = file.strip_prefix("./") {
        file = stripped;
    }
    if file.is_empty() || file.len() > 512 || file.contains('\\') || Path::new(file).is_absolute() {
        return None;
    }
    if !Path::new(file)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let line = u32::try_from(line?).ok().filter(|line| *line > 0)?;
    let column = column
        .map(u32::try_from)
        .transpose()
        .ok()?
        .filter(|column| *column > 0);
    let source = SourceLocation {
        file: file.to_owned(),
        line,
        column,
    };
    source.validate().ok().map(|_| source)
}

fn parse_u64(value: Option<&str>) -> Option<u64> {
    value?.parse().ok()
}

fn xml_attribute(
    start: &BytesStart<'_>,
    name: &[u8],
    decoder: Decoder,
) -> Result<Option<String>, DiagnosticParseError> {
    let mut found = None;
    for attribute in start.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| DiagnosticParseError::InvalidReport)?;
        if attribute.value.len() > MAX_XML_ATTRIBUTE_BYTES {
            return Err(DiagnosticParseError::LimitExceeded);
        }
        if attribute.key.as_ref() == name {
            if found.is_some() {
                return Err(DiagnosticParseError::InvalidReport);
            }
            let value = attribute
                .decode_and_unescape_value(decoder)
                .map_err(|_| DiagnosticParseError::InvalidReport)?;
            found = Some(value.into_owned());
        }
    }
    Ok(found)
}

#[derive(Clone, Debug, Default)]
struct JunitCase {
    name: Option<String>,
    class_name: Option<String>,
    file: Option<String>,
    line: Option<u64>,
    column: Option<u64>,
}

fn parse_junit(
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<Vec<FailureIndexEntry>, DiagnosticParseError> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut saw_root = false;
    let mut current_case: Option<JunitCase> = None;
    let mut entries = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|_| DiagnosticParseError::InvalidReport)?;
        nodes = nodes.saturating_add(1);
        if nodes > MAX_XML_NODES {
            return Err(DiagnosticParseError::LimitExceeded);
        }
        match event {
            Event::Start(start) => {
                depth = depth.saturating_add(1);
                if depth > MAX_XML_DEPTH {
                    return Err(DiagnosticParseError::LimitExceeded);
                }
                if start.name().as_ref() == b"testsuites" || start.name().as_ref() == b"testsuite" {
                    saw_root = true;
                } else if start.name().as_ref() == b"testcase" {
                    current_case = Some(junit_case(&start, reader.decoder())?);
                } else if start.name().as_ref() == b"failure" || start.name().as_ref() == b"error" {
                    entries.push(junit_failure(
                        &start,
                        current_case.as_ref(),
                        context,
                        reader.decoder(),
                    )?);
                }
            }
            Event::Empty(start) => {
                if start.name().as_ref() == b"testsuites" || start.name().as_ref() == b"testsuite" {
                    saw_root = true;
                } else if start.name().as_ref() == b"failure" || start.name().as_ref() == b"error" {
                    entries.push(junit_failure(
                        &start,
                        current_case.as_ref(),
                        context,
                        reader.decoder(),
                    )?);
                }
            }
            Event::End(end) => {
                if end.name().as_ref() == b"testcase" {
                    current_case = None;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or(DiagnosticParseError::InvalidReport)?;
            }
            Event::DocType(_) => return Err(DiagnosticParseError::InvalidReport),
            Event::Eof => break,
            Event::Decl(_)
            | Event::Text(_)
            | Event::CData(_)
            | Event::Comment(_)
            | Event::PI(_)
            | Event::GeneralRef(_) => {}
        }
        if entries.len() > MAX_DIAGNOSTIC_EVENTS {
            return Err(DiagnosticParseError::LimitExceeded);
        }
    }
    if !saw_root || depth != 0 {
        return Err(DiagnosticParseError::InvalidReport);
    }
    Ok(entries)
}

fn junit_case(start: &BytesStart<'_>, decoder: Decoder) -> Result<JunitCase, DiagnosticParseError> {
    Ok(JunitCase {
        name: xml_attribute(start, b"name", decoder)?,
        class_name: xml_attribute(start, b"classname", decoder)?,
        file: xml_attribute(start, b"file", decoder)?,
        line: parse_u64(xml_attribute(start, b"line", decoder)?.as_deref()),
        column: parse_u64(xml_attribute(start, b"column", decoder)?.as_deref()),
    })
}

fn junit_failure(
    start: &BytesStart<'_>,
    case: Option<&JunitCase>,
    context: &DiagnosticContext,
    decoder: Decoder,
) -> Result<FailureIndexEntry, DiagnosticParseError> {
    let failure_type = xml_attribute(start, b"type", decoder)?;
    let expected = xml_attribute(start, b"expected", decoder)?
        .map(|value| bounded_diagnostic_value(DiagnosticValueType::String, &value));
    let actual = xml_attribute(start, b"actual", decoder)?
        .map(|value| bounded_diagnostic_value(DiagnosticValueType::String, &value));
    let file =
        xml_attribute(start, b"file", decoder)?.or_else(|| case.and_then(|case| case.file.clone()));
    let line = parse_u64(xml_attribute(start, b"line", decoder)?.as_deref())
        .or_else(|| case.and_then(|case| case.line));
    let column = parse_u64(xml_attribute(start, b"column", decoder)?.as_deref())
        .or_else(|| case.and_then(|case| case.column));
    let case_name = case.and_then(|case| match (&case.class_name, &case.name) {
        (Some(class_name), Some(name)) => Some(format!("{class_name}::{name}")),
        (None, Some(name)) => Some(name.clone()),
        (Some(class_name), None) => Some(class_name.clone()),
        (None, None) => None,
    });
    let category = if start.name().as_ref() == b"failure" {
        ErrorCategory::Assertion
    } else if failure_type
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("panic"))
    {
        ErrorCategory::Panic
    } else {
        ErrorCategory::Exception
    };
    Ok(new_entry(
        context,
        case_name.as_deref(),
        LeafStatus::Failed,
        DiagnosticExit::default(),
        None,
        source_location(file.as_deref(), line, column),
        category,
        expected,
        actual,
        DiagnosticOrigin::Junit,
    ))
}

#[derive(Debug, Deserialize)]
struct PlaywrightReport {
    suites: Vec<PlaywrightSuite>,
    errors: Vec<PlaywrightError>,
}

#[derive(Debug, Deserialize)]
struct PlaywrightSuite {
    title: String,
    specs: Vec<PlaywrightSpec>,
    #[serde(default)]
    suites: Vec<PlaywrightSuite>,
}

#[derive(Debug, Deserialize)]
struct PlaywrightSpec {
    title: String,
    id: String,
    file: String,
    line: u64,
    column: u64,
    tests: Vec<PlaywrightTest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightTest {
    project_name: String,
    status: String,
    results: Vec<PlaywrightResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightResult {
    status: Option<String>,
    error_location: Option<PlaywrightLocation>,
}

#[derive(Debug, Deserialize)]
struct PlaywrightError {
    location: Option<PlaywrightLocation>,
}

#[derive(Clone, Debug, Deserialize)]
struct PlaywrightLocation {
    file: String,
    line: u64,
    column: u64,
}

fn parse_playwright(
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<Vec<FailureIndexEntry>, DiagnosticParseError> {
    let report: PlaywrightReport =
        serde_json::from_slice(input).map_err(|_| DiagnosticParseError::InvalidReport)?;
    let mut entries = Vec::new();
    let mut visited = 0usize;
    for suite in &report.suites {
        walk_playwright_suite(suite, &[], 1, context, &mut visited, &mut entries)?;
    }
    for error in report.errors {
        visited = visited.saturating_add(1);
        if visited > MAX_DIAGNOSTIC_EVENTS {
            return Err(DiagnosticParseError::LimitExceeded);
        }
        entries.push(new_entry(
            context,
            None,
            LeafStatus::Failed,
            DiagnosticExit::default(),
            None,
            playwright_location(error.location.as_ref()),
            ErrorCategory::Exception,
            None,
            None,
            DiagnosticOrigin::PlaywrightJson,
        ));
    }
    Ok(entries)
}

fn walk_playwright_suite(
    suite: &PlaywrightSuite,
    parents: &[String],
    depth: usize,
    context: &DiagnosticContext,
    visited: &mut usize,
    entries: &mut Vec<FailureIndexEntry>,
) -> Result<(), DiagnosticParseError> {
    if depth > MAX_XML_DEPTH {
        return Err(DiagnosticParseError::LimitExceeded);
    }
    *visited = visited.saturating_add(1);
    if *visited > MAX_DIAGNOSTIC_EVENTS {
        return Err(DiagnosticParseError::LimitExceeded);
    }
    let mut path = parents.to_vec();
    if let Some(title) = bounded_name(&suite.title) {
        path.push(title);
    }
    for spec in &suite.specs {
        *visited = visited.saturating_add(1);
        if *visited > MAX_DIAGNOSTIC_EVENTS {
            return Err(DiagnosticParseError::LimitExceeded);
        }
        for test in &spec.tests {
            *visited = visited.saturating_add(1);
            if *visited > MAX_DIAGNOSTIC_EVENTS {
                return Err(DiagnosticParseError::LimitExceeded);
            }
            if test.status != "unexpected" {
                continue;
            }
            let framework_case = playwright_case_name(&path, spec, test);
            let mut emitted = false;
            for result in &test.results {
                let Some((status, termination_reason, category)) =
                    playwright_failure(result.status.as_deref())
                else {
                    continue;
                };
                emitted = true;
                entries.push(new_entry(
                    context,
                    framework_case.as_deref(),
                    status,
                    DiagnosticExit::default(),
                    termination_reason,
                    playwright_location(result.error_location.as_ref()).or_else(|| {
                        source_location(Some(&spec.file), Some(spec.line), Some(spec.column))
                    }),
                    category,
                    None,
                    None,
                    DiagnosticOrigin::PlaywrightJson,
                ));
            }
            if !emitted {
                entries.push(new_entry(
                    context,
                    framework_case.as_deref(),
                    LeafStatus::Failed,
                    DiagnosticExit::default(),
                    None,
                    source_location(Some(&spec.file), Some(spec.line), Some(spec.column)),
                    ErrorCategory::Exception,
                    None,
                    None,
                    DiagnosticOrigin::PlaywrightJson,
                ));
            }
        }
    }
    for nested in &suite.suites {
        walk_playwright_suite(nested, &path, depth + 1, context, visited, entries)?;
    }
    Ok(())
}

fn playwright_case_name(
    path: &[String],
    spec: &PlaywrightSpec,
    test: &PlaywrightTest,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(project) = bounded_name(&test.project_name) {
        parts.push(project);
    }
    parts.extend(path.iter().cloned());
    if let Some(title) = bounded_name(&spec.title) {
        parts.push(title);
    }
    if parts.is_empty() {
        bounded_name(&spec.id)
    } else {
        bounded_name(&parts.join(" :: "))
    }
}

fn playwright_failure(
    status: Option<&str>,
) -> Option<(LeafStatus, Option<TerminationReason>, ErrorCategory)> {
    match status {
        Some("failed") => Some((LeafStatus::Failed, None, ErrorCategory::Exception)),
        Some("timedOut") => Some((
            LeafStatus::TimedOut,
            Some(TerminationReason::DeadlineExceeded),
            ErrorCategory::Timeout,
        )),
        Some("interrupted") => Some((
            LeafStatus::Cancelled,
            Some(TerminationReason::RunCancelled),
            ErrorCategory::Cancellation,
        )),
        _ => None,
    }
}

fn playwright_location(location: Option<&PlaywrightLocation>) -> Option<SourceLocation> {
    let location = location?;
    source_location(
        Some(&location.file),
        Some(location.line),
        Some(location.column),
    )
}

#[derive(Debug, Deserialize)]
struct RustJsonRecord {
    reason: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    event: Option<String>,
    name: Option<String>,
    message: Option<RustCompilerMessage>,
}

#[derive(Debug, Deserialize)]
struct RustCompilerMessage {
    level: String,
    spans: Vec<RustCompilerSpan>,
}

#[derive(Debug, Deserialize)]
struct RustCompilerSpan {
    file_name: String,
    line_start: u64,
    column_start: u64,
    is_primary: bool,
}

fn parse_rust_json(
    input: &[u8],
    context: &DiagnosticContext,
) -> Result<Vec<FailureIndexEntry>, DiagnosticParseError> {
    let mut entries = Vec::new();
    let mut records = 0usize;
    let mut recognized = false;
    for raw_line in input.split(|byte| *byte == b'\n') {
        let line = trim_ascii(raw_line);
        if line.is_empty() {
            continue;
        }
        records = records.saturating_add(1);
        if records > MAX_RUST_RECORDS || line.len() > MAX_RUST_RECORD_BYTES {
            return Err(DiagnosticParseError::LimitExceeded);
        }
        let record: RustJsonRecord =
            serde_json::from_slice(line).map_err(|_| DiagnosticParseError::InvalidReport)?;
        if record.reason.as_deref() == Some("compiler-message") {
            recognized = true;
            let message = record.message.ok_or(DiagnosticParseError::InvalidReport)?;
            if message.spans.len() > MAX_DIAGNOSTIC_EVENTS {
                return Err(DiagnosticParseError::LimitExceeded);
            }
            if message.level == "error" {
                let primary = message.spans.iter().find(|span| span.is_primary);
                entries.push(new_entry(
                    context,
                    None,
                    LeafStatus::Failed,
                    DiagnosticExit::default(),
                    None,
                    primary.and_then(|span| {
                        source_location(
                            Some(&span.file_name),
                            Some(span.line_start),
                            Some(span.column_start),
                        )
                    }),
                    ErrorCategory::Compiler,
                    None,
                    None,
                    DiagnosticOrigin::RustJson,
                ));
            }
        } else if record.kind.as_deref() == Some("test") {
            recognized = true;
            let framework_case = record.name.as_deref().and_then(bounded_name);
            let Some((status, termination_reason, category)) =
                rust_test_failure(record.event.as_deref())
            else {
                continue;
            };
            entries.push(new_entry(
                context,
                framework_case.as_deref(),
                status,
                DiagnosticExit::default(),
                termination_reason,
                None,
                category,
                None,
                None,
                DiagnosticOrigin::RustJson,
            ));
        } else if record.kind.as_deref() == Some("suite") {
            recognized = true;
            if record.event.as_deref() == Some("failed") {
                entries.push(new_entry(
                    context,
                    None,
                    LeafStatus::Failed,
                    DiagnosticExit::default(),
                    None,
                    None,
                    ErrorCategory::Panic,
                    None,
                    None,
                    DiagnosticOrigin::RustJson,
                ));
            }
        } else if record.reason.is_some() {
            // Other Cargo JSON messages are structurally valid but do not
            // describe a terminal failure.
            recognized = true;
        }
        if entries.len() > MAX_DIAGNOSTIC_EVENTS {
            return Err(DiagnosticParseError::LimitExceeded);
        }
    }
    if records == 0 || !recognized {
        return Err(DiagnosticParseError::InvalidReport);
    }
    Ok(entries)
}

fn rust_test_failure(
    event: Option<&str>,
) -> Option<(LeafStatus, Option<TerminationReason>, ErrorCategory)> {
    match event {
        Some("failed") => Some((LeafStatus::Failed, None, ErrorCategory::Panic)),
        Some("timeout" | "timed_out") => Some((
            LeafStatus::TimedOut,
            Some(TerminationReason::DeadlineExceeded),
            ErrorCategory::Timeout,
        )),
        Some("cancelled" | "interrupted") => Some((
            LeafStatus::Cancelled,
            Some(TerminationReason::RunCancelled),
            ErrorCategory::Cancellation,
        )),
        _ => None,
    }
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_executor_protocol::{MAX_EVENT_BYTES, Schema2};

    fn context() -> DiagnosticContext {
        DiagnosticContext {
            run_id: "run-1".into(),
            check: "unit".into(),
            case: None,
            phase: LogPhase::Check,
        }
    }

    fn source(format: DiagnosticReportFormat) -> DiagnosticReportSource {
        DiagnosticReportSource {
            format,
            path: "report".into(),
        }
    }

    #[test]
    fn junit_extracts_only_structured_fields_and_keeps_prose_cold() {
        let xml = br#"<?xml version="1.0"?>
<testsuites><testsuite name="suite"><testcase classname="parser" name="rejects bad input" file="src/parser.rs" line="81">
<failure type="AssertionError" expected="ready" actual="pending" message="IGNORE SECRET token=abc">raw stack and arbitrary prose</failure>
</testcase><testcase name="panic"><error type="PanicException">private panic body</error></testcase></testsuite></testsuites>"#;
        let parsed = parse_declared_report(&source(DiagnosticReportFormat::Junit), xml, &context())
            .expect("valid JUnit");
        assert_eq!(parsed.total_unique, 2);
        assert_eq!(parsed.entries[0].error_category, ErrorCategory::Assertion);
        assert_eq!(
            parsed.entries[0].source.as_ref().expect("source").file,
            "src/parser.rs"
        );
        assert_eq!(
            parsed.entries[0]
                .expected
                .as_ref()
                .and_then(|value| value.preview.as_deref()),
            Some("ready")
        );
        let public = serde_json::to_string(&parsed.entries).expect("serialize diagnostics");
        for secret in [
            "IGNORE SECRET",
            "token=abc",
            "raw stack",
            "private panic body",
        ] {
            assert!(!public.contains(secret), "raw prose leaked: {secret}");
        }
    }

    #[test]
    fn junit_rejects_dtd_and_does_not_expose_outside_paths() {
        let dtd = br#"<!DOCTYPE testsuite [<!ENTITY x SYSTEM "file:///etc/passwd">]><testsuite/>"#;
        assert_eq!(
            parse_declared_report(&source(DiagnosticReportFormat::Junit), dtd, &context()),
            Err(DiagnosticParseError::InvalidReport)
        );
        let outside = br#"<testsuite><testcase name="case" file="../../secret" line="2"><failure/></testcase></testsuite>"#;
        let parsed =
            parse_declared_report(&source(DiagnosticReportFormat::Junit), outside, &context())
                .expect("valid report with unusable source");
        assert!(parsed.entries[0].source.is_none());
    }

    #[test]
    fn playwright_uses_outcome_and_locations_without_messages_or_stacks() {
        let report = serde_json::json!({
            "config": {"private": "ignored"},
            "suites": [{
                "title": "checkout", "file": "tests/checkout.spec.ts", "line": 1, "column": 1,
                "specs": [{
                    "title": "submits", "id": "spec-1", "file": "tests/checkout.spec.ts", "line": 14, "column": 3,
                    "tests": [
                        {"projectName": "chromium", "status": "unexpected", "results": [{
                            "status": "failed",
                            "errorLocation": {"file": "tests/checkout.spec.ts", "line": 22, "column": 7},
                            "error": {"message": "SECRET token=abc", "stack": "raw stack"},
                            "stdout": [{"text": "raw stdout"}], "stderr": [{"text": "raw stderr"}]
                        }]},
                        {"projectName": "webkit", "status": "flaky", "results": [{"status": "failed"}, {"status": "passed"}]}
                    ]
                }],
                "suites": []
            }],
            "errors": [],
            "stats": {"unexpected": 1}
        });
        let parsed = parse_declared_report(
            &source(DiagnosticReportFormat::PlaywrightJson),
            &serde_json::to_vec(&report).expect("report"),
            &context(),
        )
        .expect("valid Playwright report");
        assert_eq!(
            parsed.total_unique, 1,
            "flaky final outcome is not a terminal failure"
        );
        let diagnostic = &parsed.entries[0];
        assert_eq!(diagnostic.error_category, ErrorCategory::Exception);
        assert_eq!(diagnostic.source.as_ref().expect("source").line, 22);
        let public = serde_json::to_string(diagnostic).expect("serialize diagnostic");
        for secret in [
            "SECRET",
            "token=abc",
            "raw stack",
            "raw stdout",
            "raw stderr",
        ] {
            assert!(
                !public.contains(secret),
                "raw Playwright content leaked: {secret}"
            );
        }
    }

    #[test]
    fn rust_json_extracts_compiler_spans_and_libtest_failures_only() {
        let report = concat!(
            r#"{"reason":"compiler-message","message":{"level":"error","message":"private compiler prose","code":{"code":"E0001"},"spans":[{"file_name":"src/lib.rs","line_start":7,"column_start":4,"is_primary":true}],"rendered":"raw rendered diagnostic"}}"#,
            "\n",
            r#"{"type":"test","event":"failed","name":"tests::bad","stdout":"thread panic with secret"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"tests::good"}"#
        );
        let parsed = parse_declared_report(
            &source(DiagnosticReportFormat::RustJson),
            report.as_bytes(),
            &context(),
        )
        .expect("valid Rust JSON lines");
        assert_eq!(parsed.total_unique, 2);
        assert_eq!(parsed.entries[0].error_category, ErrorCategory::Compiler);
        assert_eq!(parsed.entries[0].source.as_ref().expect("source").line, 7);
        assert_eq!(parsed.entries[1].error_category, ErrorCategory::Panic);
        let public = serde_json::to_string(&parsed.entries).expect("serialize diagnostics");
        for secret in [
            "private compiler prose",
            "raw rendered",
            "thread panic with secret",
        ] {
            assert!(
                !public.contains(secret),
                "raw Rust content leaked: {secret}"
            );
        }
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::RustJson),
                b"ordinary console output",
                &context()
            )
            .is_err()
        );
    }

    #[test]
    fn coordinator_json_requires_typed_count_and_identity() {
        let report = serde_json::json!({
            "schema": 2,
            "run_id": "run-1",
            "check": "unit",
            "case": null,
            "phase": "check",
            "failed_diagnostics": 1,
            "diagnostics": [{
                "class": "container_cleanup",
                "command_phase": "cleanup",
                "status": "failed",
                "exit": {"code": 1, "signal": null},
                "native_execution_started": true,
                "cleanup_status": "failed",
                "retryable": true,
                "next_action": "retry_cleanup",
                "container_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "container_name": "airfoilfoam-command-run-1",
                "container_labels": {"devcoordinator2.run":"run-1","devcoordinator2.check":"unit","devcoordinator2.repository":"repo","devcoordinator2.owner":"coordinator"},
                "stderr_ref": {"run_id":"run-1","check":"unit","phase":"check","case":null,"stream":"stderr"},
                "stdout_bytes": 0,
                "stderr_bytes": 37,
                "cleanup_stderr_bytes": 0
            }]
        });
        let parsed = parse_declared_report(
            &source(DiagnosticReportFormat::CoordinatorJson),
            &serde_json::to_vec(&report).expect("report"),
            &context(),
        )
        .expect("valid coordinator report");
        assert_eq!(parsed.total_unique, 1);
        assert_eq!(parsed.entries[0].error_category, ErrorCategory::Artifact);
        assert_eq!(parsed.entries[0].status, LeafStatus::Failed);
        assert_eq!(
            parsed.entries[0]
                .coordinator
                .as_ref()
                .map(|entry| entry.stderr_bytes),
            Some(37)
        );

        let clean = serde_json::json!({
            "schema": 2,
            "run_id": "run-1",
            "check": "unit",
            "case": null,
            "phase": "check",
            "failed_diagnostics": 0,
            "diagnostics": []
        });
        let clean = parse_declared_report(
            &source(DiagnosticReportFormat::CoordinatorJson),
            &serde_json::to_vec(&clean).expect("clean report"),
            &context(),
        )
        .expect("zero diagnostics is valid");
        assert!(clean.entries.is_empty());

        let mut malformed = report.clone();
        malformed["failed_diagnostics"] = serde_json::json!(0);
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &serde_json::to_vec(&malformed).expect("malformed report"),
                &context(),
            )
            .is_err()
        );
        let mut wrong_phase = report.clone();
        wrong_phase["diagnostics"][0]["command_phase"] = serde_json::json!("check");
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &serde_json::to_vec(&wrong_phase).expect("wrong phase"),
                &context(),
            )
            .is_err()
        );
        let mut wrong_label = report.clone();
        wrong_label["diagnostics"][0]["container_labels"]["devcoordinator2.run"] =
            serde_json::json!("other-run");
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &serde_json::to_vec(&wrong_label).expect("wrong owner label"),
                &context(),
            )
            .is_err()
        );

        let mut missing = serde_json::json!({
            "schema": 2,
            "run_id": "run-1",
            "check": "unit",
            "case": null,
            "phase": "check",
            "diagnostics": []
        });
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &serde_json::to_vec(&missing).expect("missing count"),
                &context(),
            )
            .is_err()
        );
        missing["run_id"] = serde_json::json!("other-run");
        assert!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &serde_json::to_vec(&missing).expect("wrong identity"),
                &context(),
            )
            .is_err()
        );
        assert_eq!(
            parse_declared_report(
                &source(DiagnosticReportFormat::CoordinatorJson),
                &vec![b' '; MAX_COORDINATOR_REPORT_BYTES + 1],
                &context(),
            ),
            Err(DiagnosticParseError::LimitExceeded)
        );
    }

    fn candidate(category: ErrorCategory, actual: &str) -> FailureIndexEntry {
        new_entry(
            &context(),
            Some("same"),
            LeafStatus::Failed,
            DiagnosticExit::default(),
            None,
            Some(SourceLocation {
                file: "src/lib.rs".into(),
                line: 9,
                column: Some(2),
            }),
            category,
            Some(bounded_diagnostic_value(
                DiagnosticValueType::String,
                "expected",
            )),
            Some(bounded_diagnostic_value(
                DiagnosticValueType::String,
                actual,
            )),
            DiagnosticOrigin::Junit,
        )
    }

    #[test]
    fn normalization_is_order_independent_and_collapses_variable_actuals() {
        let one = candidate(ErrorCategory::Assertion, "actual-one");
        let two = candidate(ErrorCategory::Assertion, "actual-two");
        assert_eq!(diagnostic_fingerprint(&one), diagnostic_fingerprint(&two));
        let forward = normalize_diagnostics(vec![one.clone(), two.clone()]).expect("normalize");
        let reverse = normalize_diagnostics(vec![two, one]).expect("normalize");
        assert_eq!(forward, reverse);
        assert_eq!(forward.entries.len(), 1);
        assert_eq!(forward.entries[0].occurrences, 2);
    }

    #[test]
    fn ranking_is_explicit_then_semantic_and_deterministic() {
        let mut compiler = candidate(ErrorCategory::Compiler, "a");
        let assertion = candidate(ErrorCategory::Assertion, "a");
        let mut explicit = candidate(ErrorCategory::NetworkRequest, "a");
        explicit.origin = DiagnosticOrigin::ExplicitEvent;
        compiler.source.as_mut().expect("source").line = 10;
        let result = normalize_diagnostics(vec![compiler, assertion, explicit]).expect("normalize");
        assert_eq!(result.entries[0].origin, DiagnosticOrigin::ExplicitEvent);
        assert_eq!(result.entries[1].error_category, ErrorCategory::Assertion);
        assert_eq!(result.entries[2].error_category, ErrorCategory::Compiler);
    }

    #[test]
    fn expected_actual_previews_are_bounded_and_sensitive_values_redacted() {
        let secret = bounded_diagnostic_value(
            DiagnosticValueType::String,
            "authorization: Bearer hidden-value",
        );
        assert!(secret.redacted);
        assert!(secret.preview.is_none());
        assert_eq!(secret.sha256.len(), 64);

        let ordinary = bounded_diagnostic_value(DiagnosticValueType::String, "tokenizer ready");
        assert!(!ordinary.redacted);
        assert_eq!(ordinary.preview.as_deref(), Some("tokenizer ready"));

        let long = bounded_diagnostic_value(
            DiagnosticValueType::String,
            &"é".repeat(MAX_DIAGNOSTIC_PREVIEW_BYTES),
        );
        assert!(long.truncated);
        assert!(long.preview.as_ref().expect("preview").len() <= MAX_DIAGNOSTIC_PREVIEW_BYTES);
    }

    #[test]
    fn explicit_events_are_identity_bound_and_rank_first() {
        let event = DiagnosticEvent {
            schema: Schema2,
            run_id: "run-1".into(),
            check: "unit".into(),
            case: None,
            status: LeafStatus::Failed,
            exit: DiagnosticExit {
                code: Some(1),
                signal: None,
            },
            termination_reason: None,
            source: Some(SourceLocation {
                file: "src/lib.rs".into(),
                line: 12,
                column: None,
            }),
            error_category: ErrorCategory::Assertion,
            expected: None,
            actual: None,
            log_refs: Vec::new(),
        };
        let encoded = serde_json::to_vec(&event).expect("event");
        let parsed = parse_diagnostic_event(&encoded, &context()).expect("valid event");
        assert_eq!(diagnostic_rank(&parsed), 0);
        assert_eq!(parsed.log_refs.len(), 2);
        assert!(
            parse_diagnostic_event(
                &encoded,
                &DiagnosticContext {
                    run_id: "other".into(),
                    ..context()
                }
            )
            .is_err()
        );

        let oversized = vec![b' '; MAX_EVENT_BYTES + 1];
        assert_eq!(
            parse_diagnostic_event(&oversized, &context()),
            Err(DiagnosticParseError::InvalidEvent)
        );
    }
}
