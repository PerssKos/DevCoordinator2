use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devcoordinator2_executor_core::{
    Cancellation, Executor, LocalPermitProvider, PermitProvider, UnixPermitProvider,
    log_query::{LogPruneRequest, LogQueryRequest, execute_log_query, prune_logs},
    protocol::{ArtifactReceipt, CompletionEvent, EventStatus, ExecutionPlan, MAX_REPORT_BYTES},
    receipts_match, source_digest,
};
use serde_json::json;

const SOURCE_COMMIT: &str = match option_env!("DEVCOORDINATOR2_SOURCE_COMMIT") {
    Some(value) => value,
    None => "development",
};

fn load_plan(path: &Path) -> Result<ExecutionPlan, String> {
    let input = read_bounded(path)?;
    if path.extension().and_then(|value| value.to_str()) == Some("toml") {
        let text = std::str::from_utf8(&input)
            .map_err(|error| format!("executor TOML is not UTF-8: {error}"))?;
        ExecutionPlan::from_toml(text).map_err(|error| error.to_string())
    } else {
        ExecutionPlan::from_json(&input).map_err(|error| error.to_string())
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("cannot inspect input: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_REPORT_BYTES as u64 {
        return Err("input must be a regular file no larger than 2 MiB".into());
    }
    fs::read(path).map_err(|error| format!("cannot read input: {error}"))
}

#[tokio::main]
async fn main() {
    let code = match dispatch(env::args_os().skip(1).collect()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("devcoordinator2-executor: {error}");
            2
        }
    };
    std::process::exit(code);
}

async fn dispatch(args: Vec<std::ffi::OsString>) -> Result<i32, String> {
    let Some(command) = args.first().and_then(|value| value.to_str()) else {
        return Err(usage());
    };
    match command {
        "--source-commit" if args.len() == 1 => {
            println!("{SOURCE_COMMIT}");
            Ok(0)
        }
        "validate" if args.len() == 2 => {
            let plan = load_plan(Path::new(&args[1]))?;
            println!(
                "{}",
                json!({
                    "schema": 2,
                    "valid": true,
                    "test": plan.test,
                    "declared_checks": plan.checks.len()
                })
            );
            Ok(0)
        }
        "run" if args.len() == 2 => run(Path::new(&args[1]), false).await,
        "run-local" if args.len() == 2 => run(Path::new(&args[1]), true).await,
        "fixture" if args.len() == 2 => {
            fixture_command(args[1].to_str().ok_or("invalid fixture node")?).await
        }
        "source-digest" => source_digest_command(&args[1..]),
        "receipts-match" => receipts_match_command(&args[1..]),
        "emit-event" => emit_event_command(&args[1..]),
        "log-query" => log_query_command(&args[1..]),
        "log-prune" => log_prune_command(&args[1..]),
        _ => Err(usage()),
    }
}

async fn fixture_command(node: &str) -> Result<i32, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let socket = env::var_os("DEVCOORDINATOR_CAPACITY_SOCKET")
        .ok_or("fixture requires its accepted run's broker")?;
    let run_id = env::var("DEVCOORDINATOR_RUN_ID")
        .map_err(|_| "fixture requires its accepted run identity")?;
    let check =
        env::var("DEVCOORDINATOR_CHECK_NAME").map_err(|_| "fixture requires its check identity")?;
    if check != node && !node.starts_with(&format!("{check}/")) {
        return Err("fixture node does not match its admitted check".into());
    }
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|_| "fixture broker is unavailable")?;
    let mut payload =
        serde_json::to_vec(&json!({"schema":1,"action":"fixture","run_id":run_id,"leaf_id":node}))
            .map_err(|e| e.to_string())?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .await
        .map_err(|_| "cannot submit fixture operation")?;
    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    BufReader::new(stream)
        .take(4097)
        .read_until(b'\n', &mut response)
        .await
        .map_err(|_| "fixture response unavailable")?;
    if response.len() > 4096 {
        return Err("fixture response exceeds its bound".into());
    }
    let response: serde_json::Value =
        serde_json::from_slice(&response).map_err(|_| "fixture response is invalid")?;
    if response["schema"] != 1 || response["status"] != "completed" {
        return Err("fixture operation was not authorized".into());
    }
    if let Some(scratch) = env::var_os("DEVCOORDINATOR_CHECK_SCRATCH") {
        use std::os::unix::fs::OpenOptionsExt;
        let path = PathBuf::from(scratch).join("native.log");
        if let Ok(mut log) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            && log.metadata().is_ok_and(|metadata| metadata.is_file())
        {
            std::io::copy(&mut log, &mut std::io::stderr())
                .map_err(|_| "cannot retain fixture diagnostics")?;
        }
    }
    println!(
        "{}",
        json!({"fixture":node,"status":if response["ok"]==true {"passed"}else{"failed"}})
    );
    Ok(if response["ok"] == true { 0 } else { 1 })
}

