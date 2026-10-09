//! Test-only use of the existing installation admission boundary.
//! This does not protect orphan fixtures after SIGKILL: schema-1 stale leases
//! retain their existing recovery semantics. Never report that as safe cleanup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devcoordinator2_control::test_admission::{
    DrainLease, begin_drain, end_drain, read_activity, snapshot, wait_for_zero_activity,
};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Clone, Debug, Serialize)]
pub(super) struct Receipt {
    pub acquired_at: String,
    pub drained_at: Option<String>,
    pub released_at: Option<String>,
    pub state: &'static str,
    pub interrupted: bool,
    pub error: Option<String>,
    pub crash_protection: &'static str,
}

pub(super) struct Guard {
    runtime_dir: PathBuf,
    lease: Option<DrainLease>,
    runtime: tokio::runtime::Runtime,
    signal_task: tokio::task::JoinHandle<()>,
    cancellation: tokio::sync::watch::Receiver<bool>,
    recovery: tokio::sync::watch::Receiver<u64>,
    _recovery_sender: tokio::sync::watch::Sender<u64>,
    cleanup_unverified: bool,
    pub receipt: Receipt,
}

pub(super) fn parse_deadline(value: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| "admission deadline must be an RFC3339 timestamp".into())
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamp is representable")
}

impl Guard {
    pub fn begin(runtime_dir: &Path) -> Result<Self, String> {
        if !runtime_dir.is_absolute() {
            return Err("host admission runtime must be absolute".into());
        }
        // The existing nofollow reader must find a real receipt. Do not let
        // begin_drain create an unrelated runtime or mistake unknown for idle.
        read_activity(runtime_dir)
            .map_err(|error| error.to_string())?
            .ok_or("host test activity receipt is unavailable")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("root-admission")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let (sender, cancellation) = tokio::sync::watch::channel(false);
        let (recovery_sender, recovery) = tokio::sync::watch::channel(0_u64);
        let signal_task = {
            let _entered = runtime.enter();
            let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
            let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
            let mut recover = signal(SignalKind::user_defined1()).map_err(|e| e.to_string())?;
            let recovery_sender = recovery_sender.clone();
            runtime.spawn(async move {
                loop {
                    tokio::select! {
                        _ = terminate.recv() => { let _ = sender.send(true); },
                        _ = interrupt.recv() => { let _ = sender.send(true); },
                        _ = recover.recv() => { recovery_sender.send_modify(|value| *value = value.wrapping_add(1)); },
                    }
                }
            })
        };
        let lease = begin_drain(runtime_dir, "isolated Coordinator root acceptance")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            runtime_dir: runtime_dir.to_owned(),
            lease: Some(lease),
            runtime,
            signal_task,
            cancellation,
            recovery,
            _recovery_sender: recovery_sender,
            cleanup_unverified: false,
            receipt: Receipt {
                acquired_at: now(),
                drained_at: None,
                released_at: None,
                state: "draining",
                interrupted: false,
                error: None,
                crash_protection: "unverified-stale-owner-gap",
            },
        })
    }

    pub fn interrupted(&self) -> bool {
        *self.cancellation.borrow()
    }

    pub fn wait(&mut self, deadline: OffsetDateTime) -> Result<(), String> {
        let remaining = deadline - OffsetDateTime::now_utc();
        let duration = Duration::try_from(remaining)
            .map_err(|_| "host admission deadline reached before fixture work")?;
        let mut cancelled = self.cancellation.clone();
        let result = self.runtime.block_on(async {
            tokio::select! {
                biased;
                _ = async {
                    while !*cancelled.borrow_and_update() {
                        if cancelled.changed().await.is_err() { break; }
                    }
                } => Err("host admission cancelled before fixture work".to_owned()),
                result = tokio::time::timeout(duration, wait_for_zero_activity(&self.runtime_dir)) => {
                    match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(_) => Err("host admission deadline reached before fixture work".into()),
                    }
                }
            }
        });
        if result.is_ok() {
            self.receipt.drained_at = Some(now());
            self.receipt.state = "held";
        } else {
            self.receipt.error = result.as_ref().err().cloned();
        }
        result
    }

    pub fn finish(&mut self) -> Result<(), String> {
        self.receipt.interrupted = self.interrupted();
        if self.cleanup_unverified {
            return Err("owned runtime cleanup is unverified; admission lease remains held".into());
        }
        if let Some(lease) = self.lease.as_ref() {
            if let Err(error) = end_drain(lease) {
                self.receipt.state = "release-failed";
                self.receipt.error = Some(error.to_string());
                return Err(error.to_string());
            }
            self.lease = None;
            self.receipt.released_at = Some(now());
        }
        // end_drain is nonce-conditional. Its Ok does not prove admission is
        // open: another owner may already hold the same existing boundary.
        let observation = (|| {
            read_activity(&self.runtime_dir)
                .map_err(|error| error.to_string())?
                .ok_or("host test activity receipt is unavailable after release")?;
            snapshot(&self.runtime_dir).map_err(|error| error.to_string())
        })();
        let observed = match observation {
            Ok(observed) => observed,
            Err(error) => {
                self.receipt.state = "release-observation-failed";
                self.receipt.error = Some(error.clone());
                return Err(error);
            }
        };
        self.receipt.state = if observed.draining {
            "foreign-drain"
        } else {
            "reopened"
        };
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Explicit finish, after owned cleanup, supplies the result. This is
        // only a best-effort fallback for preparation errors, never a receipt.
        if let Some(lease) = self.lease.take().filter(|_| !self.cleanup_unverified) {
            let _ = end_drain(&lease);
        }
        self.signal_task.abort();
    }
}

