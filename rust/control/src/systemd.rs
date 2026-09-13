//! Explicit-argv systemd transient-unit and cgroup boundary.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use thiserror::Error;

pub const STOP_GRACE_SECONDS: &str = "10s";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_CAP: usize = 64 * 1024;
const ERROR_CAP: usize = 512;

#[derive(Debug, Error)]
pub enum SystemdError {
    #[error("invalid systemd request: {0}")]
    Invalid(String),
    #[error("{program} invocation failed: {message}")]
    Invocation { program: String, message: String },
    #[error("{0}")]
    Operation(String),
}

#[derive(Clone, Debug)]
pub struct TransientUnitSpec {
    pub unit: String,
    pub slice_name: String,
    pub uid: u32,
    pub gid: u32,
    pub timeout_seconds: u64,
    pub working_directory: PathBuf,
    pub environment_file: Option<PathBuf>,
    pub command: Vec<OsString>,
    pub scratch_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PersistentUnitSpec {
    pub unit: String,
    pub slice_name: String,
    pub uid: u32,
    pub gid: u32,
    pub working_directory: PathBuf,
    pub environment_file: PathBuf,
    pub command: Vec<OsString>,
    pub log_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessState {
    pub state: String,
    pub active_state: String,
    pub sub_state: String,
    pub result: String,
    pub main_pid: u32,
    pub restarts: u32,
    pub cgroup: String,
}

pub trait UnitProcess: Send {
    fn id(&self) -> u32;
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

struct ChildUnitProcess(Child);

impl UnitProcess for ChildUnitProcess {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0
            .stdout
            .take()
            .map(|stdout| Box::new(stdout) as Box<dyn Read + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0
            .stderr
            .take()
            .map(|stderr| Box::new(stderr) as Box<dyn Read + Send>)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.0.wait()
    }
}

pub trait SystemdControl: Send + Sync + 'static {
    fn spawn_transient(
        &self,
        specification: &TransientUnitSpec,
    ) -> Result<Box<dyn UnitProcess>, SystemdError>;
    fn start_persistent(&self, specification: &PersistentUnitSpec) -> Result<(), SystemdError>;
    fn process_state(&self, unit: &str) -> Result<ProcessState, SystemdError>;
    fn show_unit(
        &self,
        unit: &str,
        properties: &[&str],
    ) -> Result<Vec<(String, String)>, SystemdError>;
    fn list_matching_units(&self, pattern: &str) -> Result<Vec<String>, SystemdError>;
    fn stop_unit(&self, unit: &str) -> Result<(), SystemdError>;
    fn reset_failed(&self, unit: &str) -> Result<(), SystemdError>;
    fn control_group_path(&self, unit: &str) -> Result<Option<PathBuf>, SystemdError>;
    fn cgroup_memory_current(&self, cgroup: &Path) -> Option<u64> {
        std::fs::read_to_string(cgroup.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    fn prove_cgroup_empty(&self, cgroup: Option<&Path>, deadline: Duration) -> bool;
    fn process_uids(&self, pid: u32) -> Option<[u32; 4]>;
}

#[derive(Clone, Debug)]
pub struct SystemdCli {
    systemctl: PathBuf,
    systemd_run: PathBuf,
    cgroup_root: PathBuf,
}

impl Default for SystemdCli {
    fn default() -> Self {
        Self {
            systemctl: PathBuf::from("systemctl"),
            systemd_run: PathBuf::from("systemd-run"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
        }
    }
}

impl SystemdCli {
    pub fn with_paths(systemctl: PathBuf, systemd_run: PathBuf, cgroup_root: PathBuf) -> Self {
        Self {
            systemctl,
            systemd_run,
            cgroup_root,
        }
    }

    pub fn unit_is_loaded(&self, unit: &str) -> Result<bool, SystemdError> {
        Ok(property(
            self.show_unit(unit, &["LoadState"])?.as_slice(),
            "LoadState",
        )
        .is_some_and(|value| !value.is_empty() && value != "not-found"))
    }

    pub fn unit_is_active_or_activating(&self, unit: &str) -> Result<bool, SystemdError> {
        Ok(property(
            self.show_unit(unit, &["ActiveState"])?.as_slice(),
            "ActiveState",
        )
        .is_some_and(|value| matches!(value, "active" | "activating" | "deactivating")))
    }
}

impl SystemdControl for SystemdCli {
    fn spawn_transient(
        &self,
        specification: &TransientUnitSpec,
    ) -> Result<Box<dyn UnitProcess>, SystemdError> {
        let argv = build_systemd_run_argv(specification)?;
        let child = Command::new(&self.systemd_run)
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| invocation_error(&self.systemd_run, error))?;
        Ok(Box::new(ChildUnitProcess(child)))
    }

    fn start_persistent(&self, specification: &PersistentUnitSpec) -> Result<(), SystemdError> {
        let argv = build_persistent_systemd_run_argv(specification)?;
        if self.unit_is_loaded(&specification.unit)? {
            let state = self.process_state(&specification.unit)?;
            if !matches!(state.active_state.as_str(), "inactive" | "failed") {
                return Err(SystemdError::Operation(format!(
                    "cannot recreate a non-terminal unit: {}",
                    specification.unit
                )));
            }
            let cgroup = self.control_group_path(&specification.unit)?;
            if !self.prove_cgroup_empty(cgroup.as_deref(), Duration::ZERO) {
                return Err(SystemdError::Operation(format!(
                    "cannot recreate a populated unit: {}",
                    specification.unit
                )));
            }
            self.stop_unit(&specification.unit)?;
            self.reset_failed(&specification.unit)?;
        }
        rotate_log(&specification.log_path)?;
        let output = run_bounded(&self.systemd_run, &argv[1..], STOP_TIMEOUT)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(SystemdError::Operation(format!(
                "systemd-run failed: {}",
                bounded_lossy(&output.stderr, 1_024)
            )))
        }
    }

    fn process_state(&self, unit: &str) -> Result<ProcessState, SystemdError> {
        let values = self.show_unit(
            unit,
            &[
                "ActiveState",
                "SubState",
                "Result",
                "MainPID",
                "NRestarts",
                "ExecMainStatus",
                "ControlGroup",
            ],
        )?;
        let active = property(&values, "ActiveState").unwrap_or_default();
        let cgroup = property(&values, "ControlGroup").unwrap_or_default();
        let state = if matches!(active, "" | "inactive") && cgroup.is_empty() {
            "stopped"
        } else if active == "active" {
            "running"
        } else if matches!(active, "activating" | "reloading") {
            "starting"
        } else if active == "deactivating" {
            "stopping"
        } else {
            "failed"
        };
        Ok(ProcessState {
            state: state.into(),
            active_state: if active.is_empty() {
                "absent".into()
            } else {
                active.into()
            },
            sub_state: property(&values, "SubState").unwrap_or_default().into(),
            result: property(&values, "Result").unwrap_or_default().into(),
            main_pid: property(&values, "MainPID")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            restarts: property(&values, "NRestarts")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            cgroup: cgroup.into(),
        })
    }

    fn show_unit(
        &self,
        unit: &str,
        properties: &[&str],
    ) -> Result<Vec<(String, String)>, SystemdError> {
        validate_atom("unit", unit)?;
        if properties.is_empty() || properties.iter().any(|value| !valid_property(value)) {
            return Err(SystemdError::Invalid(
                "systemd property selection is invalid".into(),
            ));
        }
        let output = run_bounded(
            &self.systemctl,
            &[
                OsString::from("show"),
                OsString::from(unit),
                OsString::from("-p"),
                OsString::from(properties.join(",")),
                OsString::from("--no-pager"),
            ],
            SYSTEMCTL_TIMEOUT,
        )?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| SystemdError::Operation("systemctl show returned non-UTF-8".into()))?;
        Ok(text
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| properties.contains(key))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }

    fn list_matching_units(&self, pattern: &str) -> Result<Vec<String>, SystemdError> {
        validate_atom("unit pattern", pattern)?;
        let output = run_bounded(
            &self.systemctl,
            &[
                OsString::from("list-units"),
                OsString::from("--all"),
                OsString::from("--plain"),
                OsString::from("--no-legend"),
                OsString::from("--no-pager"),
                OsString::from(pattern),
            ],
            SYSTEMCTL_TIMEOUT,
        )?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| SystemdError::Operation("systemctl list returned non-UTF-8".into()))?;
        Ok(text
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_owned)
            .collect())
    }