async fn run(path: &Path, local: bool) -> Result<i32, String> {
    let plan = load_plan(path)?;
    let report_path = PathBuf::from(&plan.current_dir).join("check-report.json");
    let permits: Arc<dyn PermitProvider> = if local {
        let logical_cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        Arc::new(
            LocalPermitProvider::new(logical_cpus.saturating_mul(2))
                .map_err(|error| error.to_string())?,
        )
    } else {
        let socket = env::var_os("DEVCOORDINATOR_CAPACITY_SOCKET").ok_or_else(|| {
            "DEVCOORDINATOR_CAPACITY_SOCKET is required for governed run; use run-local only for direct self-validation".to_owned()
        })?;
        Arc::new(UnixPermitProvider::new(PathBuf::from(socket)).map_err(|error| error.to_string())?)
    };
    let cancellation = Cancellation::default();
    let signal_cancellation = cancellation.clone();
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|error| format!("cannot register executor termination handler: {error}"))?;
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
        signal_cancellation.cancel();
    });
    let report = Executor::new(plan, permits, cancellation)
        .map_err(|error| error.to_string())?
        .run()
        .await
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        json!({
            "schema": 2,
            "status": report.status,
            "report": report_path
        })
    );
    Ok(
        if report.status == devcoordinator2_executor_core::protocol::RunStatus::Passed {
            0
        } else {
            1
        },
    )
}

fn source_digest_command(args: &[std::ffi::OsString]) -> Result<i32, String> {
    let worktree = exact_flag(args, "--worktree")?;
    let root = absolute_directory(&worktree)?;
    let sha256 = source_digest(&root).map_err(|error| error.to_string())?;
    println!("{}", json!({"schema": 2, "sha256": sha256}));
    Ok(0)
}

fn receipts_match_command(args: &[std::ffi::OsString]) -> Result<i32, String> {
    if args.len() != 4 || args[0] != "--worktree" || args[2] != "--receipts" {
        return Err(usage());
    }
    let root = absolute_directory(Path::new(&args[1]))?;
    let payload = if args[3] == "-" {
        let mut payload = Vec::new();
        std::io::stdin()
            .take((MAX_REPORT_BYTES + 1) as u64)
            .read_to_end(&mut payload)
            .map_err(|error| format!("cannot read receipts from stdin: {error}"))?;
        if payload.len() > MAX_REPORT_BYTES {
            return Err("receipts input exceeds 2 MiB".into());
        }
        payload
    } else {
        read_bounded(Path::new(&args[3]))?
    };
    let receipts: Vec<ArtifactReceipt> = serde_json::from_slice(&payload)
        .map_err(|error| format!("invalid receipts JSON: {error}"))?;
    let matches = receipts_match(&root, &receipts).map_err(|error| error.to_string())?;
    println!("{}", json!({"schema": 2, "matches": matches}));
    Ok(if matches { 0 } else { 1 })
}

