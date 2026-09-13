//! Linux-only, test-only real-system acceptance for the Rust control plane.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use devcoordinator2_control::ids;
use devcoordinator2_control::test_admission::{begin_drain, end_drain};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const OWNERSHIP_MARKER: &str = ".devcoordinator2-root-acceptance";

#[derive(Debug, Parser)]
#[command(name = "devcoordinator2-root-acceptance")]
struct Cli {
    #[command(subcommand)]
    command: RootCommand,
}

#[derive(Debug, Subcommand)]
enum RootCommand {
    Run {
        #[arg(long)]
        daemon: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        work_root: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        case: Vec<String>,
        #[arg(long, value_parser = parse_compose_subnet)]
        compose_subnet: Option<Ipv4Addr>,
    },
    Request {
        #[arg(long)]
        socket: PathBuf,
    },
}

#[derive(Clone)]
struct Harness {
    daemon: PathBuf,
    fixture: PathBuf,
    executable: PathBuf,
    work_root: PathBuf,
    caller_uid: u32,
    caller_gid: u32,
    compose_subnet: Option<Ipv4Addr>,
}

struct World {
    harness: Harness,
    base: PathBuf,
    repo: PathBuf,
    socket: PathBuf,
    state: PathBuf,
    policy: PathBuf,
    sandboxed: bool,
    unit_prefix: String,
    daemon: Option<Child>,
    cleanup_volumes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CaseResult {
    name: String,
    status: &'static str,
    duration_ms: u128,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct AcceptanceReport {
    schema: u8,
    suite: &'static str,
    status: &'static str,
    cases: usize,
    passed: usize,
    failed: usize,
    results: Vec<CaseResult>,
}

type Case = (&'static str, fn(&mut World) -> Result<(), String>);

macro_rules! ensure {
    ($condition:expr, $message:expr $(,)?) => {
        if !$condition {
            return Err(($message).to_owned());
        }
    };
    ($condition:expr, $format:expr, $($argument:tt)+) => {
        if !$condition {
            return Err(format!($format, $($argument)+));
        }
    };
}

impl World {
    fn new(harness: &Harness, name: &str) -> Result<Self, String> {
        let case_token = &sha256_hex(name.as_bytes())[..12];
        let base = harness
            .work_root
            .join(format!("case-{case_token}-{}", std::process::id()));
        if base.exists() {
            return Err(format!("case directory already exists: {}", base.display()));
        }
        fs::create_dir_all(&base).map_err(|error| error.to_string())?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o711))
            .map_err(|error| error.to_string())?;
        fs::write(base.join(OWNERSHIP_MARKER), b"schema=1\n").map_err(|error| error.to_string())?;
        let repo = base.join("repo");
        fs::create_dir(&repo).map_err(|error| error.to_string())?;
        chown_path(&repo, harness.caller_uid, harness.caller_gid)?;
        run_as(
            harness.caller_uid,
            harness.caller_gid,
            &repo,
            "/usr/bin/git",
            &["init", "-q"],
            &base,
        )?;
        let repository_id = ids::repository_id(&repo).map_err(|error| error.to_string())?;
        let allowlist = base.join("compose-env-allowlist.json");
        write_private_json(
            &allowlist,
            &json!({
                "schema": 1,
                "authorizations": [{
                    "repository_id": repository_id,
                    "path": "compose.env",
                }],
            }),
        )?;
        let socket = base.join("daemon.sock");
        let state = base.join("state");
        for (directory, mode) in [(&state, 0o751), (&state.join("deployments"), 0o755)] {
            fs::create_dir(directory).map_err(|error| error.to_string())?;
            fs::set_permissions(directory, fs::Permissions::from_mode(mode))
                .map_err(|error| error.to_string())?;
        }
        let unit_prefix = format!(
            "devcoordinator2-rustint-{}-{}-test",
            std::process::id(),
            &sha256_hex(name.as_bytes())[..8]
        );
        let mut world = Self {
            harness: harness.clone(),
            base,
            repo,
            socket,
            state,
            policy: allowlist,
            sandboxed: false,
            unit_prefix,
            daemon: None,
            cleanup_volumes: Vec::new(),
        };
        world.start_daemon(None, None, None)?;
        Ok(world)
    }

    fn start_daemon(
        &mut self,
        edge_uid: Option<u32>,
        admin_emails: Option<&str>,
        base_domain: Option<&str>,
    ) -> Result<(), String> {
        if self.daemon.is_some() {
            return Err("isolated daemon is already running".to_owned());
        }
        let _ = fs::remove_file(&self.socket);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.base.join("daemon.log"))
            .map_err(|error| error.to_string())?;
        log.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        let stderr = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = Command::new(&self.harness.daemon);
        command
            .arg("daemon")
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .env("RUST_BACKTRACE", "1")
            .env("DEVCOORDINATOR2_SOCKET", &self.socket)
            .env("DEVCOORDINATOR2_STATE_DIR", &self.state)
            .env("DEVCOORDINATOR2_BUGS_DIR", self.base.join("bugs"))
            .env("DEVCOORDINATOR2_UNIT_PREFIX", &self.unit_prefix)
            .env("DEVCOORDINATOR2_SLICE", "devcoordinator2-tests.slice")
            .env("DEVCOORDINATOR2_CLIENT_GROUP", "")
            .env("DEVCOORDINATOR2_INSTANCE_ENV", "/nonexistent")
            .env("DEVCOORDINATOR2_PORT_RANGE", "46000-49999")
            .env("DEVCOORDINATOR2_COMPOSE_ENV_ALLOWLIST_FILE", &self.policy);
        if let Some(uid) = edge_uid {
            command.env("DEVCOORDINATOR2_EDGE_UID", uid.to_string());
        }
        if let Some(emails) = admin_emails {
            command.env("DEVCOORDINATOR2_ADMIN_EMAILS", emails);
        }
        if let Some(domain) = base_domain {
            command.env("DEVCOORDINATOR2_BASE_DOMAIN", domain);
        }
        if self.sandboxed {
            command = self.sandbox_command(&command)?;
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("cannot start isolated daemon: {error}"))?;
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self
                .socket
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o666))
                    .map_err(|error| error.to_string())?;
                if request_over_socket_with_timeout(
                    &self.socket,
                    &request("ping", json!({}), "other", None),
                    Duration::from_millis(100),
                )
                .is_ok_and(|response| data(&response).is_ok())
                {
                    return Ok(());
                }
            }
            if let Some(status) = self
                .daemon
                .as_mut()
                .expect("daemon child")
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                return Err(format!(
                    "isolated daemon exited before publishing its socket: {status}; log={}",
                    self.base.join("daemon.log").display()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "isolated daemon did not publish its socket; log={}",
                    self.base.join("daemon.log").display()
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn sandbox_command(&self, daemon: &Command) -> Result<Command, String> {
        let mut command = Command::new("/usr/bin/systemd-run");
        command.args(["--quiet", "--pipe", "--wait", "--collect", "--unit"]);
        command.arg(self.sandbox_unit_name());
        let template = include_str!("../../../../deploy/devcoordinator2.service");
        for line in template.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if ["NoNewPrivileges", "ProtectSystem", "ProtectHome"].contains(&key) {
                command.arg(format!("--property={line}"));
            } else if key == "ReadWritePaths" {
                let mut mapped = Vec::new();
                for path in value.split_whitespace() {
                    let target = match path {
                        "/home" => self.repo.as_path(),
                        "/var/lib/devcoordinator2" => self.state.as_path(),
                        "/run/devcoordinator2" => {
                            self.socket.parent().ok_or("socket parent missing")?
                        }
                        "/etc/devcoordinator2" => {
                            self.policy.parent().ok_or("policy parent missing")?
                        }
                        _ => return Err(format!("unmapped service write path: {path}")),
                    };
                    mapped.push(target.to_string_lossy().into_owned());
                }
                command.arg(format!("--property=ReadWritePaths={}", mapped.join(" ")));
            }
        }
        command.arg(format!(
            "--property=ReadOnlyPaths={}",
            self.base.join("sandbox-etc").display()
        ));
        for (key, value) in daemon.get_envs() {
            if let Some(value) = value {
                let mut assignment = OsString::from(key);
                assignment.push("=");
                assignment.push(value);
                command.arg("--setenv").arg(assignment);
            }
        }
        command.arg(daemon.get_program()).args(daemon.get_args());
        Ok(command)
    }

    fn sandbox_unit_name(&self) -> String {
        format!(
            "{}-daemon.service",
            self.unit_prefix.trim_end_matches("-test")
        )
    }

    fn stop_daemon(&mut self, hard: bool) -> Result<(), String> {
        let Some(mut child) = self.daemon.take() else {
            return Ok(());
        };
        if self.sandboxed {
            run_status_allow_absent("/usr/bin/systemctl", &["stop", &self.sandbox_unit_name()])?;
            child.wait().map_err(|error| error.to_string())?;
            return Ok(());
        }
        let signal = if hard { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: the PID belongs to the exact child owned by this World.
        if unsafe { libc::kill(child.id() as i32, signal) } != 0 {
            return Err(format!(
                "cannot signal isolated daemon: {}",
                std::io::Error::last_os_error()
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                child.kill().map_err(|error| error.to_string())?;
                child.wait().map_err(|error| error.to_string())?;
                return Err("isolated daemon did not stop after its deadline".to_owned());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn call(&self, operation: &str, params: Value) -> Result<Value, String> {
        call_as(
            &self.harness.executable,
            self.harness.caller_uid,
            self.harness.caller_gid,
            &self.socket,
            request(operation, params, "other", None),
        )
    }

    fn call_root(&self, operation: &str, params: Value) -> Result<Value, String> {
        request_over_socket(&self.socket, &request(operation, params, "other", None))
    }

    fn write_owned(&self, relative: &str, content: impl AsRef<[u8]>) -> Result<(), String> {
        let path = self.repo.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&path, content).map_err(|error| error.to_string())?;
        chown_path(&path, self.harness.caller_uid, self.harness.caller_gid)
    }

    fn write_config(&self, body: &str) -> Result<(), String> {
        self.write_owned(".devcoordinator.toml", body)
    }

    fn git(&self, arguments: &[&str]) -> Result<(), String> {
        run_as(
            self.harness.caller_uid,
            self.harness.caller_gid,
            &self.repo,
            "/usr/bin/git",
            arguments,
            &self.base,
        )
    }

    fn wait_file(&self, relative: &str, timeout: Duration) -> Result<(), String> {
        let path = self.repo.join(relative);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.is_file() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(format!("fixture file did not appear: {}", path.display()))
    }

    fn wait_status(&self, terminal: &[&str], timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.call("test.status", json!({"path": self.repo}))?;
            let last = data(&response)?.clone();
            if last
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| terminal.contains(&status))
            {
                return Ok(last);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "status did not reach {terminal:?}; last={}",
                    bounded_json(&last)
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn units(&self) -> Result<Vec<String>, String> {
        list_units(&format!("{}-*.service", self.unit_prefix))
    }

    fn wait_units_empty(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let units = self.units()?;
            if units.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "test units survived retirement deadline: {}",
                    units.join(", ")
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn deployment_units(&self) -> Result<Vec<String>, String> {
        list_units(&format!(
            "{}-deploy*.service",
            self.unit_prefix.replace("-test", "")
        ))
    }

    fn track_volume(&mut self, volume: impl Into<String>) {
        let volume = volume.into();
        if !self.cleanup_volumes.contains(&volume) {
            self.cleanup_volumes.push(volume);
        }
    }

    fn forget_volume(&mut self, volume: &str) {
        self.cleanup_volumes.retain(|candidate| candidate != volume);
    }

    fn log_catalog(&self, run_id: &str, check: &str) -> Result<Value, String> {
        let response = self.call(
            "test.log.catalog",
            json!({
                "path": self.repo,
                "run_id": run_id,
                "check": check,
                "phase": "check",
                "limit": 100,
            }),
        )?;
        let value = data(&response)?.clone();
        ensure!(
            !value
                .to_string()
                .contains(&self.repo.to_string_lossy().to_string()),
            "log catalogue disclosed the repository path"
        );
        Ok(value)
    }

    fn log_tail(&self, run_id: &str, check: &str, stream: &str) -> Result<Value, String> {
        let response = self.call(
            "test.log.tail",
            json!({
                "path": self.repo,
                "run_id": run_id,
                "check": check,
                "phase": "check",
                "case": null,
                "stream": stream,
                "lines": 200,
                "max_bytes": 32768,
            }),
        )?;
        Ok(data(&response)?.clone())
    }

    fn cleanup(&mut self, preserve_files: bool) -> Result<(), String> {
        let mut failures = Vec::new();
        if let Err(error) = self.stop_daemon(false) {
            failures.push(error);
        }
        for pattern in [
            format!("{}-*.service", self.unit_prefix),
            format!("{}-deploy*.service", self.unit_prefix.replace("-test", "")),
        ] {
            match list_units(&pattern) {
                Ok(units) => {
                    for unit in units {
                        if let Err(error) = run_status("systemctl", &["stop", &unit]) {
                            failures.push(error);
                        }
                        if let Err(error) =
                            run_status_allow_absent("systemctl", &["reset-failed", &unit])
                        {
                            failures.push(error);
                        }
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        match docker_ids("instance", &self.unit_prefix) {
            Ok(ids) if !ids.is_empty() => {
                let mut arguments = vec!["rm", "-f", "-v"];
                arguments.extend(ids.iter().map(String::as_str));
                if let Err(error) = run_status("docker", &arguments) {
                    failures.push(error);
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(error),
        }
        for volume in std::mem::take(&mut self.cleanup_volumes) {
            let output = Command::new("docker")
                .args(["volume", "rm", "--force", &volume])
                .output();
            match output {
                Ok(output)
                    if output.status.success()
                        || String::from_utf8_lossy(&output.stderr)
                            .to_ascii_lowercase()
                            .contains("no such volume") => {}
                Ok(output) => failures.push(format!(
                    "cannot remove tracked volume {volume}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )),
                Err(error) => {
                    failures.push(format!("cannot remove tracked volume {volume}: {error}"))
                }
            }
        }
        if !preserve_files
            && self.base.join(OWNERSHIP_MARKER).is_file()
            && let Err(error) = fs::remove_dir_all(&self.base)
        {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.cleanup(true);
    }
}

fn request(operation: &str, params: Value, kind: &str, identity: Option<&str>) -> Value {
    let id = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    json!({
        "protocol": 2,
        "id": format!("root-{id:08x}"),
        "operation": operation,
        "params": params,
        "client": {
            "kind": kind,
            "session": "rust-root-acceptance",
            "identity": identity,
        },
    })
}

fn data(response: &Value) -> Result<&Value, String> {
    if response.get("protocol") != Some(&json!(2)) {
        return Err(format!(
            "invalid response protocol: {}",
            bounded_json(response)
        ));
    }
    if response.get("ok") == Some(&Value::Bool(true)) {
        response
            .get("data")
            .ok_or_else(|| "successful response omitted data".to_owned())
    } else {
        Err(format!("operation failed: {}", bounded_json(response)))
    }
}

fn error_code(response: &Value) -> Option<&str> {
    response.pointer("/error/code").and_then(Value::as_str)
}

fn bounded_json(value: &Value) -> String {
    value.to_string().chars().take(1200).collect()
}

fn request_over_socket(socket: &Path, request: &Value) -> Result<Value, String> {
    request_over_socket_with_timeout(socket, request, Duration::from_secs(900))
}

fn request_over_socket_with_timeout(
    socket: &Path,
    request: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut payload = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|error| error.to_string())?;
    if request.get("operation").and_then(Value::as_str) != Some("event.wait") {
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(|error| error.to_string())?;
    }
    let mut response = Vec::new();
    stream
        .take(256 * 1024 + 1)
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    ensure!(
        response.len() <= 256 * 1024,
        "daemon response exceeded 256 KiB"
    );
    serde_json::from_slice(&response).map_err(|error| format!("invalid daemon JSON: {error}"))
}

fn call_as(
    executable: &Path,
    uid: u32,
    gid: u32,
    socket: &Path,
    request: Value,
) -> Result<Value, String> {
    let mut child = Command::new("setpriv")
        .arg(format!("--reuid={uid}"))
        .arg(format!("--regid={gid}"))
        .arg("--clear-groups")
        .arg("--")
        .arg(executable)
        .arg("request")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start credential helper: {error}"))?;
    let mut payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    payload.push(b'\n');
    child
        .stdin
        .take()
        .ok_or_else(|| "credential helper stdin is unavailable".to_owned())?
        .write_all(&payload)
        .map_err(|error| error.to_string())?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("credential helper failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "credential helper exited {}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1000)
                .collect::<String>()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("credential helper returned invalid JSON: {error}"))
}

fn run_as(
    uid: u32,
    gid: u32,
    cwd: &Path,
    program: &str,
    arguments: &[&str],
    home: &Path,
) -> Result<(), String> {
    let output = Command::new("setpriv")
        .arg(format!("--reuid={uid}"))
        .arg(format!("--regid={gid}"))
        .arg("--clear-groups")
        .arg("--")
        .arg(program)
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("GIT_AUTHOR_NAME", "root-acceptance")
        .env("GIT_AUTHOR_EMAIL", "root-acceptance@example.invalid")
        .env("GIT_COMMITTER_NAME", "root-acceptance")
        .env("GIT_COMMITTER_EMAIL", "root-acceptance@example.invalid")
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} exited {}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1000)
                .collect::<String>()
        ))
    }
}

fn command_stdout_as(
    uid: u32,
    gid: u32,
    cwd: &Path,
    program: &str,
    arguments: &[&str],
    home: &Path,
) -> Result<String, String> {
    let output = Command::new("setpriv")
        .arg(format!("--reuid={uid}"))
        .arg(format!("--regid={gid}"))
        .arg("--clear-groups")
        .arg("--")
        .arg(program)
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    ensure!(
        output.status.success(),
        "{program} exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(1000)
            .collect::<String>()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn wait_for_value(
    label: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Result<Option<Value>, String>,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("{label} did not become ready before its deadline"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_status(program: &str, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {:?} exited {}: {}",
            arguments,
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1000)
                .collect::<String>()
        ))
    }
}

fn run_status_allow_absent(program: &str, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    let error = String::from_utf8_lossy(&output.stderr);
    if output.status.success()
        || error.contains("not loaded")
        || error.contains("not found")
        || error.contains("No such")
    {
        Ok(())
    } else {
        Err(format!(
            "{program} {:?} exited {}: {}",
            arguments,
            output.status,
            error.chars().take(1000).collect::<String>()
        ))
    }
}

fn list_units(pattern: &str) -> Result<Vec<String>, String> {
    let output = Command::new("systemctl")
        .args(["list-units", "--all", "--plain", "--no-legend", pattern])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "systemctl list-units failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect())
}

fn docker_ids(key: &str, value: &str) -> Result<Vec<String>, String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--quiet",
            "--filter",
            &format!("label=devcoordinator2.{key}={value}"),
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "docker inventory failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn write_private_json(path: &Path, value: &Value) -> Result<(), String> {
    fs::write(
        path,
        serde_json::to_vec(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| error.to_string())
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let encoded = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "path contains NUL".to_owned())?;
    // SAFETY: encoded names the exact fixture path and uid/gid are validated
    // numeric identities selected by the root harness.
    if unsafe { libc::chown(encoded.as_ptr(), uid, gid) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn caller_identity() -> Result<(u32, u32), String> {
    if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
        let uid = uid
            .parse::<u32>()
            .map_err(|_| "SUDO_UID is invalid".to_owned())?;
        let gid = gid
            .parse::<u32>()
            .map_err(|_| "SUDO_GID is invalid".to_owned())?;
        if uid != 0 {
            return Ok((uid, gid));
        }
    }
    let name = CString::new("nobody").expect("static account");
    // SAFETY: getpwnam returns process-global account metadata which is copied
    // before this single-threaded harness proceeds.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return Err("cannot resolve the nobody account".to_owned());
    }
    // SAFETY: entry was checked for null and points to a valid passwd record.
    Ok(unsafe { ((*entry).pw_uid, (*entry).pw_gid) })
}

fn command_json(command: &[String]) -> Result<String, String> {
    serde_json::to_string(command).map_err(|error| error.to_string())
}

fn unit_config(command: &[String], timeout: Option<u64>) -> Result<String, String> {
    let mut output = String::from("schema = 2\n[test.unit]\n");
    if let Some(timeout) = timeout {
        output.push_str(&format!("timeout_seconds = {timeout}\n"));
    }
    output.push_str("[[test.unit.check]]\nname = \"main\"\ntier = \"release\"\ncommand = ");
    output.push_str(&command_json(command)?);
    output.push('\n');
    Ok(output)
}

fn unit_config_postgres(
    command: &[String],
    timeout: u64,
    image: &str,
    database: Option<&str>,
    user: Option<&str>,
) -> Result<String, String> {
    let mut output = unit_config(command, Some(timeout))?;
    output.push_str("[test.unit.postgres]\n");
    output.push_str(&format!(
        "image = {}\n",
        serde_json::to_string(image).unwrap()
    ));
    if let Some(database) = database {
        output.push_str(&format!(
            "database = {}\n",
            serde_json::to_string(database).unwrap()
        ));
    }
    if let Some(user) = user {
        output.push_str(&format!(
            "user = {}\n",
            serde_json::to_string(user).unwrap()
        ));
    }
    Ok(output)
}

fn check_toml(
    name: &str,
    command: &[String],
    after: &[&str],
    requires: &[&str],
    completion: &str,
    produces: &[&str],
) -> Result<String, String> {
    let mut output = String::from("[[test.complete.check]]\n");
    output.push_str(&format!(
        "name = {}\n",
        serde_json::to_string(name).unwrap()
    ));
    output.push_str("tier = \"release\"\ncommand = ");
    output.push_str(&command_json(command)?);
    output.push('\n');
    if !after.is_empty() {
        output.push_str(&format!(
            "after = {}\n",
            serde_json::to_string(after).unwrap()
        ));
    }
    if !requires.is_empty() {
        output.push_str(&format!(
            "requires = {}\n",
            serde_json::to_string(requires).unwrap()
        ));
    }
    if completion != "process" {
        output.push_str(&format!(
            "completion = {}\n",
            serde_json::to_string(completion).unwrap()
        ));
    }
    if !produces.is_empty() {
        output.push_str(&format!(
            "produces = {}\n",
            serde_json::to_string(produces).unwrap()
        ));
    }
    Ok(output)
}

fn make_fifo(path: &Path) -> Result<(), String> {
    let encoded = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "FIFO path contains NUL".to_owned())?;
    // SAFETY: path is an exact fixture target under the marker-owned root.
    if unsafe { libc::mkfifo(encoded.as_ptr(), 0o666) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn open_fifo_nonblocking(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| error.to_string())
}

fn wait_fifo_signals(files: &mut [File], timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut received = vec![false; files.len()];
    while Instant::now() < deadline {
        for (index, file) in files.iter_mut().enumerate() {
            if received[index] {
                continue;
            }
            let mut byte = [0u8; 1];
            match file.read(&mut byte) {
                Ok(1) if byte == *b"1" => received[index] = true,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        if received.iter().all(|value| *value) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("FIFO signals did not arrive: {received:?}"))
}

fn docker_inspect_json(identifier: &str, template: &str) -> Result<Value, String> {
    let output = Command::new("docker")
        .args(["inspect", "--format", template, identifier])
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(
        output.status.success(),
        "docker inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}

fn systemctl_property(unit: &str, property: &str) -> Result<String, String> {
    let output = Command::new("systemctl")
        .args(["show", unit, "-p", property])
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(output.status.success(), "systemctl show failed");
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn component<'a>(status: &'a Value, name: &str) -> Result<&'a Value, String> {
    status["components"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .ok_or_else(|| format!("deployment status omitted component {name}"))
}

fn routes(world: &World) -> Result<Value, String> {
    serde_json::from_slice(
        &fs::read(world.state.join("public/routes.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn http_get_json(port: u16) -> Result<Value, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let marker = b"\r\n\r\n";
    let offset = response
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or_else(|| "HTTP fixture response omitted headers".to_owned())?
        + marker.len();
    serde_json::from_slice(&response[offset..]).map_err(|error| error.to_string())
}

fn tcp_reachable(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().expect("static address"),
        Duration::from_secs(5),
    )
    .is_ok()
}

fn volume_exists(name: &str) -> Result<bool, String> {
    let status = Command::new("docker")
        .args(["volume", "inspect", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    Ok(status.success())
}

fn remove_volume(name: &str) -> Result<(), String> {
    run_status("docker", &["volume", "rm", "--force", name])
}

fn compose_service_id(project: &str, service: &str) -> Result<String, String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--quiet",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
            "--filter",
            &format!("label=com.docker.compose.service={service}"),
        ])
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(output.status.success(), "compose service inventory failed");
    let ids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(
        ids.len() == 1,
        "compose service {service} count was {}",
        ids.len()
    );
    Ok(ids[0].clone())
}

fn docker_exec(container: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(arguments)
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(
        output.status.success(),
        "docker exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn command_stdout(program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(1000)
            .collect::<String>()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn deployment_database_url(
    world: &World,
    deployment_id: &str,
    generation: u64,
) -> Result<String, String> {
    let directory = world
        .state
        .join("deployments")
        .join(deployment_id)
        .join("env");
    let suffix = format!("api-g{generation}.env");
    let path = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new(&suffix))
        })
        .ok_or_else(|| format!("deployment environment {suffix} is missing"))?;
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("DATABASE_URL="))
        .ok_or_else(|| "deployment environment omitted DATABASE_URL".to_owned())?
        .trim();
    Ok(value.trim_matches('"').to_owned())
}

fn fixture_command(world: &World, arguments: &[&str]) -> Vec<String> {
    std::iter::once(world.harness.fixture.to_string_lossy().into_owned())
        .chain(arguments.iter().map(|value| (*value).to_owned()))
        .collect()
}

fn web_deployment_config(
    world: &World,
    _version: &str,
    api_command: Option<Vec<String>>,
) -> Result<String, String> {
    let api =
        api_command.unwrap_or_else(|| fixture_command(world, &["http-server-file", "marker.txt"]));
    let worker = fixture_command(world, &["sleep", "3600"]);
    Ok(format!(
        r#"schema = 2
[deployment.web]
source = ["checkout", "worktree"]
domain = {{ checkout = "app", worktree = "app-dev" }}
components = ["db", "cache", "api", "worker"]

[deployment.web.component.db]
type = "postgres"
image = "postgres:16-alpine"
database = "app"
user = "app"

[deployment.web.component.cache]
type = "docker"
image = "valkey/valkey:9.1.0-alpine"
port = 6379

[deployment.web.component.api]
type = "process"
command = {}
port = true
route = true
health = {{ path = "/healthz", timeout_seconds = 30 }}
depends_on = ["db", "cache"]

[deployment.web.component.worker]
type = "process"
command = {}
depends_on = ["db"]
"#,
        command_json(&api)?,
        command_json(&worker)?,
    ))
}

fn setup_web(world: &World, version: &str, commit: bool) -> Result<(), String> {
    world.write_owned("marker.txt", format!("{version}\n"))?;
    world.write_config(&web_deployment_config(world, version, None)?)?;
    if commit {
        world.git(&["add", "."])?;
        world.git(&["commit", "-qm", &format!("app {version}")])?;
    }
    Ok(())
}

const COMPOSE_TOML: &str = r#"schema = 2
[deployment.stack]
source = "worktree"
domain = "stack"
components = ["compose"]

[deployment.stack.component.compose]
type = "compose"
files = ["compose.yml", "compose.route.yml"]
env_file = "compose.env"
services = ["bootstrap", "cache", "worker"]
finite_services = ["bootstrap"]
independent_services = ["worker"]
build = true
port = true
route = true
timeout_seconds = 90
"#;

const COMPOSE_YAML: &str = r#"services:
  bootstrap:
    image: postgres:16-alpine
    entrypoint: ["/bin/sh", "-c"]
    command: ["n=$$(cat /state/count 2>/dev/null || echo 0); expr $$n + 1 > /state/count"]
    restart: "no"
    volumes: ["state:/state"]
    labels: ["fixture=${FIXTURE_LABEL:?set fixture label}"]
  cache:
    image: valkey/valkey:9.1.0-alpine
    depends_on:
      bootstrap:
        condition: service_completed_successfully
    volumes: ["state:/state"]
  worker:
    image: postgres:16-alpine
    entrypoint: ["/bin/sh", "-c"]
    command: ["while :; do sleep 60; done"]
    depends_on:
      bootstrap:
        condition: service_completed_successfully
    volumes: ["state:/state"]
volumes:
  state:
"#;

const COMPOSE_ROUTE_YAML: &str = r#"services:
  cache:
    ports: ["127.0.0.1:${PORT:?Coordinator must lease PORT}:6379"]
"#;

const UNPUBLISHED_COMPOSE_ROUTE_YAML: &str = "services:\n  cache: {}\n";

const UPGRADE_COMPOSE_TOML: &str = r#"schema = 2
[deployment.upgrade-stack]
source = "worktree"
domain = "upgrade-stack"
components = ["compose"]

[deployment.upgrade-stack.component.compose]
type = "compose"
files = ["upgrade-compose.yml", "upgrade-route.yml"]
services = ["cache"]
port = true
route = true
timeout_seconds = 60
"#;

const UPGRADE_COMPOSE_YAML: &str = r#"services:
  cache:
    image: valkey/valkey:9.1.0-alpine
"#;

const UPGRADE_COMPOSE_ROUTE_YAML: &str = r#"services:
  cache:
    ports: ["127.0.0.1:${PORT:?Coordinator must lease PORT}:6379"]
"#;

const FAILED_COMPOSE_TOML: &str = r#"schema = 2
[deployment.failed-stack]
source = "worktree"
domain = "failed-stack"
components = ["compose"]

[deployment.failed-stack.component.compose]
type = "compose"
files = ["failed-compose.yml", "failed-compose.route.yml"]
services = ["worker"]
port = true
route = true
timeout_seconds = 30
"#;

const FAILED_COMPOSE_YAML: &str = r#"services:
  worker:
    image: postgres:16-alpine
    entrypoint: ["/bin/sh", "-c"]
    command: ["echo coordinator-candidate-failed >&2; exit 23"]
    restart: "no"
    volumes: ["state:/state"]
volumes:
  state:
"#;

const FAILED_COMPOSE_ROUTE_YAML: &str = r#"services:
  worker:
    ports: ["127.0.0.1:${PORT:?Coordinator must lease PORT}:5432"]
"#;

fn parse_compose_subnet(value: &str) -> Result<Ipv4Addr, String> {
    let address = value
        .strip_suffix("/24")
        .and_then(|value| value.parse::<Ipv4Addr>().ok())
        .filter(|address| address.is_private() && address.octets()[3] == 0)
        .ok_or_else(|| "Compose subnet must be a canonical private IPv4 /24 network".to_owned())?;
    Ok(address)
}

fn compose_fixture(content: &str, subnet: Option<Ipv4Addr>) -> String {
    match subnet {
        Some(address) => format!(
            "{content}\nnetworks:\n  default:\n    ipam:\n      config:\n        - subnet: {address}/24\n"
        ),
        None => content.to_owned(),
    }
}

fn setup_compose(world: &World, route: &str, commit_message: &str) -> Result<(), String> {
    world.write_config(COMPOSE_TOML)?;
    world.write_owned(
        "compose.yml",
        compose_fixture(COMPOSE_YAML, world.harness.compose_subnet),
    )?;
    world.write_owned("compose.route.yml", route)?;
    world.write_owned(".gitignore", "compose.env\n")?;
    world.write_owned("compose.env", "FIXTURE_LABEL=ready\n")?;
    world.write_owned("marker.txt", "v1\n")?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", commit_message])
}

fn response_text(value: &Value) -> String {
    ["segments", "matches", "contexts"]
        .iter()
        .filter_map(|key| value.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(|row| row.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn case_pass_uid_and_catalogued_output(world: &mut World) -> Result<(), String> {
    world.write_config(&unit_config(&["/usr/bin/id".to_owned()], Some(60))?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let started = data(&started)?;
    ensure!(started["status"] == "running", "test did not start running");
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(60))?;
    ensure!(final_status["status"] == "passed", "id check failed");
    ensure!(final_status["exit_code"] == 0, "id check exit was not zero");
    ensure!(
        final_status["caller_uid"] == world.harness.caller_uid,
        "governed child used the wrong uid"
    );
    let run_id = final_status["run_id"]
        .as_str()
        .ok_or_else(|| "status omitted run_id".to_owned())?;
    let catalogue = world.log_catalog(run_id, "main")?;
    ensure!(
        catalogue["entries"]
            .as_array()
            .is_some_and(|rows| rows.len() == 2),
        "log catalogue did not contain both streams"
    );
    let tail = world.log_tail(run_id, "main", "stdout")?;
    ensure!(
        response_text(&tail).contains(&format!("uid={}", world.harness.caller_uid)),
        "stdout did not prove the caller uid"
    );
    let summary = world.repo.join(".devcoordinator/test/current/summary.json");
    ensure!(
        fs::metadata(&summary)
            .map_err(|error| error.to_string())?
            .uid()
            == world.harness.caller_uid,
        "summary owner was not the caller"
    );
    ensure!(
        serde_json::from_slice::<Value>(&fs::read(summary).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?["status"]
            == "passed",
        "summary did not retain passed status"
    );
    Ok(())
}

fn case_broken_command_terminal_failure(world: &mut World) -> Result<(), String> {
    world.write_config(&unit_config(
        &["/definitely/missing/root-acceptance-command".to_owned()],
        None,
    )?)?;
    let response = world.call("test.start", json!({"path": world.repo}))?;
    ensure!(
        error_code(&response) == Some("test_start_failed"),
        "broken executable was not rejected synchronously: {}",
        bounded_json(&response)
    );
    ensure!(
        !response.to_string().to_ascii_lowercase().contains("queued"),
        "broken executable falsely reported queued"
    );
    Ok(())
}

fn case_timeout_kills_whole_cgroup(world: &mut World) -> Result<(), String> {
    let command = fixture_command(
        world,
        &[
            "print-write-sleep",
            "timeout-log-sentinel",
            "timeout-ready",
            "ready",
            "120",
        ],
    );
    world.write_config(&unit_config(&command, Some(2))?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let run_id = data(&started)?["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    let final_status = world.wait_status(&["timed-out"], Duration::from_secs(40))?;
    ensure!(
        final_status["exit_code"].is_null(),
        "timed-out run exposed an exit code"
    );
    world.wait_units_empty(Duration::from_secs(10))?;
    ensure!(
        response_text(&world.log_tail(&run_id, "main", "stdout")?).contains("timeout-log-sentinel"),
        "timeout log sentinel was lost"
    );
    Ok(())
}

fn case_cancel(world: &mut World) -> Result<(), String> {
    let command = fixture_command(
        world,
        &[
            "print-write-sleep",
            "cancel-log-sentinel",
            "cancel-log-ready",
            "ready",
            "120",
        ],
    );
    world.write_config(&unit_config(&command, None)?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let run_id = data(&started)?["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    world.wait_file("cancel-log-ready", Duration::from_secs(20))?;
    let stopped = world.call("test.stop", json!({"path": world.repo}))?;
    ensure!(
        data(&stopped)?["status"] == "cancelled",
        "stop did not cancel"
    );
    ensure!(world.units()?.is_empty(), "cancelled cgroup unit survived");
    ensure!(
        response_text(&world.log_tail(&run_id, "main", "stdout")?).contains("cancel-log-sentinel"),
        "cancel log sentinel was lost"
    );
    Ok(())
}

fn case_supersession_latest_start_wins(world: &mut World) -> Result<(), String> {
    let command = fixture_command(
        world,
        &[
            "print-write-sleep",
            "active-log-sentinel",
            "active-log-ready",
            "ready",
            "120",
        ],
    );
    world.write_config(&unit_config(&command, None)?)?;
    let first = world.call("test.start", json!({"path": world.repo}))?;
    let first_id = data(&first)?["run_id"]
        .as_str()
        .ok_or_else(|| "first start omitted run_id".to_owned())?
        .to_owned();
    world.wait_file("active-log-ready", Duration::from_secs(20))?;
    let second = world.call("test.start", json!({"path": world.repo}))?;
    let second_data = data(&second)?;
    let second_id = second_data["run_id"]
        .as_str()
        .ok_or_else(|| "second start omitted run_id".to_owned())?;
    ensure!(
        second_id != first_id,
        "supersession reused the prior run id"
    );
    let units = world.units()?;
    ensure!(units.len() == 1, "supersession left {} units", units.len());
    ensure!(
        units[0].contains(
            second_data["unit"]
                .as_str()
                .ok_or_else(|| "second start omitted unit".to_owned())?
        ),
        "latest unit was not the surviving unit"
    );
    let status = world.call("test.status", json!({"path": world.repo}))?;
    ensure!(
        data(&status)?["run_id"] == second_id,
        "status did not select latest run"
    );
    ensure!(
        response_text(&world.log_tail(&first_id, "main", "stdout")?)
            .contains("active-log-sentinel"),
        "superseded log was unavailable"
    );
    data(&world.call("test.stop", json!({"path": world.repo}))?)?;
    Ok(())
}

fn case_flooder_is_complete_hash_bound_and_keeps_final_sentinel(
    world: &mut World,
) -> Result<(), String> {
    let count = 5 * 1024 * 1024 + 73;
    let sentinel = "DEVCOORDINATOR-END-SENTINEL";
    let command = fixture_command(
        world,
        &["repeat-stdout-sentinel", &count.to_string(), sentinel],
    );
    world.write_config(&unit_config(&command, Some(60))?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let run_id = data(&started)?["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(60))?;
    ensure!(final_status["status"] == "passed", "flooder failed");
    let mut expected = vec![b'x'; count];
    expected.extend_from_slice(b"\nDEVCOORDINATOR-END-SENTINEL\n");
    ensure!(
        final_status["stdout_bytes_observed"] == expected.len(),
        "observed byte count drifted"
    );
    let catalogue = world.log_catalog(&run_id, "main")?;
    let entry = catalogue["entries"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.pointer("/log_ref/stream") == Some(&json!("stdout")))
        })
        .ok_or_else(|| "stdout catalogue entry is missing".to_owned())?;
    ensure!(entry["bytes"] == expected.len(), "catalogued bytes drifted");
    ensure!(entry["lines"] == 2, "catalogued lines drifted");
    ensure!(entry["complete"] == true, "flooded log was incomplete");
    ensure!(
        entry["sha256"] == sha256_hex(&expected),
        "flooded log hash drifted"
    );
    ensure!(
        response_text(&world.log_tail(&run_id, "main", "stdout")?).contains(sentinel),
        "final log sentinel was lost"
    );
    Ok(())
}

fn case_daemon_restart_marks_interrupted(world: &mut World) -> Result<(), String> {
    let command = fixture_command(
        world,
        &[
            "print-write-sleep",
            "restart-log-sentinel",
            "restart-log-ready",
            "ready",
            "120",
        ],
    );
    world.write_config(&unit_config(&command, None)?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let run_id = data(&started)?["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    world.wait_file("restart-log-ready", Duration::from_secs(20))?;
    let summary = world.repo.join(".devcoordinator/test/current/summary.json");
    world.stop_daemon(true)?;
    let running: Value =
        serde_json::from_slice(&fs::read(&summary).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    ensure!(
        running["status"] == "running",
        "pre-recovery summary was not running"
    );
    world.start_daemon(None, None, None)?;
    let recovered = world.wait_status(&["interrupted"], Duration::from_secs(30))?;
    ensure!(
        recovered["status"] == "interrupted",
        "restart did not interrupt run"
    );
    ensure!(
        world.units()?.is_empty(),
        "interrupted unit survived recovery"
    );
    ensure!(
        response_text(&world.log_tail(&run_id, "main", "stdout")?).contains("restart-log-sentinel"),
        "restart log sentinel was lost"
    );
    Ok(())
}

fn case_root_caller_rejected(world: &mut World) -> Result<(), String> {
    world.write_config(&unit_config(&["/usr/bin/id".to_owned()], None)?)?;
    let response = world.call_root("test.start", json!({"path": world.repo}))?;
    ensure!(
        error_code(&response) == Some("test_start_failed"),
        "root caller was not rejected: {}",
        bounded_json(&response)
    );
    ensure!(
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("root")),
        "root rejection did not name the caller boundary"
    );
    Ok(())
}

fn case_postgres_real_query_labels_secrecy_and_cleanup(world: &mut World) -> Result<(), String> {
    postgres_real_query_labels_secrecy_and_cleanup(world, "postgres:16-alpine")
}

fn case_postgres_18_data_directory_query_and_cleanup(world: &mut World) -> Result<(), String> {
    postgres_real_query_labels_secrecy_and_cleanup(world, "postgres:18-alpine")
}

fn postgres_real_query_labels_secrecy_and_cleanup(
    world: &mut World,
    image: &str,
) -> Result<(), String> {
    let command = vec![
        "/usr/bin/psql".to_owned(),
        "-v".to_owned(),
        "ON_ERROR_STOP=1".to_owned(),
        "-c".to_owned(),
        "create table t(x int); insert into t values (42); select x from t".to_owned(),
    ];
    world.write_config(&unit_config_postgres(
        &command,
        120,
        image,
        Some("app_test"),
        Some("app"),
    )?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let started = data(&started)?;
    let run_id = started["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    let unit = started["unit"]
        .as_str()
        .ok_or_else(|| "start omitted unit".to_owned())?
        .to_owned();
    let containers = docker_ids("run", &run_id)?;
    ensure!(
        containers.len() == 1,
        "test PostgreSQL container count was {}",
        containers.len()
    );
    let labels = docker_inspect_json(&containers[0], "{{json .Config.Labels}}")?;
    ensure!(
        labels["devcoordinator2.purpose"] == "test",
        "purpose label drifted"
    );
    ensure!(
        labels["devcoordinator2.caller_uid"]
            .as_str()
            .and_then(|value| value.parse::<u32>().ok())
            == Some(world.harness.caller_uid),
        "caller label drifted"
    );
    ensure!(
        labels["devcoordinator2.data"] == "disposable",
        "data label drifted"
    );
    ensure!(
        labels["devcoordinator2.instance"] == world.unit_prefix,
        "instance label drifted"
    );
    ensure!(
        !systemctl_property(&unit, "Environment")?.contains("PGPASSWORD"),
        "PostgreSQL password entered the public unit environment"
    );
    let env_file = world.repo.join(".devcoordinator/test/current/env");
    let metadata = fs::metadata(env_file).map_err(|error| error.to_string())?;
    ensure!(
        metadata.mode() & 0o777 == 0o600 && metadata.uid() == world.harness.caller_uid,
        "private executor environment ownership or mode drifted"
    );
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    let stdout = response_text(&world.log_tail(&run_id, "main", "stdout")?);
    let stderr = response_text(&world.log_tail(&run_id, "main", "stderr")?);
    ensure!(
        final_status["status"] == "passed",
        "real PostgreSQL query failed; stdout={}; stderr={}",
        stdout,
        stderr
    );
    ensure!(stdout.contains("42"), "real SQL result was missing");
    ensure!(
        !final_status.to_string().contains("PGPASSWORD"),
        "status disclosed the PostgreSQL password"
    );
    ensure!(
        docker_ids("run", &run_id)?.is_empty(),
        "test PostgreSQL container survived completion"
    );
    Ok(())
}

fn case_digest_pinned_postgis_fixture_is_pulled_injected_and_removed(
    world: &mut World,
) -> Result<(), String> {
    const IMAGE: &str =
        "postgis/postgis@sha256:993c1a5fed969dab3974deaa8a5dcd768151725490be0579ef421333dccd6341";
    let command = vec![
        "/usr/bin/psql".to_owned(),
        "-v".to_owned(),
        "ON_ERROR_STOP=1".to_owned(),
        "-c".to_owned(),
        "create extension if not exists postgis; select postgis_version()".to_owned(),
    ];
    world.write_config(&unit_config_postgres(
        &command,
        180,
        IMAGE,
        Some("app_test"),
        Some("app"),
    )?)?;
    let started = world.call("test.start", json!({"path": world.repo}))?;
    let run_id = data(&started)?["run_id"]
        .as_str()
        .ok_or_else(|| "start omitted run_id".to_owned())?
        .to_owned();
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(180))?;
    let stdout = response_text(&world.log_tail(&run_id, "main", "stdout")?);
    let stderr = response_text(&world.log_tail(&run_id, "main", "stderr")?);
    ensure!(
        final_status["status"] == "passed",
        "PostGIS query failed; stdout={}; stderr={}",
        stdout,
        stderr
    );
    ensure!(
        stdout.contains("postgis_version") && stdout.contains("USE_GEOS=1"),
        "PostGIS output did not prove the extension"
    );
    ensure!(
        docker_ids("run", &run_id)?.is_empty(),
        "PostGIS fixture survived completion"
    );
    Ok(())
}

fn case_postgres_removed_on_supersession_and_recovery(world: &mut World) -> Result<(), String> {
    let command = fixture_command(world, &["sleep", "120"]);
    world.write_config(&unit_config_postgres(
        &command,
        180,
        "postgres:16-alpine",
        None,
        None,
    )?)?;
    let first = world.call("test.start", json!({"path": world.repo}))?;
    let first_id = data(&first)?["run_id"]
        .as_str()
        .ok_or_else(|| "first start omitted run_id".to_owned())?
        .to_owned();
    ensure!(
        docker_ids("run", &first_id)?.len() == 1,
        "first PostgreSQL fixture did not start"
    );
    let second = world.call("test.start", json!({"path": world.repo}))?;
    let second_id = data(&second)?["run_id"]
        .as_str()
        .ok_or_else(|| "second start omitted run_id".to_owned())?
        .to_owned();
    ensure!(
        docker_ids("run", &first_id)?.is_empty(),
        "superseded PostgreSQL fixture survived"
    );
    ensure!(
        docker_ids("run", &second_id)?.len() == 1,
        "successor PostgreSQL fixture did not start"
    );
    world.stop_daemon(true)?;
    ensure!(
        docker_ids("run", &second_id)?.len() == 1,
        "hard daemon stop unexpectedly removed the orphan fixture"
    );
    world.start_daemon(None, None, None)?;
    ensure!(
        docker_ids("run", &second_id)?.is_empty(),
        "restart recovery did not remove the orphan fixture"
    );
    ensure!(
        docker_ids("instance", &world.unit_prefix)?.is_empty(),
        "restart recovery left an instance-owned test container"
    );
    Ok(())
}

fn case_governed_graph_runs_all_ready_checks_and_collects_safe_failures(
    world: &mut World,
) -> Result<(), String> {
    let mut started_paths = Vec::new();
    let mut release_paths = Vec::new();
    let mut started_files = Vec::new();
    let mut release_files = Vec::new();
    for name in ["one", "two"] {
        let started = world.base.join(format!("{name}-started"));
        let release = world.base.join(format!("{name}-release"));
        make_fifo(&started)?;
        make_fifo(&release)?;
        fs::set_permissions(&started, fs::Permissions::from_mode(0o666))
            .map_err(|error| error.to_string())?;
        fs::set_permissions(&release, fs::Permissions::from_mode(0o666))
            .map_err(|error| error.to_string())?;
        started_files.push(open_fifo_nonblocking(&started)?);
        release_files.push(open_fifo_nonblocking(&release)?);
        started_paths.push(started);
        release_paths.push(release);
    }
    let mut config = String::from("schema = 2\n[test.complete]\ntimeout_seconds = 120\n");
    for index in 0..2 {
        let command = fixture_command(
            world,
            &[
                "fifo-signal-wait",
                &started_paths[index].to_string_lossy(),
                &release_paths[index].to_string_lossy(),
            ],
        );
        config.push_str(&check_toml(
            if index == 0 { "one" } else { "two" },
            &command,
            &[],
            &[],
            "process",
            &[],
        )?);
    }
    config.push_str(&check_toml(
        "fails",
        &fixture_command(world, &["stderr-exit", "exact-check-failure", "7"]),
        &[],
        &[],
        "process",
        &[],
    )?);
    config.push_str(&check_toml(
        "after",
        &fixture_command(world, &["exit", "0"]),
        &["fails"],
        &[],
        "process",
        &[],
    )?);
    config.push_str(&check_toml(
        "needs",
        &fixture_command(world, &["exit", "99"]),
        &[],
        &["fails"],
        "process",
        &[],
    )?);
    world.write_config(&config)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    wait_fifo_signals(&mut started_files, Duration::from_secs(30))?;
    for file in &mut release_files {
        file.write_all(b"1").map_err(|error| error.to_string())?;
    }
    let final_status = world.wait_status(&["failed"], Duration::from_secs(120))?;
    let states = final_status["checks"]
        .as_array()
        .ok_or_else(|| "graph status omitted checks".to_owned())?
        .iter()
        .filter_map(|row| {
            Some((
                row["name"].as_str()?.to_owned(),
                row["status"].as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        states
            == BTreeMap::from([
                ("after".to_owned(), "passed".to_owned()),
                ("fails".to_owned(), "failed".to_owned()),
                ("needs".to_owned(), "not_meaningful".to_owned()),
                ("one".to_owned(), "passed".to_owned()),
                ("two".to_owned(), "passed".to_owned()),
            ]),
        "graph states drifted: {states:?}"
    );
    ensure!(
        final_status["proof"] == "complete",
        "graph proof was not complete"
    );
    let run_id = final_status["run_id"]
        .as_str()
        .ok_or_else(|| "graph status omitted run id".to_owned())?;
    ensure!(
        response_text(&world.log_tail(run_id, "fails", "stderr")?).contains("exact-check-failure"),
        "failed-check log was unavailable"
    );
    Ok(())
}

fn case_static_cases_have_isolated_catalogued_streams(world: &mut World) -> Result<(), String> {
    let mut config = String::from(
        "schema = 2\n[test.complete]\ntimeout_seconds = 120\n[[test.complete.check]]\nname = \"cases\"\ntier = \"release\"\ncases = [{id=\"one\",args=[\"one\"]},{id=\"two\",args=[\"two\"]}]\ncase_command = ",
    );
    config.push_str(&command_json(&fixture_command(world, &["case-streams"]))?);
    config.push('\n');
    world.write_config(&config)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    ensure!(final_status["status"] == "passed", "static cases failed");
    let cases = final_status
        .pointer("/checks/0/cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "static cases were omitted".to_owned())?;
    ensure!(
        cases
            .iter()
            .filter_map(|row| row["id"].as_str())
            .collect::<Vec<_>>()
            == ["one", "two"],
        "case order drifted"
    );
    let run_id = final_status["run_id"]
        .as_str()
        .ok_or_else(|| "run id missing".to_owned())?;
    for case in ["one", "two"] {
        for stream in ["stdout", "stderr"] {
            let response = world.call(
                "test.log.tail",
                json!({
                    "path": world.repo,
                    "run_id": run_id,
                    "check": "cases",
                    "phase": "case",
                    "case": case,
                    "stream": stream,
                    "lines": 50,
                    "max_bytes": 32768,
                }),
            )?;
            let text = response_text(data(&response)?);
            ensure!(
                text.contains(&format!("{stream}-{case}")),
                "{stream} for {case} was missing"
            );
            let other = if case == "one" { "two" } else { "one" };
            ensure!(
                !text.contains(&format!("{stream}-{other}")),
                "{stream} logs crossed case boundaries"
            );
        }
    }
    Ok(())
}

fn case_event_completed_setup_stays_alive_for_dependent_check(
    world: &mut World,
) -> Result<(), String> {
    let artifact = ".devcoordinator/test/current/artifacts/browser-ready";
    let mut config = String::from("schema = 2\n[test.complete]\ntimeout_seconds = 120\n");
    config.push_str(&check_toml(
        "service",
        &fixture_command(world, &["event", "exact"]),
        &[],
        &[],
        "event",
        &[],
    )?);
    config.push_str(&check_toml(
        "browser",
        &fixture_command(world, &["write-relative", artifact, "ready"]),
        &[],
        &["service"],
        "process",
        &[artifact],
    )?);
    world.write_config(&config)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let final_status = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    ensure!(
        final_status["status"] == "passed",
        "event-completed graph failed"
    );
    let checks = final_status["checks"]
        .as_array()
        .ok_or_else(|| "event graph omitted checks".to_owned())?;
    ensure!(
        checks
            .iter()
            .any(|row| row["name"] == "service" && row["status"] == "passed"),
        "event service was not passed"
    );
    ensure!(
        checks.iter().any(|row| {
            row["name"] == "browser" && row.pointer("/artifacts/0/path") == Some(&json!(artifact))
        }),
        "dependent artifact receipt was missing"
    );
    ensure!(
        world.units()?.is_empty(),
        "event service unit survived cleanup"
    );
    Ok(())
}

fn case_selection_and_failed_check_retry_remain_non_readiness_proof(
    world: &mut World,
) -> Result<(), String> {
    world.write_owned(".gitignore", ".devcoordinator/\nbuild.bin\n")?;
    let mut config = String::from("schema = 2\n[test.complete]\ntimeout_seconds = 120\n");
    config.push_str(&check_toml(
        "build",
        &fixture_command(world, &["write-relative", "build.bin", "exact-build"]),
        &[],
        &[],
        "process",
        &["build.bin"],
    )?);
    config.push_str(&check_toml(
        "verify",
        &fixture_command(world, &["exists-exit", "fix.flag", "9"]),
        &[],
        &["build"],
        "process",
        &[],
    )?);
    config.push_str(&check_toml(
        "unrelated",
        &fixture_command(world, &["write-scratch", "ran", "yes"]),
        &[],
        &[],
        "process",
        &[],
    )?);
    world.write_config(&config)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let failed = world.wait_status(&["failed"], Duration::from_secs(120))?;
    let origin = failed["run_id"]
        .as_str()
        .ok_or_else(|| "failed run omitted id".to_owned())?
        .to_owned();
    data(&world.call(
        "test.retry",
        json!({"path": world.repo, "run_id": origin, "check": "verify"}),
    )?)?;
    let retry = world.wait_status(&["failed"], Duration::from_secs(120))?;
    ensure!(retry["proof"] == "retry", "retry proof drifted");
    ensure!(retry["origin_run_id"] == origin, "retry origin drifted");
    let states = retry["checks"]
        .as_array()
        .ok_or_else(|| "retry omitted checks".to_owned())?
        .iter()
        .filter_map(|row| Some((row["name"].as_str()?, row["status"].as_str()?)))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        states == BTreeMap::from([("build", "reused"), ("verify", "failed")]),
        "retry states drifted: {states:?}"
    );
    world.write_owned("fix.flag", "fixed")?;
    let stale = world.call(
        "test.retry",
        json!({"path": world.repo, "run_id": origin, "check": "verify"}),
    )?;
    ensure!(
        error_code(&stale).is_some()
            && stale
                .pointer("/error/message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.to_ascii_lowercase().contains("stale")),
        "stale retry was not rejected"
    );
    data(&world.call(
        "test.start",
        json!({"path": world.repo, "checks": ["verify"]}),
    )?)?;
    let selected = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    ensure!(selected["status"] == "passed", "selected run failed");
    ensure!(selected["proof"] == "selected", "selected proof drifted");
    ensure!(
        selected["selection"] == json!(["verify"]),
        "selection drifted"
    );
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let complete = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    ensure!(complete["status"] == "passed", "complete run failed");
    ensure!(complete["proof"] == "complete", "complete proof drifted");
    Ok(())
}

fn case_upgrade_drain_rejects_new_starts_and_stop_reason_is_operational(
    world: &mut World,
) -> Result<(), String> {
    world.write_config(&unit_config(
        &fixture_command(world, &["sleep", "120"]),
        None,
    )?)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let lease = begin_drain(&world.base, "test upgrade").map_err(|error| error.to_string())?;
    let tested = (|| {
        let refused = world.call("test.start", json!({"path": world.repo}))?;
        ensure!(
            error_code(&refused) == Some("tests_draining"),
            "drain did not reject a new start"
        );
        let stopped = world.call(
            "test.stop",
            json!({
                "path": world.repo,
                "reason": "operator cancelled for emergency upgrade",
            }),
        )?;
        ensure!(
            data(&stopped)?["status"] == "cancelled",
            "drain stop failed"
        );
        let status = data(&world.call("test.status", json!({"path": world.repo}))?)?.clone();
        ensure!(
            status["termination_reason"] == "operator_cancelled",
            "operational stop reason drifted"
        );
        ensure!(
            !status
                .to_string()
                .contains("operator cancelled for emergency upgrade"),
            "private operator prose entered normal status"
        );
        ensure!(world.units()?.is_empty(), "drain-cancelled unit survived");
        Ok(())
    })();
    let reopened = end_drain(&lease).map_err(|error| error.to_string());
    tested.and(reopened)
}

fn case_repository_installer_drain_waits_then_restarts_and_reconnects(
    world: &mut World,
) -> Result<(), String> {
    let release = world.base.join("release-test");
    make_fifo(&release)?;
    fs::set_permissions(&release, fs::Permissions::from_mode(0o666))
        .map_err(|error| error.to_string())?;
    let mut release_handle = open_fifo_nonblocking(&release)?;
    world.write_config(&unit_config(
        &fixture_command(world, &["wait-fifo", &release.to_string_lossy()]),
        None,
    )?)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let lease =
        begin_drain(&world.base, "repository install").map_err(|error| error.to_string())?;
    let tested = (|| {
        let refused = world.call("test.start", json!({"path": world.repo}))?;
        ensure!(
            error_code(&refused) == Some("tests_draining"),
            "installer drain did not close admission"
        );
        data(&world.call("plan.overview", json!({"path": world.repo}))?)?;
        let mut observer = UnixStream::connect(&world.socket).map_err(|error| error.to_string())?;
        observer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut subscription = serde_json::to_vec(&request(
            "event.wait",
            json!({
                "filters": [{"filter_id": "upgrade-observer", "categories": ["health"]}]
            }),
            "other",
            None,
        ))
        .map_err(|error| error.to_string())?;
        subscription.push(b'\n');
        observer
            .write_all(&subscription)
            .map_err(|error| error.to_string())?;
        release_handle
            .write_all(b"1")
            .map_err(|error| error.to_string())?;
        let completed = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
        ensure!(completed["status"] == "passed", "active run did not drain");
        let fence = world.socket.with_file_name("daemon.pre-cutover.sock");
        fs::rename(&world.socket, &fence).map_err(|error| error.to_string())?;
        let mut response = Vec::new();
        observer
            .take(256 * 1024)
            .read_to_end(&mut response)
            .map_err(|error| error.to_string())?;
        let response: Value =
            serde_json::from_slice(&response).map_err(|error| error.to_string())?;
        ensure!(
            error_code(&response) == Some("daemon_unavailable"),
            "idle observer did not receive a reconnectable upgrade response"
        );
        ensure!(
            !world.socket.exists(),
            "intentional fence was incorrectly recovered"
        );
        world.stop_daemon(false)?;
        fs::remove_file(&fence).map_err(|error| error.to_string())?;
        world.start_daemon(None, None, None)?;
        let reconnected = world.call("test.status", json!({"path": world.repo}))?;
        ensure!(
            data(&reconnected)?["run_id"] == completed["run_id"],
            "restart did not reconnect to the drained result"
        );
        Ok(())
    })();
    let reopened = end_drain(&lease).map_err(|error| error.to_string());
    tested.and(reopened)?;
    world.write_config(&unit_config(&fixture_command(world, &["exit", "0"]), None)?)?;
    data(&world.call("test.start", json!({"path": world.repo}))?)?;
    let after = world.wait_status(&["passed", "failed"], Duration::from_secs(120))?;
    ensure!(after["status"] == "passed", "post-drain start failed");
    Ok(())
}

fn case_worktree_apply_stop_start_reapply_remove(world: &mut World) -> Result<(), String> {
    setup_web(world, "v1", true)?;
    let applied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    let status = data(&applied)?.clone();
    ensure!(
        status["state"] == "running" && status["current_generation"] == 1,
        "first worktree deployment did not reach generation 1"
    );
    ensure!(
        status["previous_generation"].is_null(),
        "worktree deployment retained a previous generation"
    );
    let deployment_id = status["deployment_id"]
        .as_str()
        .ok_or_else(|| "deployment id is missing".to_owned())?
        .to_owned();
    let volume = format!("devcoordinator2-{deployment_id}-db-pgdata");
    world.track_volume(&volume);
    let api = component(&status, "api")?;
    ensure!(
        api["state"] == "running" && api["health"] == "healthy",
        "API component was not healthy"
    );
    let api_port = api["port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "API port is missing".to_owned())?;
    let body = http_get_json(api_port)?;
    let cache_port = component(&status, "cache")?["port"]
        .as_u64()
        .ok_or_else(|| "cache port is missing".to_owned())?;
    ensure!(body["version"] == "v1", "API version drifted");
    ensure!(body["generation"] == "1", "API generation drifted");
    ensure!(
        body["has_db"] == true,
        "API did not receive its database URL"
    );
    ensure!(
        body["cache_port"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            == Some(cache_port),
        "API cache port drifted"
    );
    ensure!(
        body["port"]
            .as_str()
            .and_then(|value| value.parse::<u16>().ok())
            == Some(api_port),
        "API port env drifted"
    );
    let route_document = routes(world)?;
    let route = route_document["routes"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["deployment_id"] == deployment_id)
        })
        .ok_or_else(|| "route document omitted deployment".to_owned())?;
    ensure!(
        route["label"] == "app-dev" && route["port"] == api_port,
        "worktree route drifted"
    );
    ensure!(
        route_document["payload_sha256"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "route checksum is missing"
    );
    let database_url = deployment_database_url(world, &deployment_id, 1)?;
    command_stdout(
        "/usr/bin/psql",
        &[
            &database_url,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "create table keep(x int); insert into keep values (7)",
        ],
    )?;
    let database_id = component(&status, "db")?
        .pointer("/binding/identity")
        .and_then(Value::as_str)
        .ok_or_else(|| "database binding is missing".to_owned())?
        .to_owned();
    let labels = docker_inspect_json(&database_id, "{{json .Config.Labels}}")?;
    ensure!(
        labels["devcoordinator2.purpose"] == "permanent"
            && labels["devcoordinator2.data"] == "persistent"
            && labels["devcoordinator2.caller_uid"]
                .as_str()
                .and_then(|value| value.parse::<u32>().ok())
                == Some(world.harness.caller_uid),
        "permanent database labels drifted"
    );
    let stopped = world.call(
        "deployment.stop",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    ensure!(
        data(&stopped)?["state"] == "stopped",
        "deployment did not stop"
    );
    ensure!(
        world.deployment_units()?.is_empty(),
        "deployment units survived stop"
    );
    ensure!(routes(world)?["routes"] == json!([]), "route survived stop");
    ensure!(
        command_stdout(
            "docker",
            &["inspect", "--format", "{{.State.Status}}", &database_id]
        )? == "exited",
        "database container was not stopped"
    );
    let started = world.call(
        "deployment.start",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    let started = data(&started)?.clone();
    ensure!(started["state"] == "running", "deployment did not restart");
    ensure!(
        component(&started, "db")?["binding"]["identity"] == database_id,
        "persistent database container identity changed"
    );
    ensure!(
        command_stdout(
            "/usr/bin/psql",
            &[&database_url, "-tA", "-c", "select x from keep"]
        )? == "7",
        "persistent database row was lost"
    );
    let restarted = world.call(
        "deployment.restart",
        json!({
            "path": world.repo,
            "name": "web@worktree",
            "component": "worker",
        }),
    )?;
    ensure!(
        component(data(&restarted)?, "worker")?["state"] == "running",
        "component restart failed"
    );
    let old_port = component(&started, "api")?["port"]
        .as_u64()
        .ok_or_else(|| "old API port is missing".to_owned())?;
    let old_unit = component(&started, "api")?["binding"]["identity"]
        .as_str()
        .ok_or_else(|| "old API unit is missing".to_owned())?
        .to_owned();
    setup_web(world, "v2", false)?;
    let reapplied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    let reapplied = data(&reapplied)?.clone();
    ensure!(
        reapplied["current_generation"] == 2,
        "reapply did not create generation 2"
    );
    let new_port = component(&reapplied, "api")?["port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "new API port is missing".to_owned())?;
    ensure!(
        u64::from(new_port) != old_port && http_get_json(new_port)?["version"] == "v2",
        "new API generation was not independently reachable"
    );
    ensure!(
        !world
            .deployment_units()?
            .iter()
            .any(|unit| unit == &old_unit),
        "old API unit survived reapply"
    );
    ensure!(
        routes(world)?.pointer("/routes/0/port") == Some(&json!(new_port)),
        "route did not move to the new generation"
    );
    ensure!(
        component(&reapplied, "db")?["binding"]["identity"] == database_id,
        "stable database was replaced during reapply"
    );
    let logs = world.call(
        "deployment.logs",
        json!({
            "path": world.repo,
            "name": "web@worktree",
            "component": "db",
            "tail_lines": 20,
        }),
    )?;
    ensure!(
        data(&logs)?["tail"]
            .as_str()
            .is_some_and(|tail| tail.contains("database system is ready")),
        "database logs were not available"
    );
    let inventory = world.call("health.containers", json!({}))?;
    let inventory = data(&inventory)?;
    ensure!(
        inventory["containers"].as_array().is_some_and(|rows| rows
            .iter()
            .any(|row| row["id"] == database_id
                && row["classification"] == "managed-permanent"
                && row["component"] == "db")),
        "managed permanent container was not classified"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?)?;
    ensure!(
        volume_exists(&volume)?,
        "ordinary removal unexpectedly deleted persistent data"
    );
    ensure!(
        world.deployment_units()?.is_empty(),
        "deployment units survived removal"
    );
    remove_volume(&volume)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_checkout_generations_and_rollback(world: &mut World) -> Result<(), String> {
    setup_web(world, "v1", true)?;
    let first = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@checkout"}),
    )?;
    let first = data(&first)?.clone();
    let deployment_id = first["deployment_id"]
        .as_str()
        .ok_or_else(|| "checkout deployment id is missing".to_owned())?
        .to_owned();
    let volume = format!("devcoordinator2-{deployment_id}-db-pgdata");
    world.track_volume(&volume);
    ensure!(
        world
            .state
            .join("deployments")
            .join(&deployment_id)
            .join("gen-1/.devcoordinator.toml")
            .is_file(),
        "checkout generation 1 was not materialized"
    );
    let first_port = component(&first, "api")?["port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "first checkout API port is missing".to_owned())?;
    ensure!(
        http_get_json(first_port)?["version"] == "v1",
        "checkout generation 1 served the wrong version"
    );
    setup_web(world, "v2", true)?;
    let second = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@checkout"}),
    )?;
    let second = data(&second)?.clone();
    ensure!(
        second["current_generation"] == 2 && second["previous_generation"] == 1,
        "checkout generation history drifted"
    );
    let second_port = component(&second, "api")?["port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "second checkout API port is missing".to_owned())?;
    ensure!(
        http_get_json(second_port)?["version"] == "v2",
        "checkout generation 2 served the wrong version"
    );
    world.write_owned("marker.txt", "dirty\n")?;
    ensure!(
        http_get_json(second_port)?["version"] == "v2",
        "dirty worktree changed a checkout deployment"
    );
    let rolled = world.call(
        "deployment.rollback",
        json!({"path": world.repo, "name": "web@checkout"}),
    )?;
    let rolled = data(&rolled)?.clone();
    ensure!(
        rolled["rolled_back_from"] == 2 && rolled["current_generation"] == 3,
        "rollback generation metadata drifted"
    );
    let rollback_port = component(&rolled, "api")?["port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "rollback API port is missing".to_owned())?;
    ensure!(
        http_get_json(rollback_port)?["version"] == "v1",
        "rollback did not restore version 1"
    );
    let generation_count = fs::read_dir(world.state.join("deployments").join(&deployment_id))
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("gen-"))
        .count();
    ensure!(
        generation_count <= 2,
        "checkout retained {generation_count} generations"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "web@checkout",
            "delete_data": true,
        }),
    )?)?;
    ensure!(
        !volume_exists(&volume)?,
        "explicit checkout data deletion kept the volume"
    );
    world.forget_volume(&volume);
    Ok(())
}

fn case_failed_component_is_degraded_and_busy_is_immediate(
    world: &mut World,
) -> Result<(), String> {
    world.write_owned("marker.txt", "broken\n")?;
    let broken = web_deployment_config(world, "broken", Some(vec!["/usr/bin/false".to_owned()]))?
        .replace("timeout_seconds = 30", "timeout_seconds = 120");
    world.write_config(&broken)?;
    let started = Instant::now();
    let failed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed"),
        "broken process apply did not fail: {}",
        bounded_json(&failed)
    );
    ensure!(
        started.elapsed() < Duration::from_secs(30),
        "terminal process consumed the long readiness deadline"
    );
    ensure!(
        !failed.to_string().to_ascii_lowercase().contains("queued"),
        "failed apply falsely reported queued"
    );
    let status = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    let status = data(&status)?.clone();
    ensure!(
        status["state"] == "degraded" || status["state"] == "failed",
        "failed deployment state was not degraded or failed"
    );
    ensure!(
        status["route_port"].is_null(),
        "failed candidate received a route"
    );
    let deployment_id = status["deployment_id"]
        .as_str()
        .ok_or_else(|| "failed deployment id is missing".to_owned())?;
    let volume = format!("devcoordinator2-{deployment_id}-db-pgdata");
    world.track_volume(&volume);
    // A process that failed before its binding was retained can leave the
    // candidate name occupied. Reapply must recover that exact empty unit.
    let stale_unit = format!(
        "{}-deploy-{deployment_id}-api-g1.service",
        world.unit_prefix.replace("-test", "")
    );
    ensure!(
        systemctl_property(&stale_unit, "LoadState")?.contains("LoadState=not-found"),
        "failed candidate fixture name is unexpectedly occupied"
    );
    let seeded = Command::new("systemd-run")
        .args([
            "--quiet",
            "--wait",
            "--unit",
            &stale_unit,
            "--uid",
            &world.harness.caller_uid.to_string(),
            "/usr/bin/false",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(
        !seeded.status.success()
            && systemctl_property(&stale_unit, "ActiveState")?.contains("ActiveState=failed"),
        "fixture did not leave the failed transient name reserved"
    );
    world.write_config(&web_deployment_config(world, "v1", None)?)?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let barrier = std::sync::Arc::clone(&barrier);
        let executable = world.harness.executable.clone();
        let socket = world.socket.clone();
        let repo = world.repo.clone();
        let uid = world.harness.caller_uid;
        let gid = world.harness.caller_gid;
        threads.push(thread::spawn(move || {
            barrier.wait();
            call_as(
                &executable,
                uid,
                gid,
                &socket,
                request(
                    "deployment.apply",
                    json!({"path": repo, "name": "web@worktree"}),
                    "other",
                    None,
                ),
            )
        }));
    }
    barrier.wait();
    let responses = threads
        .into_iter()
        .map(|worker| {
            worker
                .join()
                .map_err(|_| "concurrent apply helper panicked".to_owned())?
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut codes = responses
        .iter()
        .map(|response| {
            if response["ok"] == true {
                "ok"
            } else {
                error_code(response).unwrap_or("missing")
            }
        })
        .collect::<Vec<_>>();
    codes.sort_unstable();
    ensure!(
        codes == ["busy", "ok"],
        "concurrent apply results were {codes:?}"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "web@worktree",
            "delete_data": true,
        }),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_readiness_allows_process_to_recover_within_restart_policy(
    world: &mut World,
) -> Result<(), String> {
    let api = fixture_command(world, &["http-server-restart", ".restart-count", "v1"]);
    world.write_owned("marker.txt", "restart\n")?;
    world.write_config(&web_deployment_config(world, "v1", Some(api))?)?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "restart fixture"])?;
    let applied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "web@worktree"}),
    )?;
    let applied = data(&applied)?.clone();
    let deployment_id = applied["deployment_id"]
        .as_str()
        .ok_or_else(|| "restart deployment id is missing".to_owned())?;
    let volume = format!("devcoordinator2-{deployment_id}-db-pgdata");
    world.track_volume(&volume);
    let api = component(&applied, "api")?;
    ensure!(
        api["state"] == "running"
            && api["restarts"]
                .as_u64()
                .is_some_and(|restarts| restarts >= 2),
        "readiness did not tolerate recoverable systemd restarts"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "web@worktree",
            "delete_data": true,
        }),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn setup_finite_only(world: &World, script: &str) -> Result<(), String> {
    world.write_config("schema=2\n[deployment.finite]\nsource='worktree'\ncomponents=['probe']\n[deployment.finite.component.probe]\ntype='compose'\nfiles=['finite-compose.yml']\nservices=['probe']\nfinite_services=['probe']\ntimeout_seconds=3300\n")?;
    let content = format!(
        "services:\n  probe:\n    image: postgres:16-alpine\n    network_mode: none\n    user: '1000:1000'\n    entrypoint: ['/bin/sh','-c']\n    command: {}\n    restart: 'no'\n",
        json!([script])
    );
    world.write_owned(
        "finite-compose.yml",
        compose_fixture(&content, world.harness.compose_subnet),
    )?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "finite fixture"])?;
    Ok(())
}

fn case_failed_finite_sibling_finishes_and_evidence_service_can_restart(
    world: &mut World,
) -> Result<(), String> {
    world.write_config("schema=2\n[deployment.siblings]\nsource='worktree'\ncomponents=['stack']\n[deployment.siblings.component.stack]\ntype='compose'\nfiles=['siblings.yml']\nservices=['failed','refined','artifacts']\nfinite_services=['failed','refined']\nindependent_services=['artifacts']\ntimeout_seconds=30\n")?;
    world.write_owned(
        "siblings.yml",
        compose_fixture(
            r#"services:
  failed:
    image: postgres:16-alpine
    entrypoint: ['/bin/sh', '-c']
    command: ['sleep 1; exit 7']
    restart: 'no'
  refined:
    image: postgres:16-alpine
    entrypoint: ['/bin/sh', '-c']
    command: ['sleep 3; echo refined-complete > /evidence/result; echo refined-complete']
    restart: 'no'
    volumes: ['evidence:/evidence']
  artifacts:
    image: postgres:16-alpine
    entrypoint: ['/bin/sh', '-c']
    command: ['while :; do cat /evidence/result 2>/dev/null; sleep 1; done']
    restart: 'no'
    volumes: ['evidence:/evidence']
volumes:
  evidence:
"#,
            world.harness.compose_subnet,
        ),
    )?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "independent finite sibling fixture"])?;
    let applied = world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"siblings"}),
    )?;
    ensure!(
        error_code(&applied) == Some("deployment_apply_failed"),
        "ordinary failure became success"
    );
    let status = world.call(
        "deployment.status",
        json!({"path":world.repo,"name":"siblings"}),
    )?;
    let status = data(&status)?;
    let project = component(status, "stack")?["binding"]["identity"]
        .as_str()
        .ok_or("Compose identity missing")?
        .to_owned();
    let volume = format!("{project}_evidence");
    world.track_volume(&volume);
    let refined = compose_service_id(&project, "refined")?;
    let finished = docker_inspect_json(&refined, "{{json .State}}")?;
    ensure!(
        finished["Status"] == "exited" && finished["ExitCode"] == 0,
        "safe sibling was interrupted before producing its result"
    );
    let restarted = world.call(
        "deployment.start",
        json!({"path":world.repo,"name":"siblings","component":"stack/artifacts"}),
    )?;
    data(&restarted)?;
    let evidence = compose_service_id(&project, "artifacts")?;
    ensure!(
        docker_inspect_json(&evidence, "{{json .State}}")?["Running"] == true,
        "selected evidence service did not start"
    );
    ensure!(
        compose_service_id(&project, "refined")? == refined
            && docker_inspect_json(&refined, "{{json .State}}")?["StartedAt"]
                == finished["StartedAt"],
        "retrieving evidence reran the calculation"
    );
    let logs = world.call(
        "deployment.logs",
        json!({"path":world.repo,"name":"siblings","component":"stack","tail_lines":20}),
    )?;
    ensure!(
        data(&logs)?["tail"]
            .as_str()
            .is_some_and(|text| text.contains("refined-complete")),
        "completed sibling evidence is unavailable"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path":world.repo,"name":"siblings","delete_data":true}),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_finite_only_container_completion_failure_cancellation_and_cleanup(
    world: &mut World,
) -> Result<(), String> {
    setup_finite_only(world, "test $$(id -u) -ne 0 && printf 'finite-success\\n'")?;
    let first = world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"finite"}),
    )?;
    let first = data(&first)?.clone();
    ensure!(
        first["state"] == "completed",
        "finite workload did not complete truthfully"
    );
    let project = component(&first, "probe")?["binding"]["identity"]
        .as_str()
        .ok_or("missing project")?
        .to_owned();
    let first_container = compose_service_id(&project, "probe")?;
    ensure!(
        component(&first, "probe")?.pointer("/completed_services/0/exit_code") == Some(&json!(0)),
        "finite exit receipt missing"
    );
    let again = world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"finite"}),
    )?;
    ensure!(
        data(&again)?["unchanged"] == true,
        "unchanged finite apply reran"
    );
    ensure!(
        compose_service_id(&project, "probe")? == first_container,
        "unchanged finite container replaced"
    );

    setup_finite_only(world, "printf 'finite-failure\\n'; exit 7")?;
    let failure = world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"finite"}),
    )?;
    ensure!(failure["ok"] == false, "nonzero finite exit accepted");
    let logs = world.call(
        "deployment.logs",
        json!({"path":world.repo,"name":"finite","component":"probe","tail_lines":20}),
    )?;
    ensure!(
        data(&logs)?["tail"]
            .as_str()
            .is_some_and(|text| text.contains("finite-failure")),
        "finite failure logs missing"
    );

    setup_finite_only(world, "printf 'finite-recovered\\n'")?;
    ensure!(
        data(&world.call(
            "deployment.apply",
            json!({"path":world.repo,"name":"finite"})
        )?)?["state"]
            == "completed",
        "finite recovery failed"
    );
    setup_finite_only(world, "printf 'finite-waiting\\n'; sleep 60")?;
    let harness = world.harness.clone();
    let socket = world.socket.clone();
    let apply_request = request(
        "deployment.apply",
        json!({"path":world.repo,"name":"finite"}),
        "other",
        None,
    );
    let applying = thread::spawn(move || {
        call_as(
            &harness.executable,
            harness.caller_uid,
            harness.caller_gid,
            &socket,
            apply_request,
        )
    });
    let waiting = wait_for_value("finite running", Duration::from_secs(15), || {
        let status = world.call(
            "deployment.status",
            json!({"path":world.repo,"name":"finite"}),
        )?;
        let state = data(&status)?;
        Ok(component(state, "probe")?["services"]
            .as_array()
            .is_some_and(|services| {
                services
                    .iter()
                    .any(|service| service["state"] == "starting")
            })
            .then(|| state.clone()))
    });
    let stopped = world.call(
        "deployment.stop",
        json!({"path":world.repo,"name":"finite"}),
    );
    let apply_result = applying
        .join()
        .map_err(|_| "finite apply worker panicked")??;
    waiting?;
    ensure!(
        data(&stopped?)?["state"] == "cancelled",
        "finite stop did not settle cancellation"
    );
    ensure!(
        apply_result["ok"] == false,
        "cancelled apply claimed success"
    );
    let logs = world.call(
        "deployment.logs",
        json!({"path":world.repo,"name":"finite","component":"probe","tail_lines":20}),
    )?;
    ensure!(
        data(&logs)?["tail"]
            .as_str()
            .is_some_and(|text| text.contains("finite-waiting")),
        "cancelled finite output missing"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path":world.repo,"name":"finite","delete_data":true}),
    )?)?;
    let ids = docker_ids("com.docker.compose.project", &project)?;
    ensure!(ids.is_empty(), "owned finite containers survived removal");
    Ok(())
}

fn case_native_compose_finite_service_receipt_and_start_semantics(
    world: &mut World,
) -> Result<(), String> {
    setup_compose(world, COMPOSE_ROUTE_YAML, "compose fixture")?;
    world.write_config(&COMPOSE_TOML.replace("timeout_seconds = 90", "timeout_seconds = 3300"))?;
    world.git(&["add", ".devcoordinator.toml"])?;
    world.git(&["commit", "-qm", "long finite setup deadline"])?;
    let first = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    let first = data(&first)?.clone();
    ensure!(
        first["state"] == "running",
        "Compose deployment did not run"
    );
    let compose = component(&first, "compose")?;
    let project = compose["binding"]["identity"]
        .as_str()
        .ok_or_else(|| "Compose binding is missing".to_owned())?
        .to_owned();
    let volume = format!("{project}_state");
    world.track_volume(&volume);
    let service_states = compose["services"]
        .as_array()
        .ok_or_else(|| "Compose service status is missing".to_owned())?
        .iter()
        .filter_map(|row| {
            Some((
                row["name"].as_str()?.to_owned(),
                row["state"].as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    ensure!(
        service_states
            == BTreeMap::from([
                ("bootstrap".to_owned(), "completed".to_owned()),
                ("cache".to_owned(), "running".to_owned()),
                ("worker".to_owned(), "running".to_owned()),
            ]),
        "Compose service states drifted: {service_states:?}"
    );
    ensure!(
        compose.pointer("/completed_services/0/service") == Some(&json!("bootstrap"))
            && compose.pointer("/completed_services/0/exit_code") == Some(&json!(0)),
        "finite-service receipt is missing"
    );
    let cache = compose_service_id(&project, "cache")?;
    ensure!(
        docker_exec(&cache, &["cat", "/state/count"])? == "1",
        "bootstrap did not run once"
    );
    let unchanged = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(
        data(&unchanged)?["unchanged"] == true,
        "unchanged Compose apply was not identified"
    );
    ensure!(
        docker_exec(&cache, &["cat", "/state/count"])? == "1",
        "unchanged apply reran bootstrap"
    );
    let worker_stopped = world.call(
        "deployment.stop",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "component": "compose/worker",
        }),
    )?;
    let worker_stopped = data(&worker_stopped)?.clone();
    ensure!(
        worker_stopped["state"] == "degraded",
        "independent worker stop did not degrade aggregate state"
    );
    let stopped_services = component(&worker_stopped, "compose")?["services"]
        .as_array()
        .ok_or_else(|| "stopped Compose services missing".to_owned())?;
    ensure!(
        stopped_services.iter().any(|row| {
            row["name"] == "worker" && row["state"] == "stopped" && row["independent"] == true
        }) && stopped_services
            .iter()
            .any(|row| row["name"] == "cache" && row["state"] == "running"),
        "independent Compose control changed the wrong services"
    );
    ensure!(
        worker_stopped["route_port"] == first["route_port"],
        "independent worker stop withdrew the healthy route"
    );
    let worker_started = world.call(
        "deployment.start",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "component": "compose/worker",
        }),
    )?;
    ensure!(
        data(&worker_started)?["state"] == "running",
        "independent worker did not restart"
    );
    data(&world.call(
        "deployment.stop",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "component": "compose/worker",
        }),
    )?)?;
    let full_started = world.call(
        "deployment.start",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(
        data(&full_started)?["state"] == "running",
        "full start did not restore desired services"
    );
    ensure!(
        docker_exec(&cache, &["cat", "/state/count"])? == "1",
        "full start reran bootstrap"
    );
    let stopped = world.call(
        "deployment.stop",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(data(&stopped)?["state"] == "stopped", "Compose stop failed");
    let started = world.call(
        "deployment.start",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(
        data(&started)?["state"] == "running",
        "Compose start failed"
    );
    ensure!(
        docker_exec(&cache, &["cat", "/state/count"])? == "1",
        "ordinary start reran bootstrap"
    );
    data(&world.call(
        "deployment.stop",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?)?;
    world.write_owned("marker.txt", "v2\n")?;
    let changed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    let changed = data(&changed)?.clone();
    let changed_cache = compose_service_id(&project, "cache")?;
    ensure!(
        docker_exec(&changed_cache, &["cat", "/state/count"])? == "2",
        "changed apply did not rerun bootstrap exactly once"
    );
    let route_port = changed["route_port"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "Compose route port is missing".to_owned())?;
    ensure!(
        docker_exec(&changed_cache, &["valkey-cli", "PING"])? == "PONG"
            && tcp_reachable(route_port),
        "Compose route was not reachable"
    );
    data(&world.call(
        "deployment.stop",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "component": "compose/worker",
        }),
    )?)?;
    world.write_owned(
        "compose.yml",
        compose_fixture(
            &COMPOSE_YAML.replace(
                "command: [\"while :; do sleep 60; done\"]",
                "command: [\"exit 29\"]",
            ),
            world.harness.compose_subnet,
        ),
    )?;
    world.write_owned("marker.txt", "v3\n")?;
    let failed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed"),
        "failed Compose candidate unexpectedly succeeded"
    );
    let after = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    let after = data(&after)?;
    let after_services = component(after, "compose")?["services"]
        .as_array()
        .ok_or_else(|| "post-failure service states missing".to_owned())?;
    ensure!(
        after_services
            .iter()
            .any(|row| row["name"] == "worker" && row["desired_state"] == "stopped")
            && after_services
                .iter()
                .any(|row| row["name"] == "cache" && row["desired_state"] == "running"),
        "failed candidate lost prior desired states"
    );
    ensure!(
        after["route_port"] == route_port && tcp_reachable(route_port),
        "failed candidate disrupted the prior route"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "delete_data": true,
        }),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_routed_compose_without_published_port_fails_and_never_routes(
    world: &mut World,
) -> Result<(), String> {
    setup_compose(
        world,
        UNPUBLISHED_COMPOSE_ROUTE_YAML,
        "unpublished compose fixture",
    )?;
    let failed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed")
            && failed
                .pointer("/error/message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("not published by the Compose project")),
        "unpublished Compose route did not fail safely: {}",
        bounded_json(&failed)
    );
    let status = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "stack@worktree"}),
    )?;
    let status = data(&status)?.clone();
    let project = component(&status, "compose")?["binding"]["identity"]
        .as_str()
        .ok_or_else(|| "failed Compose binding is missing".to_owned())?
        .to_owned();
    let volume = format!("{project}_state");
    world.track_volume(&volume);
    ensure!(
        component(&status, "compose")?["state"] == "failed" && status["route_port"].is_null(),
        "unpublished Compose candidate retained a usable route"
    );
    if world.state.join("public/routes.json").is_file() {
        ensure!(
            routes(world)?["routes"] == json!([]),
            "route document published an unreachable Compose port"
        );
    }
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "stack@worktree",
            "delete_data": true,
        }),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_unchanged_apply_rechecks_already_bad_published_compose_route(
    world: &mut World,
) -> Result<(), String> {
    world.write_config(UPGRADE_COMPOSE_TOML)?;
    world.write_owned(
        "upgrade-compose.yml",
        compose_fixture(UPGRADE_COMPOSE_YAML, world.harness.compose_subnet),
    )?;
    world.write_owned("upgrade-route.yml", UPGRADE_COMPOSE_ROUTE_YAML)?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "reachable compose fixture"])?;
    let first = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "upgrade-stack@worktree"}),
    )?;
    let first = data(&first)?.clone();
    let deployment_id = first["deployment_id"]
        .as_str()
        .ok_or_else(|| "upgrade deployment id is missing".to_owned())?
        .to_owned();
    let route_port = first["route_port"]
        .as_u64()
        .ok_or_else(|| "upgrade route port is missing".to_owned())?;
    let generation = first["current_generation"]
        .as_u64()
        .ok_or_else(|| "upgrade generation is missing".to_owned())?;
    let routes_path = world.state.join("public/routes.json");
    let stale_routes = fs::read(&routes_path).map_err(|error| error.to_string())?;
    world.write_owned("upgrade-route.yml", UNPUBLISHED_COMPOSE_ROUTE_YAML)?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "remove compose port publication"])?;
    let initial_failure = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "upgrade-stack@worktree"}),
    )?;
    ensure!(
        error_code(&initial_failure) == Some("deployment_apply_failed"),
        "initial unreachable route did not fail"
    );
    {
        let connection = rusqlite::Connection::open(world.state.join("authority.sqlite3"))
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "UPDATE deployments SET state='running' WHERE deployment_id=?1",
                rusqlite::params![deployment_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "UPDATE components SET state='running', health='unhealthy', last_error='seeded unreachable route' WHERE deployment_id=?1 AND name='compose'",
                rusqlite::params![deployment_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT OR REPLACE INTO domain_routes(domain, deployment_id, component, port, generation, published_at) VALUES(?1,?2,'compose',?3,?4,'seeded')",
                rusqlite::params![
                    "upgrade-stack",
                    deployment_id,
                    i64::try_from(route_port).map_err(|_| "route port overflow")?,
                    i64::try_from(generation).map_err(|_| "generation overflow")?
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    fs::write(&routes_path, stale_routes).map_err(|error| error.to_string())?;
    let before = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "upgrade-stack@worktree"}),
    )?;
    let before = data(&before)?;
    ensure!(
        before["state"] == "degraded" && before["route_port"] == route_port,
        "seeded stale route state was not observable"
    );
    ensure!(
        routes(world)?.pointer("/routes/0/port") == Some(&json!(route_port)),
        "seeded route document did not retain the old port"
    );
    let replay = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "upgrade-stack@worktree"}),
    )?;
    ensure!(
        error_code(&replay) == Some("deployment_apply_failed") && replay.get("data").is_none(),
        "unchanged replay bypassed health revalidation"
    );
    let after = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "upgrade-stack@worktree"}),
    )?;
    ensure!(
        data(&after)?["route_port"].is_null() && routes(world)?["routes"] == json!([]),
        "failed unchanged replay retained the stale route"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "upgrade-stack@worktree",
            "delete_data": true,
        }),
    )?)?;
    Ok(())
}

