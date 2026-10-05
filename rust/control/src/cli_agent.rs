use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;

use super::{CliValidationError, Invocation, remote};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum HarnessArg {
    Codex,
    Claude,
    Cursor,
    Antigravity,
    Other,
}

impl HarnessArg {
    fn value(self) -> devcoordinator2_api::ClientKind {
        match self {
            Self::Codex => devcoordinator2_api::ClientKind::Codex,
            Self::Claude => devcoordinator2_api::ClientKind::Claude,
            Self::Cursor => devcoordinator2_api::ClientKind::Cursor,
            Self::Antigravity => devcoordinator2_api::ClientKind::Antigravity,
            Self::Other => devcoordinator2_api::ClientKind::Other,
        }
    }
}

#[derive(Debug, Subcommand)]
pub(super) enum AgentCommand {
    SettingsGet(AgentSettingsSelector),
    SettingsSave(AgentSettingsSaveArgs),
    RolesSave(FileRevisionArgs),
    CapabilitiesReport(FileArgs),
    InstructionCurrent {
        #[arg(long)]
        repository_id: Option<String>,
    },
}

#[derive(Debug, Args)]
pub(super) struct AgentSettingsSelector {
    #[arg(long, value_enum, default_value_t=HarnessArg::Codex)]
    harness: HarnessArg,
    #[arg(long)]
    repository_id: Option<String>,
}

#[derive(Debug, Args)]
pub(super) struct AgentSettingsSaveArgs {
    #[arg(long, value_enum, default_value_t=HarnessArg::Codex)]
    harness: HarnessArg,
    #[arg(long)]
    repository_id: Option<String>,
    #[arg(long)]
    expected_revision: u32,
    #[arg(long)]
    file: String,
}

#[derive(Debug, Args)]
pub(super) struct FileRevisionArgs {
    #[arg(long)]
    expected_revision: u32,
    #[arg(long)]
    file: String,
}
#[derive(Debug, Args)]
pub(super) struct FileArgs {
    #[arg(long)]
    file: String,
}

fn read_json(path: &str) -> Result<Value, CliValidationError> {
    let bytes = std::fs::read(path)
        .map_err(|e| CliValidationError::Invalid(format!("cannot read {path}: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| CliValidationError::Invalid(format!("invalid JSON in {path}: {e}")))
}

impl AgentCommand {
    pub(super) fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::SettingsGet(args) => remote(
                "agent.settings.get",
                serde_json::json!({"scope":if args.repository_id.is_some(){"repository"}else{"global"},"repository_id":args.repository_id,"harness":args.harness.value()}),
            ),
            Self::SettingsSave(args) => {
                let mut value = read_json(&args.file)?;
                let rules = value.get_mut("rules").cloned().unwrap_or(value);
                remote(
                    "agent.settings.save",
                    serde_json::json!({"scope":if args.repository_id.is_some(){"repository"}else{"global"},"repository_id":args.repository_id,"harness":args.harness.value(),"expected_revision":args.expected_revision,"rules":rules}),
                )
            }
            Self::RolesSave(args) => {
                let value = read_json(&args.file)?;
                let roles = value.get("roles").cloned().unwrap_or(value);
                remote(
                    "agent.roles.save",
                    serde_json::json!({"expected_revision":args.expected_revision,"roles":roles}),
                )
            }
            Self::CapabilitiesReport(args) => {
                remote("agent.capabilities.report", read_json(&args.file)?)
            }
            Self::InstructionCurrent { repository_id } => remote(
                "agent.instruction.current",
                serde_json::json!({"repository_id":repository_id}),
            ),
        }
    }
}
