//! Rust-native fixture driver for the retained Node formal Web UI verifier.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::audit_common::sha256_file;
use crate::audit_ledger::{
    create_directory_all_nofollow, read_bytes_nofollow, validate_directory_nofollow,
    write_bytes_nofollow,
};

const RECEIPT_LIMIT: usize = 2048;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    schema_version: u64,
    cases: Vec<MatrixCase>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixCase {
    name: String,
    path: String,
    critical: bool,
    #[serde(default)]
    critical_rules: Vec<String>,
    #[serde(default)]
    warning_rules: Vec<String>,
    #[serde(default)]
    forbidden_rules: Vec<String>,
    #[serde(default)]
    minimum_rule_counts: BTreeMap<String, usize>,
    #[serde(default)]
    svg_collision_kinds: Vec<String>,
    #[serde(default)]
    forbidden_text: Vec<String>,
    #[serde(default)]
    scrollbars: Vec<ScrollbarExpectation>,
    #[serde(default)]
    forbidden_scrollbar_suffixes: Vec<String>,
    #[serde(default)]
    metric_contains: Vec<String>,
    #[serde(default)]
    target: Map<String, Value>,
    final_path: Option<String>,
    continuation_focus: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrollbarExpectation {
    selector_suffix: String,
    axis: String,
    depth: u64,
}

#[derive(Clone, Debug)]
struct HttpRequest {
    path: String,
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct HttpResponse {
    status: &'static str,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl HttpResponse {
    fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn status(status: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
}

fn html_page(body: &str, css: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>formal fixture</title><style>body {{ margin: 0; font: 16px system-ui, sans-serif; background: #fff; color: #111; }} main {{ padding: 20px; }} {css}</style></head><body><main data-ui-continuation-anchor>{body}</main></body></html>"
    )
}

type HttpHandler = Arc<dyn Fn(HttpRequest) -> HttpResponse + Send + Sync>;

struct StaticServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StaticServer {
    fn start(pages: BTreeMap<String, Vec<u8>>) -> Result<Self, String> {
        let pages = Arc::new(pages);
        Self::start_handler(Arc::new(move |request| {
            let path = request
                .path
                .split('?')
                .next()
                .unwrap_or("/")
                .trim_start_matches('/');
            match pages.get(path) {
                Some(body) => HttpResponse {
                    status: "200 OK",
                    content_type: if path.ends_with(".json") {
                        "application/json; charset=utf-8"
                    } else if path.ends_with(".txt") {
                        "text/plain; charset=utf-8"
                    } else {
                        "text/html; charset=utf-8"
                    },
                    headers: Vec::new(),
                    body: body.clone(),
                    delay: Duration::ZERO,
                },
                None => HttpResponse::status("404 Not Found", b"not found".to_vec()),
            }
        }))
    }

    fn start_handler(handler: HttpHandler) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("cannot bind self-test server: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot configure self-test server: {error}"))?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_worker.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        thread::spawn(move || serve_connection(stream, &handler));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_connection(mut stream: TcpStream, handler: &HttpHandler) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request = [0u8; 16 * 1024];
    let Ok(count) = stream.read(&mut request) else {
        return;
    };
    let raw = String::from_utf8_lossy(&request[..count]);
    let path = raw
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let headers = raw
        .lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let response = handler(HttpRequest { path, headers });
    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }
    let mut header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    for (name, value) in response.headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("\r\n");
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tooling package is nested under the repository root")
        .to_owned()
}

fn load_json(path: &Path, root: &Path, label: &str) -> Result<Value, String> {
    let bytes = read_bytes_nofollow(path, Some(root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{label} is missing: {}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{label} is invalid JSON: {error}"))
}

fn load_pages(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let fixture_root = root.join("skills/formal-web-ui-verification/fixtures/self-test");
    let value = load_json(
        &fixture_root.join("pages.json"),
        &fixture_root,
        "formal UI pages",
    )?;
    value
        .as_object()
        .ok_or_else(|| "formal UI pages must be a JSON object".to_owned())?
        .iter()
        .map(|(name, body)| {
            body.as_str()
                .map(|body| (name.clone(), body.as_bytes().to_vec()))
                .ok_or_else(|| format!("formal UI page {name} must be text"))
        })
        .collect()
}

fn load_matrix(root: &Path) -> Result<Matrix, String> {
    let fixture_root = root.join("skills/formal-web-ui-verification/fixtures/self-test");
    let value = load_json(
        &fixture_root.join("matrix.json"),
        &fixture_root,
        "formal UI matrix",
    )?;
    let matrix: Matrix = serde_json::from_value(value)
        .map_err(|error| format!("invalid formal UI matrix: {error}"))?;
    if matrix.schema_version != 1 || matrix.cases.is_empty() {
        return Err("formal UI matrix schema or cases are invalid".to_owned());
    }
    let mut names = BTreeSet::new();
    for case in &matrix.cases {
        if case.name.is_empty() || !names.insert(&case.name) {
            return Err("formal UI matrix case names must be unique and non-empty".to_owned());
        }
    }
    Ok(matrix)
}

fn playwright_module_dir(root: &Path) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES") {
        candidates.push(PathBuf::from(explicit));
    }
    candidates.push(root.join("ci/playwright/node_modules"));
    if let Some(node_path) = std::env::var_os("NODE_PATH") {
        candidates.extend(std::env::split_paths(&node_path));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates
            .push(PathBuf::from(home).join(
                ".cache/codex-runtimes/codex-primary-runtime/dependencies/node/node_modules",
            ));
    }
    let mut checked = Vec::new();
    for candidate in candidates {
        if checked.contains(&candidate) {
            continue;
        }
        checked.push(candidate.clone());
        if candidate.join("playwright/package.json").is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "Playwright is unavailable for the formal UI self-test; checked {checked:?}"
    ))
}

fn unique_work_dir(parent: Option<&Path>) -> Result<PathBuf, String> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = parent
        .map(Path::to_owned)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("formal-web-ui-rust-self-test-{token}"));
    create_directory_all_nofollow(&root, 0o700).map_err(|error| error.to_string())?;
    Ok(root)
}

fn wait_child(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "formal UI verifier exceeded {} seconds",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)
            .map_err(|error| error.to_string())?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)
            .map_err(|error| error.to_string())?;
    }
    Ok((status, stdout, stderr))
}

fn default_target_contract() -> Map<String, Value> {
    json!({
        "journeys":[{"id":"fixture-primary","name":"Exercise the fixture","frequencyPercent":100,"risk":"normal","rationale":"Rust self-test target"}],
        "primaryJourney":"fixture-primary",
        "regions":[{"selector":"main","role":"primary-content","journey":"fixture-primary","name":"Fixture content"}],
        "theme":"light","reviewInputs":[{"path":"SKILL.md","kind":"ui-code"}],
    })
    .as_object()
    .unwrap()
    .clone()
}

