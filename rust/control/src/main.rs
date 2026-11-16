use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use devcoordinator2_api::results::BugList;
use devcoordinator2_api::{ErrorCode, ProtocolError, ResponseEnvelope};
use devcoordinator2_control::artifact_materialize::{self, MaterializeRequest};
use devcoordinator2_control::bugs;
use devcoordinator2_control::check_event;
use devcoordinator2_control::cli::{Cli, Invocation, OfflineBugAction, OutputFormat};
use devcoordinator2_control::telegram::TelegramEvent;
use devcoordinator2_control::{
    client, config::Config, control_plane::ControlPlane, daemon, database::Database, mcp,
};
use tokio::sync::watch;
use tokio::task::JoinSet;

type ServiceResult = (&'static str, Result<(), String>);
type JoinedService = Result<ServiceResult, tokio::task::JoinError>;

#[tokio::main]
async fn main() -> ExitCode {
    if source_commit_requested() {
        println!("{}", devcoordinator2_control::SOURCE_COMMIT);
        return ExitCode::SUCCESS;
    }
    if std::env::args_os().skip(1).collect::<Vec<_>>()
        == [std::ffi::OsString::from("--build-identity")]
    {
        println!(
            "{}",
            serde_json::json!({"schema":1,"source_commit":devcoordinator2_control::SOURCE_COMMIT,"protocol_version":devcoordinator2_api::PROTOCOL_VERSION,"root_acceptance":cfg!(feature="root-acceptance")})
        );
        return ExitCode::SUCCESS;
    }
    if let Some(path) = repository_config_validation_requested() {
        return validate_repository_config(&path);
    }
    if std::env::args().nth(1).as_deref() == Some("storage-maintenance") {
        #[derive(clap::Parser)]
        struct Maintenance {
            #[arg(long)]
            job_id: String,
            #[arg(long)]
            artifact_id: String,
        }
        let Ok(arguments) = Maintenance::try_parse_from(std::env::args_os().skip(1)) else {
            return local_error(
                OutputFormat::Json,
                ErrorCode::ParamsInvalid,
                "invalid maintenance arguments",
                "",
                2,
            );
        };
        return match Config::load() {
            Ok(config) => match devcoordinator2_control::storage::run_mount_maintenance(
                &config,
                &arguments.job_id,
                &arguments.artifact_id,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => local_error(OutputFormat::Json, error.code, &error.message, "", 2),
            },
            Err(_) => local_error(
                OutputFormat::Json,
                ErrorCode::ConfigurationInvalid,
                "maintenance configuration is unavailable",
                "",
                2,
            ),
        };
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(
                "devcoordinator2.agent_tool=info"
                    .parse()
                    .expect("agent tool log directive is valid"),
            ),
        )
        .init();
    let cli = Cli::parse();
    let format = cli.format;
    let context = cli.client_context();
    let invocation = match cli.into_invocation() {
        Ok(invocation) => invocation,
        Err(error) => {
            return local_error(
                format,
                ErrorCode::ParamsInvalid,
                "command-line arguments are invalid",
                &error.to_string(),
                2,
            );
        }
    };
    match invocation {
        Invocation::Inspect(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Invocation::Daemon => match Config::load_for_daemon() {
            Ok(config) => run_daemon(&config).await,
            Err(error) => configuration_error(format, &error.to_string()),
        },
        Invocation::Mcp => match Config::load() {
            Ok(config) => match mcp::run_stdio(config.socket_path).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => local_error(
                    format,
                    ErrorCode::DaemonUnavailable,
                    "MCP server failed",
                    &error.to_string(),
                    2,
                ),
            },
            Err(error) => configuration_error(format, &error.to_string()),
        },
        Invocation::Remote { operation, params } => match Config::load() {
            Ok(config) => {
                match client::call(&config.socket_path, operation.clone(), params, context).await {
                    Ok(response) => render_response(response, format, Some(operation.as_str())),
                    Err(error) => {
                        let response = ResponseEnvelope::failure("", error);
                        let _ = devcoordinator2_control::cli::render_response(
                            &response,
                            format,
                            Some(operation.as_str()),
                        );
                        ExitCode::from(2)
                    }
                }
            }
            Err(error) => configuration_error(format, &error.to_string()),
        },
        Invocation::OfflineBug { action } => run_offline_bug(action, format),
        Invocation::TestEvent { status } => match check_event::emit_from_environment(status) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => local_error(
                format,
                ErrorCode::ParamsInvalid,
                "cannot emit governed-check event",
                &error.to_string(),
                2,
            ),
        },
        Invocation::ArtifactMaterialize {
            path,
            run_id,
            check,
            artifacts,
            destination,
        } => match Config::load() {
            Ok(config) => {
                let request = MaterializeRequest {
                    path: path.into(),
                    run_id,
                    check,
                    artifacts,
                    destination,
                };
                match artifact_materialize::materialize_with_client(
                    request,
                    config.socket_path,
                    context,
                )
                .await
                {
                    Ok(receipt) => match ResponseEnvelope::success("materialize", receipt) {
                        Ok(response) => {
                            render_response(response, format, Some("test.artifact.materialize"))
                        }
                        Err(error) => {
                            local_error(format, error.code, &error.message, &error.detail, 2)
                        }
                    },
                    Err(error) => local_error(
                        format,
                        error.code,
                        &error.message,
                        &error.detail,
                        error.exit_code(),
                    ),
                }
            }
            Err(error) => configuration_error(format, &error.to_string()),
        },
    }
}