    fn stop_unit(&self, unit: &str) -> Result<(), SystemdError> {
        validate_atom("unit", unit)?;
        let output = run_bounded(
            &self.systemctl,
            &[OsString::from("stop"), OsString::from(unit)],
            STOP_TIMEOUT,
        )?;
        if output.status.success() || !self.unit_is_loaded(unit)? {
            return Ok(());
        }
        Err(SystemdError::Operation(format!(
            "systemctl stop {unit} failed: {}",
            bounded_lossy(&output.stderr, ERROR_CAP)
        )))
    }

    fn reset_failed(&self, unit: &str) -> Result<(), SystemdError> {
        validate_atom("unit", unit)?;
        let _ = run_bounded(
            &self.systemctl,
            &[OsString::from("reset-failed"), OsString::from(unit)],
            SYSTEMCTL_TIMEOUT,
        )?;
        Ok(())
    }

    fn control_group_path(&self, unit: &str) -> Result<Option<PathBuf>, SystemdError> {
        let values = self.show_unit(unit, &["ControlGroup"])?;
        let Some(value) = property(&values, "ControlGroup").filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let relative = Path::new(value.trim_start_matches('/'));
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(SystemdError::Operation(
                "systemd returned an unsafe cgroup path".into(),
            ));
        }
        Ok(Some(self.cgroup_root.join(relative)))
    }

    fn prove_cgroup_empty(&self, cgroup: Option<&Path>, deadline: Duration) -> bool {
        cgroup.is_none_or(|cgroup| prove_cgroup_empty(cgroup, deadline))
    }

    fn process_uids(&self, pid: u32) -> Option<[u32; 4]> {
        process_uids_at(Path::new("/proc"), pid)
    }
}