fn contracted_config(root: &Path, mut config: Value) -> Value {
    let object = config.as_object_mut().expect("fixture config is an object");
    object
        .entry("repoRoot")
        .or_insert_with(|| json!(root.join("skills/formal-web-ui-verification")));
    object
        .entry("performance")
        .or_insert_with(|| json!({"ttfbMs":10000,"lcpMs":10000,"ttfbLocalOnly":false}));
    let defaults = default_target_contract();
    let target_defaults = object.entry("targetDefaults").or_insert_with(|| json!({}));
    if let Some(target_defaults) = target_defaults.as_object_mut() {
        for (key, value) in &defaults {
            target_defaults
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(targets) = object.get_mut("targets").and_then(Value::as_array_mut) {
        for target in targets.iter_mut().filter_map(Value::as_object_mut) {
            for (key, value) in &defaults {
                target.entry(key.clone()).or_insert_with(|| value.clone());
            }
            if let Some(states) = target.get_mut("states").and_then(Value::as_array_mut) {
                for state in states.iter_mut().filter_map(Value::as_object_mut) {
                    if state.contains_key("continuation") {
                        continue;
                    }
                    let has_trigger = state
                        .get("actions")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .any(|action| {
                            action
                                .get("action")
                                .and_then(Value::as_str)
                                .is_some_and(|action| {
                                    ["click", "press", "check", "uncheck", "selectOption"]
                                        .contains(&action)
                                })
                        });
                    if has_trigger {
                        state.insert(
                            "continuation".to_owned(),
                            json!({"kind":"in-page","anchor":"main","focusWithin":"main"}),
                        );
                    }
                }
            }
        }
    }
    config
}

fn build_matrix_config(source_root: &Path, server: &StaticServer, matrix: &Matrix) -> Value {
    let defaults = default_target_contract();
    let targets = matrix
        .cases
        .iter()
        .map(|case| {
            let mut target = case.target.clone();
            target.insert("name".to_owned(), json!(case.name));
            target.insert(
                "url".to_owned(),
                json!(format!("{}/{}", server.base_url(), case.path)),
            );
            for (key, value) in &defaults {
                target.entry(key.clone()).or_insert_with(|| value.clone());
            }
            Value::Object(target)
        })
        .collect::<Vec<_>>();
    json!({
        "repoRoot":source_root.join("skills/formal-web-ui-verification"),
        "performance":{"ttfbMs":10000,"lcpMs":10000,"ttfbLocalOnly":false},
        "targets":targets,
        "viewports":[{"name":"mobile","width":390,"height":844}],
        "maxPageCount":matrix.cases.len(),
    })
}

fn sha_text(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn receipt_paths(receipt: &Value) -> Result<(PathBuf, PathBuf), String> {
    let artifacts = receipt
        .get("artifacts")
        .and_then(Value::as_object)
        .ok_or_else(|| "receipt must name report artifacts".to_owned())?;
    if let Some(directory) = artifacts.get("directory").and_then(Value::as_str) {
        return Ok((
            Path::new(directory).join(
                artifacts
                    .get("json")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Path::new(directory).join(
                artifacts
                    .get("markdown")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
        ));
    }
    Ok((
        PathBuf::from(
            artifacts
                .get("json")
                .and_then(Value::as_str)
                .ok_or_else(|| "receipt JSON artifact path is missing".to_owned())?,
        ),
        PathBuf::from(
            artifacts
                .get("markdown")
                .and_then(Value::as_str)
                .ok_or_else(|| "receipt Markdown artifact path is missing".to_owned())?,
        ),
    ))
}

fn validate_complete_report(report: &Value, markdown: &str, output: &Path) -> Result<(), String> {
    if report.get("schemaVersion") != Some(&json!(2))
        || !report.get("runId").is_some_and(Value::is_string)
        || !report.get("pages").is_some_and(Value::is_array)
        || !report.get("findings").is_some_and(Value::is_array)
        || !report.get("coverage").is_some_and(Value::is_object)
        || !report.get("startedAt").is_some_and(Value::is_string)
        || !report.get("endedAt").is_some_and(Value::is_string)
        || !sha_text(report.pointer("/evidence/verifier/sha256"))
        || !sha_text(report.pointer("/evidence/config/sha256"))
        || report.pointer("/plan/widthCoverage") != Some(&json!("sampled-only"))
        || !report.get("review").is_some_and(Value::is_object)
        || !sha_text(report.pointer("/review/queueSha256"))
    {
        return Err("formal verifier JSON omitted complete report evidence".to_owned());
    }
    for heading in [
        "# Formal Web UI Verification Report",
        "## Target Coverage",
        "## Changed Visual Review",
        "## Findings",
    ] {
        if !markdown.contains(heading) {
            return Err(format!("formal Markdown report lacks {heading}"));
        }
    }
    let pages = report["pages"].as_array().unwrap();
    if report
        .pointer("/coverage/cells")
        .and_then(Value::as_array)
        .is_none_or(|cells| cells.len() != pages.len())
    {
        return Err("formal coverage cells do not match page results".to_owned());
    }
    for page in pages {
        if page.get("requestedPath").is_none()
            || page.get("finalPath").is_none()
            || !page.get("sourceBinding").is_some_and(Value::is_object)
            || !page.get("startedAt").is_some_and(Value::is_string)
            || !page.get("endedAt").is_some_and(Value::is_string)
        {
            return Err("formal page omitted path, source binding, or timing evidence".to_owned());
        }
        if page.get("outcome") != Some(&json!("checked")) {
            continue;
        }
        for role in ["viewport", "fullPage"] {
            let screenshot = page
                .pointer(&format!("/screenshots/{role}"))
                .and_then(Value::as_object)
                .ok_or_else(|| format!("checked page lacks {role} screenshot"))?;
            let path = screenshot
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{role} screenshot path is missing"))?;
            read_bytes_nofollow(&path, Some(output))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("{role} screenshot is missing"))?;
            if screenshot.get("mime") != Some(&json!("image/png"))
                || screenshot.get("sha256") != Some(&json!(sha256_file(&path)?))
            {
                return Err(format!("{role} screenshot evidence is invalid"));
            }
        }
    }
    let progress = report
        .pointer("/execution/progressPath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "progress artifact path is missing".to_owned())?;
    let progress = read_bytes_nofollow(&progress, Some(output))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "progress artifact is missing".to_owned())?;
    let rows = String::from_utf8(progress).map_err(|_| "progress artifact is not UTF-8")?;
    let values = rows
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    if values.first().and_then(|value| value.get("kind")) != Some(&json!("run-start"))
        || values.last().and_then(|value| value.get("kind")) != Some(&json!("run-complete"))
    {
        return Err("progress artifact does not bind the complete run".to_owned());
    }
    Ok(())
}

fn validate_setup_report(report: &Value, markdown: &str) -> Result<(), String> {
    if report.get("status") != Some(&json!("setup-failure"))
        || !report.get("runId").is_some_and(Value::is_string)
        || !report
            .pointer("/error/message")
            .is_some_and(Value::is_string)
        || !report.get("startedAt").is_some_and(Value::is_string)
        || !report.get("endedAt").is_some_and(Value::is_string)
        || !sha_text(report.pointer("/evidence/verifier/sha256"))
        || !markdown.contains("# Formal Web UI Verification Setup Failure")
        || !markdown.contains("## Diagnostic")
    {
        return Err("setup-failure artifacts omitted diagnostic evidence".to_owned());
    }
    Ok(())
}

fn run_verifier(
    source_root: &Path,
    config: &Value,
    output: &Path,
    expected_exits: &[i32],
    timeout: Duration,
    extra: &[&str],
) -> Result<Value, String> {
    create_directory_all_nofollow(output, 0o700).map_err(|error| error.to_string())?;
    let config_path = output.join("formal-web-ui.json");
    let json_out = output.join("report.json");
    let markdown_out = output.join("report.md");
    crate::audit_queue::write_json(&config_path, config)?;
    let verifier =
        source_root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
    let node = std::env::var_os("FORMAL_WEB_UI_NODE").unwrap_or_else(|| "node".into());
    let playwright = playwright_module_dir(source_root)?;
    let mut command = Command::new(node);
    command
        .arg(verifier)
        .arg("--playwright-module-dir")
        .arg(playwright)
        .arg("--config")
        .arg(&config_path)
        .arg("--json-out")
        .arg(&json_out)
        .arg("--markdown-out")
        .arg(&markdown_out)
        .args(["--fail-on", "critical"])
        .args(extra)
        .env_remove("NODE_PATH")
        .env_remove("DEVCOORDINATOR_EVIDENCE_DIR")
        .env_remove("DEVCOORDINATOR_RUN_ID")
        .env_remove("DEVCOORDINATOR_CHECK_NAME")
        .env("TMPDIR", output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|error| format!("cannot launch formal verifier: {error}"))?;
    let (status, stdout, stderr) = wait_child(child, timeout)?;
    let exit = status.code().unwrap_or(2);
    if !expected_exits.contains(&exit) {
        return Err(format!(
            "formal verifier exit mismatch: expected {expected_exits:?}, got {exit}; stderr={} stdout={}",
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(500)
                .collect::<String>(),
            String::from_utf8_lossy(&stdout)
                .chars()
                .take(500)
                .collect::<String>(),
        ));
    }
    if !stderr.is_empty() {
        return Err(format!(
            "default formal verifier invocation emitted stderr: {}",
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    if stdout.len() > RECEIPT_LIMIT {
        return Err("default formal verifier receipt exceeded 2048 bytes".to_owned());
    }
    let receipt_text =
        String::from_utf8(stdout).map_err(|_| "formal verifier receipt is not UTF-8".to_owned())?;
    if receipt_text.trim().is_empty() || receipt_text.trim().contains('\n') {
        return Err("default stdout must be exactly one bounded receipt line".to_owned());
    }
    if receipt_text.contains("# Formal Web UI Verification") || receipt_text.contains("## Findings")
    {
        return Err("default stdout leaked the report body".to_owned());
    }
    let receipt: Value = serde_json::from_str(receipt_text.trim())
        .map_err(|error| format!("default stdout is not a JSON receipt: {error}"))?;
    if receipt.get("exitCode") != Some(&json!(exit)) {
        return Err("formal verifier receipt exit code mismatch".to_owned());
    }
    let (receipt_json, receipt_markdown) = receipt_paths(&receipt)?;
    if receipt_json != json_out || receipt_markdown != markdown_out {
        return Err(
            "formal verifier receipt does not point to the exact report artifacts".to_owned(),
        );
    }
    let report_bytes = read_bytes_nofollow(&json_out, Some(output))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "formal JSON report is missing".to_owned())?;
    let markdown = read_bytes_nofollow(&markdown_out, Some(output))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "formal Markdown report is missing".to_owned())?;
    let report: Value = serde_json::from_slice(&report_bytes)
        .map_err(|error| format!("formal JSON report is invalid: {error}"))?;
    let markdown =
        String::from_utf8(markdown).map_err(|_| "formal Markdown report is not UTF-8")?;
    if exit == 2 {
        validate_setup_report(&report, &markdown)?;
    } else {
        validate_complete_report(&report, &markdown, output)?;
        if expected_exits.len() > 1 {
            let critical = report["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|finding| finding["severity"] == "critical");
            if exit != i32::from(critical) {
                return Err(
                    "formal verifier exit disagrees with structured critical findings".to_owned(),
                );
            }
        }
    }
    Ok(report)
}

fn page_rules(page: &Value) -> Vec<(&str, &str)> {
    page.get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|finding| {
            Some((
                finding.get("rule")?.as_str()?,
                finding.get("severity")?.as_str()?,
            ))
        })
        .collect()
}

fn visible_scrollbars(page: &Value) -> Vec<&Map<String, Value>> {
    page.pointer("/metrics/visibleScrollbars")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect()
}

fn validate_matrix_report(report: &Value, matrix: &Matrix) -> Result<(), String> {
    let pages = report
        .get("pages")
        .and_then(Value::as_array)
        .ok_or_else(|| "matrix report pages are missing".to_owned())?;
    let by_name = pages
        .iter()
        .filter_map(|page| {
            let declared = page.pointer("/target/name")?.as_str()?;
            let base = declared.split_once(" [").map_or(declared, |(name, _)| name);
            Some((base.to_owned(), page))
        })
        .collect::<BTreeMap<_, _>>();
    if by_name.len() != matrix.cases.len() {
        return Err(format!(
            "matrix report page count mismatch: expected {}, got {}",
            matrix.cases.len(),
            by_name.len()
        ));
    }
    let report_text = serde_json::to_string(report).map_err(|error| error.to_string())?;
    for case in &matrix.cases {
        let page = by_name
            .get(&case.name)
            .ok_or_else(|| format!("matrix report lacks {}", case.name))?;
        if page.get("outcome") != Some(&json!("checked")) {
            return Err(format!("matrix case {} was not checked", case.name));
        }
        let rules = page_rules(page);
        let has_critical = rules.iter().any(|(_, severity)| *severity == "critical");
        if has_critical != case.critical {
            return Err(format!(
                "matrix case {} critical result mismatch: expected {}, got {}",
                case.name, case.critical, has_critical
            ));
        }
        for rule in &case.critical_rules {
            if !rules
                .iter()
                .any(|(actual, severity)| actual == rule && *severity == "critical")
            {
                return Err(format!(
                    "matrix case {} lacks critical rule {rule}",
                    case.name
                ));
            }
        }
        for rule in &case.warning_rules {
            if !rules
                .iter()
                .any(|(actual, severity)| actual == rule && *severity == "warning")
            {
                return Err(format!(
                    "matrix case {} lacks warning rule {rule}",
                    case.name
                ));
            }
        }
        for rule in &case.forbidden_rules {
            if rules.iter().any(|(actual, _)| actual == rule) {
                return Err(format!(
                    "matrix case {} unexpectedly produced {rule}",
                    case.name
                ));
            }
        }
        for expected_kind in &case.svg_collision_kinds {
            let found = page
                .get("findings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|finding| finding["rule"] == "svg-internal-overlap")
                .flat_map(|finding| {
                    finding
                        .pointer("/evidence/collisions")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .any(|collision| {
                    collision.get("kind").and_then(Value::as_str) == Some(expected_kind)
                });
            if !found {
                return Err(format!(
                    "matrix case {} lacks SVG collision kind {expected_kind}",
                    case.name
                ));
            }
        }
        for (rule, minimum) in &case.minimum_rule_counts {
            let count = rules.iter().filter(|(actual, _)| actual == rule).count();
            if count < *minimum {
                return Err(format!(
                    "matrix case {} produced {count} {rule} findings, expected at least {minimum}",
                    case.name
                ));
            }
        }
        for secret in &case.forbidden_text {
            if report_text.contains(secret) {
                return Err(format!(
                    "matrix case {} leaked forbidden report text",
                    case.name
                ));
            }
        }
        let scrollbars = visible_scrollbars(page);
        for expected in &case.scrollbars {
            if !scrollbars.iter().any(|entry| {
                entry
                    .get("selector")
                    .and_then(Value::as_str)
                    .is_some_and(|selector| selector.contains(&expected.selector_suffix))
                    && entry.get("axis") == Some(&json!(expected.axis))
                    && entry.get("sameAxisDepth") == Some(&json!(expected.depth))
            }) {
                return Err(format!(
                    "matrix case {} lacks scrollbar {} {} depth {}",
                    case.name, expected.selector_suffix, expected.axis, expected.depth
                ));
            }
        }
        for suffix in &case.forbidden_scrollbar_suffixes {
            if scrollbars.iter().any(|entry| {
                entry
                    .get("selector")
                    .and_then(Value::as_str)
                    .is_some_and(|selector| selector.contains(suffix))
            }) {
                return Err(format!(
                    "matrix case {} invented scrollbar {suffix}",
                    case.name
                ));
            }
        }
        let metrics = serde_json::to_string(page.get("metrics").unwrap_or(&Value::Null))
            .map_err(|error| error.to_string())?;
        for token in &case.metric_contains {
            if !metrics
                .to_ascii_lowercase()
                .contains(&token.to_ascii_lowercase())
            {
                return Err(format!("matrix case {} metrics lack {token}", case.name));
            }
        }
        if let Some(expected) = &case.final_path
            && page.get("finalPath") != Some(&json!(expected))
        {
            return Err(format!("matrix case {} final path mismatch", case.name));
        }
        if let Some(expected) = case.continuation_focus
            && page.pointer("/continuation/evidence/focusSatisfied") != Some(&json!(expected))
        {
            return Err(format!(
                "matrix case {} continuation focus mismatch",
                case.name
            ));
        }
    }
    Ok(())
}

fn has_rule(report: &Value, rule: &str, severity: Option<&str>) -> bool {
    report
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|finding| {
            finding.get("rule") == Some(&json!(rule))
                && severity.is_none_or(|severity| finding.get("severity") == Some(&json!(severity)))
        })
}

fn require_rule(report: &Value, rule: &str, severity: &str) -> Result<(), String> {
    if has_rule(report, rule, Some(severity)) {
        Ok(())
    } else {
        Err(format!("formal report lacks {severity} rule {rule}"))
    }
}

fn no_critical(report: &Value) -> Result<(), String> {
    if report
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|finding| finding.get("severity") == Some(&json!("critical")))
    {
        Err("formal report contains an unexpected critical finding".to_owned())
    } else {
        Ok(())
    }
}

fn page_by_state<'a>(report: &'a Value, state: &str) -> Option<&'a Value> {
    report
        .get("pages")?
        .as_array()?
        .iter()
        .find(|page| page.pointer("/target/stateName") == Some(&json!(state)))
}

fn run_state_and_wait_phase(
    root: &Path,
    server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    let base = server.base_url();
    let mut scenarios = 0usize;
    run_node_probe(
        root,
        &format!(
            "process.env.FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES = {}; await import({});",
            json!(playwright_module_dir(root)?),
            json!(root.join("rust/tooling/tests/formal_occlusion.mjs")),
        ),
        timeout,
    )?;
    scenarios += 1;
    let popup_clean = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/popup-scroll-probe-clean.html"),"regions":[{"selector":"#page-heading","role":"primary-content","journey":"fixture-primary"}]}],
                "viewports":[{"name":"reported-narrow","width":390,"height":921}],
                "scroll":false
            }),
        ),
        &work.join("popup-scroll-probe-clean"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&popup_clean)?;
    if has_rule(&popup_clean, "occluded", None)
        || has_rule(&popup_clean, "partially-occluded", None)
    {
        return Err(
            "an unobstructed heading was blamed for scroll state left by an earlier popup option"
                .to_owned(),
        );
    }
    scenarios += 1;

    let popup_covered = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/popup-scroll-probe-covered.html"),"regions":[{"selector":"#page-heading","role":"primary-content","journey":"fixture-primary"}]}],
                "viewports":[{"name":"reported-narrow","width":390,"height":921}],
                "scroll":false
            }),
        ),
        &work.join("popup-scroll-probe-covered"),
        &[1],
        timeout,
        &[],
    )?;
    if !popup_covered
        .get("findings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|finding| {
            finding.get("rule") == Some(&json!("occluded"))
                && finding.get("severity") == Some(&json!("critical"))
                && finding.get("selector") == Some(&json!("#page-heading"))
        })
    {
        return Err("a genuinely covered heading did not retain its critical occlusion".to_owned());
    }
    scenarios += 1;

    let dialog_cases = [
        ("modal-scroll-reachable", None),
        ("modal-scroll-locked", Some("clipped-by-ancestor")),
        ("modal-active-occlusion", Some("occluded")),
        ("fixed-unscrollable-cut", Some("fixed-offscreen-cut")),
    ];
    let dialog_report = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":dialog_cases.iter().map(|(name,_)| json!({
                    "name":name,"url":format!("{base}/{name}.html"),
                    "regions":[{"selector":if name.starts_with("modal") {"dialog"} else {"main"},"role":"primary-content","journey":"fixture-primary"}]
                })).collect::<Vec<_>>(),
                "viewports":[{"name":"dialog-narrow","width":390,"height":844},{"name":"dialog-wide","width":1440,"height":900}]
            }),
        ),
        &work.join("dialog-reachability"),
        &[1],
        timeout,
        &[],
    )?;
    let pages = dialog_report["pages"]
        .as_array()
        .ok_or("dialog report omitted pages")?;
    if pages.len() != 8 {
        return Err("dialog reachability requires all eight desktop/mobile cells".to_owned());
    }
    for page in pages {
        let name = page
            .pointer("/target/name")
            .and_then(Value::as_str)
            .ok_or("dialog target omitted name")?;
        let (_, expected) = dialog_cases
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .ok_or("unknown dialog fixture")?;
        if let Some(rule) = expected {
            require_rule(page, rule, "critical")?;
        } else {
            no_critical(page)?;
        }
    }
    scenarios += pages.len();
    let narrow = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":[{"url":format!("{base}/device-responsive.html")}],"viewports":[{"name":"narrow-browser","width":390,"height":844}]}),
        ),
        &work.join("narrow-not-device"),
        &[0],
        timeout,
        &[],
    )?;
    if has_rule(&narrow, "clipped-x", None) {
        return Err("narrow browser was incorrectly treated as a mobile device".to_owned());
    }
    scenarios += 1;
    let device = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":[{"url":format!("{base}/device-responsive.html")}],"viewports":[{"name":"iphone","device":"iPhone 13"}]}),
        ),
        &work.join("mobile-device"),
        &[1],
        timeout,
        &[],
    )?;
    require_rule(&device, "clipped-x", "critical")?;
    scenarios += 1;

    let interaction = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/interaction-states.html"),"states":[{
                    "name":"actions-open","actions":[{"action":"click","selector":"#open-bad"}],
                    "waitFor":{"selector":"#bad-panel:not([hidden])","settleMs":25},
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("interaction-state"),
        &[1],
        timeout,
        &[],
    )?;
    let base_page = page_by_state(&interaction, "base")
        .ok_or_else(|| "interaction report lacks base state".to_owned())?;
    let open_page = page_by_state(&interaction, "actions-open")
        .ok_or_else(|| "interaction report lacks opened state".to_owned())?;
    if page_rules(base_page)
        .iter()
        .any(|(rule, _)| *rule == "clipped-x")
        || !page_rules(open_page)
            .iter()
            .any(|(rule, _)| *rule == "clipped-x")
        || interaction.to_string().contains("\"actions\"")
    {
        return Err("interaction state visibility or action redaction regressed".to_owned());
    }
    scenarios += 1;
    let opened_menu = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{
                    "name":"project-actions","url":format!("{base}/transient-menu-offcanvas.html"),"includeBase":false,
                    "states":[{
                        "name":"actions-open","actions":[{"action":"click","selector":"#open-actions"}],
                        "waitFor":{"selector":"#actions-menu:not([hidden])","renderFrames":2,"timeoutMs":1000},
                        "continuation":{"kind":"in-page","anchor":"#first-action","focusWithin":"#actions-menu"}
                    }]
                }],
                "viewports":[{"name":"reported-narrow","width":601,"height":921}],
                "requiredCoverage":[{"target":"project-actions","state":"actions-open","viewport":"reported-narrow","width":601}]
            }),
        ),
        &work.join("transient-menu-open"),
        &[1],
        timeout,
        &[],
    )?;
    require_rule(&opened_menu, "offcanvas-cut", "critical")?;
    if opened_menu.pointer("/coverage/requiredCoverage/failed") != Some(&json!(false))
        || opened_menu.pointer("/coverage/requiredCoverage/satisfiedCount") != Some(&json!(1))
    {
        return Err("exact opened menu coverage was not satisfied".to_owned());
    }
    let opened_evidence = load_json(
        &work.join("transient-menu-open/journey-evidence.json"),
        work,
        "opened menu journey evidence",
    )?;
    if opened_evidence.pointer("/coverage/requiredCoverage/entries/0/status")
        != Some(&json!("satisfied"))
    {
        return Err("journey evidence omitted the required-cell disposition".to_owned());
    }
    scenarios += 1;

    let missing_state = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"name":"project-actions","url":format!("{base}/transient-menu-contained.html")}],
                "viewports":[{"name":"reported-narrow","width":601,"height":921}],
                "requiredCoverage":[{"target":"project-actions","state":"actions-open","viewport":"reported-narrow","width":601}]
            }),
        ),
        &work.join("required-state-missing"),
        &[3],
        timeout,
        &[],
    )?;
    no_critical(&missing_state)?;
    if missing_state.pointer("/coverage/requiredCoverage/entries/0/status")
        != Some(&json!("missing"))
        || !missing_state
            .pointer("/coverage/requiredCoverage/entries/0/reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("actions-open") && reason.contains("601px"))
    {
        return Err(
            "a missing required transient state did not fail coverage explicitly".to_owned(),
        );
    }
    scenarios += 1;

    let wrong_width = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{
                    "name":"project-actions","url":format!("{base}/transient-menu-contained.html"),"includeBase":false,
                    "states":[{
                        "name":"actions-open","actions":[{"action":"click","selector":"#open-actions"}],
                        "waitFor":{"selector":"#actions-menu:not([hidden])","renderFrames":2,"timeoutMs":1000},
                        "continuation":{"kind":"in-page","anchor":"#first-action","focusWithin":"#actions-menu"}
                    }]
                }],
                "viewports":[{"name":"reported-narrow","width":961,"height":921}],
                "requiredCoverage":[{"target":"project-actions","state":"actions-open","viewport":"reported-narrow","width":601}]
            }),
        ),
        &work.join("required-width-missing"),
        &[3],
        timeout,
        &[],
    )?;
    no_critical(&wrong_width)?;
    if wrong_width.pointer("/coverage/requiredCoverage/entries/0/status") != Some(&json!("missing"))
    {
        return Err("a required CSS width accepted a differently sized viewport".to_owned());
    }
    scenarios += 1;

    let ambiguous_cell = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[
                    {
                        "name":"project-actions","url":format!("{base}/transient-menu-contained.html?copy=one"),"includeBase":false,
                        "states":[{
                            "name":"actions-open","actions":[{"action":"click","selector":"#open-actions"}],
                            "waitFor":{"selector":"#actions-menu:not([hidden])","renderFrames":2,"timeoutMs":1000},
                            "continuation":{"kind":"in-page","anchor":"#first-action","focusWithin":"#actions-menu"}
                        }]
                    },
                    {
                        "name":"project-actions","url":format!("{base}/transient-menu-contained.html?copy=two"),"includeBase":false,
                        "states":[{
                            "name":"actions-open","actions":[{"action":"click","selector":"#open-actions"}],
                            "waitFor":{"selector":"#actions-menu:not([hidden])","renderFrames":2,"timeoutMs":1000},
                            "continuation":{"kind":"in-page","anchor":"#first-action","focusWithin":"#actions-menu"}
                        }]
                    }
                ],
                "viewports":[{"name":"reported-narrow","width":601,"height":921}],
                "requiredCoverage":[{"target":"project-actions","state":"actions-open","viewport":"reported-narrow","width":601}]
            }),
        ),
        &work.join("required-cell-ambiguous"),
        &[3],
        timeout,
        &[],
    )?;
    no_critical(&ambiguous_cell)?;
    if ambiguous_cell.pointer("/coverage/requiredCoverage/entries/0/status")
        != Some(&json!("ambiguous"))
        || ambiguous_cell
            .pointer("/coverage/requiredCoverage/entries/0/matchingCellIds")
            .and_then(Value::as_array)
            .is_none_or(|matches| matches.len() != 2)
    {
        return Err("an ambiguous required cell did not fail coverage explicitly".to_owned());
    }
    scenarios += 1;

    let contained_menu = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{
                    "name":"project-actions","url":format!("{base}/transient-menu-contained.html"),"includeBase":false,
                    "states":[{
                        "name":"actions-open","actions":[{"action":"click","selector":"#open-actions"}],
                        "waitFor":{"selector":"#actions-menu:not([hidden])","renderFrames":2,"timeoutMs":1000},
                        "continuation":{"kind":"in-page","anchor":"#first-action","focusWithin":"#actions-menu"}
                    }]
                }],
                "viewports":[{"name":"reported-narrow","width":601,"height":921}],
                "requiredCoverage":[{"target":"project-actions","state":"actions-open","viewport":"reported-narrow","width":601}]
            }),
        ),
        &work.join("required-cell-contained"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&contained_menu)?;
    if contained_menu.pointer("/coverage/requiredCoverage/entries/0/status")
        != Some(&json!("satisfied"))
    {
        return Err("an exact contained required cell did not pass coverage".to_owned());
    }
    scenarios += 1;

    let failed = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/interaction-states.html"),"includeBase":false,"states":[{
                    "name":"missing-trigger","actions":[{"action":"fill","selector":"#does-not-exist","value":"SECRET_ACTION_VALUE_MUST_NOT_LEAK","timeoutMs":100}]
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("failed-interaction"),
        &[3],
        timeout,
        &[],
    )?;
    if !failed
        .pointer("/coverage/failed")
        .is_some_and(|value| value == true)
        || failed
            .to_string()
            .contains("SECRET_ACTION_VALUE_MUST_NOT_LEAK")
        || failed
            .pointer("/pages/0/actionTimings/0/durationMs")
            .and_then(Value::as_u64)
            .is_none_or(|duration| duration >= 500)
    {
        return Err(
            "failed interaction coverage, redaction, or zero-wait behavior regressed".to_owned(),
        );
    }
    scenarios += 1;

    let ownership = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/conditional-ownership.html"),"includeBase":false,
                    "journeys":[{"id":"general","frequencyPercent":95,"risk":"normal"},{"id":"special","frequencyPercent":5,"risk":"normal"}],
                    "primaryJourney":"general","regions":[{"selector":"main","role":"primary-content","journey":"general"}],
                    "states":[
                        {"name":"general-conditional","actions":[{"action":"click","selector":"#special","ownerJourney":"special","ownerState":"specialized"}]},
                        {"name":"specialized","primaryJourney":"special","priorityOverrideReason":"Specialized ownership fixture",
                         "regions":[{"selector":"main","role":"primary-content","journey":"special"}],
                         "actions":[{"action":"click","selector":"#show-special"},{"action":"click","selector":"#special","ownerJourney":"special","ownerState":"specialized"}],
                         "continuation":{"kind":"in-page","anchor":"#special-form","focusWithin":"#special-form","triggerActionIndex":1}}
                    ]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("conditional-ownership"),
        &[0],
        timeout,
        &[],
    )?;
    let general = page_by_state(&ownership, "general-conditional")
        .ok_or_else(|| "conditional ownership lacks general state".to_owned())?;
    let specialized = page_by_state(&ownership, "specialized")
        .ok_or_else(|| "conditional ownership lacks specialized state".to_owned())?;
    if general
        .get("handoffs")
        .and_then(Value::as_array)
        .is_none_or(|items| items.len() != 1 || items[0].get("locatorWaitMs") != Some(&json!(0)))
        || specialized.get("outcome") != Some(&json!("checked"))
    {
        return Err("conditional control ownership did not hand off immediately".to_owned());
    }
    scenarios += 1;

    let missing_owner = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/conditional-ownership.html"),"includeBase":false,"states":[{
                    "name":"general-conditional","actions":[{"action":"click","selector":"#special","ownerJourney":"fixture-primary","ownerState":"missing-owner-state"}]
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("conditional-owner-missing"),
        &[3],
        timeout,
        &[],
    )?;
    if missing_owner.pointer("/pages/0/outcome") != Some(&json!("journey_contract_error")) {
        return Err("missing conditional owner did not fail the journey contract".to_owned());
    }
    scenarios += 1;

    let event_ready = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/event-readiness.html"),"includeBase":false,"states":[{
                    "name":"event-ready","actions":[{"action":"click","selector":"#start"}],
                    "waitFor":{"selector":"#ready","errorSelector":"#error","responseUrl":"**/readback.json","renderFrames":2,"timeoutMs":2000},
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("event-readiness"),
        &[0],
        timeout,
        &[],
    )?;
    let wait_kinds = event_ready
        .pointer("/pages/0/waitEvidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("kind").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if !["response", "ready-or-error-dom", "render-frames"]
        .iter()
        .all(|kind| wait_kinds.contains(kind))
    {
        return Err("event readiness evidence is incomplete".to_owned());
    }
    scenarios += 1;

    let event_error = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/event-readiness.html"),"includeBase":false,"states":[{
                    "name":"event-error","actions":[{"action":"click","selector":"#fail"}],
                    "waitFor":{"selector":"#ready","errorSelector":"#error","timeoutMs":1000},
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("event-readiness-error"),
        &[3],
        timeout,
        &[],
    )?;
    if event_error.pointer("/pages/0/outcome") != Some(&json!("interaction_error")) {
        return Err("ready-or-error DOM race did not surface the error state".to_owned());
    }
    scenarios += 1;

    let missing_event = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/event-readiness.html"),"includeBase":false,"states":[{
                    "name":"event-missing","actions":[{"action":"click","selector":"#fail"}],
                    "waitFor":{"selector":"#never-ready","timeoutMs":150},
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("event-missing"),
        &[3],
        timeout,
        &[],
    )?;
    if missing_event
        .pointer("/pages/0/durationMs")
        .and_then(Value::as_u64)
        .is_none_or(|duration| !(100..=1000).contains(&duration))
    {
        return Err("missing event did not fail at its bounded deadline".to_owned());
    }
    scenarios += 1;

    let slow_event = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/event-readiness.html"),"includeBase":false,"states":[{
                    "name":"event-slow-valid","actions":[{"action":"click","selector":"#slow"}],
                    "waitFor":{"selector":"#ready","timeoutMs":1000},
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("event-slow"),
        &[0],
        timeout,
        &[],
    )?;
    let selector_wait = slow_event
        .pointer("/pages/0/waitEvidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|item| item.get("kind") == Some(&json!("selector")))
        .and_then(|item| item.get("durationMs"))
        .and_then(Value::as_u64);
    if selector_wait.is_none_or(|duration| !(100..900).contains(&duration)) {
        return Err("slow valid event did not return when the selector arrived".to_owned());
    }
    scenarios += 1;

    let readback = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html"),"waitFor":{"readback":{
                    "url":format!("{base}/readback.json"),"status":200,"jsonPath":"ready","equals":true,"intervalMs":25
                },"timeoutMs":1000}}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("server-readback"),
        &[0],
        timeout,
        &[],
    )?;
    if !readback
        .pointer("/pages/0/waitEvidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| item.get("kind") == Some(&json!("server-readback")))
    {
        return Err("server readback readiness was not recorded".to_owned());
    }
    scenarios += 1;

    let excessive = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}],
                "viewports":[{"name":"desktop","width":1280,"height":800}],"waitFor":{"settleMs":101}
            }),
        ),
        &work.join("excessive-delay"),
        &[2],
        timeout,
        &[],
    )?;
    if !excessive
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("must not exceed 100 ms"))
    {
        return Err("a deliberate delay above 100 ms was accepted".to_owned());
    }
    scenarios += 1;

    let invalid_performance = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}],
                "viewports":[{"name":"desktop","width":1280,"height":800}],
                "performance":{"ttfbMs":0,"lcpMs":800}
            }),
        ),
        &work.join("invalid-performance"),
        &[2],
        timeout,
        &[],
    )?;
    if !invalid_performance
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("performance.ttfbMs must be a positive number"))
    {
        return Err("a non-positive performance threshold was accepted".to_owned());
    }
    scenarios += 1;
    Ok(scenarios)
}