fn case_failed_first_compose_candidate_remains_managed_and_removable(
    world: &mut World,
) -> Result<(), String> {
    world.write_config(FAILED_COMPOSE_TOML)?;
    world.write_owned(
        "failed-compose.yml",
        compose_fixture(FAILED_COMPOSE_YAML, world.harness.compose_subnet),
    )?;
    world.write_owned("failed-compose.route.yml", FAILED_COMPOSE_ROUTE_YAML)?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "failed compose fixture"])?;
    let failed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "failed-stack@worktree"}),
    )?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed"),
        "failed first Compose candidate unexpectedly succeeded"
    );
    let status = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "failed-stack@worktree"}),
    )?;
    let status = data(&status)?.clone();
    let compose = component(&status, "compose")?;
    ensure!(
        compose["state"] == "failed" && compose["binding"]["kind"] == "compose",
        "failed candidate lost managed Compose identity"
    );
    let project = compose["binding"]["identity"]
        .as_str()
        .ok_or_else(|| "failed Compose project is missing".to_owned())?
        .to_owned();
    let volume = format!("{project}_state");
    world.track_volume(&volume);
    let logs = world.call(
        "deployment.logs",
        json!({
            "path": world.repo,
            "name": "failed-stack@worktree",
            "component": "compose",
            "tail_lines": 20,
        }),
    )?;
    ensure!(
        data(&logs)?["tail"]
            .as_str()
            .is_some_and(|tail| tail.contains("coordinator-candidate-failed")),
        "failed Compose logs were unavailable"
    );
    ensure!(
        volume_exists(&volume)?,
        "failed first candidate volume was not managed"
    );
    data(&world.call(
        "deployment.remove",
        json!({
            "path": world.repo,
            "name": "failed-stack@worktree",
            "delete_data": true,
        }),
    )?)?;
    ensure!(
        !volume_exists(&volume)?,
        "explicit removal kept the failed candidate volume"
    );
    world.forget_volume(&volume);
    Ok(())
}