fn source_commit_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    arguments.next().as_deref() == Some(std::ffi::OsStr::new("--source-commit"))
        && arguments.next().is_none()
}

fn repository_config_validation_requested() -> Option<std::path::PathBuf> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--validate-repository-config")) {
        return None;
    }
    let path = arguments.next()?;
    arguments.next().is_none().then(|| path.into())
}

fn validate_repository_config(path: &std::path::Path) -> ExitCode {
    let result = (|| {
        let (tests, deployments) =
            devcoordinator2_control::repository_config::validate_repository_config(path)
                .map_err(|error| error.to_string())?;
        Ok::<_, String>(serde_json::json!({
            "schema":2,
            "tests":tests.into_iter().map(|test| test.name).collect::<Vec<_>>(),
            "deployments":deployments,
        }))
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("repository configuration is not ready for schema 2: {error}");
            ExitCode::from(2)
        }
    }
}

fn configuration_error(format: OutputFormat, detail: &str) -> ExitCode {
    local_error(
        format,
        ErrorCode::ParamsInvalid,
        "configuration failed",
        detail,
        2,
    )
}

fn run_offline_bug(action: OfflineBugAction, format: OutputFormat) -> ExitCode {
    let directory = bugs::bugs_dir();
    let reporter = format!("uid:{}", rustix::process::getuid().as_raw());
    let (operation, result) = match action {
        OfflineBugAction::Report(request) => {
            let result = bugs::report(&directory, &request, &reporter).map(|mut record| {
                record.notified = Some(false);
                serde_json::to_value(record)
            });
            ("bug.report", result)
        }
        OfflineBugAction::List => (
            "bug.list",
            bugs::list_open(&directory).map(|records| {
                serde_json::to_value(BugList {
                    bugs: records,
                    store: None,
                })
            }),
        ),
        OfflineBugAction::Close { bug_id } => {
            let result = bugs::close(&directory, &bug_id).map(|mut closed| {
                closed.notified = Some(false);
                serde_json::to_value(closed)
            });
            ("bug.close", result)
        }
    };
    match result {
        Ok(Ok(data)) => match ResponseEnvelope::success("offline", data) {
            Ok(response) => render_response(response, format, Some(operation)),
            Err(error) => local_error(format, error.code, &error.message, &error.detail, 2),
        },
        Ok(Err(error)) => local_error(
            format,
            ErrorCode::InternalError,
            "cannot encode offline bug result",
            &error.to_string(),
            2,
        ),
        Err(error) => local_error(format, ErrorCode::ParamsInvalid, &error.to_string(), "", 1),
    }
}