fn run_transport_and_cache_phase(
    root: &Path,
    static_server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    let mut scenarios = 0usize;
    let static_base = static_server.base_url();
    for (name, url) in [
        ("dead-target", "http://127.0.0.1:9/".to_owned()),
        ("not-found", format!("{static_base}/missing-route.html")),
        ("non-html", format!("{static_base}/plain.txt")),
    ] {
        let report = run_verifier(
            root,
            &contracted_config(
                root,
                json!({"targets":[{"url":url}],"viewports":[{"name":"mobile","width":390,"height":844}]}),
            ),
            &work.join(name),
            &[3],
            timeout,
            &[],
        )?;
        if report.pointer("/coverage/failed") != Some(&json!(true)) {
            return Err(format!("{name} did not record a coverage failure"));
        }
        scenarios += 1;
    }

    let redirect = StaticServer::start_handler(Arc::new(|request| {
        if request.path.split('?').next() == Some("/dashboard") {
            let mut response = HttpResponse::status("302 Found", Vec::new());
            response
                .headers
                .push(("Location".to_owned(), "/sign-in".to_owned()));
            response
        } else {
            HttpResponse::html(html_page(
                "<h1>Sign in</h1><form><input placeholder='Email address'></form>",
                "",
            ))
        }
    }))?;
    let redirected = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":[{"url":format!("{}/dashboard",redirect.base_url())}],"viewports":[{"name":"mobile","width":390,"height":844}]}),
        ),
        &work.join("redirected-to-sign-in"),
        &[3],
        timeout,
        &[],
    )?;
    if redirected.pointer("/pages/0/outcome") != Some(&json!("route_mismatch"))
        || redirected.pointer("/pages/0/requestedPath") != Some(&json!("/dashboard"))
        || redirected.pointer("/pages/0/finalPath") != Some(&json!("/sign-in"))
        || redirected.pointer("/coverage/checkedPages") != Some(&json!(0))
    {
        return Err("sign-in redirect counted as checked coverage".to_owned());
    }
    scenarios += 1;

    let requests = Arc::new(AtomicUsize::new(0));
    let request_counter = Arc::clone(&requests);
    let binding = StaticServer::start_handler(Arc::new(move |_| {
        request_counter.fetch_add(1, Ordering::AcqRel);
        let mut response = HttpResponse::html(html_page(
            "<h1>Bound deployment</h1><p>Current page content.</p>",
            "",
        ));
        response.headers.push((
            "X-UI-Source-Revision".to_owned(),
            "deployed-revision".to_owned(),
        ));
        response
    }))?;
    let binding_config = contracted_config(
        root,
        json!({
            "targets":[{"url":format!("{}/bound",binding.base_url()),"sourceBinding":{"expected":"deployed-revision"}}],
            "viewports":[{"name":"desktop","width":1280,"height":800}],"maxPageCount":1
        }),
    );
    let matched = run_verifier(
        root,
        &binding_config,
        &work.join("source-binding-match"),
        &[0],
        timeout,
        &[],
    )?;
    if matched.pointer("/pages/0/sourceBinding/status") != Some(&json!("matched"))
        || matched.pointer("/pages/0/requestedPath") != Some(&json!("/bound"))
        || matched.pointer("/pages/0/finalPath") != Some(&json!("/bound"))
    {
        return Err("matching deployment/source binding was not recorded".to_owned());
    }
    let repeated = run_verifier(
        root,
        &binding_config,
        &work.join("source-binding-repeat"),
        &[0],
        timeout,
        &[],
    )?;
    if matched.pointer("/evidence/config/sha256") != repeated.pointer("/evidence/config/sha256")
        || matched.pointer("/evidence/verifier/sha256")
            != Some(&json!(sha256_file(&root.join(
                "skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs"
            ))?))
    {
        return Err(
            "equivalent source-binding configs did not preserve evidence identity".to_owned(),
        );
    }
    scenarios += 2;

    let cache_root = work.join("cell-cache");
    create_directory_all_nofollow(&cache_root, 0o700).map_err(|error| error.to_string())?;
    let mut cached_config = binding_config.clone();
    cached_config["development"] =
        json!({"cache":{"directory":cache_root,"dataRevision":"fixture-data-v1"}});
    let before = requests.load(Ordering::Acquire);
    let first = run_verifier(
        root,
        &cached_config,
        &work.join("cache-first"),
        &[0],
        timeout,
        &[],
    )?;
    let after_first = requests.load(Ordering::Acquire);
    if first.pointer("/pages/0/cache/write/written") != Some(&json!(true))
        || first.pointer("/coverage/readinessEligible") != Some(&json!(false))
        || after_first <= before
    {
        return Err(
            "first development cache run did not write exact ineligible evidence".to_owned(),
        );
    }
    let second = run_verifier(
        root,
        &cached_config,
        &work.join("cache-second"),
        &[0],
        timeout,
        &[],
    )?;
    if second.pointer("/pages/0/cache/hit") != Some(&json!(true))
        || requests.load(Ordering::Acquire) != after_first
        || !second
            .pointer("/pages/0/screenshots/fullPage/path")
            .is_some_and(Value::is_string)
    {
        return Err("exact cache hit navigated or failed to restore screenshots".to_owned());
    }
    let cache_key = second
        .pointer("/pages/0/cache/key")
        .and_then(Value::as_str)
        .ok_or_else(|| "cache hit lacks key".to_owned())?;
    let manifest = cache_root
        .join("v1")
        .join(&cache_key[..2])
        .join(cache_key)
        .join("manifest.json");
    crate::audit_queue::write_json(&manifest, &json!({"corrupt":true}))?;
    let before_corrupt = requests.load(Ordering::Acquire);
    let corrupt = run_verifier(
        root,
        &cached_config,
        &work.join("cache-corrupt"),
        &[0],
        timeout,
        &[],
    )?;
    if corrupt.pointer("/pages/0/cache/hit") == Some(&json!(true))
        || requests.load(Ordering::Acquire) <= before_corrupt
        || !corrupt
            .pointer("/pages/0/cache/reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.starts_with("rejected:"))
    {
        return Err("corrupt cache entry was reused or not explicitly rejected".to_owned());
    }
    scenarios += 3;

    let mut changed_data = cached_config.clone();
    changed_data["development"]["cache"]["dataRevision"] = json!("fixture-data-v2");
    let changed = run_verifier(
        root,
        &changed_data,
        &work.join("cache-data-changed"),
        &[0],
        timeout,
        &[],
    )?;
    if changed.pointer("/pages/0/cache/hit") == Some(&json!(true)) {
        return Err("changed fixture revision reused stale cache evidence".to_owned());
    }
    let mut changed_viewport = cached_config.clone();
    changed_viewport["viewports"] = json!([{"name":"different","width":1024,"height":700}]);
    let changed = run_verifier(
        root,
        &changed_viewport,
        &work.join("cache-viewport-changed"),
        &[0],
        timeout,
        &[],
    )?;
    if changed.pointer("/pages/0/cache/hit") == Some(&json!(true)) {
        return Err("changed viewport reused stale cache evidence".to_owned());
    }
    scenarios += 2;

    let symlink_root = work.join("cell-cache-symlink");
    create_directory_all_nofollow(&symlink_root, 0o700).map_err(|error| error.to_string())?;
    let mut symlink_config = binding_config.clone();
    symlink_config["development"] =
        json!({"cache":{"directory":symlink_root,"dataRevision":"fixture-data-v1"}});
    let first = run_verifier(
        root,
        &symlink_config,
        &work.join("cache-symlink-first"),
        &[0],
        timeout,
        &[],
    )?;
    let key = first
        .pointer("/pages/0/cache/write/key")
        .and_then(Value::as_str)
        .ok_or_else(|| "cache write lacks key".to_owned())?;
    let manifest = symlink_root
        .join("v1")
        .join(&key[..2])
        .join(key)
        .join("manifest.json");
    let outside = work.join("outside-cache-manifest.json");
    crate::audit_queue::write_json(&outside, &json!({}))?;
    std::fs::remove_file(&manifest)
        .map_err(|error| format!("cannot replace disposable cache manifest: {error}"))?;
    std::os::unix::fs::symlink(&outside, &manifest)
        .map_err(|error| format!("cannot create cache symlink fixture: {error}"))?;
    let rejected = run_verifier(
        root,
        &symlink_config,
        &work.join("cache-symlink-second"),
        &[0],
        timeout,
        &[],
    )?;
    if rejected.pointer("/pages/0/cache/hit") == Some(&json!(true))
        || !rejected
            .pointer("/pages/0/cache/reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("symlink"))
    {
        return Err("symlinked cache evidence was not rejected".to_owned());
    }
    scenarios += 1;

    let stale = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{}/bound",binding.base_url()),"sourceBinding":{"expected":"newer-source-revision"}}],
                "viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("source-binding-stale"),
        &[3],
        timeout,
        &[],
    )?;
    if stale.pointer("/pages/0/outcome") != Some(&json!("stale_deployment"))
        || stale.pointer("/pages/0/sourceBinding/status") != Some(&json!("mismatched"))
        || stale.pointer("/coverage/checkedPages") != Some(&json!(0))
    {
        return Err("stale deployment counted as checked coverage".to_owned());
    }
    let missing = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{static_base}/clean.html"),"sourceBinding":{"expected":"required-revision"}}],
                "viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("source-binding-missing"),
        &[3],
        timeout,
        &[],
    )?;
    if missing.pointer("/pages/0/outcome") != Some(&json!("source_binding_missing")) {
        return Err("missing required source binding did not fail coverage".to_owned());
    }
    let meta = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{static_base}/source-binding-meta.html"),"sourceBinding":{"expected":"meta-deployed-revision"}}],
                "viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("source-binding-meta"),
        &[0],
        timeout,
        &[],
    )?;
    if meta.pointer("/pages/0/sourceBinding/status") != Some(&json!("matched"))
        || meta.pointer("/pages/0/sourceBinding/observedFrom")
            != Some(&json!("meta:ui-source-revision"))
    {
        return Err("meta source-binding fallback was not exercised".to_owned());
    }
    scenarios += 3;

    let optional = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{static_base}/clean.html")},{"url":format!("{static_base}/optional.html"),"allowFailure":"optional route is absent in this fixture"}],
                "viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("allowed-missing"),
        &[0],
        timeout,
        &[],
    )?;
    if optional.pointer("/coverage/failed") != Some(&json!(false))
        || optional
            .pointer("/coverage/tolerated")
            .and_then(Value::as_array)
            .is_none_or(|items| items.len() != 1)
    {
        return Err("reasoned optional target exemption did not remain visible".to_owned());
    }
    scenarios += 1;
    Ok(scenarios)
}

