//! A disposable, Unix-socket-only PostgreSQL fixture runtime.
//!
//! This module never connects to the installed Coordinator or enumerates other
//! databases. Each fixture owns its cluster and cleans up only its own files.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use postgres::{Client, NoTls};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::{DatabaseConfig, Error, Result};

pub struct FixtureCluster {
    root: TempDir,
    socket: TempDir,
    bin: PathBuf,
    running: Mutex<bool>,
    identity: Option<(u32, u32)>,
}

impl FixtureCluster {
    pub fn start() -> Result<Self> {
        let identity = if unsafe { libc::geteuid() } == 0 {
            let uid = std::env::var("DEVCOORDINATOR2_TEST_POSTGRES_UID")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|uid| *uid != 0);
            let gid = std::env::var("DEVCOORDINATOR2_TEST_POSTGRES_GID")
                .ok()
                .and_then(|v| v.parse::<u32>().ok());
            Some((uid.ok_or_else(||Error::Fixture("root fixture requires explicit non-root DEVCOORDINATOR2_TEST_POSTGRES_UID/GID".into()))?,gid.ok_or_else(||Error::Fixture("root fixture requires explicit non-root DEVCOORDINATOR2_TEST_POSTGRES_UID/GID".into()))?))
        } else {
            None
        };
        Self::start_with(None, identity)
    }
    /// Root acceptance supplies its existing trusted caller identity and a
    /// directory below the new, marker-bound external test work root.
    pub fn start_in(base: &Path, uid: u32, gid: u32) -> Result<Self> {
        if uid == 0 {
            return Err(Error::Fixture(
                "PostgreSQL fixture identity must be non-root".into(),
            ));
        }
        let root = unsafe { libc::geteuid() } == 0;
        if !root && (uid != unsafe { libc::geteuid() } || gid != unsafe { libc::getegid() }) {
            return Err(Error::Fixture(
                "fixture cannot select another identity without root acceptance".into(),
            ));
        }
        Self::start_with(Some(base), if root { Some((uid, gid)) } else { None })
    }
    fn start_with(base: Option<&Path>, identity: Option<(u32, u32)>) -> Result<Self> {
        let bin = discover_binaries()?;
        let base = base
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("DEVCOORDINATOR2_TEST_POSTGRES_ROOT").map(PathBuf::from))
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/postgres-fixtures")
            });
        fs::create_dir_all(&base).map_err(io_error)?;
        let root = tempfile::Builder::new()
            .prefix("cluster-")
            .tempdir_in(base)
            .map_err(io_error)?;
        let socket = tempfile::Builder::new()
            .prefix("dc2pg-")
            .tempdir()
            .map_err(io_error)?;
        let fixture = Self {
            root,
            socket,
            bin,
            running: Mutex::new(false),
            identity,
        };
        if let Some((uid, gid)) = identity {
            for path in [fixture.root.path(), fixture.socket.path()] {
                let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                    .map_err(|_| Error::Fixture("invalid fixture path".into()))?;
                if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
                    return Err(io_error(std::io::Error::last_os_error()));
                }
            }
        }
        fixture.command(
            "initdb",
            &[
                "--no-locale",
                "--encoding=UTF8",
                "--auth-local=trust",
                "--auth-host=reject",
                "--username=dc2_fixture",
                "-D",
            ],
            true,
        )?;
        let mut config = OpenOptions::new()
            .append(true)
            .open(fixture.data().join("postgresql.conf"))
            .map_err(io_error)?;
        writeln!(config,"\nlisten_addresses = ''\nunix_socket_directories = '{}'\nunix_socket_permissions = 0700\nport = 5432\nmax_connections = 160\nshared_buffers = 8MB\nfsync = on\nsynchronous_commit = on\nlogging_collector = off\nlog_statement = 'none'\n",fixture.socket.path().display().to_string().replace('\'',"''")).map_err(io_error)?;
        fixture.start_server()?;
        Ok(fixture)
    }
    fn data(&self) -> PathBuf {
        self.root.path().join("data")
    }
    pub fn log_path(&self) -> PathBuf {
        self.root.path().join("postgres.log")
    }
    fn command(&self, name: &str, args: &[&str], append_data: bool) -> Result<()> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.path().join("commands.log"))
            .map_err(io_error)?;
        let mut command = Command::new(self.bin.join(name));
        if let Some((uid, gid)) = self.identity {
            // Root acceptance chooses this same-owner test identity explicitly.
            // Clear inherited root groups before dropping all three IDs.
            unsafe {
                command.pre_exec(move || {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command.args(args);
        if append_data {
            command.arg(self.data());
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(io_error)?))
            .stderr(Stdio::from(log))
            .status()
            .map_err(io_error)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Fixture(format!(
                "{name} failed; private fixture log retained until cleanup"
            )))
        }
    }
    fn start_server(&self) -> Result<()> {
        let log = self.log_path();
        let data = self.data();
        self.command(
            "pg_ctl",
            &[
                "-D",
                data.to_str()
                    .ok_or_else(|| Error::Fixture("non-UTF8 scratch path".into()))?,
                "-l",
                log.to_str()
                    .ok_or_else(|| Error::Fixture("non-UTF8 log path".into()))?,
                "-w",
                "-t",
                "30",
                "start",
            ],
            false,
        )?;
        *self
            .running
            .lock()
            .map_err(|_| Error::Fixture("fixture mutex poisoned".into()))? = true;
        Ok(())
    }
    pub fn stop(&self) -> Result<()> {
        let mut running = self
            .running
            .lock()
            .map_err(|_| Error::Fixture("fixture mutex poisoned".into()))?;
        if *running {
            let data = self.data();
            self.command(
                "pg_ctl",
                &[
                    "-D",
                    data.to_str()
                        .ok_or_else(|| Error::Fixture("non-UTF8 scratch path".into()))?,
                    "-w",
                    "-t",
                    "30",
                    "-m",
                    "fast",
                    "stop",
                ],
                false,
            )?;
            *running = false;
        }
        Ok(())
    }
    /// Real server restart; the data directory and exact database are retained.
    pub fn restart(&self) -> Result<()> {
        self.stop()?;
        self.start_server()
    }
    fn pg_config(&self, database: &str) -> postgres::Config {
        let mut c = postgres::Config::new();
        c.host_path(self.socket.path())
            .port(5432)
            .user("dc2_fixture")
            .dbname(database)
            .connect_timeout(std::time::Duration::from_secs(5));
        c
    }
    pub fn client(&self, database: &str) -> Result<Client> {
        Ok(self.pg_config(database).connect(NoTls)?)
    }
    pub fn create_database(&self, name: &str) -> Result<DatabaseConfig> {
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || name.is_empty()
            || name.len() > 63
        {
            return Err(Error::Fixture("invalid fixture database name".into()));
        }
        let mut client = self.client("postgres")?;
        let exists = client
            .query_opt("SELECT 1 FROM pg_database WHERE datname=$1", &[&name])?
            .is_some();
        if !exists {
            client.batch_execute(&format!(
                "CREATE DATABASE \"{name}\" TEMPLATE template0 ENCODING 'UTF8'"
            ))?;
        }
        let mut config = DatabaseConfig::new(format!(
            "host='{}' port=5432 user=dc2_fixture dbname={name}",
            self.socket
                .path()
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('\'', "\\'")
        ));
        // The test manifest path may contain `..`; persist the physical path
        // so the production private-file validator can reject traversal while
        // fixture connections still carry a valid owned binding.
        config.data_directory = Some(self.data().canonicalize().map_err(io_error)?);
        Ok(config)
    }
    pub fn socket_directory(&self) -> &Path {
        self.socket.path()
    }
}
impl Drop for FixtureCluster {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Shared within a test process, with per-path databases and RAII ownership.
/// A weak cache never keeps a cluster running after its last fixture drops.
pub struct FixtureDatabase {
    pub cluster: Arc<FixtureCluster>,
    pub config: DatabaseConfig,
    pub name: String,
}
impl FixtureDatabase {
    pub fn for_path(path: &Path) -> Result<Self> {
        static CLUSTER: OnceLock<Mutex<Weak<FixtureCluster>>> = OnceLock::new();
        let cache = CLUSTER.get_or_init(|| Mutex::new(Weak::new()));
        let mut cached = cache
            .lock()
            .map_err(|_| Error::Fixture("fixture cache mutex poisoned".into()))?;
        let cluster = if let Some(cluster) = cached.upgrade() {
            cluster
        } else {
            let cluster = Arc::new(FixtureCluster::start()?);
            *cached = Arc::downgrade(&cluster);
            cluster
        };
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(io_error)?.join(path)
        };
        let digest = Sha256::digest(absolute.as_os_str().as_encoded_bytes());
        let name = format!("dc2_{}", crate::hex(&digest))[..52].to_owned();
        let config = cluster.create_database(&name)?;
        Ok(Self {
            cluster,
            config,
            name,
        })
    }
}

fn discover_binaries() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("DEVCOORDINATOR2_TEST_POSTGRES_BIN") {
        candidates.push(PathBuf::from(path));
    }
    // Explicit candidate toolchains are allowed for CI. PATH lookup and live
    // daemon discovery are intentionally absent.
    if let Ok(entries) = fs::read_dir("/usr/lib/postgresql") {
        let mut versions = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("bin"))
            .collect::<Vec<_>>();
        versions.sort();
        versions.reverse();
        candidates.extend(versions);
    }
    for candidate in candidates {
        if ["postgres", "initdb", "pg_ctl"]
            .iter()
            .all(|name| candidate.join(name).is_file())
        {
            return Ok(candidate);
        }
    }
    Err(Error::Fixture("PostgreSQL server tools are required in an installed version directory or DEVCOORDINATOR2_TEST_POSTGRES_BIN; no SQLite fallback exists".into()))
}
fn io_error(error: std::io::Error) -> Error {
    Error::Fixture(error.to_string())
}