pub fn build_systemd_run_argv(
    specification: &TransientUnitSpec,
) -> Result<Vec<OsString>, SystemdError> {
    validate_atom("unit", &specification.unit)?;
    validate_atom("slice", &specification.slice_name)?;
    if specification.command.is_empty()
        || specification.timeout_seconds == 0
        || !specification.working_directory.is_absolute()
        || !specification.scratch_directory.is_absolute()
        || specification
            .environment_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
    {
        return Err(SystemdError::Invalid(
            "transient unit paths, timeout, or command are invalid".into(),
        ));
    }
    let user = user_record(specification.uid)?;
    let groups = supplementary_groups(specification.uid)?;
    Ok(build_argv_with_identity(specification, &user, &groups))
}

pub fn build_persistent_systemd_run_argv(
    specification: &PersistentUnitSpec,
) -> Result<Vec<OsString>, SystemdError> {
    validate_atom("unit", &specification.unit)?;
    validate_atom("slice", &specification.slice_name)?;
    if specification.command.is_empty()
        || !specification.working_directory.is_absolute()
        || !specification.environment_file.is_absolute()
        || !specification.log_path.is_absolute()
    {
        return Err(SystemdError::Invalid(
            "persistent unit paths or command are invalid".into(),
        ));
    }
    let groups = supplementary_groups(specification.uid)?;
    let mut argv = vec![
        OsString::from("systemd-run"),
        OsString::from("--quiet"),
        OsString::from(format!("--unit={}", specification.unit)),
        OsString::from(format!("--slice={}", specification.slice_name)),
        OsString::from(format!("--uid={}", specification.uid)),
        OsString::from(format!("--gid={}", specification.gid)),
        OsString::from("--property=KillMode=control-group"),
        OsString::from("--property=TimeoutStopSec=15s"),
        OsString::from("--property=Restart=on-failure"),
        OsString::from("--property=RestartSec=2s"),
        OsString::from("--property=StartLimitIntervalSec=120"),
        OsString::from("--property=StartLimitBurst=5"),
        OsString::from("--property=NoNewPrivileges=yes"),
        OsString::from("--property=UMask=0027"),
        prefixed(
            "--property=EnvironmentFile=",
            &specification.environment_file,
        ),
        prefixed("--property=StandardOutput=append:", &specification.log_path),
        prefixed("--property=StandardError=append:", &specification.log_path),
        prefixed("--working-directory=", &specification.working_directory),
    ];
    if !groups.is_empty() {
        let mut value = OsString::from("--property=SupplementaryGroups=");
        for (index, (_, name)) in groups.iter().enumerate() {
            if index > 0 {
                value.push(" ");
            }
            value.push(name);
        }
        argv.push(value);
    }
    argv.push(OsString::from("--"));
    argv.extend(specification.command.iter().cloned());
    Ok(argv)
}