fn performance_target(root: &Path, url: String) -> Map<String, Value> {
    let mut target = default_target_contract();
    target.insert("url".to_owned(), json!(url));
    target.insert(
        "reviewInputs".to_owned(),
        json!([{"path":"SKILL.md","kind":"ui-code"}]),
    );
    let _ = root;
    target
}

fn performance_config(root: &Path, targets: Vec<Value>) -> Value {
    let page_count = targets.len();
    json!({
        "repoRoot":root.join("skills/formal-web-ui-verification"),
        "targets":targets,"viewports":[{"name":"desktop","width":1280,"height":800}],
        "maxPageCount":page_count,
    })
}

fn warm_http(server: &StaticServer, path: &str) {
    if let Ok(mut stream) = TcpStream::connect_timeout(&server.address, Duration::from_secs(1)) {
        let _ = stream.write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                server.address
            )
            .as_bytes(),
        );
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    }
}

fn run_node_probe(root: &Path, source: &str, timeout: Duration) -> Result<(), String> {
    let node = std::env::var_os("FORMAL_WEB_UI_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(node);
    command
        .args(["--input-type=module", "--eval", source])
        .env_remove("NODE_PATH")
        .env_remove("DEVCOORDINATOR_EVIDENCE_DIR")
        .env_remove("DEVCOORDINATOR_RUN_ID")
        .env_remove("DEVCOORDINATOR_CHECK_NAME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, stdout, stderr) = wait_child(
        command
            .spawn()
            .map_err(|error| format!("cannot launch Node probe: {error}"))?,
        timeout,
    )?;
    if !status.success() {
        return Err(format!(
            "Node probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&stdout)
                .chars()
                .take(500)
                .collect::<String>(),
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    let _ = root;
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], payload: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    let mut checksum = Vec::with_capacity(kind.len() + payload.len());
    checksum.extend_from_slice(kind);
    checksum.extend_from_slice(payload);
    output.extend_from_slice(&crc32(&checksum).to_be_bytes());
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut first = 1_u32;
    let mut second = 0_u32;
    for byte in bytes {
        first = (first + u32::from(*byte)) % 65_521;
        second = (second + first) % 65_521;
    }
    (second << 16) | first
}

fn lcp_png() -> Vec<u8> {
    let width = 256_u32;
    let height = 256_u32;
    let mut raw = Vec::with_capacity((height * (1 + width * 4)) as usize);
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            raw.extend_from_slice(&[x as u8, y as u8, (x + y) as u8, 255]);
        }
    }
    let mut zlib = vec![0x78, 0x01];
    let mut remaining = raw.as_slice();
    while !remaining.is_empty() {
        let count = remaining.len().min(65_535);
        let final_block = count == remaining.len();
        zlib.push(u8::from(final_block));
        zlib.extend_from_slice(&(count as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(count as u16)).to_le_bytes());
        zlib.extend_from_slice(&remaining[..count]);
        remaining = &remaining[count..];
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(b"IHDR", &ihdr, &mut png);
    png_chunk(b"IDAT", &zlib, &mut png);
    png_chunk(b"IEND", &[], &mut png);
    png
}

fn run_performance_phase(root: &Path, work: &Path, timeout: Duration) -> Result<usize, String> {
    let png = lcp_png();
    let server = StaticServer::start_handler(Arc::new(move |request| {
        let route = request.path.split('?').next().unwrap_or("/");
        match route {
            "/lcp.png" => HttpResponse {
                status: "200 OK",
                content_type: "image/png",
                headers: Vec::new(),
                body: png.clone(),
                delay: Duration::ZERO,
            },
            "/slow-ttfb" => {
                let mut response = HttpResponse::html(html_page(
                    "<h1>Slow response</h1><p>The document paints quickly after its delayed first byte.</p>",
                    "",
                ));
                response.delay = Duration::from_millis(30);
                response
            }
            "/slow-lcp" => HttpResponse::html(html_page(
                "<p>Initial content</p><img id='late-lcp' width='1000' height='420' alt='Late largest image'><script>setTimeout(()=>{const image=document.querySelector('#late-lcp');image.addEventListener('load',()=>requestAnimationFrame(()=>requestAnimationFrame(()=>{image.dataset.lcpReady='true';})),{once:true});image.src='/lcp.png';},1200)</script>",
                "",
            )),
            "/no-lcp" => HttpResponse::html(html_page(
                "<input aria-label='Name' style='width:320px;height:52px'>",
                "",
            )),
            _ => HttpResponse::html(html_page(
                "<h1>Fast performance</h1><p>Immediate local content.</p>",
                "",
            )),
        }
    }))?;
    warm_http(&server, "/fast");
    let base = server.base_url();
    let fast = run_verifier(
        root,
        &performance_config(
            root,
            vec![Value::Object(performance_target(
                root,
                format!("{base}/fast"),
            ))],
        ),
        &work.join("fast-defaults"),
        &[0, 1],
        timeout,
        &[],
    )?;
    let metrics = fast
        .pointer("/pages/0/metrics/performance")
        .and_then(Value::as_object)
        .ok_or_else(|| "default performance metrics are missing".to_owned())?;
    let mut expected_rules = BTreeSet::new();
    for (metric, threshold) in [("ttfb", 10.0), ("lcp", 800.0)] {
        let evidence = metrics
            .get(metric)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("default {metric} evidence is missing"))?;
        if evidence.get("thresholdMs").and_then(Value::as_f64) != Some(threshold)
            || evidence.get("comparison") != Some(&json!("<"))
        {
            return Err(format!("default {metric} threshold contract changed"));
        }
        let value = evidence
            .get("valueMs")
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("default {metric} was not measured"))?;
        let status = if value < threshold { "pass" } else { "fail" };
        if evidence.get("status") != Some(&json!(status)) {
            return Err(format!(
                "default {metric} classification disagrees with its measurement"
            ));
        }
        if status == "fail" {
            expected_rules.insert(format!("{metric}-above-threshold"));
        }
    }
    let actual_rules = fast["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| finding["severity"] == "critical")
        .filter_map(|finding| finding["rule"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    if actual_rules != expected_rules {
        return Err("default performance findings disagree with measured metrics".to_owned());
    }

    let mut slow_ttfb = performance_target(root, format!("{base}/slow-ttfb"));
    slow_ttfb.insert("performance".to_owned(), json!({"ttfbMs":10,"lcpMs":10000}));
    let slow_ttfb = run_verifier(
        root,
        &performance_config(root, vec![Value::Object(slow_ttfb)]),
        &work.join("slow-ttfb"),
        &[1],
        timeout,
        &[],
    )?;
    require_rule(&slow_ttfb, "ttfb-above-threshold", "critical")?;
    if slow_ttfb
        .pointer("/pages/0/metrics/performance/ttfb/valueMs")
        .and_then(Value::as_f64)
        .is_none_or(|value| value < 10.0)
    {
        return Err("slow TTFB fixture did not exceed its threshold".to_owned());
    }

    let mut slow_lcp_target = performance_target(root, format!("{base}/slow-lcp"));
    slow_lcp_target.insert("waitFor".to_owned(),json!({"selector":"#late-lcp[data-lcp-ready='true']","responseUrl":"**/lcp.png","timeoutMs":5000}));
    slow_lcp_target.insert(
        "performance".to_owned(),
        json!({"ttfbMs":10000,"lcpMs":800}),
    );
    let slow_lcp = run_verifier(
        root,
        &performance_config(root, vec![Value::Object(slow_lcp_target)]),
        &work.join("slow-lcp"),
        &[1],
        timeout,
        &[],
    )?;
    require_rule(&slow_lcp, "lcp-above-threshold", "critical")?;
    if slow_lcp
        .pointer("/pages/0/metrics/performance/lcp/valueMs")
        .and_then(Value::as_f64)
        .is_none_or(|value| value < 800.0)
    {
        return Err("slow LCP fixture did not exceed its threshold".to_owned());
    }
    let private = json!({
        "metrics":slow_lcp.pointer("/pages/0/metrics/performance"),
        "findings":slow_lcp["findings"].as_array().into_iter().flatten()
            .filter(|finding|["lcp-above-threshold","ttfb-above-threshold","performance-metric-unavailable"].contains(&finding["rule"].as_str().unwrap_or("")))
            .filter_map(|finding|finding.get("evidence").cloned()).collect::<Vec<_>>()
    }).to_string();
    if private.contains("late-lcp")
        || private.contains("Late largest image")
        || private.contains(&base)
    {
        return Err("performance evidence leaked a selector, text, or URL".to_owned());
    }

    let mut prescribed_ttfb = performance_target(root, format!("{base}/slow-ttfb"));
    prescribed_ttfb.insert(
        "performance".to_owned(),
        json!({"ttfbMs":10000,"lcpMs":10000}),
    );
    let mut prescribed_lcp = performance_target(root, format!("{base}/slow-lcp"));
    prescribed_lcp.insert("waitFor".to_owned(),json!({"selector":"#late-lcp[data-lcp-ready='true']","responseUrl":"**/lcp.png","timeoutMs":5000}));
    prescribed_lcp.insert(
        "performance".to_owned(),
        json!({"ttfbMs":10000,"lcpMs":10000}),
    );
    let prescribed = run_verifier(
        root,
        &performance_config(
            root,
            vec![
                Value::Object(prescribed_ttfb),
                Value::Object(prescribed_lcp),
            ],
        ),
        &work.join("prescribed"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&prescribed)?;
    if prescribed["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|page| {
            page.pointer("/metrics/performance/ttfb/thresholdMs")
                .and_then(Value::as_u64)
                != Some(10000)
                || page
                    .pointer("/metrics/performance/lcp/thresholdMs")
                    .and_then(Value::as_u64)
                    != Some(10000)
        })
    {
        return Err("per-target performance thresholds did not override defaults".to_owned());
    }

    let mut unavailable_target = performance_target(root, format!("{base}/no-lcp"));
    unavailable_target.insert(
        "performance".to_owned(),
        json!({"ttfbMs":10000,"lcpMs":800}),
    );
    let unavailable = run_verifier(
        root,
        &performance_config(root, vec![Value::Object(unavailable_target)]),
        &work.join("lcp-unavailable"),
        &[0],
        timeout,
        &[],
    )?;
    if !unavailable["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|finding| {
            finding["rule"] == "performance-metric-unavailable"
                && finding.pointer("/evidence/metric") == Some(&json!("LCP"))
        })
    {
        return Err("unavailable LCP was silently treated as a pass".to_owned());
    }

    let verifier = root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
    let module_url = format!("file://{}", verifier.display());
    let probe = format!(
        "import {{ isLocalServerUrl, performanceThresholdStatus }} from {}; if (!isLocalServerUrl('http://127.0.0.1:3000/') || !isLocalServerUrl('http://localhost:3000/') || isLocalServerUrl('https://example.test/') || performanceThresholdStatus(10,10)!=='fail' || performanceThresholdStatus(9.99,10)!=='pass' || performanceThresholdStatus(null,10)!=='unavailable' || performanceThresholdStatus(20,10,false)!=='not-applicable') process.exit(9);",
        serde_json::to_string(&module_url).map_err(|error| error.to_string())?
    );
    run_node_probe(root, &probe, timeout)?;
    Ok(6)
}

fn recursive_named_files(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                stack.push(entry.path());
            } else if metadata.is_file() && entry.file_name().to_str() == Some(name) {
                result.push(entry.path());
            }
        }
    }
    result.sort();
    result
}

