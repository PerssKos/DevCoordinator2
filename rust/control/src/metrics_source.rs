//! Bounded host, cgroup, filesystem, and Docker metric sources.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::docker::{DockerControl, DockerInvocation};

const READ_LIMIT: u64 = 1024 * 1024;
const OUTPUT_LIMIT: usize = 64 * 1024;
const PROCESS_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HostMemory {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FilesystemStats {
    pub size: u64,
    pub free: u64,
    pub used: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CgroupStats {
    pub cpu_usec: u64,
    pub memory_current: u64,
    pub memory_peak: u64,
    pub pids: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DockerStorage {
    pub images: u64,
    pub build_cache: u64,
    pub volumes: BTreeMap<String, u64>,
}

pub trait MetricSource: Send + Sync + 'static {
    fn host_cpu_ticks(&self) -> (u64, u64);
    fn host_memory(&self) -> HostMemory;
    fn host_load(&self) -> (f64, f64, f64);
    fn filesystem(&self, path: &Path) -> FilesystemStats;
    fn cgroup_stats(&self, path: Option<&Path>) -> Option<CgroupStats>;
    fn own_cgroup(&self) -> Option<PathBuf>;
    fn container_cgroup(&self, container_id: &str) -> PathBuf;
    fn directory_size(&self, path: &Path, timeout: Duration) -> Option<u64>;
    fn container_sizes(&self) -> BTreeMap<String, u64>;
    fn docker_shared_sizes(&self) -> DockerStorage;
    fn logical_cpus(&self) -> u32;
}

#[derive(Clone)]
pub struct HostMetricSource {
    proc_root: PathBuf,
    cgroup_root: PathBuf,
    du: PathBuf,
    docker: Arc<dyn DockerControl>,
}

impl HostMetricSource {
    pub fn new(docker: Arc<dyn DockerControl>) -> Self {
        Self {
            proc_root: "/proc".into(),
            cgroup_root: "/sys/fs/cgroup".into(),
            du: "/usr/bin/du".into(),
            docker,
        }
    }

    #[cfg(test)]
    fn with_roots(
        proc_root: PathBuf,
        cgroup_root: PathBuf,
        du: PathBuf,
        docker: Arc<dyn DockerControl>,
    ) -> Self {
        Self {
            proc_root,
            cgroup_root,
            du,
            docker,
        }
    }
}

impl MetricSource for HostMetricSource {
    fn host_cpu_ticks(&self) -> (u64, u64) {
        let Some(text) = read_bounded_text(&self.proc_root.join("stat")) else {
            return (0, 0);
        };
        let Some(line) = text.lines().next() else {
            return (0, 0);
        };
        let values = line
            .split_whitespace()
            .skip(1)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>();
        let Ok(values) = values else {
            return (0, 0);
        };
        if values.len() < 4 {
            return (0, 0);
        }
        let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
        let total = values.iter().copied().fold(0_u64, u64::saturating_add);
        (total.saturating_sub(idle), total)
    }

    fn host_memory(&self) -> HostMemory {
        let mut memory = HostMemory::default();
        let Some(text) = read_bounded_text(&self.proc_root.join("meminfo")) else {
            return memory;
        };
        for line in text.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let Some(bytes) = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|value| value.checked_mul(1024))
            else {
                continue;
            };
            match name {
                "MemTotal" => memory.total = bytes,
                "MemAvailable" => memory.available = bytes,
                "SwapTotal" => memory.swap_total = bytes,
                "SwapFree" => memory.swap_free = bytes,
                _ => {}
            }
        }
        memory.used = memory.total.saturating_sub(memory.available);
        memory
    }

    fn host_load(&self) -> (f64, f64, f64) {
        let Some(text) = read_bounded_text(&self.proc_root.join("loadavg")) else {
            return (0.0, 0.0, 0.0);
        };
        let values = text
            .split_whitespace()
            .take(3)
            .map(str::parse::<f64>)
            .collect::<Result<Vec<_>, _>>();
        match values.as_deref() {
            Ok([one, five, fifteen]) => (*one, *five, *fifteen),
            _ => (0.0, 0.0, 0.0),
        }
    }

    fn filesystem(&self, path: &Path) -> FilesystemStats {
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return FilesystemStats::default();
        };
        // SAFETY: statvfs writes to the provided live output object and does
        // not retain either pointer.
        let mut value: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut value) } != 0 {
            return FilesystemStats::default();
        }
        let fragment: u64 = value.f_frsize as _;
        let size = fragment.saturating_mul(value.f_blocks as _);
        let free = fragment.saturating_mul(value.f_bavail as _);
        let used = size.saturating_sub(fragment.saturating_mul(value.f_bfree as _));
        FilesystemStats { size, free, used }
    }

    fn cgroup_stats(&self, path: Option<&Path>) -> Option<CgroupStats> {
        let path = path.filter(|path| path.is_dir())?;
        let mut result = CgroupStats::default();
        if let Some(cpu) = read_bounded_text(&path.join("cpu.stat")) {
            result.cpu_usec = cpu
                .lines()
                .find_map(|line| line.strip_prefix("usage_usec "))
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
        result.memory_current = read_integer(&path.join("memory.current"));
        result.memory_peak = read_integer(&path.join("memory.peak"));
        result.pids = read_integer(&path.join("pids.current"));
        if let Some(io) = read_bounded_text(&path.join("io.stat")) {
            for field in io
                .lines()
                .flat_map(str::split_whitespace)
                .filter_map(|field| field.split_once('='))
            {
                match field.0 {
                    "rbytes" => {
                        result.io_read_bytes = result
                            .io_read_bytes
                            .saturating_add(field.1.parse().unwrap_or(0));
                    }
                    "wbytes" => {
                        result.io_write_bytes = result
                            .io_write_bytes
                            .saturating_add(field.1.parse().unwrap_or(0));
                    }
                    _ => {}
                }
            }
        }
        Some(result)
    }

    fn own_cgroup(&self) -> Option<PathBuf> {
        read_bounded_text(&self.proc_root.join("self/cgroup"))?
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(|path| self.cgroup_root.join(path.trim_start_matches('/')))
    }

    fn container_cgroup(&self, container_id: &str) -> PathBuf {
        self.cgroup_root
            .join("system.slice")
            .join(format!("docker-{container_id}.scope"))
    }

    fn directory_size(&self, path: &Path, timeout: Duration) -> Option<u64> {
        if !path.is_absolute() || !path.exists() {
            return None;
        }
        let output = run_bounded(
            &self.du,
            &[OsString::from("-sbx"), path.as_os_str().to_owned()],
            timeout,
        )?;
        if !output.status.success() || output.truncated {
            return None;
        }
        String::from_utf8(output.stdout)
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    fn container_sizes(&self) -> BTreeMap<String, u64> {
        let invocation = DockerInvocation::new(
            vec![
                "ps".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--size".into(),
                "--format".into(),
                "{{.ID}} {{.Size}}".into(),
            ],
            Duration::from_secs(120),
        )
        .expect("static Docker size invocation is valid");
        let Ok(output) = self.docker.invoke(invocation) else {
            return BTreeMap::new();
        };
        if !output.success() || output.stdout_truncated {
            return BTreeMap::new();
        }
        output
            .stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.into(), parse_size(fields.next()?)))
            })
            .collect()
    }

    fn docker_shared_sizes(&self) -> DockerStorage {
        let invocation = DockerInvocation::new(
            vec![
                "system".into(),
                "df".into(),
                "-v".into(),
                "--format".into(),
                "{{range .Images}}image={{.UniqueSize}}{{println}}{{end}}{{range .BuildCache}}build_cache={{.Size}}{{println}}{{end}}{{range .Volumes}}volume={{.Name}}={{.Size}}{{println}}{{end}}".into(),
            ],
            Duration::from_secs(120),
        )
        .expect("static Docker storage invocation is valid");
        let Ok(output) = self.docker.invoke(invocation) else {
            return DockerStorage::default();
        };
        if !output.success() || output.stdout_truncated {
            return DockerStorage::default();
        }
        parse_projected_docker_storage(&output.stdout)
    }

    fn logical_cpus(&self) -> u32 {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(1)
    }
}