fn rotate_log(path: &Path) -> Result<(), SystemdError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(SystemdError::Invalid(
                "deployment log path must be a regular non-symlink file".into(),
            ));
        }
        Ok(_) => {
            let mut previous = path.as_os_str().to_owned();
            previous.push(".1");
            let _ = std::fs::rename(path, PathBuf::from(previous));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(invocation_error(path, error)),
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct UserRecord {
    name: OsString,
    home: OsString,
}

fn build_argv_with_identity(
    specification: &TransientUnitSpec,
    user: &UserRecord,
    groups: &[(u32, OsString)],
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("systemd-run"),
        OsString::from("--quiet"),
        OsString::from("--pipe"),
        OsString::from(format!("--unit={}", specification.unit)),
        OsString::from(format!("--slice={}", specification.slice_name)),
        OsString::from(format!("--uid={}", specification.uid)),
        OsString::from(format!("--gid={}", specification.gid)),
        OsString::from("--property=KillMode=control-group"),
        OsString::from(format!("--property=TimeoutStopSec={STOP_GRACE_SECONDS}")),
        OsString::from(format!(
            "--property=RuntimeMaxSec={}s",
            specification.timeout_seconds
        )),
        OsString::from("--property=NoNewPrivileges=yes"),
        OsString::from("--property=UMask=0077"),
        prefixed("--working-directory=", &specification.working_directory),
        prefixed_os("--setenv=HOME=", &user.home),
        prefixed_os("--setenv=USER=", &user.name),
        prefixed_os("--setenv=LOGNAME=", &user.name),
        prefixed("--setenv=TMPDIR=", &specification.scratch_directory),
    ];
    if !groups.is_empty() {
        let mut value = OsString::from("--property=SupplementaryGroups=");
        for (index, (_, name)) in groups.iter().enumerate() {
            if index > 0 {
                value.push(" ");
            }
            value.push(name);
        }
        argv.push(value);
    }
    if let Some(path) = &specification.environment_file {
        argv.push(prefixed("--property=EnvironmentFile=", path));
    }
    argv.push(OsString::from("--"));
    argv.extend(specification.command.iter().cloned());
    argv
}

pub fn supplementary_groups(uid: u32) -> Result<Vec<(u32, OsString)>, SystemdError> {
    let (user, primary_gid) = user_record_with_gid(uid)?;
    let user = CString::new(user.name.as_bytes()).map_err(|_| {
        SystemdError::Operation("user database returned a name containing NUL".into())
    })?;
    let mut count: libc::c_int = 0;
    // SAFETY: getgrouplist reads the NUL-terminated user name and uses the
    // null/zero pair only to report the required element count.
    unsafe {
        libc::getgrouplist(user.as_ptr(), primary_gid, std::ptr::null_mut(), &mut count);
    }
    if count <= 0 || count > 65_536 {
        return Ok(Vec::new());
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    // SAFETY: `groups` has exactly `count` writable gid_t elements.
    let status =
        unsafe { libc::getgrouplist(user.as_ptr(), primary_gid, groups.as_mut_ptr(), &mut count) };
    if status < 0 {
        return Err(SystemdError::Operation(
            "cannot resolve supplementary groups".into(),
        ));
    }
    groups.truncate(count as usize);
    groups.sort_unstable();
    groups.dedup();
    groups
        .into_iter()
        .filter(|gid| *gid != 0)
        .map(|gid| Ok((gid, group_name(gid)?)))
        .collect()
}

pub fn primary_gid(uid: u32) -> Result<u32, SystemdError> {
    user_record_with_gid(uid).map(|(_, gid)| gid)
}

fn user_record(uid: u32) -> Result<UserRecord, SystemdError> {
    user_record_with_gid(uid).map(|value| value.0)
}

fn user_record_with_gid(uid: u32) -> Result<(UserRecord, libc::gid_t), SystemdError> {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: every pointer names writable storage for getpwuid_r's documented
    // lifetime; fields are read only after a successful non-null result.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(SystemdError::Operation(format!(
            "no operating-system account for uid {uid}"
        )));
    }
    // SAFETY: successful getpwuid_r initialized the record and its pointers
    // remain backed by `buffer` for this scope.
    let record = unsafe { record.assume_init() };
    Ok((
        UserRecord {
            // SAFETY: passwd string fields are NUL-terminated on success.
            name: unsafe { OsString::from_vec(CStr::from_ptr(record.pw_name).to_bytes().to_vec()) },
            // SAFETY: same contract as pw_name.
            home: unsafe { OsString::from_vec(CStr::from_ptr(record.pw_dir).to_bytes().to_vec()) },
        },
        record.pw_gid,
    ))
}