fn run_auth_phase(root: &Path, work: &Path, timeout: Duration) -> Result<usize, String> {
    let cookie_server = StaticServer::start_handler(Arc::new(|request| {
        let authenticated = request
            .headers
            .get("cookie")
            .is_some_and(|cookie| cookie.contains("sess=ok"));
        if authenticated {
            HttpResponse::html(html_page(
                "<h1>Dashboard</h1><p>Session accepted.</p><button>Save changes</button>",
                "",
            ))
        } else {
            HttpResponse::html(html_page(
                "<p class='bad'>Invisible message</p>",
                ".bad { color: #fff; background: #fff; }",
            ))
        }
    }))?;
    let base = cookie_server.base_url();
    let anonymous = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":[{"url":format!("{base}/gated.html")}],"viewports":[{"name":"mobile","width":390,"height":844}]}),
        ),
        &work.join("cookie-anonymous"),
        &[1],
        timeout,
        &[],
    )?;
    require_rule(&anonymous, "invisible-text", "critical")?;
    let authenticated = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":[{"url":format!("{base}/gated.html")}],"viewports":[{"name":"mobile","width":390,"height":844}]}),
        ),
        &work.join("cookie-authenticated"),
        &[0],
        timeout,
        &["--cookie", "sess=ok"],
    )?;
    no_critical(&authenticated)?;
    let scoped = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/gated.html")}],
                "cookies":[{"name":"sess","value":"ok","domain":"127.0.0.1","path":"/"}],
                "viewports":[{"name":"mobile","width":390,"height":844}]
            }),
        ),
        &work.join("cookie-scoped"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&scoped)?;
    let invalid = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/gated.html")}],
                "cookies":[{"name":"sess","value":"ok","domain":true}],
                "viewports":[{"name":"mobile","width":390,"height":844}]
            }),
        ),
        &work.join("cookie-invalid"),
        &[2],
        timeout,
        &[],
    )?;
    if !invalid
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("domain must be a non-empty string"))
    {
        return Err("malformed cookie domain did not preserve its validator message".to_owned());
    }

    let auth_calls = Arc::new(AtomicUsize::new(0));
    let observations = Arc::new(Mutex::new(Vec::<String>::new()));
    let auth_calls_handler = Arc::clone(&auth_calls);
    let observations_handler = Arc::clone(&observations);
    let auth_server = StaticServer::start_handler(Arc::new(move |request| {
        let (route, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
        let mut response = match route {
            "/login" => HttpResponse::html(html_page(
                "<h1>Sign in once</h1><input id='password' type='password'><button id='login'>Sign in</button><div id='ready' hidden>Ready</div><script>localStorage.setItem('auth-seed','ready');document.querySelector('#login').onclick=()=>fetch('/authenticate').then(()=>{document.querySelector('#ready').hidden=false});</script>",
                "",
            )),
            "/authenticate" => {
                auth_calls_handler.fetch_add(1, Ordering::AcqRel);
                let mut response = HttpResponse::html("ok");
                response.headers.push((
                    "Set-Cookie".to_owned(),
                    "auth=ok; Path=/; SameSite=Lax".to_owned(),
                ));
                response
            }
            "/observe" => {
                let value = query.strip_prefix("value=").unwrap_or("missing").to_owned();
                observations_handler.lock().unwrap().push(value);
                HttpResponse::html("observed")
            }
            "/protected"
                if !request
                    .headers
                    .get("cookie")
                    .is_some_and(|value| value.contains("auth=ok")) =>
            {
                HttpResponse::status("401 Unauthorized", html_page("<h1>Unauthorized</h1>", ""))
            }
            "/protected" => HttpResponse::html(html_page(
                "<h1>Protected dashboard</h1><div id='protected'>Authenticated</div><div id='ready' hidden>Ready</div><script>const before=localStorage.getItem('cellMutation')||'clean';fetch('/observe?value='+encodeURIComponent(before)).then(()=>{document.querySelector('#ready').hidden=false});localStorage.setItem('cellMutation','dirty');</script>",
                "",
            )),
            _ => HttpResponse::status("404 Not Found", html_page("<h1>Not found</h1>", "")),
        };
        response.headers.push((
            "X-UI-Source-Revision".to_owned(),
            "auth-fixture-revision".to_owned(),
        ));
        response
    }))?;
    let auth_base = auth_server.base_url();
    let cache = work.join("auth-cache");
    create_directory_all_nofollow(&cache, 0o700).map_err(|error| error.to_string())?;
    let auth = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "authProfiles":[{"name":"admin","url":format!("{auth_base}/login"),
                    "actions":[{"action":"fill","selector":"#password","value":"AUTH_SECRET_MUST_NOT_LEAK"},{"action":"click","selector":"#login"}],
                    "waitFor":{"responseUrl":"**/authenticate","selector":"#ready","timeoutMs":2000}}],
                "targets":[{"url":format!("{auth_base}/protected"),"authProfile":"admin","sourceBinding":{"expected":"auth-fixture-revision"},
                    "waitFor":{"selector":"#ready","timeoutMs":2000},"execution":{"parallelSafe":true}}],
                "execution":{"maxConcurrency":2},"development":{"cache":{"directory":cache,"dataRevision":"auth-fixture-data-v1"}},
                "viewports":[{"name":"mobile","width":390,"height":844},{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("auth-reuse"),
        &[0],
        timeout,
        &[],
    )?;
    let mut observed = observations.lock().unwrap().clone();
    observed.sort();
    if auth_calls.load(Ordering::Acquire) != 1
        || observed != ["clean", "clean"]
        || auth.pointer("/authentication/0/status") != Some(&json!("ready"))
        || auth.to_string().contains("AUTH_SECRET_MUST_NOT_LEAK")
        || auth["pages"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|page| page["outcome"] != "checked")
    {
        return Err(
            "authentication reuse, context isolation, or secret redaction regressed".to_owned(),
        );
    }
    let manifests = recursive_named_files(&cache, "manifest.json");
    if manifests.len() != 2 {
        return Err(
            "authenticated successful cells did not create exact cache evidence".to_owned(),
        );
    }
    for manifest in manifests {
        let text = String::from_utf8(
            read_bytes_nofollow(&manifest, Some(&cache))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "auth cache manifest disappeared".to_owned())?,
        )
        .map_err(|_| "auth cache manifest is not UTF-8")?;
        if text.contains("AUTH_SECRET_MUST_NOT_LEAK")
            || text.contains("\"cookies\"")
            || text.contains("\"storageState\"")
        {
            return Err("authentication state leaked into development cache".to_owned());
        }
    }

    let isolation = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "authProfiles":[
                    {"name":"admin","url":format!("{auth_base}/login"),"actions":[{"action":"click","selector":"#login"}],"waitFor":{"responseUrl":"**/authenticate","selector":"#ready"}},
                    {"name":"broken-role","url":format!("{auth_base}/login"),"actions":[{"action":"click","selector":"#missing-login-control"}]}
                ],
                "targets":[
                    {"name":"good-role-target","url":format!("{auth_base}/protected"),"authProfile":"admin","waitFor":{"selector":"#ready"}},
                    {"name":"bad-role-target","url":format!("{auth_base}/protected"),"authProfile":"broken-role"}
                ],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("auth-isolation"),
        &[3],
        timeout,
        &[],
    )?;
    let pages = isolation["pages"]
        .as_array()
        .ok_or_else(|| "auth isolation pages are missing".to_owned())?;
    let outcome = |name: &str| {
        pages
            .iter()
            .find(|page| page.pointer("/target/name") == Some(&json!(name)))
            .and_then(|page| page["outcome"].as_str())
    };
    if outcome("good-role-target") != Some("checked")
        || outcome("bad-role-target") != Some("auth_setup_error")
    {
        return Err("failed auth profile blocked an unrelated ready profile".to_owned());
    }
    Ok(6)
}

#[derive(Default)]
struct ConcurrencyState {
    arrivals: usize,
    active: usize,
    max_active: usize,
}