fn case_long_failed_compose_build_retains_private_diagnostics(
    world: &mut World,
) -> Result<(), String> {
    world.write_config(
        r#"schema = 2
[deployment.build-failure]
source = "worktree"
components = ["worker"]
[deployment.build-failure.component.worker]
type = "compose"
files = ["compose.yml"]
services = ["worker"]
build = true
"#,
    )?;
    world.write_owned("compose.yml", "services:\n  worker:\n    build: .\n")?;
    world.write_owned("Dockerfile", "FROM postgres:16-alpine\nRUN echo first-build-diagnostic; line=0; while [ \"$line\" -lt 600 ]; do echo fixture-build-progress; line=$((line+1)); done; sleep 11; echo last-build-diagnostic >&2; exit 23\n")?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "long build failure fixture"])?;
    let failed = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "build-failure@worktree"}),
    )?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed"),
        "long failed build lost its deployment result"
    );
    let ordinary = failed.to_string();
    ensure!(
        !ordinary.contains("first-build-diagnostic") && !ordinary.contains("last-build-diagnostic"),
        "private build output escaped into ordinary metadata"
    );
    let logs = world.call("deployment.logs", json!({"path": world.repo, "name": "build-failure@worktree", "component": "build", "tail_lines": 1000}))?;
    let tail = data(&logs)?["tail"]
        .as_str()
        .ok_or("build log tail missing")?;
    ensure!(
        tail.lines()
            .any(|line| line.ends_with(" first-build-diagnostic"))
            && tail
                .lines()
                .any(|line| line.ends_with(" last-build-diagnostic")),
        "complete failed-build diagnostics were not retained"
    );
    let status = world.call(
        "deployment.status",
        json!({"path": world.repo, "name": "build-failure@worktree"}),
    )?;
    ensure!(
        data(&status)?["state"] != "running" && data(&status)?["readiness"]["ready"] == false,
        "failed build reported a ready deployment"
    );
    ensure!(
        component(data(&status)?, "worker")?["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("inspect deployment logs with component build")),
        "failed build lost its current diagnostic pointer"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path": world.repo, "name": "build-failure@worktree", "delete_data": true}),
    )?)?;
    Ok(())
}