fn emit_event_command(args: &[std::ffi::OsString]) -> Result<i32, String> {
    if args.is_empty() || args.len() > 2 {
        return Err(usage());
    }
    let status = match args[0].to_str() {
        Some("passed") => EventStatus::Passed,
        Some("failed") => EventStatus::Failed,
        Some("unsafe") => EventStatus::Unsafe,
        _ => return Err("event status must be passed, failed, or unsafe".into()),
    };
    let reason = args
        .get(1)
        .map(|value| value.to_string_lossy().into_owned());
    if reason.as_ref().is_some_and(|value| value.len() > 512) {
        return Err("event reason exceeds 512 bytes".into());
    }
    let run_id = env::var("DEVCOORDINATOR_RUN_ID")
        .map_err(|_| "DEVCOORDINATOR_RUN_ID is unavailable".to_owned())?;
    let check = env::var("DEVCOORDINATOR_CHECK_NAME")
        .map_err(|_| "DEVCOORDINATOR_CHECK_NAME is unavailable".to_owned())?;
    let fd: i32 = env::var("DEVCOORDINATOR_EVENT_FD")
        .map_err(|_| "DEVCOORDINATOR_EVENT_FD is unavailable".to_owned())?
        .parse()
        .map_err(|_| "DEVCOORDINATOR_EVENT_FD is invalid".to_owned())?;
    if fd < 3 {
        return Err("DEVCOORDINATOR_EVENT_FD is invalid".into());
    }
    let event = CompletionEvent {
        schema: Default::default(),
        run_id,
        check,
        status,
        reason,
    };
    let mut payload = serde_json::to_vec(&event)
        .map_err(|error| format!("cannot encode completion event: {error}"))?;
    payload.push(b'\n');
    // SAFETY: the descriptor is injected by the executor for this one event.
    let mut output = unsafe { fs::File::from_raw_fd(fd) };
    output
        .write_all(&payload)
        .map_err(|error| format!("cannot emit completion event: {error}"))?;
    Ok(0)
}

fn log_query_command(args: &[std::ffi::OsString]) -> Result<i32, String> {
    let (worktree, payload) = bridge_request(args)?;
    let request: Result<LogQueryRequest, _> = serde_json::from_slice(&payload);
    match request {
        Ok(request) => match execute_log_query(&worktree, request) {
            Ok(result) => {
                println!("{}", json!({"schema": 2, "ok": true, "result": result}));
                Ok(0)
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"schema": 2, "ok": false, "error": {"code": error.code()}})
                );
                Ok(1)
            }
        },
        Err(_) => {
            println!(
                "{}",
                json!({"schema": 2, "ok": false, "error": {"code": "args_invalid"}})
            );
            Ok(1)
        }
    }
}

fn log_prune_command(args: &[std::ffi::OsString]) -> Result<i32, String> {
    let (worktree, payload) = bridge_request(args)?;
    let request: Result<LogPruneRequest, _> = serde_json::from_slice(&payload);
    match request {
        Ok(request) => match prune_logs(&worktree, request) {
            Ok(result) => {
                println!("{}", json!({"schema": 2, "ok": true, "result": result}));
                Ok(0)
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"schema": 2, "ok": false, "error": {"code": error.code()}})
                );
                Ok(1)
            }
        },
        Err(_) => {
            println!(
                "{}",
                json!({"schema": 2, "ok": false, "error": {"code": "args_invalid"}})
            );
            Ok(1)
        }
    }
}

fn bridge_request(args: &[std::ffi::OsString]) -> Result<(PathBuf, Vec<u8>), String> {
    if args.len() != 4 || args[0] != "--worktree" || args[2] != "--request" || args[3] != "-" {
        return Err(usage());
    }
    let worktree = absolute_directory(Path::new(&args[1]))?;
    let mut payload = Vec::new();
    std::io::stdin()
        .take(65_537)
        .read_to_end(&mut payload)
        .map_err(|error| format!("cannot read log request: {error}"))?;
    if payload.len() > 65_536 {
        return Err("log request exceeds 64 KiB".into());
    }
    Ok((worktree, payload))
}

fn exact_flag(args: &[std::ffi::OsString], flag: &str) -> Result<PathBuf, String> {
    if args.len() != 2 || args[0] != flag {
        return Err(usage());
    }
    Ok(PathBuf::from(&args[1]))
}

fn absolute_directory(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("worktree path must be absolute".into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve worktree: {error}"))?;
    if !canonical.is_dir() {
        return Err("worktree path is not a directory".into());
    }
    Ok(canonical)
}

fn usage() -> String {
    [
        "usage:",
        "  devcoordinator2-executor run PLAN.json",
        "  devcoordinator2-executor run-local PLAN.json",
        "  devcoordinator2-executor validate PLAN.json",
        "  devcoordinator2-executor source-digest --worktree PATH",
        "  devcoordinator2-executor receipts-match --worktree PATH --receipts FILE_OR_-",
        "  devcoordinator2-executor emit-event passed|failed|unsafe [REASON]",
        "  devcoordinator2-executor log-query --worktree PATH --request -",
        "  devcoordinator2-executor log-prune --worktree PATH --request -",
    ]
    .join("\n")
}
