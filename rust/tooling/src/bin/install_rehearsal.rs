//! Explicit, disposable Linux installer acceptance. Never selects installed state.
use clap::{Parser, Subcommand};
use devcoordinator2_tooling::cutover::rehearsal::{self, Config};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    StartupGuardHold {
        #[arg(long)]
        work_root: PathBuf,
        #[arg(long,value_parser=["before","after","drift"])]
        case: String,
    },
    #[command(hide = true)]
    StartupGuardRecover {
        #[arg(long)]
        work_root: PathBuf,
        #[arg(long,value_parser=["before","after","drift"])]
        case: String,
    },
    /// Test only: normal v3 base and candidate, with no historical bundle.
    StartupGuard {
        #[arg(long)]
        base_daemon: PathBuf,
        #[arg(long)]
        base_source_commit: String,
        #[arg(long)]
        candidate_daemon: PathBuf,
        #[arg(long)]
        source_root: PathBuf,
        #[arg(long)]
        work_root: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        postgres_bin: PathBuf,
        #[arg(long)]
        postgres_account: String,
        #[arg(long)]
        caller_uid: u32,
        #[arg(long)]
        caller_gid: u32,
    },
    Run {
        #[arg(long)]
        candidate_manifest: PathBuf,
        #[arg(long)]
        prior_manifest: PathBuf,
        /// Bootstrap this verified normal v3 release, then rehearse a native upgrade.
        #[arg(long)]
        native_prior_manifest: Option<PathBuf>,
        #[arg(long)]
        work_root: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        postgres_bin: PathBuf,
        #[arg(long)]
        postgres_account: String,
        #[arg(long)]
        caller_uid: u32,
        #[arg(long)]
        caller_gid: u32,
    },
    /// Kernel-attributed fixture client, with a bounded envelope on stdin.
    Request {
        #[arg(long)]
        socket: PathBuf,
    },
}
fn main() -> ExitCode {
    if std::env::args_os().skip(1).collect::<Vec<_>>()
        == [std::ffi::OsString::from("--source-commit")]
    {
        println!("{}", devcoordinator2_control::SOURCE_COMMIT);
        return ExitCode::SUCCESS;
    }
    let result=match Cli::parse().command {
        Command::StartupGuardHold{work_root,case} => {
            match rehearsal::hold_startup_guard(&work_root,&case) {
                Ok(()) => { println!("owned-startup-guard-held"); std::process::exit(86) },
                Err(error) => Err(error),
            }
        },
        Command::StartupGuardRecover{work_root,case} => rehearsal::recover_startup_guard(&work_root,&case),
        Command::StartupGuard{base_daemon,base_source_commit,candidate_daemon,source_root,work_root,report,postgres_bin,postgres_account,caller_uid,caller_gid} => {
            rehearsal::run_startup_guard(rehearsal::GuardConfig{base_daemon,base_source_commit,candidate_daemon,source_root,work_root,report,postgres_bin,postgres_account,caller_uid,caller_gid})
        },
        Command::Run{candidate_manifest,prior_manifest,native_prior_manifest,work_root,report,postgres_bin,postgres_account,caller_uid,caller_gid}=>{
            rehearsal::run(Config{candidate_manifest,prior_manifest,native_prior_manifest,work_root,report,postgres_bin,postgres_account,caller_uid,caller_gid})
                .map(|report|serde_json::json!({"schema":1,"fixture_only":true,"status":report.status,"checks":report.checks,"report":report.report_path}))
        },
        Command::Request{socket}=>rehearsal::request_stdin(&socket),
    };
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
