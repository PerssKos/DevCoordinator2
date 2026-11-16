//! Compact registry-driven v3 client. Domain grammars live only in typed operation contracts.
use clap::{Parser, Subcommand, ValueEnum};
use devcoordinator2_api::{ClientContext, ClientKind, ProtocolError, ResponseEnvelope, operation};
use serde_json::{Map, Value, json};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use thiserror::Error;

#[path = "cli_work_context.rs"]
mod work_context_cli;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Json,
    Human,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ClientArg {
    Codex,
    Claude,
    Cursor,
    Antigravity,
    Human,
    Edge,
    #[default]
    Other,
}
impl From<ClientArg> for ClientKind {
    fn from(value: ClientArg) -> Self {
        match value {
            ClientArg::Codex => Self::Codex,
            ClientArg::Claude => Self::Claude,
            ClientArg::Cursor => Self::Cursor,
            ClientArg::Antigravity => Self::Antigravity,
            ClientArg::Human => Self::Human,
            ClientArg::Edge => Self::Edge,
            ClientArg::Other => Self::Other,
        }
    }
}
#[derive(Debug, Parser)]
#[command(
    name = "devcoordinator2",
    version,
    about = "Development coordinator — protocol v3"
)]
pub struct Cli {
    #[arg(long,global=true,value_enum,default_value_t=OutputFormat::Json)]
    pub format: OutputFormat,
    #[arg(long,global=true,value_enum,default_value_t=ClientArg::Other)]
    pub client: ClientArg,
    #[arg(long, global = true)]
    pub session: Option<String>,
    #[arg(long, global = true)]
    pub model: Option<String>,
    #[arg(long, global = true)]
    pub effort: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    Daemon,
    Mcp,
    /// Invoke an operation. Use --params-file - for bounded JSON on stdin.
    Call {
        operation: String,
        #[arg(long, conflicts_with = "params_file")]
        params: Option<String>,
        #[arg(long, conflicts_with = "params")]
        params_file: Option<PathBuf>,
    },
    /// Discover operation names and summaries without contacting the daemon.
    Operations {
        #[arg(long, default_value = "")]
        prefix: String,
    },
    /// Read one operation's input, result and authorization contract.
    Schema {
        operation: String,
    },
    /// Emit a check result through the inherited executor descriptor.
    EmitEvent {
        #[arg(value_enum)]
        status: TestEventStatus,
    },
    /// Download a retained artifact into a new destination and verify every digest.
    Materialize {
        path: String,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        check: String,
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        #[arg(long)]
        destination: PathBuf,
    },
}
#[derive(Debug)]
pub enum Invocation {
    Daemon,
    Mcp,
    Remote {
        operation: String,
        params: Value,
    },
    Inspect(Value),
    OfflineBug {
        action: OfflineBugAction,
    },
    TestEvent {
        status: TestEventStatus,
    },
    ArtifactMaterialize {
        path: String,
        run_id: String,
        check: String,
        artifacts: Vec<String>,
        destination: PathBuf,
    },
}
#[derive(Debug, Error)]
pub enum CliValidationError {
    #[error("{0}")]
    Invalid(String),
    #[error("operation parameters are invalid: {0}")]
    Contract(#[from] ProtocolError),
}
fn invalid(message: impl Into<String>) -> CliValidationError {
    CliValidationError::Invalid(message.into())
}
fn bounded_input(input: impl Read) -> Result<Value, CliValidationError> {
    let mut bytes = Vec::new();
    input
        .take((devcoordinator2_api::MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Cannot read operation parameters"))?;
    if bytes.len() > devcoordinator2_api::MAX_REQUEST_BYTES {
        return Err(invalid("Operation parameters exceed 64 KiB"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("Operation parameters are not valid JSON"))
}
fn parameters_file(path: &std::path::Path) -> Result<Value, CliValidationError> {
    use std::os::unix::fs::OpenOptionsExt;
    // Reject pipes/devices without blocking while opening them. Stdin remains
    // an explicit supported stream through --params-file -.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| invalid("Cannot open parameters file"))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("Cannot inspect parameters file"))?;
    if !metadata.is_file() || metadata.len() > devcoordinator2_api::MAX_REQUEST_BYTES as u64 {
        return Err(invalid(
            "Parameters file must be a regular file no larger than 64 KiB",
        ));
    }
    bounded_input(file)
}
impl Cli {
    pub fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        Ok(match self.command {
            Command::Daemon => Invocation::Daemon,
            Command::Mcp => Invocation::Mcp,
            Command::Operations { prefix } => {
                Invocation::Inspect(devcoordinator2_api::catalog_summary(&prefix))
            }
            Command::Schema { operation: name } => Invocation::Inspect(
                devcoordinator2_api::operation_schema(&name)
                    .ok_or_else(|| invalid("Unknown operation"))?,
            ),
            Command::EmitEvent { status } => Invocation::TestEvent { status },
            Command::Materialize {
                path,
                run_id,
                check,
                artifacts,
                destination,
            } => {
                if !std::path::Path::new(&path).is_absolute() || !destination.is_absolute() {
                    return Err(invalid("Source and destination must be absolute paths"));
                }
                let mut unique = std::collections::HashSet::new();
                if artifacts
                    .iter()
                    .any(|name| name.is_empty() || !unique.insert(name))
                {
                    return Err(invalid("Artifact names must be unique and nonempty"));
                }
                Invocation::ArtifactMaterialize {
                    path,
                    run_id,
                    check,
                    artifacts,
                    destination,
                }
            }
            Command::Call {
                operation: name,
                params,
                params_file,
            } => {
                let definition = operation(&name).ok_or_else(|| {
                    invalid("Unknown operation; use operations or schema to discover it")
                })?;
                let params = match (params, params_file) {
                    (Some(text), None) => bounded_input(text.as_bytes())?,
                    (None, Some(path)) if path == std::path::Path::new("-") => {
                        bounded_input(io::stdin().lock())?
                    }
                    (None, Some(path)) => parameters_file(&path)?,
                    (None, None) => json!({}),
                    _ => return Err(invalid("Choose one parameters source")),
                };
                if !params.is_object() {
                    return Err(invalid("Operation parameters must be an object"));
                }
                (definition.validate_params)(&params)?;
                match definition.id {
                    devcoordinator2_api::OperationId::BugReport => {
                        let p: devcoordinator2_api::params::BugReport =
                            serde_json::from_value(params)
                                .map_err(|_| invalid("Invalid bug report"))?;
                        Invocation::OfflineBug {
                            action: OfflineBugAction::Report(Box::new(p)),
                        }
                    }
                    devcoordinator2_api::OperationId::BugList => Invocation::OfflineBug {
                        action: OfflineBugAction::List,
                    },
                    devcoordinator2_api::OperationId::BugClose => {
                        let p: devcoordinator2_api::params::BugClose =
                            serde_json::from_value(params)
                                .map_err(|_| invalid("Invalid bug identity"))?;
                        Invocation::OfflineBug {
                            action: OfflineBugAction::Close { bug_id: p.bug_id },
                        }
                    }
                    _ => Invocation::Remote {
                        operation: name,
                        params,
                    },
                }
            }
        })
    }
}
#[derive(Debug)]
pub enum OfflineBugAction {
    Report(Box<devcoordinator2_api::params::BugReport>),
    List,
    Close { bug_id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum TestEventStatus {
    Passed,
    Failed,
    Unsafe,
}

impl TestEventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unsafe => "unsafe",
        }
    }
}

/// Render one protocol-v3 response to stdout. JSON is the stable automation
/// surface; human output keeps every returned value and every error field.
pub fn render_response(
    response: &ResponseEnvelope,
    format: OutputFormat,
    operation: Option<&str>,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    render_response_to(&mut output, response, format, operation)
}

fn render_response_to(
    output: &mut impl Write,
    response: &ResponseEnvelope,
    format: OutputFormat,
    operation: Option<&str>,
) -> io::Result<()> {
    match format {
        OutputFormat::Json => {
            serde_json::to_writer(&mut *output, response).map_err(io::Error::other)?;
            writeln!(output)
        }
        OutputFormat::Human => match response {
            ResponseEnvelope::Success {
                data,
                agent_messages,
                ..
            } => {
                render_human_success(output, data, operation)?;
                for message in agent_messages.iter().flatten() {
                    writeln!(
                        output,
                        "Reminder {}: {}",
                        message.message_id, message.summary
                    )?;
                }
                Ok(())
            }
            ResponseEnvelope::Failure { error, .. } => {
                writeln!(output, "error {}: {}", error.code, error.message)?;
                if !error.detail.is_empty() {
                    writeln!(output, "detail: {}", error.detail)?;
                }
                if let Some(recovery) = &error.recovery {
                    writeln!(
                        output,
                        "recovery: class={:?} retryable={} waitable={} safe_to_continue={}",
                        recovery.class,
                        recovery.retryable,
                        recovery.waitable,
                        recovery.safe_to_continue
                    )?;
                    if let Some(state) = &recovery.state {
                        writeln!(output, "state: {state}")?;
                    }
                    writeln!(output, "reason: {}", recovery.reason)?;
                    for option in recovery.options.iter().take(6) {
                        let operation = option.operation.as_deref().unwrap_or("manual action");
                        writeln!(
                            output,
                            "option {}: {} via {}",
                            option.id, option.effect, operation
                        )?;
                    }
                }
                Ok(())
            }
        },
    }
}

fn render_human_success(
    output: &mut impl Write,
    data: &Value,
    operation: Option<&str>,
) -> io::Result<()> {
    if matches!(operation, Some("test.start" | "test.retry"))
        && let Some(previous) = data.get("superseded_run_id").and_then(Value::as_str)
    {
        writeln!(
            output,
            "Previous run {previous} was cancelled by this start."
        )?;
    }
    if operation == Some("ping")
        && let Some(object) = data.as_object()
    {
        let daemon_version = object.get("daemon_version").and_then(Value::as_str);
        let protocol = object.get("protocol_version").and_then(Value::as_u64);
        let schema = object.get("schema_version").and_then(Value::as_u64);
        if let (Some(daemon_version), Some(protocol), Some(schema)) =
            (daemon_version, protocol, schema)
        {
            writeln!(
                output,
                "DevCoordinator2 {daemon_version} — protocol {protocol}, schema {schema}"
            )?;
            let mut remainder = object.clone();
            remainder.remove("daemon_version");
            remainder.remove("protocol_version");
            remainder.remove("schema_version");
            if !remainder.is_empty() {
                write_pretty_value(output, &Value::Object(remainder))?;
            }
            return Ok(());
        }
    }
    if matches!(
        operation,
        Some("deployment.apply" | "deployment.status" | "deployment.wait")
    ) && let Some(object) = data.as_object()
        && object.get("preview_url").is_some()
    {
        writeln!(
            output,
            "preview: {}",
            object
                .get("preview_url")
                .and_then(Value::as_str)
                .unwrap_or("unavailable")
        )?;
        writeln!(
            output,
            "generation: {}",
            object
                .get("current_generation")
                .map(Value::to_string)
                .unwrap_or_else(|| "unknown".into())
        )?;
        writeln!(
            output,
            "runtime: {}",
            object
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        )?;
        if let Some(readiness) = object.get("readiness").and_then(Value::as_object) {
            writeln!(
                output,
                "source: {}",
                readiness
                    .get("source_state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )?;
            writeln!(
                output,
                "verification: {}",
                object
                    .get("verification_state")
                    .and_then(Value::as_str)
                    .unwrap_or("not_run")
            )?;
        }
    }
    let label = operation.unwrap_or("success");
    if data.as_object().is_some_and(Map::is_empty) {
        return writeln!(output, "{label}: ok");
    }
    writeln!(output, "{label}:")?;
    write_pretty_value(output, data)
}

fn write_pretty_value(output: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *output, value).map_err(io::Error::other)?;
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    #[test]
    fn generic_cli_uses_the_typed_catalogue_and_rejects_legacy_commands() {
        Cli::command().debug_assert();
        for name in [
            "ping",
            "agent.roles.save",
            "decision.tail",
            "test.capacity.set",
        ] {
            let schema = Cli::try_parse_from(["devcoordinator2", "schema", name])
                .unwrap()
                .into_invocation()
                .unwrap();
            assert!(matches!(schema,Invocation::Inspect(value) if value["name"]==name));
        }
        let call = Cli::try_parse_from([
            "devcoordinator2",
            "call",
            "test.capacity.set",
            "--params",
            r#"{"cap":null}"#,
        ])
        .unwrap()
        .into_invocation()
        .unwrap();
        assert!(
            matches!(call,Invocation::Remote{operation,params} if operation=="test.capacity.set" && params==json!({"cap":null}))
        );
        assert!(Cli::try_parse_from(["devcoordinator2", "task", "history", "x"]).is_err());
        for text in ["[]", r#"{"unexpected":true}"#] {
            assert!(
                Cli::try_parse_from(["devcoordinator2", "call", "ping", "--params", text])
                    .unwrap()
                    .into_invocation()
                    .is_err()
            );
        }
        assert!(
            bounded_input(
                "x".repeat(devcoordinator2_api::MAX_REQUEST_BYTES + 1)
                    .as_bytes()
            )
            .is_err()
        );
    }
    #[test]
    fn local_bug_reporting_does_not_require_the_daemon() {
        assert!(matches!(
            Cli::try_parse_from(["devcoordinator2", "call", "bug.list"])
                .unwrap()
                .into_invocation()
                .unwrap(),
            Invocation::OfflineBug {
                action: OfflineBugAction::List
            }
        ));
    }
    #[test]
    fn human_start_names_the_exact_cancelled_predecessor() {
        let mut output = Vec::new();
        render_human_success(
            &mut output,
            &json!({"superseded_run_id":"old-run"}),
            Some("test.start"),
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .starts_with("Previous run old-run was cancelled by this start.")
        );
    }
}