async fn run_daemon(config: &Config) -> ExitCode {
    let endpoint = match daemon::DaemonEndpoint::bind(&config.socket_path).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("daemon endpoint failed: {error}");
            return ExitCode::from(1);
        }
    };
    let pg_config = match config.postgres_config() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("daemon database configuration failed: {error}");
            return ExitCode::from(1);
        }
    };
    let installation_fence = config.socket_path.with_file_name("daemon.pre-cutover.sock");
    let recovery_deferred = installation_fence.try_exists().unwrap_or(true);
    let plane_config = config.clone();
    let initializer = match std::thread::Builder::new()
        .name("devcoordinator2-initialize".to_owned())
        .spawn(move || -> Result<_, String> {
            let database =
                Database::connect(&pg_config).map_err(|_| "PostgreSQL authority unavailable")?;
            devcoordinator2_control::routes::initialize_empty_projection(
                &database,
                &plane_config.routes_path(),
                &plane_config.base_domain,
            )
            .map_err(|_| "initial empty route projection unavailable")?;
            let plane = ControlPlane::new(plane_config, database.clone())
                .map_err(|error| error.to_string())?;
            if !recovery_deferred && let Err(error) = plane.recover_tests() {
                tracing::error!(code=%error.code,"governed-test recovery incomplete");
            }
            // One controlled observation seeds candidate read-only health checks.
            // Recurring jobs and external deliveries wait for activation commit.
            // Seed the cache before serving. Health reads never run probes themselves.
            if let Err(error) = plane.health().sampler().tick() {
                tracing::warn!(code=%error.code,"initial health sample unavailable");
            }
            let scheduler = Arc::new(devcoordinator2_control::maintenance::Scheduler::new(
                database, 2, 32,
            )?);
            plane
                .storage()
                .set_maintenance_scheduler(Arc::downgrade(&scheduler));
            Ok((plane, scheduler))
        }) {
        Ok(initializer) => initializer,
        Err(error) => {
            eprintln!("daemon initialization thread failed: {error}");
            return ExitCode::from(1);
        }
    };
    let (plane, scheduler) = match initializer.join() {
        Ok(Ok(initialized)) => initialized,
        Ok(Err(error)) => {
            eprintln!("daemon initialization failed: {error}");
            return ExitCode::from(1);
        }
        Err(_) => {
            eprintln!("daemon initialization thread panicked");
            return ExitCode::from(1);
        }
    };
    let app = Arc::new(
        daemon::App::with_executor(config.edge_uid, Arc::new(plane.clone()))
            .with_installation_fence(config.socket_path.clone()),
    );
    let capacity = plane.capacity().clone();
    let metrics = plane.health().sampler().clone();
    let telegram_service = plane.telegram().clone();
    let _ = telegram_service.enqueue_event(&TelegramEvent::new("coordinator.started"));
    let _ = plane
        .events()
        .publish(devcoordinator2_control::events::NewEvent {
            occurred_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC 3339 timestamp"),
            event: devcoordinator2_api::results::OwnedEvent::Other(
                devcoordinator2_api::results::OtherOwnedEvent {
                    review: None,
                    kind: "coordinator.started".to_owned(),
                    repository_id: None,
                    deployment_id: None,
                    subject_kind: "coordinator".to_owned(),
                    subject_id: "daemon".to_owned(),
                },
            ),
            dedupe_key: None,
        });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_shutdown = shutdown_tx.clone();
    let signal = tokio::spawn(async move {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = signal_shutdown.send(true);
    });
    let mut services = JoinSet::new();
    let bridge_app = Arc::clone(&app);
    let mut daemon_shutdown = shutdown_rx.clone();
    services.spawn(async move {
        (
            "daemon",
            endpoint
                .serve(&mut daemon_shutdown, app)
                .await
                .map_err(|error| error.to_string()),
        )
    });
    let bridge_directory = config.sandbox_bridge_dir.clone();
    let bridge_shutdown = shutdown_rx.clone();
    services.spawn(async move {
        (
            "sandbox request bridge",
            serve_sandbox_bridge(bridge_app, bridge_directory, bridge_shutdown).await,
        )
    });
    let mut capacity_shutdown = shutdown_rx.clone();
    let capacity_fence = installation_fence.clone();
    services.spawn(async move {
        if !wait_for_activation(&capacity_fence, &mut capacity_shutdown).await {
            return ("capacity broker", Ok(()));
        }
        (
            "capacity broker",
            capacity
                .serve(capacity_shutdown)
                .await
                .map_err(|error| error.to_string()),
        )
    });
    let storage = plane.storage().clone();
    let mut storage_shutdown = shutdown_rx.clone();
    let storage_fence = installation_fence.clone();
    services.spawn(async move {
        if !wait_for_activation(&storage_fence, &mut storage_shutdown).await {
            return ("storage maintenance", Ok(()));
        }
        storage.serve(storage_shutdown).await;
        ("storage maintenance", Ok(()))
    });
    let periodic = devcoordinator2_control::maintenance::jobs::periodic(&plane);
    let periodic_scheduler = scheduler.clone();
    let recovery_plane = plane.clone();
    services.spawn(async move {
        let mut shutdown = shutdown_rx;
        if !wait_for_activation(&installation_fence, &mut shutdown).await {
            return ("maintenance scheduler", Ok(()));
        }
        if recovery_deferred {
            let recovery = tokio::task::spawn_blocking(move || {
                devcoordinator2_control::database::maintenance_context(|| {
                    recovery_plane.recover_tests()
                })
            })
            .await;
            if !matches!(recovery, Ok(Ok(()))) {
                tracing::error!("deferred governed-test recovery incomplete");
            }
        }
        periodic_scheduler.serve(periodic, shutdown).await;
        ("maintenance scheduler", Ok(()))
    });
    let first = services.join_next().await;
    let _ = shutdown_tx.send(true);
    let mut failure = service_failure(first);
    while let Some(result) = services.join_next().await {
        failure = failure.or_else(|| service_failure(Some(result)));
    }
    // Join accepted maintenance before the final metric flush and process exit.
    let _ = tokio::task::spawn_blocking(move || {
        scheduler.shutdown_and_join();
        let result = devcoordinator2_control::database::maintenance_context(|| metrics.flush());
        drop(plane);
        result
    })
    .await;
    signal.abort();
    match failure {
        None => ExitCode::SUCCESS,
        Some((surface, error)) => {
            eprintln!("{surface} failed: {error}");
            ExitCode::from(1)
        }
    }
}