fn run_scheduler_phase(
    root: &Path,
    static_server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    let state = Arc::new((Mutex::new(ConcurrencyState::default()), Condvar::new()));
    let handler_state = Arc::clone(&state);
    let concurrent = StaticServer::start_handler(Arc::new(move |_| {
        let (lock, condition) = &*handler_state;
        let mut current = lock.lock().unwrap();
        current.arrivals += 1;
        current.active += 1;
        current.max_active = current.max_active.max(current.active);
        condition.notify_all();
        let (mut current, _) = condition
            .wait_timeout_while(current, Duration::from_secs(2), |state| state.arrivals < 3)
            .unwrap();
        current.active = current.active.saturating_sub(1);
        condition.notify_all();
        HttpResponse::html(html_page(
            "<h1>Parallel cell</h1><p>Independent evidence.</p>",
            "",
        ))
    }))?;
    let parallel_targets = (0..3)
        .map(|index| {
            json!({"name":format!("parallel-{index}"),"url":format!("{}/parallel-{index}",concurrent.base_url()),"execution":{"parallelSafe":true}})
        })
        .collect::<Vec<_>>();
    let parallel = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":parallel_targets,"execution":{"maxConcurrency":3},
                "viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("parallel"),
        &[0],
        timeout,
        &[],
    )?;
    if state.0.lock().unwrap().max_active < 3 {
        return Err("independent browser cells did not overlap".to_owned());
    }
    let mut execution_indices = parallel["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|page| {
            page.pointer("/execution/executionIndex")
                .and_then(Value::as_u64)
        })
        .collect::<Vec<_>>();
    execution_indices.sort_unstable();
    if execution_indices != [1, 2, 3] {
        return Err("parallel execution indices are incomplete".to_owned());
    }

    let base = static_server.base_url();
    let locked = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[
                    {"name":"locked-a","url":format!("{base}/clean.html"),"execution":{"parallelSafe":true,"resourceLocks":["shared-account"]}},
                    {"name":"locked-b","url":format!("{base}/clean.html"),"execution":{"parallelSafe":true,"resourceLocks":["shared-account"]}}
                ],"execution":{"maxConcurrency":2},"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("resource-locks"),
        &[0],
        timeout,
        &[],
    )?;
    let mut locked_pages = locked["pages"].as_array().cloned().unwrap_or_default();
    locked_pages.sort_by_key(|page| {
        page.pointer("/execution/executionIndex")
            .and_then(Value::as_u64)
    });
    if locked_pages.len() != 2
        || locked_pages[1]["startedAt"].as_str().unwrap_or("")
            < locked_pages[0]["endedAt"].as_str().unwrap_or("")
    {
        return Err("conflicting resource locks overlapped".to_owned());
    }

    let priority = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[
                    {"name":"low-priority","url":format!("{base}/clean.html"),"execution":{"priority":1}},
                    {"name":"high-priority","url":format!("{base}/clean.html"),"execution":{"priority":100}}
                ],"execution":{"maxConcurrency":1},"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("priority"),
        &[0],
        timeout,
        &[],
    )?;
    let pages = priority["pages"]
        .as_array()
        .ok_or_else(|| "priority pages are missing".to_owned())?;
    let names = pages
        .iter()
        .filter_map(|page| page.pointer("/target/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let indices = pages
        .iter()
        .filter_map(|page| {
            Some((
                page.pointer("/target/name")?.as_str()?,
                page.pointer("/execution/executionIndex")?.as_u64()?,
            ))
        })
        .collect::<BTreeMap<_, _>>();
    if names != ["low-priority", "high-priority"]
        || indices.get("high-priority") != Some(&1)
        || indices.get("low-priority") != Some(&2)
    {
        return Err(
            "priority execution did not run high value first while preserving report order"
                .to_owned(),
        );
    }
    let progress_path = priority
        .pointer("/execution/progressPath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "priority progress path is missing".to_owned())?;
    let progress = String::from_utf8(
        read_bytes_nofollow(&progress_path, Some(&work.join("priority")))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "priority progress is missing".to_owned())?,
    )
    .map_err(|_| "priority progress is not UTF-8")?;
    let progress_rows = progress
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    if progress_rows.get(1).and_then(|row| row.get("targetName")) != Some(&json!("high-priority")) {
        return Err("progress evidence did not expose the high-priority first result".to_owned());
    }

    let locale_targets=(1..=7).map(|index| {
        if index==2 {
            json!({"name":"locale-2","url":format!("{base}/event-readiness.html"),"includeBase":false,"states":[{
                "name":"focus-check","actions":[{"action":"click","selector":"#start"},{"action":"focus","selector":"#missing-locale-focus"}],
                "afterFailureWaitFor":{"selector":"#ready","timeoutMs":1000},
                "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
            }]})
        } else {
            json!({"name":format!("locale-{index}"),"url":format!("{base}/clean.html")})
        }
    }).collect::<Vec<_>>();
    let locales = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":locale_targets,"execution":{"maxConcurrency":1},"viewports":[{"name":"desktop","width":1280,"height":800}]}),
        ),
        &work.join("complete-after-failure"),
        &[3],
        timeout,
        &[],
    )?;
    let locale_pages = locales["pages"]
        .as_array()
        .ok_or_else(|| "locale pages are missing".to_owned())?;
    if locale_pages.len() != 7
        || locale_pages[1]["outcome"] != "interaction_error"
        || locale_pages
            .iter()
            .skip(2)
            .any(|page| page["outcome"] != "checked")
        || locale_pages
            .iter()
            .any(|page| page.pointer("/cleanup/status") != Some(&json!("completed")))
    {
        return Err("ordinary locale failure stopped safe later cells or cleanup".to_owned());
    }
    if locale_pages[1].pointer("/interactionFailure/afterFailureObservation/checked")
        != Some(&json!(true))
        || locales
            .pointer("/coverage/failures")
            .and_then(Value::as_array)
            .is_none_or(|items| items.len() != 1)
    {
        return Err(
            "ordinary failure lost downstream observation or combined failure accounting"
                .to_owned(),
        );
    }

    let unsafe_targets = (1..=7)
        .map(|index| {
            if index == 2 {
                json!({"name":"locale-2","url":format!("{base}/event-readiness.html"),"includeBase":false,
                    "execution":{"stopOnFailure":true,"stopReason":"Injected shared fixture corruption"},
                    "states":[{"name":"focus-check","actions":[{"action":"click","selector":"#start"},{"action":"focus","selector":"#missing-locale-focus"}],
                        "afterFailureWaitFor":{"selector":"#ready","timeoutMs":1000},"continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}}]})
            } else {
                json!({"name":format!("locale-{index}"),"url":format!("{base}/clean.html")})
            }
        })
        .collect::<Vec<_>>();
    let unsafe_stop = run_verifier(
        root,
        &contracted_config(
            root,
            json!({"targets":unsafe_targets,"execution":{"maxConcurrency":1},"viewports":[{"name":"desktop","width":1280,"height":800}]}),
        ),
        &work.join("declared-unsafe-stop"),
        &[3],
        timeout,
        &[],
    )?;
    let unsafe_pages = unsafe_stop["pages"]
        .as_array()
        .ok_or_else(|| "unsafe-stop pages are missing".to_owned())?;
    if unsafe_pages.len() != 7
        || unsafe_stop.pointer("/execution/executedCount") != Some(&json!(2))
        || unsafe_pages
            .iter()
            .skip(2)
            .any(|page| page["outcome"] != "unsafe_stop_unexecuted")
        || !unsafe_stop
            .pointer("/execution/unsafeStop")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("Injected shared fixture corruption"))
    {
        return Err(
            "declared unsafe stop did not preserve planned and unexecuted cells".to_owned(),
        );
    }

    let progress = work.join("unsafe-progress.jsonl");
    let verifier = root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
    let module_url = format!("file://{}", verifier.display());
    let probe = format!(
        r#"
import {{ executePlan }} from {module};
const cells = [0, 1, 2].map((planIndex) => ({{
  cellId: `cell-${{planIndex + 1}}`, planIndex, executionPriority: 10 - planIndex,
  target: {{ name: `unsafe-${{planIndex + 1}}`, url: 'http://127.0.0.1/unsafe', stateName: 'base', execution: {{ parallelSafe: false, resourceLocks: [], priority: null }}, reviewEvidence: null, intentFingerprint: null }},
  viewport: {{ name: 'desktop', width: 1280, height: 800, contextOptions: {{}} }},
}}));
const runner = async (_browser, cell, _config, _label, _auth, _cache, executionIndex) => ({{
  page: {{ cellId: cell.cellId, target: cell.target, viewport: cell.viewport, outcome: 'internal_cell_error', skipped: true, skipReason: 'browser authority lost', durationMs: 1, cache: {{ hit: false }}, cleanup: {{ status: 'completed' }}, execution: {{ planIndex: cell.planIndex, executionIndex }} }},
  unsafeStop: 'browser-authority-lost',
}});
const result = await executePlan({{}}, cells, {{ execution: {{ maxConcurrency: 2 }}, progressOut: {progress} }}, 'fixture-browser', new Map(), null, runner);
if (result.pages.length !== 3 || result.pages.slice(1).some((page) => page.outcome !== 'unsafe_stop_unexecuted')) process.exit(7);
if (result.executionCount !== 1 || result.unsafeStop !== 'browser-authority-lost') process.exit(8);
"#,
        module = serde_json::to_string(&module_url).map_err(|error| error.to_string())?,
        progress =
            serde_json::to_string(&progress.to_string_lossy()).map_err(|error| error.to_string())?
    );
    run_node_probe(root, &probe, timeout)?;
    Ok(6)
}

fn review_config(root: &Path, repo: &Path, base: &str) -> Value {
    contracted_config(
        root,
        json!({
            "repoRoot":repo,
            "targets":[{"url":format!("{base}/dynamic-review.html"),"reviewInputs":[{"path":"ui/screen.css","kind":"style"}]}],
            "viewports":[{"name":"mobile","width":390,"height":844}]
        }),
    )
}

fn write_review_decisions(
    path: &Path,
    key: &str,
    decision: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let mut row = json!({"reviewCellKey":key,"decision":decision});
    if let Some(note) = note {
        row["note"] = json!(note);
    }
    crate::audit_queue::write_json(path, &json!({"decisions":[row]}))
}

fn run_review_phase(
    root: &Path,
    server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    let repo = work.join("review-repo");
    create_directory_all_nofollow(&repo.join("ui"), 0o700).map_err(|error| error.to_string())?;
    write_bytes_nofollow(
        &repo.join("ui/screen.css"),
        b".screen { padding: 16px; }\n",
        0o600,
    )
    .map_err(|error| error.to_string())?;
    write_bytes_nofollow(&repo.join("backend.txt"), b"unrelated backend v1\n", 0o600)
        .map_err(|error| error.to_string())?;
    let base = server.base_url();
    let base_config = review_config(root, &repo, &base);
    let first_dir = work.join("first");
    let first = run_verifier(root, &base_config, &first_dir, &[0], timeout, &[])?;
    if first.pointer("/review/pendingCount") != Some(&json!(1)) {
        return Err("a newly covered cell did not enter visual review".to_owned());
    }
    let cell = first
        .pointer("/review/cells/0")
        .ok_or_else(|| "first review cell is missing".to_owned())?;
    let key = cell
        .get("reviewCellKey")
        .and_then(Value::as_str)
        .ok_or_else(|| "review cell key is missing".to_owned())?
        .to_owned();
    let first_hash = cell
        .pointer("/screenshots/viewport/sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| "first review screenshot hash is missing".to_owned())?
        .to_owned();
    let decisions = first_dir.join("decisions.json");
    write_review_decisions(&decisions, &key, "pass", None)?;
    let prior = first_dir.join("manual-review.json");
    let review = crate::formal_review::finalize(
        &first_dir.join("report.json"),
        &first_dir.join("review-queue.json"),
        Some(&decisions),
        "2026-09-04T00:01:00Z",
    )?;
    crate::formal_review::write_new_review(&prior, &review)?;
    if crate::formal_review::summary(&review)?["ok"] != true {
        return Err("pass review remained blocking".to_owned());
    }

    let mut second_config = base_config.clone();
    second_config["reviewAgainst"] = json!(prior.clone());
    let second = run_verifier(
        root,
        &second_config,
        &work.join("second"),
        &[0],
        timeout,
        &[],
    )?;
    if second.pointer("/review/pendingCount") != Some(&json!(0))
        || second.pointer("/review/carriedPassCount") != Some(&json!(1))
    {
        return Err("unchanged inputs did not carry the prior review pass".to_owned());
    }
    let second_hash = second
        .pointer("/review/cells/0/screenshots/viewport/sha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    if first_hash == second_hash {
        return Err("dynamic fixture did not prove pixel drift is ignored".to_owned());
    }

    write_bytes_nofollow(&repo.join("backend.txt"), b"unrelated backend v2\n", 0o600)
        .map_err(|error| error.to_string())?;
    let unrelated = run_verifier(
        root,
        &second_config,
        &work.join("unrelated"),
        &[0],
        timeout,
        &[],
    )?;
    if unrelated.pointer("/review/pendingCount") != Some(&json!(0)) {
        return Err("unrelated file change triggered visual review".to_owned());
    }

    write_bytes_nofollow(
        &repo.join("ui/screen.css"),
        b".screen { padding: 24px; }\n",
        0o600,
    )
    .map_err(|error| error.to_string())?;
    let changed = run_verifier(
        root,
        &second_config,
        &work.join("mapped-change"),
        &[0],
        timeout,
        &[],
    )?;
    if changed.pointer("/review/pendingCount") != Some(&json!(1)) {
        return Err("mapped UI input change did not trigger review".to_owned());
    }
    write_bytes_nofollow(
        &repo.join("ui/screen.css"),
        b".screen { padding: 16px; }\n",
        0o600,
    )
    .map_err(|error| error.to_string())?;

    let mut intent = second_config.clone();
    intent["targets"][0]["theme"] = json!("mixed");
    let intent = run_verifier(
        root,
        &intent,
        &work.join("intent-change"),
        &[0],
        timeout,
        &[],
    )?;
    if intent.pointer("/review/pendingCount") != Some(&json!(1)) {
        return Err("changed theme intent did not trigger review".to_owned());
    }

    let mut viewport = second_config.clone();
    viewport["viewports"] = json!([{"name":"mobile","width":390,"height":844},{"name":"desktop","width":1280,"height":800}]);
    let viewport = run_verifier(
        root,
        &viewport,
        &work.join("new-viewport"),
        &[0],
        timeout,
        &[],
    )?;
    if viewport.pointer("/review/pendingCount") != Some(&json!(1))
        || viewport.pointer("/review/carriedPassCount") != Some(&json!(1))
    {
        return Err("new viewport did not preserve and extend review coverage".to_owned());
    }

    let mut raw = base_config.clone();
    raw["reviewAgainst"] = json!(first_dir.join("report.json"));
    let raw = run_verifier(root, &raw, &work.join("raw-prior"), &[2], timeout, &[])?;
    if !raw
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("not a supported reviewed manifest"))
    {
        return Err("unreviewed formal report was accepted as a baseline".to_owned());
    }

    let mut removed = second_config.clone();
    removed["viewports"] = json!([{"name":"desktop","width":1280,"height":800}]);
    let undisposed = run_verifier(
        root,
        &removed,
        &work.join("removed-undisposed"),
        &[3],
        timeout,
        &[],
    )?;
    if undisposed
        .pointer("/coverage/reviewFailures")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err("removed reviewed cell disappeared without a disposition".to_owned());
    }
    removed["reviewRemovedCells"] = json!([{"reviewCellKey":key.clone(),"reason":"The mobile viewport is no longer supported"}]);
    let disposed = run_verifier(
        root,
        &removed,
        &work.join("removed-disposed"),
        &[0],
        timeout,
        &[],
    )?;
    if disposed
        .pointer("/coverage/reviewFailures")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        return Err("explicitly dispositioned removed cell still failed coverage".to_owned());
    }

    let gap_decisions = first_dir.join("gap-decisions.json");
    write_review_decisions(
        &gap_decisions,
        &key,
        "gap",
        Some("Palette needs correction"),
    )?;
    let prior_gap = first_dir.join("manual-review-gap.json");
    let gap_review = crate::formal_review::finalize(
        &first_dir.join("report.json"),
        &first_dir.join("review-queue.json"),
        Some(&gap_decisions),
        "2026-09-04T00:02:00Z",
    )?;
    crate::formal_review::write_new_review(&prior_gap, &gap_review)?;
    if crate::formal_review::summary(&gap_review)?["ok"] != false {
        return Err("gap review was not blocking".to_owned());
    }
    let mut carried_config = base_config.clone();
    carried_config["reviewAgainst"] = json!(prior_gap.clone());
    let carried = run_verifier(
        root,
        &carried_config,
        &work.join("carried-gap"),
        &[1],
        timeout,
        &[],
    )?;
    if carried.pointer("/review/pendingCount") != Some(&json!(0)) {
        return Err("unchanged prior gap reopened images".to_owned());
    }
    require_rule(&carried, "manual-review-gap-carried", "critical")?;

    let screenshot = PathBuf::from(
        cell.pointer("/screenshots/viewport/path")
            .and_then(Value::as_str)
            .ok_or_else(|| "review screenshot path is missing".to_owned())?,
    );
    let mut bytes = read_bytes_nofollow(&screenshot, Some(&first_dir))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "review screenshot is missing".to_owned())?;
    bytes.extend_from_slice(b"tampered");
    write_bytes_nofollow(&screenshot, &bytes, 0o600).map_err(|error| error.to_string())?;
    let error = crate::formal_review::validate(
        &prior,
        &first_dir.join("report.json"),
        &first_dir.join("review-queue.json"),
    )
    .unwrap_err();
    if !error.contains("hash mismatch") {
        return Err("screenshot tampering was not rejected".to_owned());
    }
    Ok(11)
}

