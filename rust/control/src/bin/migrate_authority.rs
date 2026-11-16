//! Offline PostgreSQL migration, archive and activation evidence.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use devcoordinator2_control::config::Config;
use devcoordinator2_persistence::{
    Persistence, backup,
    migration::{import_sqlite, inspect_sqlite, verify_import},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Inspect,
    Import,
    #[value(alias = "verify-import")]
    Verify,
    Backup,
    VerifyBackup,
    VerifyRecoverableBackup,
    RestoreCandidate,
    Migrate,
    InspectPostgres,
    VerifyRoutes,
}
#[derive(Debug, Parser)]
#[command(name = "devcoordinator2-migrate-authority")]
struct Arguments {
    /// Closed, checkpointed SQLite schema-34 source. Never removed here.
    #[arg(long)]
    source: Option<PathBuf>,
    /// Private mode-0600 connection file; connection strings are not arguments.
    #[arg(long, alias = "config")]
    database_config: Option<PathBuf>,
    #[arg(long)]
    destination: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    edge_state: Option<PathBuf>,
    /// New private evidence file. Existing files are never overwritten.
    #[arg(long)]
    report: PathBuf,
    #[arg(long, value_enum, default_value = "inspect")]
    mode: Mode,
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--source-commit")
        && std::env::args().count() == 2
    {
        println!("{}", devcoordinator2_control::SOURCE_COMMIT);
        return;
    }
    if let Err(error) = run() {
        eprintln!("Authority operation failed: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let parent = arguments
        .report
        .parent()
        .ok_or("report requires a parent directory")?;
    if !parent.is_dir() || arguments.report.exists() {
        return Err("report must be a new file in an existing private directory".into());
    }
    let mut report = tempfile::NamedTempFile::new_in(parent)?;
    report
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let source = || {
        arguments
            .source
            .as_deref()
            .ok_or("--source is required for inspection, import and import verification")
    };
    let config = || -> Result<_, Box<dyn std::error::Error>> {
        Ok(Config::read_postgres_config(
            arguments
                .database_config
                .as_deref()
                .ok_or("--database-config is required for this operation")?,
        )?)
    };
    let destination = || {
        arguments
            .destination
            .as_deref()
            .ok_or("--destination is required for archive operations")
    };
    let mut comparison_failed = false;
    let document = match arguments.mode {
        Mode::Inspect => serde_json::to_value(inspect_sqlite(source()?)?)?,
        Mode::Import | Mode::Verify => {
            let database = Persistence::connect(&config()?)?;
            let receipt = match arguments.mode {
                Mode::Import => import_sqlite(&database, source()?)?,
                _ => verify_import(&database, source()?)?,
            };
            serde_json::to_value(receipt)?
        }
        Mode::Backup => {
            backup::dump(&config()?, destination()?)?;
            let stamp = backup::archive_stamp(destination()?)?;
            serde_json::json!({"schema":1,"verified":true,"verification":"private_archive_created_and_hashed","archive_sha256":stamp.sha256,"archive_bytes":stamp.bytes})
        }
        Mode::VerifyBackup => {
            let comparison = backup::compare_archive(&config()?, destination()?)?;
            comparison_failed = !comparison.verification.verified;
            serde_json::to_value(comparison)?
        }
        Mode::VerifyRecoverableBackup => serde_json::to_value(backup::verify_recoverable_archive(
            &config()?,
            destination()?,
        )?)?,
        Mode::RestoreCandidate => {
            serde_json::to_value(backup::restore_candidate(&config()?, destination()?)?)?
        }
        Mode::Migrate => {
            let database = Persistence::connect(&config()?)?;
            database.migrate()?;
            let version: String =
                database.read::<_, devcoordinator2_persistence::Error, _>(|c| {
                    c.query_row(
                        "SELECT value FROM meta WHERE key='schema_version'",
                        [],
                        |r| r.get(0),
                    )
                })?;
            if version != devcoordinator2_persistence::schema::DATABASE_SCHEMA_VERSION.to_string() {
                return Err("native migration did not reach the supported schema".into());
            }
            serde_json::json!({"schema":1,"verified":true,"database_schema":version})
        }
        Mode::InspectPostgres => {
            let database = Persistence::connect(&config()?)?;
            let (applying, schema_version) = database
                .read::<_, devcoordinator2_persistence::Error, _>(|c| {
                    let version: String = c.query_row(
                        "SELECT value FROM meta WHERE key='schema_version'",
                        [],
                        |r| r.get(0),
                    )?;
                    let version = version.parse::<u32>().map_err(|_| {
                        devcoordinator2_persistence::Error::Migration(
                            "invalid authority schema metadata".into(),
                        )
                    })?;
                    let applying = c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM deployments WHERE state='applying')",
                        [],
                        |r| r.get::<_, bool>(0),
                    )?;
                    Ok((applying, version))
                })?;
            serde_json::json!({"schema":1,"verified":true,"applying":applying,"schema_version":schema_version})
        }
        Mode::VerifyRoutes => {
            let database =
                devcoordinator2_control::database::Database::connect_existing(&config()?)?;
            devcoordinator2_control::routes::verify_activation_routes(
                &database,
                arguments
                    .state_dir
                    .as_deref()
                    .ok_or("--state-dir is required for route verification")?,
                arguments
                    .edge_state
                    .as_deref()
                    .ok_or("--edge-state is required for route verification")?,
            )?;
            serde_json::json!({"schema":1,"verified":true,"verification":"route_leases_listeners_and_edge_acknowledgement"})
        }
    };
    report.write_all(&serde_json::to_vec_pretty(&document)?)?;
    report.as_file().sync_all()?;
    report
        .persist_noclobber(&arguments.report)
        .map_err(|_| "cannot publish evidence without overwriting an existing path")?;
    std::fs::File::open(parent)?.sync_all()?;
    if comparison_failed {
        return Err("restored archive differs from frozen authority data or identities; private comparison report retained".into());
    }
    println!(
        "Authority {:?} succeeded; private evidence written; source retained.",
        arguments.mode
    );
    Ok(())
}