fn case_edge_identity_trust_roles_and_revocation(world: &mut World) -> Result<(), String> {
    let nobody = CString::new("nobody").expect("static account");
    // SAFETY: the returned passwd record is checked and copied immediately.
    let entry = unsafe { libc::getpwnam(nobody.as_ptr()) };
    ensure!(!entry.is_null(), "cannot resolve edge fixture account");
    // SAFETY: entry was checked for null.
    let (edge_uid, edge_gid) = unsafe { ((*entry).pw_uid, (*entry).pw_gid) };
    world.stop_daemon(false)?;
    world.start_daemon(
        Some(edge_uid),
        Some("owner@example.test"),
        Some("example.test"),
    )?;
    let command = fixture_command(world, &["http-server", "access"]);
    let config = format!(
        r#"schema = 2
[deployment.svc]
components = ["api"]
domain = "svc"
[deployment.svc.component.api]
type = "process"
command = {}
port = true
route = true
health = {{ tcp = true, timeout_seconds = 30 }}
"#,
        command_json(&command)?
    );
    world.write_config(&config)?;
    let applied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "svc"}),
    )?;
    let deployment_id = data(&applied)?["deployment_id"]
        .as_str()
        .ok_or_else(|| "access deployment id is missing".to_owned())?
        .to_owned();
    let spoof = call_as(
        &world.harness.executable,
        world.harness.caller_uid,
        world.harness.caller_gid,
        &world.socket,
        request("user.whoami", json!({}), "edge", Some("owner@example.test")),
    )?;
    ensure!(
        error_code(&spoof) == Some("permission_denied"),
        "non-edge caller asserted a public identity"
    );
    let edge_call = |operation: &str, params: Value, identity: &str| {
        call_as(
            &world.harness.executable,
            edge_uid,
            edge_gid,
            &world.socket,
            request(operation, params, "edge", Some(identity)),
        )
    };
    let owner = edge_call("user.whoami", json!({}), "owner@example.test")?;
    ensure!(
        data(&owner)?["administrator"] == true,
        "bootstrap administrator was not recognized"
    );
    let denied = edge_call("deployment.list", json!({}), "dev@example.test")?;
    ensure!(
        error_code(&denied) == Some("permission_denied"),
        "unknown public identity was admitted"
    );
    data(&world.call(
        "user.invite",
        json!({
            "email": "dev@example.test",
            "grants": [{"deployment_id": deployment_id, "role": "viewer"}],
        }),
    )?)?;
    let accepted = edge_call(
        "user.accept_invitation",
        json!({"email": "dev@example.test", "subject": "s1"}),
        "dev@example.test",
    )?;
    ensure!(
        data(&accepted)?["accepted"] == true,
        "invitation was not accepted"
    );
    let listed = edge_call("deployment.list", json!({}), "dev@example.test")?;
    ensure!(
        data(&listed)?["deployments"]
            .as_array()
            .is_some_and(|rows| rows.len() == 1 && rows[0]["deployment_id"] == deployment_id),
        "viewer deployment scope drifted"
    );
    let mut last_viewed = Value::Null;
    let viewed_result = wait_for_value("viewer deployment status", Duration::from_secs(10), || {
        let viewed = edge_call(
            "deployment.status",
            json!({"deployment_id": deployment_id}),
            "dev@example.test",
        )?;
        let viewed = data(&viewed)?.clone();
        last_viewed = viewed.clone();
        Ok((viewed["state"] == "running").then_some(viewed))
    });
    viewed_result.map_err(|error| format!("{error}; last={}", bounded_json(&last_viewed)))?;
    let forbidden = edge_call(
        "deployment.stop",
        json!({"deployment_id": deployment_id}),
        "dev@example.test",
    )?;
    ensure!(
        error_code(&forbidden) == Some("permission_denied"),
        "viewer stopped a deployment"
    );
    let host = edge_call("health.summary", json!({}), "dev@example.test")?;
    ensure!(
        error_code(&host) == Some("permission_denied"),
        "viewer read host health"
    );
    let repositories = edge_call("health.repositories", json!({}), "dev@example.test")?;
    ensure!(
        data(&repositories)?.get("host").is_none(),
        "viewer repository health included host data"
    );
    let route_document = routes(world)?;
    ensure!(
        route_document.pointer("/access/owners") == Some(&json!(["owner@example.test"]))
            && route_document.pointer("/access/grants")
                == Some(&json!([{
                    "identity": "dev@example.test",
                    "deployment_id": deployment_id,
                    "role": "viewer",
                }]))
            && route_document.pointer("/routes/0/domain") == Some(&json!("svc.example.test"))
            && route_document.pointer("/routes/0/auth") == Some(&json!("authenticated")),
        "route access publication drifted"
    );
    data(&world.call(
        "grant.set",
        json!({
            "email": "dev@example.test",
            "deployment_id": deployment_id,
            "role": "operator",
        }),
    )?)?;
    let stopped = edge_call(
        "deployment.stop",
        json!({"deployment_id": deployment_id}),
        "dev@example.test",
    )?;
    ensure!(
        data(&stopped)?["state"] == "stopped",
        "operator could not stop deployment"
    );
    data(&world.call("user.remove", json!({"email": "dev@example.test"}))?)?;
    let revoked = edge_call(
        "deployment.status",
        json!({"deployment_id": deployment_id}),
        "dev@example.test",
    )?;
    ensure!(
        error_code(&revoked) == Some("permission_denied"),
        "revocation was not immediate"
    );
    ensure!(
        routes(world)?.pointer("/access/grants") == Some(&json!([])),
        "revoked grant remained in route publication"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path": world.repo, "name": "svc", "delete_data": true}),
    )?)?;
    Ok(())
}