fn run_compatibility_phase(
    root: &Path,
    server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    let base = server.base_url();
    let wait = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}],"viewports":[{"name":"mobile","width":390,"height":844}],
                "waitFor":{"selector":"main","networkIdleMs":500,"settleMs":100},"rules":{"failOn":"critical"}
            }),
        ),
        &work.join("structured-wait"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&wait)?;

    let lazy = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/lazy-scroll.html")}],"viewports":[{"name":"mobile","width":390,"height":844}]
            }),
        ),
        &work.join("lazy-no-scroll"),
        &[0],
        timeout,
        &["--no-scroll"],
    )?;
    if has_rule(&lazy, "clipped-x", None) {
        return Err("no-scroll mode created lazy below-fold content".to_owned());
    }

    let clean_state = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/interaction-states.html"),"includeBase":false,"states":[{
                    "name":"help-open","actions":[{"action":"click","selector":"#open-good"}],
                    "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                }]}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("clean-interaction"),
        &[0],
        timeout,
        &[],
    )?;
    no_critical(&clean_state)?;

    let distinct = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[
                    {"name":"base-only","url":format!("{base}/interaction-states.html")},
                    {"name":"opened-only","url":format!("{base}/interaction-states.html"),"includeBase":false,"states":[{
                        "name":"actions-open","actions":[{"action":"click","selector":"#open-bad"}],"waitFor":{"selector":"#bad-panel:not([hidden])","settleMs":25},
                        "continuation":{"kind":"in-page","anchor":"main","focusWithin":"main"}
                    }]}
                ],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("same-url-states"),
        &[1],
        timeout,
        &[],
    )?;
    let states = distinct["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|page| page.pointer("/target/stateName").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if distinct["pages"]
        .as_array()
        .is_none_or(|pages| pages.len() != 2)
        || states != BTreeSet::from(["base", "actions-open"])
    {
        return Err("same URL with distinct state configurations collapsed coverage".to_owned());
    }

    let missing_contract = run_verifier(
        root,
        &json!({
            "targets":[{"url":format!("{base}/clean.html")}],"viewports":[{"name":"mobile","width":390,"height":844}]
        }),
        &work.join("missing-contract"),
        &[3],
        timeout,
        &[],
    )?;
    if missing_contract.pointer("/pages/0/outcome") != Some(&json!("journey_contract_error")) {
        return Err("target without journey intent did not fail coverage".to_owned());
    }

    let breakpoint = contracted_config(
        root,
        json!({
            "targets":[
                {"name":"breakpoint edge","url":format!("{base}/breakpoint-edge.html"),"breakpointProfile":{"name":"navigation","breakpoints":[768],"height":800}},
                {"name":"plain target","url":format!("{base}/clean.html")}
            ],"viewports":[{"name":"configured-768","width":768,"height":800}],"maxPageCount":4
        }),
    );
    let breakpoint_report = run_verifier(
        root,
        &breakpoint,
        &work.join("breakpoint"),
        &[1],
        timeout,
        &[],
    )?;
    let edge_pages = breakpoint_report["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|page| page.pointer("/target/name") == Some(&json!("breakpoint edge")))
        .collect::<Vec<_>>();
    let mut widths = edge_pages
        .iter()
        .filter_map(|page| page.pointer("/viewport/width").and_then(Value::as_u64))
        .collect::<Vec<_>>();
    widths.sort_unstable();
    let plain = breakpoint_report["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|page| page.pointer("/target/name") == Some(&json!("plain target")))
        .collect::<Vec<_>>();
    if widths != [767, 768, 769]
        || plain.len() != 1
        || plain[0].pointer("/viewport/width") != Some(&json!(768))
        || !edge_pages.iter().any(|page| {
            page.pointer("/viewport/width") == Some(&json!(767))
                && page_rules(page)
                    .iter()
                    .any(|(rule, _)| *rule == "clipped-x")
        })
        || breakpoint_report.pointer("/coverage/widthCoverageMode") != Some(&json!("sampled-only"))
    {
        return Err(
            "breakpoint profile did not preserve exact sampled boundary coverage".to_owned(),
        );
    }
    let exact = edge_pages
        .iter()
        .find(|page| page.pointer("/viewport/width") == Some(&json!(768)))
        .ok_or_else(|| "exact breakpoint page is missing".to_owned())?;
    let sources = exact
        .pointer("/viewport/sampling/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    if sources != BTreeSet::from(["configured", "breakpoint-profile"]) {
        return Err("equivalent breakpoint cells were not deduplicated".to_owned());
    }
    let mut over_budget = breakpoint.clone();
    over_budget["maxPageCount"] = json!(3);
    let budget = run_verifier(
        root,
        &over_budget,
        &work.join("breakpoint-budget"),
        &[2],
        timeout,
        &[],
    )?;
    if !budget
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("exceeding maxPageCount 3"))
    {
        return Err("breakpoint expansion did not fail at page budget".to_owned());
    }

    let change_repo = work.join("change-repo");
    create_directory_all_nofollow(&change_repo.join("ui"), 0o700)
        .map_err(|error| error.to_string())?;
    write_bytes_nofollow(
        &change_repo.join("ui/a.css"),
        b".a { color: #111; }\n",
        0o600,
    )
    .map_err(|error| error.to_string())?;
    write_bytes_nofollow(
        &change_repo.join("ui/b.css"),
        b".b { color: #111; }\n",
        0o600,
    )
    .map_err(|error| error.to_string())?;
    let changed = contracted_config(
        root,
        json!({
            "repoRoot":change_repo,
            "targets":[
                {"name":"route-a","url":format!("{base}/clean.html"),"reviewInputs":[{"path":"ui/a.css","kind":"style"}]},
                {"name":"route-b","url":format!("{base}/clean.html"),"reviewInputs":[{"path":"ui/b.css","kind":"style"}]}
            ],"development":{"changedPaths":["ui/a.css"]},"viewports":[{"name":"desktop","width":1280,"height":800}]
        }),
    );
    let subset = run_verifier(
        root,
        &changed,
        &work.join("changed-subset"),
        &[0],
        timeout,
        &[],
    )?;
    let names = subset["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|page| page.pointer("/target/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if names != ["route-a"]
        || subset.pointer("/plan/selection/fullPlanCount") != Some(&json!(2))
        || subset.pointer("/plan/selection/selectedCount") != Some(&json!(1))
        || subset.pointer("/coverage/readinessEligible") != Some(&json!(false))
    {
        return Err("changed input selection did not isolate one non-readiness target".to_owned());
    }
    let mut unmapped = changed.clone();
    unmapped["development"]["changedPaths"] = json!(["unmapped/shared.css"]);
    let unmapped = run_verifier(
        root,
        &unmapped,
        &work.join("changed-unmapped"),
        &[0],
        timeout,
        &[],
    )?;
    if unmapped["pages"]
        .as_array()
        .is_none_or(|pages| pages.len() != 2)
        || unmapped.pointer("/plan/selection/fallbackToFull") != Some(&json!(true))
    {
        return Err("unmapped change did not safely expand to full plan".to_owned());
    }

    for value in [true, false] {
        let report = run_verifier(
            root,
            &contracted_config(
                root,
                json!({
                    "targets":[{"url":format!("{base}/clean.html")}],"viewports":[{"name":"desktop","width":1280,"height":800}],"receiptOnly":value
                }),
            ),
            &work.join(format!("receipt-config-{value}")),
            &[0],
            timeout,
            &[],
        )?;
        no_critical(&report)?;
    }
    let alias = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
        &work.join("receipt-alias"),
        &[0],
        timeout,
        &["--receipt-only"],
    )?;
    no_critical(&alias)?;
    let invalid = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}],"receiptOnly":"yes"
            }),
        ),
        &work.join("receipt-invalid"),
        &[2],
        timeout,
        &[],
    )?;
    if !invalid
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("receiptOnly must be a boolean"))
    {
        return Err("non-boolean receiptOnly did not fail through setup artifacts".to_owned());
    }
    let severity = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{base}/clean.html")}]
            }),
        ),
        &work.join("invalid-severity"),
        &[2],
        timeout,
        &["--fail-on", "not-a-severity"],
    )?;
    if !severity
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("Invalid failOn severity"))
    {
        return Err("invalid failOn severity did not preserve its diagnostic".to_owned());
    }
    Ok(14)
}

struct ReceiptInvocation {
    report: Value,
    json_path: PathBuf,
    markdown_path: PathBuf,
}

fn invoke_receipt(
    root: &Path,
    arguments: &[String],
    expected_exit: i32,
    timeout: Duration,
    cwd: &Path,
    environment: &[(String, String)],
) -> Result<ReceiptInvocation, String> {
    let node = std::env::var_os("FORMAL_WEB_UI_NODE").unwrap_or_else(|| "node".into());
    let verifier = root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
    let mut command = Command::new(node);
    command
        .arg(verifier)
        .arg("--playwright-module-dir")
        .arg(playwright_module_dir(root)?)
        .args(arguments)
        .current_dir(cwd)
        .env_remove("NODE_PATH")
        .env_remove("DEVCOORDINATOR_EVIDENCE_DIR")
        .env_remove("DEVCOORDINATOR_RUN_ID")
        .env_remove("DEVCOORDINATOR_CHECK_NAME")
        .envs(environment.iter().cloned())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, stdout, stderr) = wait_child(
        command
            .spawn()
            .map_err(|error| format!("cannot launch formal verifier: {error}"))?,
        timeout,
    )?;
    if status.code() != Some(expected_exit) || !stderr.is_empty() {
        return Err(format!(
            "formal receipt invocation failed: status={:?} stderr={}",
            status.code(),
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    if stdout.len() > RECEIPT_LIMIT {
        return Err("formal receipt exceeded 2048 bytes".to_owned());
    }
    let stdout = String::from_utf8(stdout).map_err(|_| "formal receipt is not UTF-8")?;
    if stdout.trim().is_empty() || stdout.trim().contains('\n') {
        return Err("formal stdout is not one receipt line".to_owned());
    }
    let receipt: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("formal receipt is invalid JSON: {error}"))?;
    if receipt["exitCode"] != expected_exit {
        return Err("formal receipt exit code mismatch".to_owned());
    }
    let (json_path, markdown_path) = receipt_paths(&receipt)?;
    let report: Value = serde_json::from_slice(
        &read_bytes_nofollow(&json_path, None)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "receipt JSON artifact is missing".to_owned())?,
    )
    .map_err(|error| format!("receipt JSON artifact is invalid: {error}"))?;
    let markdown = String::from_utf8(
        read_bytes_nofollow(&markdown_path, None)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "receipt Markdown artifact is missing".to_owned())?,
    )
    .map_err(|_| "receipt Markdown artifact is not UTF-8")?;
    if expected_exit == 2 {
        validate_setup_report(&report, &markdown)?;
    } else {
        validate_complete_report(
            &report,
            &markdown,
            json_path
                .parent()
                .ok_or_else(|| "report has no parent".to_owned())?,
        )?;
    }
    Ok(ReceiptInvocation {
        report,
        json_path,
        markdown_path,
    })
}

fn run_output_phase(
    root: &Path,
    server: &StaticServer,
    work: &Path,
    timeout: Duration,
) -> Result<usize, String> {
    create_directory_all_nofollow(work, 0o700).map_err(|error| error.to_string())?;
    let config_path = work.join("clean-contract.json");
    crate::audit_queue::write_json(
        &config_path,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("{}/clean.html",server.base_url())}],"viewports":[{"name":"desktop","width":1280,"height":800}]
            }),
        ),
    )?;
    let audited = work.join("audited-worktree");
    let automatic_root = work.join("auto-artifacts");
    create_directory_all_nofollow(&audited, 0o700).map_err(|error| error.to_string())?;
    create_directory_all_nofollow(&automatic_root, 0o700).map_err(|error| error.to_string())?;
    let automatic = invoke_receipt(
        root,
        &[
            "--config".to_owned(),
            config_path.to_string_lossy().into_owned(),
        ],
        0,
        timeout,
        &audited,
        &[(
            "TMPDIR".to_owned(),
            automatic_root.to_string_lossy().into_owned(),
        )],
    )?;
    if !automatic.json_path.starts_with(&automatic_root)
        || automatic.json_path.parent() == Some(automatic_root.as_path())
        || !automatic
            .json_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("formal-web-ui-verification-"))
        || automatic.json_path.starts_with(&audited)
    {
        return Err(
            "automatic formal artifacts were not isolated in a unique external directory"
                .to_owned(),
        );
    }

    let governed = work.join("governed-run/checks/formal-ui/check/evidence");
    let browser_tmp = work.join("browser-tmp");
    create_directory_all_nofollow(&browser_tmp, 0o700).map_err(|error| error.to_string())?;
    let governed_env = vec![
        (
            "DEVCOORDINATOR_EVIDENCE_DIR".to_owned(),
            governed.to_string_lossy().into_owned(),
        ),
        (
            "DEVCOORDINATOR_RUN_ID".to_owned(),
            "t20260902T010203Z-abcdef".to_owned(),
        ),
        (
            "DEVCOORDINATOR_CHECK_NAME".to_owned(),
            "formal-ui".to_owned(),
        ),
        (
            "TMPDIR".to_owned(),
            browser_tmp.to_string_lossy().into_owned(),
        ),
    ];
    let governed_run = invoke_receipt(
        root,
        &[
            "--config".to_owned(),
            config_path.to_string_lossy().into_owned(),
        ],
        0,
        timeout,
        &audited,
        &governed_env,
    )?;
    let journey_path = governed.join("journey-evidence.json");
    let journey: Value = serde_json::from_slice(
        &read_bytes_nofollow(&journey_path, Some(&governed))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "governed journey evidence is missing".to_owned())?,
    )
    .map_err(|error| error.to_string())?;
    if governed_run.json_path.parent() != Some(governed.as_path())
        || journey["governedRunId"] != "t20260902T010203Z-abcdef"
        || journey["governedCheck"] != "formal-ui"
        || governed_run.report.pointer("/evidence/journey/path") != Some(&json!(journey_path))
    {
        return Err("governed run evidence did not bind its exact leaf".to_owned());
    }

    let mut custom_runs = BTreeSet::new();
    for index in 0..2 {
        let directory = work.join(format!("governed-custom-{index}"));
        let json_path = directory.join("report.json");
        let markdown_path = directory.join("report.md");
        let screenshots = directory.join("screenshots");
        let invocation = invoke_receipt(
            root,
            &[
                "--config".to_owned(),
                config_path.to_string_lossy().into_owned(),
                "--json-out".to_owned(),
                json_path.to_string_lossy().into_owned(),
                "--markdown-out".to_owned(),
                markdown_path.to_string_lossy().into_owned(),
                "--screenshot-dir".to_owned(),
                screenshots.to_string_lossy().into_owned(),
            ],
            0,
            timeout,
            &audited,
            &governed_env,
        )?;
        if invocation.json_path != json_path || invocation.markdown_path != markdown_path {
            return Err("custom output was replaced by governed publication".to_owned());
        }
        custom_runs.insert(invocation.report["runId"].as_str().unwrap_or("").to_owned());
    }
    let bundles = std::fs::read_dir(governed.join("formal-runs"))
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if bundles.len() != 2 || custom_runs.len() != 2 {
        return Err("governed custom outputs did not publish unique bundles".to_owned());
    }
    let mut published = BTreeSet::new();
    for bundle in bundles {
        let name = bundle
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("governed custom bundle identity is invalid".to_owned());
        }
        let manifest_path = bundle.join("journey-evidence.json");
        let manifest: Value = serde_json::from_slice(
            &read_bytes_nofollow(&manifest_path, Some(&bundle))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "governed bundle manifest is missing".to_owned())?,
        )
        .map_err(|error| error.to_string())?;
        if manifest["governedRunId"] != "t20260902T010203Z-abcdef"
            || manifest["governedCheck"] != "formal-ui"
        {
            return Err("governed custom bundle lost governed identity".to_owned());
        }
        published.insert(manifest["runId"].as_str().unwrap_or("").to_owned());
        for screenshot in manifest["cells"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|cell| {
                cell["screenshots"]
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.values())
            })
            .filter(|screenshot| !screenshot.is_null())
        {
            let relative = screenshot["path"]
                .as_str()
                .ok_or_else(|| "bundle screenshot path is missing".to_owned())?;
            read_bytes_nofollow(&bundle.join(relative), Some(&bundle))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "bundle screenshot is missing or escaped".to_owned())?;
        }
    }
    if published != custom_runs {
        return Err("governed custom bundles lost or replaced a run".to_owned());
    }

    let human_dir = work.join("human");
    create_directory_all_nofollow(&human_dir, 0o700).map_err(|error| error.to_string())?;
    let human_json = human_dir.join("report.json");
    let human_markdown = human_dir.join("report.md");
    let node = std::env::var_os("FORMAL_WEB_UI_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(node);
    command
        .arg(root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs"))
        .arg("--playwright-module-dir")
        .arg(playwright_module_dir(root)?)
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--json-out",
            human_json.to_str().unwrap(),
            "--markdown-out",
            human_markdown.to_str().unwrap(),
            "--human-readable-stdout",
        ])
        .env_remove("NODE_PATH")
        .env_remove("DEVCOORDINATOR_EVIDENCE_DIR")
        .env_remove("DEVCOORDINATOR_RUN_ID")
        .env_remove("DEVCOORDINATOR_CHECK_NAME")
        .env("TMPDIR", &human_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, stdout, stderr) =
        wait_child(command.spawn().map_err(|error| error.to_string())?, timeout)?;
    if !status.success()
        || !stderr.is_empty()
        || !String::from_utf8_lossy(&stdout).contains("# Formal Web UI Verification Report")
    {
        return Err("explicit human-readable stdout did not print the report".to_owned());
    }
    let human_report: Value = serde_json::from_slice(
        &read_bytes_nofollow(&human_json, Some(&human_dir))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "human report is missing".to_owned())?,
    )
    .map_err(|error| error.to_string())?;
    let human_md = String::from_utf8(
        read_bytes_nofollow(&human_markdown, Some(&human_dir))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "human markdown is missing".to_owned())?,
    )
    .map_err(|_| "human markdown is not UTF-8")?;
    validate_complete_report(&human_report, &human_md, &human_dir)?;

    let no_path = invoke_receipt(
        root,
        &["--unknown-option".to_owned()],
        2,
        timeout,
        &audited,
        &[(
            "TMPDIR".to_owned(),
            automatic_root.to_string_lossy().into_owned(),
        )],
    )?;
    if no_path.report["status"] != "setup-failure" {
        return Err("no-path parse failure lacked setup artifact".to_owned());
    }
    Ok(6)
}