fn group_name(gid: libc::gid_t) -> Result<OsString, SystemdError> {
    let mut record = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: pointers reference appropriately sized writable storage.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(SystemdError::Operation(
            "cannot resolve supplementary group".into(),
        ));
    }
    if result.is_null() {
        return Ok(OsString::from(gid.to_string()));
    }
    // SAFETY: successful getgrgid_r initialized the record and NUL-terminated
    // name backed by `buffer`.
    let record = unsafe { record.assume_init() };
    Ok(unsafe { OsString::from_vec(CStr::from_ptr(record.gr_name).to_bytes().to_vec()) })
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_bounded(
    program: &Path,
    arguments: &[OsString],
    deadline: Duration,
) -> Result<CommandOutput, SystemdError> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| invocation_error(program, error))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SystemdError::Operation("system command stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SystemdError::Operation("system command stderr unavailable".into()))?;
    let stdout = thread::spawn(move || drain_bounded(stdout));
    let stderr = thread::spawn(move || drain_bounded(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SystemdError::Invocation {
                    program: program.display().to_string(),
                    message: "timed out".into(),
                });
            }
            Err(error) => return Err(invocation_error(program, error)),
        }
    };
    let stdout = stdout
        .join()
        .map_err(|_| SystemdError::Operation("stdout drain failed".into()))??;
    let stderr = stderr
        .join()
        .map_err(|_| SystemdError::Operation("stderr drain failed".into()))??;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn drain_bounded(mut reader: impl Read) -> Result<Vec<u8>, SystemdError> {
    let mut retained = Vec::new();
    let mut block = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut block)
            .map_err(|error| SystemdError::Operation(format!("output drain failed: {error}")))?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = OUTPUT_CAP.saturating_sub(retained.len());
        retained.extend_from_slice(&block[..read.min(remaining)]);
    }
}

fn prove_cgroup_empty(cgroup: &Path, deadline: Duration) -> bool {
    let events = cgroup.join("cgroup.events");
    let file = match open(
        &events,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(error) if error == rustix::io::Errno::NOENT => return true,
        Err(_) => return false,
    };
    let started = Instant::now();
    loop {
        let mut content = String::new();
        let mut current = match file.try_clone() {
            Ok(current) => current,
            Err(_) => return false,
        };
        if io::Seek::rewind(&mut current).is_err() || current.read_to_string(&mut content).is_err()
        {
            return false;
        }
        if content.lines().any(|line| line == "populated 0") {
            return true;
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        let milliseconds = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&file),
            events: libc::POLLPRI | libc::POLLERR,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if ready <= 0 {
            return false;
        }
    }
}

fn process_uids_at(proc_root: &Path, pid: u32) -> Option<[u32; 4]> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("status")).ok()?;
    let values = text
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    values.try_into().ok()
}

fn validate_atom(label: &str, value: &str) -> Result<(), SystemdError> {
    if value.is_empty()
        || value.len() > 255
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(SystemdError::Invalid(format!("{label} is invalid")));
    }
    Ok(())
}