async fn wait_for_activation(
    fence: &std::path::Path,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        if *shutdown.borrow() {
            return false;
        }
        if !fence.try_exists().unwrap_or(true) {
            return true;
        }
        tokio::select! {
            result=shutdown.changed()=>{if result.is_err()||*shutdown.borrow(){return false;}},
            _=tokio::time::sleep(Duration::from_millis(250))=>{},
        }
    }
}

async fn serve_sandbox_bridge(
    app: Arc<daemon::App>,
    directory: std::path::PathBuf,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        match devcoordinator2_control::sandbox_bridge::serve(
            Arc::clone(&app),
            &directory,
            shutdown.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::error!(path = %directory.display(), %error, "sandbox request bridge unavailable");
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                    }
                    () = tokio::time::sleep(retry_delay) => {
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
    }
}

fn service_failure(result: Option<JoinedService>) -> Option<(&'static str, String)> {
    match result {
        Some(Ok((_, Ok(())))) | None => None,
        Some(Ok((surface, Err(error)))) => Some((surface, error)),
        Some(Err(error)) => Some(("daemon service task", error.to_string())),
    }
}

fn render_response(
    response: ResponseEnvelope,
    format: OutputFormat,
    operation: Option<&str>,
) -> ExitCode {
    let ok = response.is_ok();
    if devcoordinator2_control::cli::render_response(&response, format, operation).is_err() {
        return ExitCode::from(2);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn local_error(
    format: OutputFormat,
    code: ErrorCode,
    message: &str,
    detail: &str,
    exit: u8,
) -> ExitCode {
    let response = ResponseEnvelope::failure(
        "",
        ProtocolError::new(code, message).with_detail(detail.to_owned()),
    );
    let _ = devcoordinator2_control::cli::render_response(&response, format, None);
    ExitCode::from(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_start_is_rejected_before_opening_or_recovering_state() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            sandbox_bridge_dir: std::path::PathBuf::from("/tmp/devcoordinator2-bridge"),
            state_dir: temporary.path().join("unopened-state"),
            unit_prefix: "isolated-duplicate-test".to_owned(),
            slice_name: "isolated-duplicate-test.slice".to_owned(),
            client_group: "isolated-duplicate-test".to_owned(),
            port_range: (29000, 29100),
            base_domain: "example.test".to_owned(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "http://127.0.0.1:1".to_owned(),
            bugs_dir: temporary.path().join("unopened-bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: Default::default(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        };
        let endpoint = daemon::DaemonEndpoint::bind(&config.socket_path)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), run_daemon(&config))
            .await
            .expect("duplicate startup must stop before initialization");
        assert_eq!(result, ExitCode::from(1));
        assert!(!config.state_dir.exists());
        assert!(!config.bugs_dir.exists());
        assert!(
            tokio::net::UnixStream::connect(&config.socket_path)
                .await
                .is_ok()
        );
        drop(endpoint);
    }
}