/// The same harness retains its real lease while a known owned actor survives.
/// A signal requests another exact cleanup attempt; only independent resource
/// observation permits release. No new admission owner or scheduler is created.
pub(super) fn recover_owned_runtime<T>(
    guard: &mut Guard,
    owner: &mut T,
    mut cleanup: impl FnMut(&mut T) -> Result<(), String>,
    mut quiescent: impl FnMut(&mut T) -> Result<(), String>,
    mut publish_hold: impl FnMut(&Receipt) -> Result<(), String>,
) {
    if quiescent(owner).is_ok() {
        return;
    }
    let _ = cleanup(owner); // one ordinary bounded retry before recovery hold
    loop {
        match quiescent(owner) {
            Ok(()) => {
                guard.cleanup_unverified = false;
                guard.receipt.state = "held-after-cleanup-recovery";
                return;
            }
            Err(error) => {
                guard.cleanup_unverified = true;
                guard.receipt.state = "cleanup-held";
                guard.receipt.error = Some(error);
                // Register the next version before publishing the hold so a
                // recovery signal between report and wait cannot be missed.
                guard.recovery.borrow_and_update();
                if publish_hold(&guard.receipt).is_err() {
                    eprintln!(
                        "root acceptance cleanup is held; recovery receipt could not be written"
                    );
                }
                guard.runtime.block_on(async {
                    // The guard retains a sender; the channel cannot close
                    // while the live admission owner is holding this wait.
                    let _ = guard.recovery.changed().await;
                });
                let _ = cleanup(owner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_control::test_admission::{DRAIN_FILE, TestAdmission};
    use serde_json::{Value, json};
    use std::fs;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    fn wait_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture event not received; terminal evidence: {}",
                fs::read_to_string(path.parent().unwrap().join("result.json")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn child(directory: &Path, mode: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_admission::tests::process_fixture",
                "--nocapture",
            ])
            .env("DC2_HOST_ADMISSION_FIXTURE", directory)
            .env("DC2_HOST_ADMISSION_MODE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn finish_child(child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "fixture observer failed");
                return;
            }
            assert!(Instant::now() < deadline, "fixture child did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct FixtureRuntime {
        process: Option<Child>,
        artifact: PathBuf,
    }
    impl Drop for FixtureRuntime {
        fn drop(&mut self) {
            if let Some(child) = self.process.as_mut() {
                let _ = super::super::stop_owned_child(child);
            }
        }
    }

    fn fixture_cleanup(
        owner: &mut FixtureRuntime,
        directory: &Path,
        artifact_only: bool,
    ) -> Result<(), String> {
        if !artifact_only && !directory.join("allow-cleanup").exists() {
            return Err("injected stop failure: the owned process is still alive".into());
        }
        if let Some(child) = owner.process.as_mut() {
            super::super::stop_owned_child(child)?;
            owner.process = None;
        }
        if artifact_only {
            return Err("injected artifact removal failure after actual process stop".into());
        }
        fs::remove_file(&owner.artifact).map_err(|e| e.to_string())
    }

    fn fixture_quiescent(owner: &mut FixtureRuntime) -> Result<(), String> {
        if let Some(child) = owner.process.as_mut() {
            if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                return Err("actual owned fixture process is still alive".into());
            }
            owner.process = None;
        }
        Ok(())
    }

    #[test]
    fn process_fixture() {
        let Some(directory) = std::env::var_os("DC2_HOST_ADMISSION_FIXTURE") else {
            return;
        };
        let directory = PathBuf::from(directory);
        let mode = std::env::var("DC2_HOST_ADMISSION_MODE").unwrap();
        let mut guard = Guard::begin(&directory.join("runtime")).unwrap();
        fs::write(directory.join("acquired"), b"owned").unwrap();
        let deadline = OffsetDateTime::now_utc()
            + time::Duration::milliseconds(if mode == "deadline" { 200 } else { 5_000 });
        let waited = guard.wait(deadline);
        let mut retained_runtime = None;
        let result = if waited.is_ok() && ["cleanup-live", "artifact-only"].contains(&mode.as_str())
        {
            let mut owner = FixtureRuntime {
                process: Some(Command::new("/bin/sleep").arg("60").spawn().unwrap()),
                artifact: directory.join("resource"),
            };
            fs::write(&owner.artifact, b"owned disposable fixture").unwrap();
            fs::write(directory.join("body"), b"started").unwrap();
            let result = super::super::run_owned_case(
                &mut owner,
                |_| {
                    wait_file(&directory.join("finish"));
                    Ok(())
                },
                |owner, _| fixture_cleanup(owner, &directory, mode == "artifact-only"),
            );
            assert!(result.cleanup_failed && result.result.is_err());
            let mut holds = 0;
            // Explicit test-only mutation reproduces the reviewed old caller
            // omission. The parent fixture must reject its premature release.
            if std::env::var_os("DC2_HOST_ADMISSION_REPRO_OLD_RELEASE").is_none() {
                recover_owned_runtime(
                    &mut guard,
                    &mut owner,
                    |owner| fixture_cleanup(owner, &directory, mode == "artifact-only"),
                    fixture_quiescent,
                    |receipt| {
                        holds += 1;
                        fs::write(
                            directory.join(format!("hold-{holds}")),
                            serde_json::to_vec(receipt).unwrap(),
                        )
                        .map_err(|e| e.to_string())
                    },
                );
            }
            fs::write(
                directory.join("quiescent"),
                if owner.process.is_none() {
                    b"true".as_slice()
                } else {
                    b"false".as_slice()
                },
            )
            .unwrap();
            retained_runtime = Some(owner);
            result.result
        } else if waited.is_ok() {
            let mut resource = directory.join("resource");
            fs::write(&resource, b"owned disposable fixture").unwrap();
            fs::write(directory.join("body"), b"started").unwrap();
            super::super::run_owned_case(
                &mut resource,
                |_| {
                    if mode == "signal-body" {
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !guard.interrupted() {
                            assert!(Instant::now() < deadline, "signal was not delivered");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        fs::write(directory.join("cancel-observed"), b"observed").unwrap();
                    }
                    wait_file(&directory.join("finish"));
                    if mode == "error" {
                        return Err("controlled fixture failure".into());
                    }
                    assert_ne!(mode, "panic", "controlled fixture panic");
                    Ok(())
                },
                |resource, _| fs::remove_file(resource).map_err(|e| e.to_string()),
            )
            .result
        } else {
            waited
        };
        let released = guard.finish();
        let actor_alive_at_release = retained_runtime
            .as_mut()
            .and_then(|owner| owner.process.as_mut())
            .is_some_and(|child| child.try_wait().unwrap().is_none());
        fs::write(
            directory.join("result.json"),
            serde_json::to_vec(&json!({
                "result_ok": result.is_ok(), "release_ok": released.is_ok(),
                "receipt": guard.receipt, "resource_exists": directory.join("resource").exists(),
                "actor_alive_at_release": actor_alive_at_release,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn real_process_admission_preserves_work_cancellation_cleanup_and_crash_limit() {
        // Missing, malformed and foreign boundaries never become a new idle
        // host. No candidate daemon or network resource is needed to prove it.
        let invalid = tempfile::tempdir().unwrap();
        let missing = invalid.path().join("missing");
        assert!(Guard::begin(&missing).is_err());
        assert!(!missing.exists());
        let boundary = invalid.path().join("boundary");
        let admission = TestAdmission::new(&boundary).unwrap();
        admission.reset().unwrap();
        let activity_path = boundary.join(devcoordinator2_control::test_admission::ACTIVITY_FILE);
        let original_activity = fs::read(&activity_path).unwrap();
        fs::write(&activity_path, b"not an activity receipt").unwrap();
        assert!(Guard::begin(&boundary).is_err());
        assert!(!boundary.join(DRAIN_FILE).exists());
        fs::write(&activity_path, &original_activity).unwrap();
        let linked = invalid.path().join("linked");
        std::os::unix::fs::symlink(&boundary, &linked).unwrap();
        assert!(Guard::begin(&linked).is_err());
        let foreign = begin_drain(&boundary, "existing owner").unwrap();
        let foreign_bytes = fs::read(boundary.join(DRAIN_FILE)).unwrap();
        assert!(Guard::begin(&boundary).is_err());
        assert_eq!(fs::read(boundary.join(DRAIN_FILE)).unwrap(), foreign_bytes);
        end_drain(&foreign).unwrap();
        let mut guard = Guard::begin(&boundary).unwrap();
        fs::write(&activity_path, b"broken after release").unwrap();
        assert!(guard.finish().is_err());
        assert_eq!(guard.receipt.state, "release-observation-failed");
        fs::write(&activity_path, &original_activity).unwrap();
        guard.finish().unwrap();
        assert_eq!(guard.receipt.state, "reopened");
        drop(guard);
        let lock = boundary.join(devcoordinator2_control::test_admission::LOCK_FILE);
        fs::remove_file(&lock).unwrap();
        let unrelated = invalid.path().join("unrelated");
        fs::write(&unrelated, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&unrelated, &lock).unwrap();
        assert!(Guard::begin(&boundary).is_err());
        assert_eq!(fs::read(&unrelated).unwrap(), b"unchanged");
        for mode in [
            "normal",
            "error",
            "panic",
            "signal-body",
            "signal-wait",
            "deadline",
            "foreign",
            "crash",
            "cleanup-live",
            "artifact-only",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let runtime = directory.path().join("runtime");
            let admission = TestAdmission::new(&runtime).unwrap();
            admission.reset().unwrap();
            admission.started("accepted", "existing-unit").unwrap();
            let mut process = child(directory.path(), mode);
            wait_file(&directory.path().join("acquired"));
            assert!(snapshot(&runtime).unwrap().draining);
            assert!(admission.start_guard().is_err());
            assert!(!directory.path().join("body").exists());
            assert_eq!(read_activity(&runtime).unwrap().unwrap().active.len(), 1);
            if mode == "signal-wait" {
                assert_eq!(unsafe { libc::kill(process.id() as i32, libc::SIGTERM) }, 0);
            } else if mode != "deadline" {
                admission.finished("accepted").unwrap();
                wait_file(&directory.path().join("body"));
            }
            if mode == "signal-body" {
                assert_eq!(unsafe { libc::kill(process.id() as i32, libc::SIGINT) }, 0);
                // The signal must leave the current body and resource alive
                // until its ordinary bounded completion/cleanup boundary.
                wait_file(&directory.path().join("cancel-observed"));
                assert!(process.try_wait().unwrap().is_none());
                assert!(directory.path().join("resource").exists());
            }
            if mode == "crash" {
                process.kill().unwrap();
                assert!(!process.wait().unwrap().success());
                assert!(directory.path().join("resource").exists());
                assert!(!snapshot(&runtime).unwrap().lease_live);
                drop(admission.start_guard().unwrap());
                assert!(!runtime.join(DRAIN_FILE).exists());
                assert!(!directory.path().join("result.json").exists());
                // This is a must-catch demonstration of the inherited gap,
                // not a successful root receipt or a crash-protection claim.
                fs::remove_file(directory.path().join("resource")).unwrap();
                continue;
            }
            let foreign = if mode == "foreign" {
                let original: Value =
                    serde_json::from_slice(&fs::read(runtime.join(DRAIN_FILE)).unwrap()).unwrap();
                end_drain(&DrainLease {
                    runtime_dir: runtime.clone(),
                    nonce: original["nonce"].as_str().unwrap().into(),
                })
                .unwrap();
                Some(begin_drain(&runtime, "concurrent owner").unwrap())
            } else {
                None
            };
            let foreign_bytes = foreign
                .as_ref()
                .map(|_| fs::read(runtime.join(DRAIN_FILE)).unwrap());
            fs::write(directory.path().join("finish"), b"complete").unwrap();
            if mode == "cleanup-live" {
                wait_file(&directory.path().join("hold-1"));
                let lease = fs::read(runtime.join(DRAIN_FILE)).unwrap();
                assert!(admission.start_guard().is_err());
                assert_eq!(unsafe { libc::kill(process.id() as i32, libc::SIGTERM) }, 0);
                assert_eq!(unsafe { libc::kill(process.id() as i32, libc::SIGUSR1) }, 0);
                wait_file(&directory.path().join("hold-2"));
                assert!(process.try_wait().unwrap().is_none());
                assert!(!directory.path().join("quiescent").exists());
                assert_eq!(fs::read(runtime.join(DRAIN_FILE)).unwrap(), lease);
                assert!(admission.start_guard().is_err());
                fs::write(directory.path().join("allow-cleanup"), b"repaired").unwrap();
                assert_eq!(unsafe { libc::kill(process.id() as i32, libc::SIGUSR1) }, 0);
            }
            finish_child(&mut process);
            let result: Value =
                serde_json::from_slice(&fs::read(directory.path().join("result.json")).unwrap())
                    .unwrap();
            assert_eq!(result["release_ok"], true, "{mode}");
            assert_eq!(result["actor_alive_at_release"], false, "{mode}");
            assert_eq!(result["resource_exists"], mode == "artifact-only", "{mode}");
            assert_eq!(
                result["result_ok"],
                ![
                    "error",
                    "panic",
                    "signal-wait",
                    "deadline",
                    "cleanup-live",
                    "artifact-only"
                ]
                .contains(&mode),
                "{mode}"
            );
            assert_eq!(
                result["receipt"]["interrupted"],
                mode.starts_with("signal-") || mode == "cleanup-live",
                "{mode}"
            );
            assert_eq!(
                result["receipt"]["crash_protection"],
                "unverified-stale-owner-gap"
            );
            if let Some(lease) = foreign {
                assert_eq!(result["receipt"]["state"], "foreign-drain");
                assert_eq!(
                    fs::read(runtime.join(DRAIN_FILE)).unwrap(),
                    foreign_bytes.unwrap()
                );
                assert!(admission.start_guard().is_err());
                end_drain(&lease).unwrap();
            } else {
                assert_eq!(result["receipt"]["state"], "reopened");
            }
            drop(admission.start_guard().unwrap());
            if ["cleanup-live", "artifact-only"].contains(&mode) {
                assert_eq!(
                    fs::read(directory.path().join("quiescent")).unwrap(),
                    b"true"
                );
            }
            if mode == "artifact-only" {
                assert!(!directory.path().join("hold-1").exists());
                fs::remove_file(directory.path().join("resource")).unwrap();
            }
            if ["signal-wait", "deadline"].contains(&mode) {
                assert!(!directory.path().join("body").exists());
                assert_eq!(read_activity(&runtime).unwrap().unwrap().active.len(), 1);
                admission.finished("accepted").unwrap();
            }
        }
    }
}