fn case_health_views_measure_real_workloads(world: &mut World) -> Result<(), String> {
    let api = fixture_command(world, &["http-server", "health"]);
    let config = format!(
        r#"schema = 2
[deployment.svc]
components = ["db", "api"]
[deployment.svc.component.db]
type = "postgres"
image = "postgres:16-alpine"
[deployment.svc.component.api]
type = "process"
command = {}
port = true
health = {{ tcp = true, timeout_seconds = 30 }}
"#,
        command_json(&api)?
    );
    world.write_config(&config)?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "health fixture"])?;
    let applied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "svc"}),
    )?;
    let applied = data(&applied)?.clone();
    let deployment_id = applied["deployment_id"]
        .as_str()
        .ok_or_else(|| "health deployment id is missing".to_owned())?
        .to_owned();
    let repository_id = applied["repository_id"]
        .as_str()
        .ok_or_else(|| "health repository id is missing".to_owned())?
        .to_owned();
    let volume = format!("devcoordinator2-{deployment_id}-db-pgdata");
    world.track_volume(&volume);
    let database_id = component(&applied, "db")?["binding"]["identity"]
        .as_str()
        .ok_or_else(|| "health database binding is missing".to_owned())?
        .to_owned();
    let mut last_summary = Value::Null;
    let summary_result = wait_for_value(
        "managed health reconciliation",
        Duration::from_secs(60),
        || {
            let response = world.call("health.summary", json!({}))?;
            let summary = data(&response)?.clone();
            last_summary = summary.clone();
            Ok(summary
                .pointer("/host/reconciliation/managed_memory")
                .and_then(Value::as_u64)
                .is_some_and(|value| value > 0)
                .then_some(summary))
        },
    );
    let summary = summary_result.map_err(|error| {
        format!(
            "{error}; last health summary={}",
            bounded_json(&last_summary)
        )
    })?;
    ensure!(
        summary
            .pointer("/host/memory_total")
            .and_then(Value::as_u64)
            .is_some_and(|v| v > 0)
            && summary
                .pointer("/host/reconciliation/managed_memory")
                .and_then(Value::as_u64)
                .is_some_and(|v| v > 0),
        "host reconciliation did not measure managed workloads"
    );
    ensure!(
        summary.pointer("/sampling/retention_days") == Some(&json!(30)),
        "health retention drifted"
    );
    let repositories = world.call("health.repositories", json!({}))?;
    let repositories = data(&repositories)?;
    let mine = repositories["repositories"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["repository_id"] == repository_id)
        })
        .ok_or_else(|| "repository health row is missing".to_owned())?;
    ensure!(
        mine["memory_bytes"].as_u64().is_some_and(|value| value > 0)
            && mine["health"] == "healthy"
            && mine["deployments"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["deployment_id"] == deployment_id)),
        "repository health attribution drifted"
    );
    let detail = wait_for_value(
        "component health attribution",
        Duration::from_secs(60),
        || {
            let response = world.call("health.repository", json!({"path": world.repo}))?;
            let detail = data(&response)?.clone();
            let kinds = detail["components"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| Some((row["kind"].as_str()?, row["component"].as_str()?)))
                .collect::<Vec<_>>();
            Ok(
                (kinds.contains(&("container", "db")) && kinds.contains(&("component", "api")))
                    .then_some(detail),
            )
        },
    )?;
    ensure!(
        detail["components"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| {
                row["id"] == database_id
                    && row["memory_bytes"].as_u64().is_some_and(|value| value > 0)
                    && row["pids"].as_u64().is_some_and(|value| value >= 1)
            })),
        "database process metrics were not attributed"
    );
    let storage = wait_for_value(
        "repository storage attribution",
        Duration::from_secs(90),
        || {
            let response = world.call("health.repositories", json!({}))?;
            let value = data(&response)?.clone();
            let ready = value["repositories"].as_array().is_some_and(|rows| {
                rows.iter().any(|row| {
                    row["repository_id"] == repository_id
                        && row
                            .pointer("/storage/checkout")
                            .and_then(Value::as_u64)
                            .is_some_and(|value| value > 0)
                        && row
                            .pointer("/storage/postgres_data")
                            .and_then(Value::as_u64)
                            .is_some_and(|v| v > 0)
                })
            });
            Ok(ready.then_some(value))
        },
    )?;
    let mine = storage["repositories"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["repository_id"] == repository_id)
        })
        .ok_or_else(|| "storage repository row is missing".to_owned())?;
    ensure!(
        mine.pointer("/storage/checkout")
            .and_then(Value::as_u64)
            .is_some_and(|v| v > 0)
            && mine
                .pointer("/storage/postgres_data")
                .and_then(Value::as_u64)
                .is_some_and(|v| v > 0),
        "repository storage attribution drifted"
    );
    let host_storage = world.call("health.summary", json!({}))?;
    let host_storage = data(&host_storage)?;
    ensure!(
        host_storage
            .pointer("/storage/fs_used")
            .and_then(Value::as_u64)
            .zip(
                host_storage
                    .pointer("/storage/managed_repositories")
                    .and_then(Value::as_u64)
            )
            .is_some_and(|(used, managed)| used >= managed)
            && host_storage
                .pointer("/storage/docker_shared")
                .and_then(Value::as_u64)
                .is_some(),
        "host storage reconciliation drifted"
    );
    let mut last_postgres_storage = Value::Null;
    wait_for_value(
        "PostgreSQL operational storage facts",
        Duration::from_secs(90),
        || {
            let detail =
                data(&world.call("health.repository", json!({"path": world.repo}))?)?.clone();
            let postgres = detail["components"].as_array().and_then(|rows| {
                rows.iter()
                    .find(|row| row["id"] == format!("{deployment_id}/db"))
            });
            last_postgres_storage = postgres
                .and_then(|row| row.get("storage"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(["pg_connections", "pg_wal_bytes", "pg_database_bytes"]
                .iter()
                .all(|metric| {
                    last_postgres_storage
                        .get(metric)
                        .and_then(Value::as_u64)
                        .is_some_and(|value| value > 0)
                })
                .then(|| last_postgres_storage.clone()))
        },
    )
    .map_err(|error| {
        format!(
            "{error}; last numeric PostgreSQL storage={}",
            bounded_json(&last_postgres_storage)
        )
    })?;
    let history = wait_for_value("minute health history", Duration::from_secs(90), || {
        let response = world.call(
            "health.history",
            json!({
                "subject_kind": "repository",
                "subject_id": repository_id,
                "metric": "memory_bytes",
                "minutes": 10,
            }),
        )?;
        let value = data(&response)?.clone();
        Ok(value["points"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
            .then_some(value))
    })?;
    ensure!(
        history
            .pointer("/points/0/avg")
            .and_then(Value::as_f64)
            .is_some_and(|v| v > 0.0),
        "one-minute metric aggregate was not queryable"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path": world.repo, "name": "svc", "delete_data": true}),
    )?)?;
    world.forget_volume(&volume);
    Ok(())
}

fn case_plan_ledger_preview_flow(world: &mut World) -> Result<(), String> {
    let api = fixture_command(world, &["http-server", "plan"]);
    let config = format!(
        r#"schema = 2
[deployment.app]
source = ["worktree"]
domain = {{ worktree = "planflow" }}
components = ["api"]

[deployment.app.component.api]
type = "process"
command = {}
port = true
route = true
health = {{ path = "/", timeout_seconds = 30 }}
"#,
        command_json(&api)?
    );
    world.write_config(&config)?;
    world.git(&["add", "-A"])?;
    world.git(&["commit", "-qm", "app"])?;
    let commit = command_stdout_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &world.repo,
        "/usr/bin/git",
        &["rev-parse", "HEAD"],
        &world.base,
    )?;
    let first = world.call(
        "release.create",
        json!({"path": world.repo, "name": "First release", "kind": "release"}),
    )?;
    let first_id = data(&first)?["release_id"]
        .as_str()
        .ok_or_else(|| "first release id is missing".to_owned())?
        .to_owned();
    let second = world.call(
        "release.create",
        json!({"path": world.repo, "name": "Second release", "kind": "release"}),
    )?;
    let second_id = data(&second)?["release_id"]
        .as_str()
        .ok_or_else(|| "second release id is missing".to_owned())?
        .to_owned();
    let parent = world.call(
        "task.create",
        json!({
            "path": world.repo,
            "title": "People can see the app",
            "kind": "goal",
            "release_id": first_id,
            "outcome": "Opening the app in a browser shows a working page.",
        }),
    )?;
    let parent_id = data(&parent)?["task_id"]
        .as_str()
        .ok_or_else(|| "parent task id is missing".to_owned())?
        .to_owned();
    let child = world.call(
        "task.create",
        json!({
            "path": world.repo,
            "title": "A friendly start page",
            "kind": "goal",
            "parent_task_id": parent_id,
            "release_id": first_id,
            "estimated_loc": 120,
        }),
    )?;
    let child_id = data(&child)?["task_id"]
        .as_str()
        .ok_or_else(|| "child task id is missing".to_owned())?
        .to_owned();
    let stub = world.call(
        "task.create",
        json!({
            "path": world.repo,
            "title": "The help page is still empty",
            "kind": "stub",
            "parent_task_id": parent_id,
            "release_id": first_id,
            "estimated_loc": 80,
            "impact": "Readers find nothing behind the Help link.",
            "technical_note": "help.html renders a bare template",
        }),
    )?;
    let stub_id = data(&stub)?["task_id"]
        .as_str()
        .ok_or_else(|| "stub task id is missing".to_owned())?
        .to_owned();
    let moved = world.call(
        "task.update",
        json!({
            "task_id": stub_id,
            "release_id": second_id,
            "note": "Owner postponed the help page.",
        }),
    )?;
    ensure!(
        data(&moved)?["release_id"] == second_id,
        "task release move failed"
    );
    let requested = world.call(
        "release.request",
        json!({"path": world.repo, "note": "Show me the current state."}),
    )?;
    let requested_id = data(&requested)?["release_id"]
        .as_str()
        .ok_or_else(|| "requested release id is missing".to_owned())?
        .to_owned();
    ensure!(
        data(&requested)?["status"] == "requested",
        "preview request did not persist"
    );
    let overview = world.call("plan.overview", json!({"path": world.repo}))?;
    ensure!(
        data(&overview)?.pointer("/preview_requested/0/release_id") == Some(&json!(requested_id)),
        "preview request was not visible"
    );
    let touched = world.call(
        "task.update",
        json!({"task_id": child_id, "status": "in_progress"}),
    )?;
    ensure!(
        data(&touched)?["preview_requested"] == true,
        "next task write omitted preview notification"
    );
    world.write_owned("NOTES.txt", "work in progress\n")?;
    let applied = world.call(
        "deployment.apply",
        json!({"path": world.repo, "name": "app"}),
    )?;
    let applied = data(&applied)?.clone();
    let deployment_id = applied["deployment_id"]
        .as_str()
        .ok_or_else(|| "preview deployment id is missing".to_owned())?
        .to_owned();
    let delivered = world.call(
        "release.deliver",
        json!({
            "release_id": requested_id,
            "deployment_id": deployment_id,
            "note": "First look for the owner.",
        }),
    )?;
    let delivered = data(&delivered)?;
    ensure!(
        delivered["status"] == "delivered"
            && delivered["dirty"] == true
            && delivered["commit_hash"] == commit
            && delivered["url"] == "https://planflow"
            && delivered["port"].is_number(),
        "preview delivery evidence drifted"
    );
    data(&world.call(
        "decision.record",
        json!({
            "path": world.repo,
            "aspect": "ui",
            "title": "The start page speaks plainly",
            "body": "The first page greets the reader instead of showing settings, because the owner wants a friendly first impression.",
        }),
    )?)?;
    let recorded = world.call(
        "decision.record",
        json!({
            "path": world.repo,
            "aspect": "process",
            "title": "Previews come from live work",
            "body": "A requested preview deploys the current unfinished work so the owner can steer early.",
            "ref": "PLANFLOW-PREVIEWS",
        }),
    )?;
    ensure!(
        data(&recorded)?["unsummarized_count"] == 2,
        "decision count drifted"
    );
    data(&world.call(
        "decision.summarize",
        json!({
            "path": world.repo,
            "covers_through_seq": 1,
            "body": "The story so far: the app greets people plainly.",
        }),
    )?)?;
    let found = world.call(
        "decision.search",
        json!({"path": world.repo, "query": "PLANFLOW-PREVIEWS"}),
    )?;
    ensure!(
        data(&found)?["decisions"]
            .as_array()
            .is_some_and(|rows| rows.len() == 1 && rows[0]["seq"] == 2),
        "decision search drifted"
    );
    let history = world.call("task.history", json!({"task_id": stub_id}))?;
    let history = data(&history)?;
    ensure!(
        history["events"].as_array().is_some_and(|rows| rows
            .iter()
            .filter_map(|row| row["event"].as_str())
            .collect::<Vec<_>>()
            == ["created", "release_move"])
            && history.pointer("/events/1/note") == Some(&json!("Owner postponed the help page."))
            && history.pointer("/task/technical_note")
                == Some(&json!("help.html renders a bare template")),
        "task history lost the postponement story"
    );
    data(&world.call(
        "deployment.remove",
        json!({"path": world.repo, "name": "app", "delete_data": true}),
    )?)?;
    Ok(())
}