fn read_bounded_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(READ_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > READ_LIMIT {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn read_integer(path: &Path) -> u64 {
    read_bounded_text(path)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

struct ProcessOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    truncated: bool,
}

fn run_bounded(program: &Path, arguments: &[OsString], timeout: Duration) -> Option<ProcessOutput> {
    let mut child = Command::new(program)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || read_output(stdout));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
        thread::sleep(PROCESS_POLL.min(deadline.saturating_duration_since(now)));
    };
    let (stdout, truncated) = reader.join().ok()?.ok()?;
    Some(ProcessOutput {
        status,
        stdout,
        truncated,
    })
}

fn read_output(mut source: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok((output, truncated));
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
}

fn parse_size(value: &str) -> u64 {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number = value[..split].parse::<f64>().unwrap_or(0.0);
    let unit = value[split..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "b" | "" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "tb" => 1_000_000_000_000.0,
        "kib" => 1_024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        _ => 0.0,
    };
    (number * multiplier).max(0.0).round() as u64
}

fn parse_projected_docker_storage(output: &str) -> DockerStorage {
    let mut result = DockerStorage::default();
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("image=") {
            result.images = result.images.saturating_add(parse_size(value));
        } else if let Some(value) = line.strip_prefix("build_cache=") {
            result.build_cache = result.build_cache.saturating_add(parse_size(value));
        } else if let Some(value) = line.strip_prefix("volume=")
            && let Some((name, size)) = value.split_once('=')
            && !name.is_empty()
        {
            result.volumes.insert(name.to_owned(), parse_size(size));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{DockerError, DockerOutput, ExactContainerId, LogFollower};
    use tempfile::tempdir;

    struct NoDocker;
    impl DockerControl for NoDocker {
        fn invoke(&self, _invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            Err(DockerError::CliUnavailable)
        }
        fn spawn_follow_logs(
            &self,
            _container_id: &ExactContainerId,
        ) -> Result<LogFollower, DockerError> {
            Err(DockerError::CliUnavailable)
        }
    }

    #[test]
    fn proc_and_cgroup_sources_are_bounded_and_numeric() {
        let temporary = tempdir().unwrap();
        let proc = temporary.path().join("proc");
        let cgroups = temporary.path().join("cgroup");
        std::fs::create_dir_all(proc.join("self")).unwrap();
        std::fs::create_dir_all(cgroups.join("scope")).unwrap();
        std::fs::write(proc.join("stat"), "cpu  10 2 3 5 1\n").unwrap();
        std::fs::write(
            proc.join("meminfo"),
            "MemTotal: 100 kB\nMemAvailable: 40 kB\nSwapTotal: 10 kB\nSwapFree: 3 kB\n",
        )
        .unwrap();
        std::fs::write(proc.join("loadavg"), "1.0 2.0 3.0 1/2 3\n").unwrap();
        std::fs::write(proc.join("self/cgroup"), "0::/scope\n").unwrap();
        let scope = cgroups.join("scope");
        std::fs::write(scope.join("cpu.stat"), "usage_usec 123\n").unwrap();
        std::fs::write(scope.join("memory.current"), "456\n").unwrap();
        std::fs::write(scope.join("memory.peak"), "500\n").unwrap();
        std::fs::write(scope.join("pids.current"), "7\n").unwrap();
        std::fs::write(scope.join("io.stat"), "8:0 rbytes=10 wbytes=20\n").unwrap();
        let source =
            HostMetricSource::with_roots(proc, cgroups, "/usr/bin/du".into(), Arc::new(NoDocker));
        assert_eq!(source.host_cpu_ticks(), (15, 21));
        assert_eq!(source.host_memory().used, 60 * 1024);
        assert_eq!(source.host_load(), (1.0, 2.0, 3.0));
        assert_eq!(
            source.cgroup_stats(source.own_cgroup().as_deref()).unwrap(),
            CgroupStats {
                cpu_usec: 123,
                memory_current: 456,
                memory_peak: 500,
                pids: 7,
                io_read_bytes: 10,
                io_write_bytes: 20,
            }
        );
        assert_eq!(parse_size("1.5MiB"), 1_572_864);
        assert_eq!(parse_size("2GB"), 2_000_000_000);
        let storage = parse_projected_docker_storage(
            "image=1.5MiB\nimage=500B\nbuild_cache=2GB\nvolume=managed-data=47.74MB\ninvalid\n",
        );
        assert_eq!(storage.images, 1_573_364);
        assert_eq!(storage.build_cache, 2_000_000_000);
        assert_eq!(storage.volumes["managed-data"], 47_740_000);
    }
}