struct TlsServer {
    child: std::process::Child,
    port: u16,
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_tls_server(work: &Path, timeout: Duration) -> Result<TlsServer, String> {
    create_directory_all_nofollow(work, 0o700).map_err(|error| error.to_string())?;
    let certificate = work.join("certificate.pem");
    let key = work.join("key.pem");
    let mut request = Command::new("openssl");
    request
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&certificate)
        .args([
            "-days",
            "2",
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, _, stderr) = wait_child(
        request
            .spawn()
            .map_err(|error| format!("cannot launch openssl certificate request: {error}"))?,
        timeout,
    )?;
    if !status.success() {
        return Err(format!(
            "openssl certificate request failed: {}",
            String::from_utf8_lossy(&stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    let probe = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let port = probe
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(probe);
    let child = Command::new("openssl")
        .args(["s_server", "-quiet", "-WWW", "-accept"])
        .arg(format!("127.0.0.1:{port}"))
        .arg("-cert")
        .arg(&certificate)
        .arg("-key")
        .arg(&key)
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot launch openssl TLS fixture: {error}"))?;
    let started = Instant::now();
    while TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(100),
    )
    .is_err()
    {
        if started.elapsed() > Duration::from_secs(5) {
            return Err("TLS fixture did not become ready".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(TlsServer { child, port })
}

fn run_tls_phase(root: &Path, work: &Path, timeout: Duration) -> Result<usize, String> {
    let pages = load_pages(root)?;
    let body = pages
        .get("clean.html")
        .ok_or_else(|| "clean TLS fixture is missing".to_owned())?;
    create_directory_all_nofollow(work, 0o700).map_err(|error| error.to_string())?;
    write_bytes_nofollow(&work.join("clean.html"), body, 0o600)
        .map_err(|error| error.to_string())?;
    let server = start_tls_server(work, timeout)?;
    let report = run_verifier(
        root,
        &contracted_config(
            root,
            json!({
                "targets":[{"url":format!("https://127.0.0.1:{}/clean.html",server.port)}],"viewports":[{"name":"mobile","width":390,"height":844}]
            }),
        ),
        &work.join("report"),
        &[0],
        timeout,
        &["--ignore-https-errors"],
    )?;
    no_critical(&report)?;
    if !report
        .pointer("/pages/0/metrics")
        .is_some_and(Value::is_object)
    {
        return Err("self-signed TLS page was skipped".to_owned());
    }
    Ok(1)
}

fn sibling_coordinator_fixture() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(executable
        .parent()
        .ok_or_else(|| "tooling executable has no parent".to_owned())?
        .join("devcoordinator2-selftest-coordinator"))
}

fn run_discovery_phase(
    root: &Path,
    work: &Path,
    timeout: Duration,
    fixture_override: Option<&Path>,
) -> Result<usize, String> {
    let fixture = fixture_override
        .map(Path::to_owned)
        .map(Ok)
        .unwrap_or_else(sibling_coordinator_fixture)?;
    if !fixture.is_file() {
        return Err(format!(
            "test-only Rust coordinator fixture is missing: {}",
            fixture.display()
        ));
    }
    let base = contracted_config(
        root,
        json!({
            "fromCoordinator":true,"coordinatorCommand":fixture,"minCheckedPages":0,
            "viewports":[{"name":"desktop","width":1280,"height":800}]
        }),
    );
    let failed = run_verifier(root, &base, &work.join("default"), &[3], timeout, &[])?;
    if failed.pointer("/coverage/failed") != Some(&json!(true)) {
        return Err("discovered target failure was not required by default".to_owned());
    }
    let mut tolerated = base.clone();
    tolerated["allowDiscoveredTargetFailures"] = json!(true);
    let tolerated = run_verifier(
        root,
        &tolerated,
        &work.join("tolerated"),
        &[0],
        timeout,
        &[],
    )?;
    if tolerated.pointer("/coverage/failed") != Some(&json!(false))
        || tolerated
            .pointer("/coverage/tolerated")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err("explicit discovered-target tolerance was not preserved".to_owned());
    }
    create_directory_all_nofollow(work, 0o700).map_err(|error| error.to_string())?;
    let legacy = work.join("coordinator-protocol-v1");
    std::fs::copy(&fixture, &legacy)
        .map_err(|error| format!("cannot copy v1 executable fixture: {error}"))?;
    let mut legacy_config = base;
    legacy_config["coordinatorCommand"] = json!(legacy);
    let legacy = run_verifier(
        root,
        &legacy_config,
        &work.join("v1-rejected"),
        &[2],
        timeout,
        &[],
    )?;
    if !legacy
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("invalid response"))
    {
        return Err("protocol-v1 deployment discovery was not rejected".to_owned());
    }
    Ok(3)
}

fn validate_skill_contract(root: &Path) -> Result<(), String> {
    let path = root.join("skills/formal-web-ui-verification/SKILL.md");
    let bytes = read_bytes_nofollow(&path, Some(root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "formal UI skill contract is missing".to_owned())?;
    let contract =
        String::from_utf8(bytes).map_err(|_| "formal UI skill contract is not UTF-8".to_owned())?;
    let required = [
        "--human-readable-stdout",
        "--receipt-only",
        "human-only",
        "bounded",
        "control-text-clipped",
        "control-outside-container",
        "requiredCoverage",
        "data-ui-verify-svg-overlap",
        "svg-internal-overlap",
        "data-ui-verify-min-content-inset",
        "breakpointProfile",
        "maxPageCount",
        "sampled-only",
        "sourceBinding",
        "journey_review_contract.md",
        "review-queue.json",
        "journey-evidence.json",
        "devcoordinator2-tooling formal-ui review",
        "secondary-workflow-precedes-primary",
        "insufficient-text-contrast",
        "declared-theme-contradiction",
        "source-over compositing",
        "inert",
        "aria-hidden=true",
    ];
    let missing = required
        .iter()
        .filter(|token| !contract.contains(**token))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() || !contract.to_ascii_lowercase().contains("deprecated") {
        return Err(format!(
            "formal UI skill contract omits required behavior: {missing:?}"
        ));
    }
    let reference =
        root.join("skills/formal-web-ui-verification/references/journey_review_contract.md");
    let reference = read_bytes_nofollow(&reference, Some(root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "journey review reference is missing".to_owned())?;
    let reference =
        String::from_utf8(reference).map_err(|_| "journey review reference is not UTF-8")?;
    for token in [
        "primaryJourney",
        "continuation",
        "reviewInputs",
        "manual-review.json",
        "Screenshot SHA-256",
    ] {
        if !reference.contains(token) {
            return Err(format!("journey review reference lacks {token}"));
        }
    }
    let verifier = root.join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
    let verifier = read_bytes_nofollow(&verifier, Some(root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "formal Node verifier is missing".to_owned())?;
    let verifier = String::from_utf8(verifier).map_err(|_| "formal Node verifier is not UTF-8")?;
    if verifier.contains("settleMs = 120") || verifier.contains("waitForTimeout(120") {
        return Err("formal Node verifier retained a deliberate delay above 100 ms".to_owned());
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SelfTestOptions {
    pub workspace_parent: Option<PathBuf>,
    pub keep: bool,
    pub timeout_seconds: u64,
    pub phase: String,
    pub coordinator_fixture: Option<PathBuf>,
}

fn settle_phase(
    label: &str,
    result: Result<usize, String>,
    count: &mut usize,
    failures: &mut Vec<String>,
) {
    match result {
        Ok(value) => *count = value,
        Err(error) => failures.push(format!("{label}: {error}")),
    }
}

pub fn run(options: &SelfTestOptions) -> Result<Value, String> {
    if options.timeout_seconds == 0 {
        return Err("formal UI self-test timeout must be positive".to_owned());
    }
    if ![
        "all",
        "static",
        "rendering",
        "state",
        "transport",
        "performance",
        "auth",
        "scheduler",
        "review",
        "compatibility",
        "output",
        "tls",
        "discovery",
    ]
    .contains(&options.phase.as_str())
    {
        return Err(
            "formal UI self-test phase must be all, static, rendering, state, transport, performance, auth, scheduler, review, compatibility, output, tls, or discovery"
                .to_owned(),
        );
    }
    let root = source_root();
    validate_skill_contract(&root)?;
    let pages = load_pages(&root)?;
    let mut matrix = load_matrix(&root)?;
    if options.phase == "rendering" {
        matrix.cases.retain(|case| {
            case.name.starts_with("translucent-") || case.name.starts_with("custom-modal-")
        });
    }
    for case in &matrix.cases {
        if !pages.contains_key(&case.path) {
            return Err(format!(
                "formal UI matrix case {} references missing page {}",
                case.name, case.path
            ));
        }
    }
    let work = unique_work_dir(options.workspace_parent.as_deref())?;
    let result: Result<Value, String> = (|| {
        let needs_static_server = [
            "all",
            "static",
            "rendering",
            "state",
            "transport",
            "scheduler",
            "review",
            "compatibility",
            "output",
        ]
        .contains(&options.phase.as_str());
        let server = needs_static_server
            .then(|| StaticServer::start(pages))
            .transpose()?;
        let timeout = Duration::from_secs(options.timeout_seconds);
        let mut static_cases = 0usize;
        let mut stateful_cases = 0usize;
        let mut transport_cases = 0usize;
        let mut performance_cases = 0usize;
        let mut auth_cases = 0usize;
        let mut scheduler_cases = 0usize;
        let mut review_cases = 0usize;
        let mut compatibility_cases = 0usize;
        let mut output_cases = 0usize;
        let mut tls_cases = 0usize;
        let mut discovery_cases = 0usize;
        let mut failures = Vec::new();
        if ["all", "static", "rendering"].contains(&options.phase.as_str()) {
            let server = server.as_ref().unwrap();
            let result = (|| {
                let config = build_matrix_config(&root, server, &matrix);
                let expected_exit = if matrix.cases.iter().any(|case| case.critical) {
                    1
                } else {
                    0
                };
                let report = run_verifier(
                    &root,
                    &config,
                    &work.join("core-matrix"),
                    &[expected_exit],
                    timeout,
                    &[],
                )?;
                validate_matrix_report(&report, &matrix)?;
                Ok(matrix.cases.len())
            })();
            settle_phase("static", result, &mut static_cases, &mut failures);
        }
        if ["all", "state"].contains(&options.phase.as_str()) {
            settle_phase(
                "state",
                run_state_and_wait_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("state-and-wait"),
                    timeout,
                ),
                &mut stateful_cases,
                &mut failures,
            );
        }
        if ["all", "transport"].contains(&options.phase.as_str()) {
            settle_phase(
                "transport",
                run_transport_and_cache_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("transport-and-cache"),
                    timeout,
                ),
                &mut transport_cases,
                &mut failures,
            );
        }
        if ["all", "performance"].contains(&options.phase.as_str()) {
            settle_phase(
                "performance",
                run_performance_phase(&root, &work.join("performance"), timeout),
                &mut performance_cases,
                &mut failures,
            );
        }
        if ["all", "auth"].contains(&options.phase.as_str()) {
            settle_phase(
                "auth",
                run_auth_phase(&root, &work.join("auth"), timeout),
                &mut auth_cases,
                &mut failures,
            );
        }
        if ["all", "scheduler"].contains(&options.phase.as_str()) {
            settle_phase(
                "scheduler",
                run_scheduler_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("scheduler"),
                    timeout,
                ),
                &mut scheduler_cases,
                &mut failures,
            );
        }
        if ["all", "review"].contains(&options.phase.as_str()) {
            settle_phase(
                "review",
                run_review_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("review"),
                    timeout,
                ),
                &mut review_cases,
                &mut failures,
            );
        }
        if ["all", "compatibility"].contains(&options.phase.as_str()) {
            settle_phase(
                "compatibility",
                run_compatibility_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("compatibility"),
                    timeout,
                ),
                &mut compatibility_cases,
                &mut failures,
            );
        }
        if ["all", "output"].contains(&options.phase.as_str()) {
            settle_phase(
                "output",
                run_output_phase(
                    &root,
                    server.as_ref().unwrap(),
                    &work.join("output"),
                    timeout,
                ),
                &mut output_cases,
                &mut failures,
            );
        }
        if ["all", "tls"].contains(&options.phase.as_str()) {
            settle_phase(
                "tls",
                run_tls_phase(&root, &work.join("tls"), timeout),
                &mut tls_cases,
                &mut failures,
            );
        }
        if ["all", "discovery"].contains(&options.phase.as_str()) {
            settle_phase(
                "discovery",
                run_discovery_phase(
                    &root,
                    &work.join("discovery"),
                    timeout,
                    options.coordinator_fixture.as_deref(),
                ),
                &mut discovery_cases,
                &mut failures,
            );
        }
        if !failures.is_empty() {
            return Err(format!(
                "{} phase failure(s): {}",
                failures.len(),
                failures.join(" | ")
            ));
        }
        Ok(json!({
            "ok":true,"suite":"formal-web-ui-verification","phase":options.phase,"static_cases":static_cases,
            "state_and_wait_cases":stateful_cases,
            "transport_and_cache_cases":transport_cases,
            "performance_cases":performance_cases,
            "auth_cases":auth_cases,
            "scheduler_cases":scheduler_cases,
            "review_cases":review_cases,
            "compatibility_cases":compatibility_cases,
            "output_cases":output_cases,
            "tls_cases":tls_cases,
            "discovery_cases":discovery_cases,
            "report":if static_cases>0 {json!(work.join("core-matrix/report.json"))} else {Value::Null},
        }))
    })();
    match result {
        Ok(mut summary) => {
            if options.keep {
                summary["workspace"] = json!(work);
            } else {
                validate_directory_nofollow(&work).map_err(|error| error.to_string())?;
                std::fs::remove_dir_all(&work)
                    .map_err(|error| format!("cannot clean self-test workspace: {error}"))?;
                summary["workspace"] = Value::Null;
            }
            Ok(summary)
        }
        Err(error) => Err(format!(
            "{error}; preserved self-test workspace: {}",
            work.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_fixture_matrix_is_complete_and_contract_bound() {
        let root = source_root();
        validate_skill_contract(&root).unwrap();
        let pages = load_pages(&root).unwrap();
        let matrix = load_matrix(&root).unwrap();
        assert!(matrix.cases.len() >= 60);
        assert!(
            matrix
                .cases
                .iter()
                .all(|case| pages.contains_key(&case.path))
        );
        assert!(matrix.cases.iter().any(|case| case.critical));
        assert!(matrix.cases.iter().any(|case| !case.critical));
        assert!(
            matrix
                .cases
                .iter()
                .any(|case| !case.forbidden_text.is_empty())
        );
    }

    #[test]
    fn all_settled_phase_collection_keeps_later_work_running() {
        let mut first_count = 0;
        let mut later_count = 0;
        let mut failures = Vec::new();
        settle_phase(
            "first",
            Err("injected ordinary mismatch".to_owned()),
            &mut first_count,
            &mut failures,
        );
        let later_ran = true;
        settle_phase("later", Ok(7), &mut later_count, &mut failures);
        assert!(later_ran);
        assert_eq!(first_count, 0);
        assert_eq!(later_count, 7);
        assert_eq!(failures, ["first: injected ordinary mismatch"]);
    }
}