fn case_event_wait_replays_planning_and_groups_heartbeats(world: &mut World) -> Result<(), String> {
    world.write_config(&unit_config(&fixture_command(world, &["exit", "0"]), None)?)?;
    let started = world.call("test.start", json!({"path":world.repo}))?;
    let repository_id = data(&started)?["repository_id"]
        .as_str()
        .ok_or_else(|| "test start omitted repository_id".to_owned())?
        .to_owned();
    world.wait_status(&["passed"], Duration::from_secs(30))?;
    world.wait_units_empty(Duration::from_secs(10))?;
    let test_events = world.call(
        "event.wait",
        json!({
            "cursor":0,
            "filters":[{
                "filter_id":"tests",
                "categories":["test"],
                "repository_ids":[repository_id]
            }]
        }),
    )?;
    let test_events = data(&test_events)?;
    ensure!(
        test_events["events"].as_array().is_some_and(|events| {
            events.len() == 2
                && events[0].pointer("/event/event/data/kind") == Some(&json!("test.started"))
                && events[1].pointer("/event/event/data/kind") == Some(&json!("test.finished"))
        }),
        "native test lifecycle events were not delivered in cursor order"
    );
    ensure!(
        !test_events.to_string().contains("caller_uid")
            && !test_events.to_string().contains("client"),
        "test event disclosed caller attribution"
    );
    let created = world.call(
        "task.create",
        json!({
            "path":world.repo,
            "title":"Private root acceptance title",
            "kind":"improvement",
            "estimated_loc":10
        }),
    )?;
    let created = data(&created)?;
    let repository_id = created["repository_id"]
        .as_str()
        .ok_or_else(|| "task create omitted repository_id".to_owned())?;
    let task_id = created["task_id"]
        .as_str()
        .ok_or_else(|| "task create omitted task_id".to_owned())?;
    let event = world.call(
        "event.wait",
        json!({
            "cursor":0,
            "filters":[{
                "filter_id":"planning",
                "categories":["planning"],
                "kinds":["task.created"],
                "repository_ids":[repository_id]
            }]
        }),
    )?;
    let event = data(&event)?;
    ensure!(
        event["events"].as_array().is_some_and(|events| {
            events.len() == 1
                && events[0]["filter_ids"] == json!(["planning"])
                && events[0].pointer("/event/event/category") == Some(&json!("planning"))
                && events[0].pointer("/event/event/data/subject_id") == Some(&json!(task_id))
        }),
        "planning event was not replayed through event.wait"
    );
    ensure!(
        !event.to_string().contains("Private root acceptance title"),
        "planning event disclosed task text"
    );
    let heartbeat = world.call(
        "event.wait",
        json!({
            "cursor":event["cursor"],
            "filters":[
                {"filter_id":"health","categories":["health"],"deadline_at":"2000-01-01T00:00:00Z"},
                {"filter_id":"deployment","categories":["deployment"],"deadline_at":"2000-01-01T00:00:00Z"}
            ]
        }),
    )?;
    let heartbeat = data(&heartbeat)?;
    ensure!(
        heartbeat["heartbeat_due"].as_array().is_some_and(|due| due
            .iter()
            .filter_map(|row| row["filter_id"].as_str())
            .collect::<Vec<_>>()
            == ["health", "deployment"]),
        "due event filters were not returned together"
    );
    Ok(())
}