fn valid_property(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn property<'a>(values: &'a [(String, String)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn prefixed(prefix: &str, path: &Path) -> OsString {
    prefixed_os(prefix, path.as_os_str())
}

fn prefixed_os(prefix: &str, value: &OsStr) -> OsString {
    let mut result = OsString::from(prefix);
    result.push(value);
    result
}

fn invocation_error(program: &Path, error: io::Error) -> SystemdError {
    SystemdError::Invocation {
        program: program.display().to_string(),
        message: bounded_text(&error.to_string(), ERROR_CAP),
    }
}

fn bounded_lossy(bytes: &[u8], limit: usize) -> String {
    bounded_text(&String::from_utf8_lossy(bytes), limit)
}

fn bounded_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn specification(root: &Path) -> TransientUnitSpec {
        TransientUnitSpec {
            unit: "devcoordinator2-test-w1-run.service".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
            uid: 1000,
            gid: 1000,
            timeout_seconds: 30,
            working_directory: root.join("repo"),
            environment_file: Some(root.join("environment")),
            command: vec![OsString::from("/usr/bin/true")],
            scratch_directory: root.join("scratch"),
        }
    }

    #[test]
    fn transient_argv_is_exact_and_never_includes_root_supplementary_group() {
        let root = Path::new("/tmp/fixture");
        let spec = specification(root);
        let groups = vec![(0, OsString::from("root")), (44, OsString::from("video"))]
            .into_iter()
            .filter(|(gid, _)| *gid != 0)
            .collect::<Vec<_>>();
        let argv = build_argv_with_identity(
            &spec,
            &UserRecord {
                name: OsString::from("agent"),
                home: OsString::from("/home/agent"),
            },
            &groups,
        );
        let rendered = argv
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(rendered[0], "systemd-run");
        assert!(rendered.contains(&"--uid=1000".into()));
        assert!(rendered.contains(&"--property=NoNewPrivileges=yes".into()));
        assert!(rendered.contains(&"--property=SupplementaryGroups=video".into()));
        assert!(!rendered.iter().any(|value| value.ends_with("root")));
        assert_eq!(rendered.last().map(String::as_str), Some("/usr/bin/true"));
    }

    #[test]
    fn fake_systemctl_parses_properties_lists_and_safe_cgroup_paths() {
        let temporary = tempdir().expect("tempdir");
        let script = temporary.path().join("systemctl");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1\" in\nshow) printf 'LoadState=loaded\\nActiveState=active\\nControlGroup=/slice/unit\\n';;\nlist-units) printf 'one.service loaded active running x\\ntwo.service loaded failed failed y\\n';;\n*) exit 0;;\nesac\n",
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let cli = SystemdCli::with_paths(
            script.clone(),
            temporary.path().join("systemd-run"),
            temporary.path().join("cgroup"),
        );
        assert!(cli.unit_is_loaded("one.service").expect("loaded"));
        assert!(
            cli.unit_is_active_or_activating("one.service")
                .expect("active")
        );
        assert_eq!(
            cli.list_matching_units("*.service").expect("list"),
            vec!["one.service", "two.service"]
        );
        assert_eq!(
            cli.control_group_path("one.service").expect("cgroup"),
            Some(temporary.path().join("cgroup/slice/unit"))
        );
        let state = cli.process_state("one.service").expect("process state");
        assert_eq!(state.state, "running");
        assert_eq!(state.active_state, "active");
    }

    #[test]
    fn persistent_unit_uses_exact_properties_and_rotates_one_bounded_log() {
        let temporary = tempdir().expect("tempdir");
        let recorder = temporary.path().join("arguments");
        let launcher = temporary.path().join("systemd-run");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                recorder.display()
            ),
        )
        .expect("launcher");
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let systemctl = temporary.path().join("systemctl");
        std::fs::write(&systemctl, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = temporary.path().join("component.log");
        std::fs::write(&log, "prior").unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let cli = SystemdCli::with_paths(systemctl, launcher, temporary.path().join("cgroup"));
        cli.start_persistent(&PersistentUnitSpec {
            unit: "devcoordinator2-deploy-d1-api-g1.service".into(),
            slice_name: "devcoordinator2-deploy-d1.slice".into(),
            uid,
            gid,
            working_directory: temporary.path().to_owned(),
            environment_file: temporary.path().join("environment"),
            command: vec![OsString::from("/usr/bin/true")],
            log_path: log.clone(),
        })
        .expect("persistent start");
        assert_eq!(
            std::fs::read_to_string(log.with_extension("log.1")).unwrap(),
            "prior"
        );
        let arguments = std::fs::read_to_string(recorder).unwrap();
        assert!(arguments.contains("--property=Restart=on-failure"));
        assert!(arguments.contains("--property=NoNewPrivileges=yes"));
        assert!(arguments.contains("--property=StandardOutput=append:"));
        assert!(arguments.contains("/usr/bin/true"));
    }

    #[test]
    fn persistent_recreation_preserves_live_units_and_recovers_only_empty_terminal_units() {
        for (active, populated, stop_exit, valid, recoverable) in [
            ("not-found", false, 0, true, true),
            ("failed", false, 0, true, true),
            ("inactive", false, 0, true, true),
            ("failed", true, 0, true, false),
            ("failed", false, 1, true, false),
            ("failed", false, 0, false, false),
            ("active", false, 0, true, false),
            ("activating", false, 0, true, false),
            ("deactivating", false, 0, true, false),
            ("reloading", false, 0, true, false),
            ("unknown", false, 0, true, false),
        ] {
            let temporary = tempdir().unwrap();
            let recorder = temporary.path().join("calls");
            let controller = temporary.path().join("systemctl");
            let launcher = temporary.path().join("systemd-run");
            let cgroup = temporary.path().join("cgroup/unit");
            std::fs::create_dir_all(&cgroup).unwrap();
            std::fs::write(
                cgroup.join("cgroup.events"),
                format!("populated {}\n", u8::from(populated)),
            )
            .unwrap();
            std::fs::write(
                &controller,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nif [ \"$1\" = show ]; then printf 'LoadState={}\\nActiveState={active}\\nControlGroup=/unit\\n'; fi\nif [ \"$1\" = stop ]; then exit {stop_exit}; fi\n",
                    recorder.display(), if active == "not-found" { "not-found" } else { "loaded" }
                ),
            )
            .unwrap();
            std::fs::write(
                &launcher,
                format!(
                    "#!/bin/sh\nprintf 'launch\\n' >> '{}'\n",
                    recorder.display()
                ),
            )
            .unwrap();
            for executable in [&controller, &launcher] {
                std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            let log = temporary.path().join("component.log");
            std::fs::write(&log, "original failure evidence").unwrap();
            let cli = SystemdCli::with_paths(controller, launcher, temporary.path().join("cgroup"));
            let result = cli.start_persistent(&PersistentUnitSpec {
                unit: "devcoordinator2-deploy-d1-api-g1.service".into(),
                slice_name: "devcoordinator2-deploy-d1.slice".into(),
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
                working_directory: temporary.path().to_owned(),
                environment_file: temporary.path().join("environment"),
                command: if valid {
                    vec![OsString::from("/usr/bin/true")]
                } else {
                    Vec::new()
                },
                log_path: log.clone(),
            });
            assert_eq!(
                result.is_ok(),
                recoverable,
                "{active}/{populated}/{stop_exit}/{valid}"
            );
            let calls = std::fs::read_to_string(&recorder).unwrap_or_default();
            if recoverable {
                if active == "not-found" {
                    assert_eq!(calls, "show\nlaunch\n");
                } else {
                    assert!(calls.ends_with("stop\nreset-failed\nlaunch\n"));
                }
                assert_eq!(
                    std::fs::read_to_string(log.with_extension("log.1")).unwrap(),
                    "original failure evidence"
                );
            } else {
                assert_eq!(calls.contains("stop\n"), stop_exit != 0);
                assert!(!calls.contains("reset-failed\n"));
                assert!(!calls.contains("launch\n"));
                assert_eq!(
                    std::fs::read_to_string(log).unwrap(),
                    "original failure evidence"
                );
                if !valid {
                    assert!(
                        calls.is_empty(),
                        "invalid specification must not invoke systemd"
                    );
                }
            }
        }
    }

    #[test]
    fn cgroup_and_proc_readers_are_bounded_and_truthful() {
        let temporary = tempdir().expect("tempdir");
        let cgroup = temporary.path().join("group");
        std::fs::create_dir(&cgroup).expect("cgroup");
        std::fs::write(cgroup.join("cgroup.events"), "populated 0\nfrozen 0\n").expect("events");
        assert!(prove_cgroup_empty(&cgroup, Duration::from_millis(1)));
        std::fs::write(cgroup.join("cgroup.events"), "populated 1\n").expect("events");
        assert!(!prove_cgroup_empty(&cgroup, Duration::from_millis(1)));
        let proc = temporary.path().join("proc/42");
        std::fs::create_dir_all(&proc).expect("proc");
        std::fs::write(proc.join("status"), "Name:\ttest\nUid:\t1\t2\t3\t4\n").expect("status");
        assert_eq!(
            process_uids_at(&temporary.path().join("proc"), 42),
            Some([1, 2, 3, 4])
        );
    }

    #[test]
    fn invalid_atoms_and_traversing_cgroups_are_rejected() {
        assert!(validate_atom("unit", "bad unit").is_err());
        assert!(!valid_property("Control-Group"));
        assert_eq!(bounded_text(&"é".repeat(400), 511).len(), 510);
        let values = vec![("ControlGroup".into(), "/../escape".into())];
        let relative = Path::new(
            property(&values, "ControlGroup")
                .unwrap()
                .trim_start_matches('/'),
        );
        assert!(
            relative
                .components()
                .any(|part| part == Component::ParentDir)
        );
    }
}