fn case_live_configuration_and_socket_recovery(world: &mut World) -> Result<(), String> {
    world.write_owned("compose.yml", "services: {}\n")?;
    world.write_owned(".gitignore", "live.env\n")?;
    world.write_owned("live.env", "FIXTURE=private-fixture-value\n")?;
    world.write_config("schema=2\n[deployment.web]\nsource=\"worktree\"\ncomponents=[\"stack\"]\n[deployment.web.component.stack]\ntype=\"compose\"\nfiles=[\"compose.yml\"]\nservices=[\"worker\"]\nenv_file=\"live.env\"\n")?;
    let daemon_id = world.daemon.as_ref().ok_or("daemon missing")?.id();
    let initial = world.call("config.get", json!({}))?;
    let initial = data(&initial)?.clone();
    let preflight = world.call(
        "deployment.preflight",
        json!({"path":world.repo,"name":"web"}),
    )?;
    ensure!(
        data(&preflight)?["blockers"][0]["code"] == "authorization_required",
        "preflight must explain missing authorization"
    );
    let applied = world.call("deployment.apply", json!({"path":world.repo,"name":"web"}))?;
    ensure!(
        error_code(&applied) == Some("authorization_required"),
        "apply must stop at preflight"
    );
    let updated = world.call("config.env.set", json!({"path":world.repo,"name":"web","file":"live.env","authorized":true,"expected_revision":initial["active_revision"]}))?;
    let updated = data(&updated)?.clone();
    ensure!(
        updated["authorization_count"] == 2,
        "live update lost the existing unrelated authorization"
    );
    let preflight = world.call(
        "deployment.preflight",
        json!({"path":world.repo,"name":"web"}),
    )?;
    ensure!(
        data(&preflight)?["ready"] == true,
        "active preflight did not see the new grant"
    );
    let stale = world.call("config.env.set", json!({"path":world.repo,"name":"web","file":"live.env","authorized":false,"expected_revision":initial["active_revision"]}))?;
    ensure!(
        error_code(&stale) == Some("configuration_conflict"),
        "stale update was not rejected"
    );
    let policy = world.policy.clone();
    let original = fs::read(&policy).map_err(|error| error.to_string())?;
    let metadata = fs::metadata(&policy).map_err(|error| error.to_string())?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o077 == 0,
        "policy ownership/mode changed"
    );
    fs::write(&policy, b"invalid-private-fixture-value").map_err(|error| error.to_string())?;
    let invalid = world.call(
        "config.reload",
        json!({"expected_revision":updated["active_revision"]}),
    )?;
    ensure!(
        error_code(&invalid) == Some("configuration_invalid"),
        "invalid reload was not rejected"
    );
    let retained = world.call("config.get", json!({}))?;
    ensure!(
        data(&retained)?["active_revision"] == updated["active_revision"],
        "invalid reload changed the active revision"
    );
    ensure!(
        !retained.to_string().contains("private-fixture-value"),
        "configuration result exposed fixture contents"
    );
    fs::write(&policy, original).map_err(|error| error.to_string())?;
    fs::remove_file(&world.socket).map_err(|error| error.to_string())?;
    wait_for_value("recovered endpoint", Duration::from_secs(5), || {
        Ok(world
            .call("ping", json!({}))
            .ok()
            .filter(|response| error_code(response).is_none()))
    })?;
    ensure!(
        world.daemon.as_ref().ok_or("daemon missing")?.id() == daemon_id,
        "recovery restarted the daemon"
    );
    let revoked = world.call("config.env.set", json!({"path":world.repo,"name":"web","file":"live.env","authorized":false,"expected_revision":updated["active_revision"]}))?;
    ensure!(
        data(&revoked)?["authorization_count"] == 1,
        "revocation changed unrelated authorization"
    );
    let preflight = world.call(
        "deployment.preflight",
        json!({"path":world.repo,"name":"web"}),
    )?;
    ensure!(
        data(&preflight)?["ready"] == false,
        "revocation was not active immediately"
    );
    ensure!(
        world.deployment_units()?.is_empty(),
        "preflight changed runtime resources"
    );
    Ok(())
}

fn case_live_configuration_in_service_sandbox(world: &mut World) -> Result<(), String> {
    world.stop_daemon(false)?;
    let protected = world.base.join("sandbox-etc");
    let directory = protected.join("devcoordinator2");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let policy = directory.join("compose-env-allowlist.json");
    fs::rename(&world.policy, &policy).map_err(|error| error.to_string())?;
    world.policy = policy;
    let runtime = world.base.join("run");
    fs::create_dir(&runtime).map_err(|error| error.to_string())?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))
        .map_err(|error| error.to_string())?;
    world.socket = runtime.join("daemon.sock");
    world.sandboxed = true;
    world.start_daemon(None, None, None)?;
    let unit = world.sandbox_unit_name();
    ensure!(
        systemctl_property(&unit, "ProtectSystem")?.trim() == "ProtectSystem=full",
        "service filesystem protection changed"
    );
    let pid = systemctl_property(&unit, "MainPID")?;
    let pid = pid
        .trim()
        .strip_prefix("MainPID=")
        .ok_or("sandbox daemon PID missing")?;
    ensure!(
        pid.parse::<u32>().is_ok_and(|pid| pid > 1),
        "sandbox daemon PID missing"
    );
    let guard = protected.join("unrelated-policy");
    fs::write(&guard, b"unchanged").map_err(|error| error.to_string())?;
    let mount = Command::new("/usr/bin/nsenter")
        .args([
            "--target",
            pid,
            "--mount",
            "--",
            "/usr/bin/findmnt",
            "--noheadings",
            "--output",
            "VFS-OPTIONS",
            "--target",
        ])
        .arg(&guard)
        .output()
        .map_err(|error| error.to_string())?;
    ensure!(
        mount.status.success(),
        "cannot inspect fixture's protected mount"
    );
    ensure!(
        String::from_utf8_lossy(&mount.stdout)
            .trim()
            .split(',')
            .any(|option| option == "ro"),
        "unrelated configuration became writable"
    );
    case_live_configuration_and_socket_recovery(world)?;
    ensure!(
        fs::read(&guard).map_err(|error| error.to_string())? == b"unchanged",
        "unrelated configuration changed"
    );
    Ok(())
}

fn cases() -> Vec<Case> {
    vec![
        (
            "live_configuration_in_service_sandbox",
            case_live_configuration_in_service_sandbox,
        ),
        (
            "live_configuration_and_socket_recovery",
            case_live_configuration_and_socket_recovery,
        ),
        (
            "pass_uid_and_catalogued_output",
            case_pass_uid_and_catalogued_output,
        ),
        (
            "broken_command_terminal_failure",
            case_broken_command_terminal_failure,
        ),
        (
            "timeout_kills_whole_cgroup",
            case_timeout_kills_whole_cgroup,
        ),
        ("cancel", case_cancel),
        (
            "supersession_latest_start_wins",
            case_supersession_latest_start_wins,
        ),
        (
            "flooder_is_complete_hash_bound_and_keeps_final_sentinel",
            case_flooder_is_complete_hash_bound_and_keeps_final_sentinel,
        ),
        (
            "daemon_restart_marks_interrupted",
            case_daemon_restart_marks_interrupted,
        ),
        ("root_caller_rejected", case_root_caller_rejected),
        (
            "postgres_real_query_labels_secrecy_and_cleanup",
            case_postgres_real_query_labels_secrecy_and_cleanup,
        ),
        (
            "postgres_18_data_directory_query_and_cleanup",
            case_postgres_18_data_directory_query_and_cleanup,
        ),
        (
            "digest_pinned_postgis_fixture_is_pulled_injected_and_removed",
            case_digest_pinned_postgis_fixture_is_pulled_injected_and_removed,
        ),
        (
            "postgres_removed_on_supersession_and_recovery",
            case_postgres_removed_on_supersession_and_recovery,
        ),
        (
            "governed_graph_runs_all_ready_checks_and_collects_safe_failures",
            case_governed_graph_runs_all_ready_checks_and_collects_safe_failures,
        ),
        (
            "static_cases_have_isolated_catalogued_streams",
            case_static_cases_have_isolated_catalogued_streams,
        ),
        (
            "event_completed_setup_stays_alive_for_dependent_check",
            case_event_completed_setup_stays_alive_for_dependent_check,
        ),
        (
            "selection_and_failed_check_retry_remain_non_readiness_proof",
            case_selection_and_failed_check_retry_remain_non_readiness_proof,
        ),
        (
            "upgrade_drain_rejects_new_starts_and_stop_reason_is_operational",
            case_upgrade_drain_rejects_new_starts_and_stop_reason_is_operational,
        ),
        (
            "repository_installer_drain_waits_then_restarts_and_reconnects",
            case_repository_installer_drain_waits_then_restarts_and_reconnects,
        ),
        (
            "worktree_apply_stop_start_reapply_remove",
            case_worktree_apply_stop_start_reapply_remove,
        ),
        (
            "checkout_generations_and_rollback",
            case_checkout_generations_and_rollback,
        ),
        (
            "failed_component_is_degraded_and_busy_is_immediate",
            case_failed_component_is_degraded_and_busy_is_immediate,
        ),
        (
            "readiness_allows_process_to_recover_within_restart_policy",
            case_readiness_allows_process_to_recover_within_restart_policy,
        ),
        (
            "finite_only_container_completion_failure_cancellation_and_cleanup",
            case_finite_only_container_completion_failure_cancellation_and_cleanup,
        ),
        (
            "failed_finite_sibling_finishes_and_evidence_service_can_restart",
            case_failed_finite_sibling_finishes_and_evidence_service_can_restart,
        ),
        (
            "native_compose_finite_service_receipt_and_start_semantics",
            case_native_compose_finite_service_receipt_and_start_semantics,
        ),
        (
            "routed_compose_without_published_port_fails_and_never_routes",
            case_routed_compose_without_published_port_fails_and_never_routes,
        ),
        (
            "unchanged_apply_rechecks_already_bad_published_compose_route",
            case_unchanged_apply_rechecks_already_bad_published_compose_route,
        ),
        (
            "failed_first_compose_candidate_remains_managed_and_removable",
            case_failed_first_compose_candidate_remains_managed_and_removable,
        ),
        (
            "edge_identity_trust_roles_and_revocation",
            case_edge_identity_trust_roles_and_revocation,
        ),
        (
            "long_failed_compose_build_retains_private_diagnostics",
            case_long_failed_compose_build_retains_private_diagnostics,
        ),
        (
            "health_views_measure_real_workloads",
            case_health_views_measure_real_workloads,
        ),
        ("plan_ledger_preview_flow", case_plan_ledger_preview_flow),
        (
            "event_wait_replays_planning_and_groups_heartbeats",
            case_event_wait_replays_planning_and_groups_heartbeats,
        ),
    ]
}

fn sha256_hex(payload: &[u8]) -> String {
    Sha256::digest(payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_run_inputs(
    daemon: &Path,
    fixture: &Path,
    work_root: &Path,
    report: &Path,
) -> Result<(), String> {
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "root acceptance requires effective uid 0"
    );
    ensure!(
        std::env::var("DEVCOORDINATOR2_ROOT_ACCEPTANCE").as_deref() == Ok("1"),
        "set DEVCOORDINATOR2_ROOT_ACCEPTANCE=1 for the isolated root suite"
    );
    for (path, label) in [(daemon, "daemon"), (fixture, "fixture")] {
        ensure!(
            path.is_absolute() && path.is_file(),
            format!("{label} must be an absolute regular file")
        );
    }
    ensure!(work_root.is_absolute(), "work root must be absolute");
    ensure!(report.is_absolute(), "report path must be absolute");
    if work_root.exists() {
        ensure!(work_root.is_dir(), "existing work root is not a directory");
        ensure!(
            fs::read_dir(work_root)
                .map_err(|error| error.to_string())?
                .next()
                .is_none(),
            "work root must be empty"
        );
    } else {
        fs::create_dir_all(work_root).map_err(|error| error.to_string())?;
    }
    fs::set_permissions(work_root, fs::Permissions::from_mode(0o711))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn run_suite(
    daemon: PathBuf,
    fixture: PathBuf,
    work_root: PathBuf,
    report: PathBuf,
    selected: Vec<String>,
    compose_subnet: Option<Ipv4Addr>,
) -> Result<AcceptanceReport, String> {
    validate_run_inputs(&daemon, &fixture, &work_root, &report)?;
    let (caller_uid, caller_gid) = caller_identity()?;
    let harness = Harness {
        daemon,
        fixture,
        executable: std::env::current_exe().map_err(|error| error.to_string())?,
        work_root,
        caller_uid,
        caller_gid,
        compose_subnet,
    };
    let selected = selected
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let available = cases();
    for name in &selected {
        ensure!(
            available.iter().any(|(candidate, _)| candidate == name),
            "unknown root-acceptance case: {name}"
        );
    }
    let mut results = Vec::new();
    for (name, test) in available {
        if !selected.is_empty() && !selected.contains(name) {
            continue;
        }
        let started = Instant::now();
        let mut world = match World::new(&harness, name) {
            Ok(world) => world,
            Err(error) => {
                results.push(CaseResult {
                    name: name.to_owned(),
                    status: "failed",
                    duration_ms: started.elapsed().as_millis(),
                    detail: Some(error),
                });
                continue;
            }
        };
        let tested = test(&mut world);
        let preserve = tested.is_err();
        let cleaned = world.cleanup(preserve);
        let outcome = match (tested, cleaned) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(cleanup)) => Err(format!("cleanup failed: {cleanup}")),
            (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup failed: {cleanup}")),
        };
        results.push(match outcome {
            Ok(()) => CaseResult {
                name: name.to_owned(),
                status: "passed",
                duration_ms: started.elapsed().as_millis(),
                detail: None,
            },
            Err(error) => CaseResult {
                name: name.to_owned(),
                status: "failed",
                duration_ms: started.elapsed().as_millis(),
                detail: Some(error.chars().take(2000).collect()),
            },
        });
    }
    let failed = results
        .iter()
        .filter(|result| result.status == "failed")
        .count();
    let acceptance = AcceptanceReport {
        schema: 1,
        suite: "devcoordinator2-rust-root-acceptance",
        status: if failed == 0 { "passed" } else { "failed" },
        cases: results.len(),
        passed: results.len() - failed,
        failed,
        results,
    };
    if let Some(parent) = report.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(
        &report,
        serde_json::to_vec_pretty(&acceptance).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(acceptance)
}

fn request_mode(socket: PathBuf) -> Result<(), String> {
    let mut payload = Vec::new();
    std::io::stdin()
        .take(64 * 1024 + 1)
        .read_to_end(&mut payload)
        .map_err(|error| error.to_string())?;
    ensure!(payload.len() <= 64 * 1024, "request exceeded 64 KiB");
    let request: Value =
        serde_json::from_slice(&payload).map_err(|error| format!("invalid request: {error}"))?;
    let response = request_over_socket(&socket, &request)?;
    println!("{}", serde_json::to_string(&response).unwrap());
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        RootCommand::Request { socket } => request_mode(socket).map(|()| None),
        RootCommand::Run {
            daemon,
            fixture,
            work_root,
            report,
            case,
            compose_subnet,
        } => run_suite(
            daemon,
            fixture,
            work_root,
            report.clone(),
            case,
            compose_subnet,
        )
        .map(|result| {
            println!(
                "{}",
                json!({
                    "schema": 1,
                    "status": result.status,
                    "cases": result.cases,
                    "passed": result.passed,
                    "failed": result.failed,
                    "report": report,
                })
            );
            Some(result.failed)
        }),
    };
    match outcome {
        Ok(Some(0) | None) => ExitCode::SUCCESS,
        Ok(Some(_)) => ExitCode::from(1),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_socket_alone_does_not_prove_daemon_readiness() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("not-ready.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(
            request_over_socket_with_timeout(
                &socket,
                &request("ping", json!({}), "other", None),
                Duration::from_millis(20),
            )
            .is_err()
        );
    }

    #[test]
    fn compose_subnet_is_explicit_private_and_network_aligned() {
        for value in ["10.231.73.0/24", "172.16.252.0/24", "192.168.241.0/24"] {
            let address = parse_compose_subnet(value).unwrap();
            assert_eq!(format!("{address}/24"), value);
        }
        for value in [
            "10.231.73.1/24",
            "172.16.0.0/16",
            "127.0.0.0/24",
            "169.254.1.0/24",
            "8.8.8.0/24",
            "::1/24",
            "10.231.73.0/24\nservices: {}",
            "10.231.73.0",
        ] {
            assert!(parse_compose_subnet(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn compose_subnet_changes_only_the_fixture_network() {
        assert_eq!(compose_fixture(COMPOSE_YAML, None), COMPOSE_YAML);
        let subnet = parse_compose_subnet("172.16.252.0/24").unwrap();
        for content in [COMPOSE_YAML, UPGRADE_COMPOSE_YAML, FAILED_COMPOSE_YAML] {
            let fixture = compose_fixture(content, Some(subnet));
            assert!(fixture.starts_with(content));
            assert!(fixture.ends_with("        - subnet: 172.16.252.0/24\n"));
            assert_eq!(fixture.matches("\nnetworks:").count(), 1);
        }
    }
}
