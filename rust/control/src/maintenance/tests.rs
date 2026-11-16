use super::*;
use crate::{
    automation_test_support::{Fixture, FixtureClock},
    control_plane::ControlPlane,
    daemon::{App, DaemonEndpoint},
};
use devcoordinator2_api::{ClientContext, ResponseEnvelope};
use std::sync::mpsc;

// This extends the existing real PostgreSQL/service fixture through the actual
// Unix transport: saturating maintenance must not delay a planning mutation or
// its read-back. No fake persistence or daemon executor is substituted.
struct OccupyMaintenance {
    database: Database,
    key: &'static str,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Job for OccupyMaintenance {
    fn name(&self) -> &'static str {
        self.key
    }
    fn run(&self, _: &mut Budget, _: Option<String>) -> JobResult {
        self.database
            .maintenance(|c| {
                if let Some(signal) = self.entered.lock().unwrap().take() {
                    let _ = signal.send(());
                }
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(15))
                    .expect("test must release maintenance");
                c.query_row("SELECT 1::bigint", [], |r| r.get::<_, i64>(0))?;
                Ok(())
            })
            .unwrap();
        JobResult::complete()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_exhaustion_keeps_real_plan_reads_and_mutations_responsive() {
    let fixture = Fixture::new();
    let config = fixture.config.clone();
    let database = fixture.database.clone();
    let plane = tokio::task::spawn_blocking(move || {
        ControlPlane::with_adapters(
            config,
            database,
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(FixtureClock),
        )
    })
    .await
    .unwrap()
    .unwrap();
    tokio::task::spawn_blocking({
        let plane = plane.clone();
        move || plane.health().sampler().tick()
    })
    .await
    .unwrap()
    .unwrap();
    let endpoint = DaemonEndpoint::bind(&fixture.config.socket_path)
        .await
        .unwrap();
    let app = Arc::new(App::with_executor(None, Arc::new(plane.clone())));
    let (stop, mut shutdown) = watch::channel(false);
    let server = tokio::spawn(async move { endpoint.serve(&mut shutdown, app).await });
    let scheduler = Scheduler::new(fixture.database.clone(), 2, 8).unwrap();
    let mut releases = Vec::new();
    let mut pending = Vec::new();
    for key in ["fixture-hold-a", "fixture-hold-b"] {
        let (entered, ready) = oneshot::channel();
        let (release, receiver) = mpsc::channel();
        let job = Arc::new(OccupyMaintenance {
            database: fixture.database.clone(),
            key,
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(receiver),
        });
        let scheduler = scheduler.clone();
        pending.push(tokio::spawn(async move {
            scheduler
                .submit_async(job, Budget::new(Duration::from_secs(15), 256, 65536), None)
                .await
        }));
        releases.push(release);
        tokio::time::timeout(Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
    }
    let mut samples = Vec::new();
    for index in 0..31 {
        let started = Instant::now();
        let (operation, params) = if index == 0 {
            (
                "task.update",
                serde_json::json!({"task_id":"spec","title":"Updated while maintenance waits"}),
            )
        } else {
            match index % 5 {
                0 => (
                    "decision.tail",
                    serde_json::json!({"repository_id":"project-alpha","n":10}),
                ),
                1 => (
                    "plan.overview",
                    serde_json::json!({"repository_id":"project-alpha","limit":10}),
                ),
                2 => ("task.history", serde_json::json!({"task_id":"spec"})),
                3 => ("health.summary", serde_json::json!({})),
                _ => ("test.admission.status", serde_json::json!({})),
            }
        };
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            crate::client::call(
                &fixture.config.socket_path,
                operation,
                params,
                ClientContext::default(),
            ),
        )
        .await;
        samples.push((started.elapsed(), response));
    }
    // Always release and join before asserting, preserving finite fixture work
    // even when the tested request boundary fails.
    for release in releases {
        let _ = release.send(());
    }
    for work in pending {
        work.await.unwrap().unwrap();
    }
    scheduler.shutdown_and_join();
    stop.send(true).unwrap();
    server.await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || drop(plane))
        .await
        .unwrap();
    let mut latency = samples
        .iter()
        .skip(1)
        .map(|(elapsed, _)| elapsed.as_micros())
        .collect::<Vec<_>>();
    latency.sort_unstable();
    eprintln!(
        "maintenance isolation: cheap_read_samples={} p95_us={} max_us={}",
        latency.len(),
        latency[(latency.len() * 95).div_ceil(100) - 1],
        latency.last().unwrap()
    );
    for (elapsed, response) in samples {
        let response = response
            .expect("maintenance must not cause a one-second request timeout")
            .unwrap();
        assert!(
            matches!(response, ResponseEnvelope::Success { .. }),
            "{response:?}"
        );
        assert!(elapsed < Duration::from_secs(1));
    }
    let title: String = fixture
        .database
        .read(|c| {
            Ok(
                c.query_row("SELECT title FROM tasks WHERE task_id='spec'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(title, "Updated while maintenance waits");
}

struct Resumable {
    observed: Arc<Mutex<Vec<Option<String>>>>,
    fail: AtomicBool,
}
impl Job for Resumable {
    fn name(&self) -> &'static str {
        "fixture-cursor"
    }
    fn run(&self, _: &mut Budget, cursor: Option<String>) -> JobResult {
        self.observed.lock().unwrap().push(cursor.clone());
        let status = if self.fail.swap(false, Ordering::SeqCst) {
            JobStatus::Retry {
                reason: "fixture_failure".into(),
            }
        } else if cursor.is_none() {
            JobStatus::Continue {
                cursor: "second-page".into(),
            }
        } else {
            JobStatus::Complete
        };
        JobResult {
            status,
            rows: 1,
            bytes: 32,
        }
    }
}

#[test]
fn restart_resumes_saved_cursor_and_retry_does_not_advance_it() {
    let fixture = Fixture::new();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let job = Arc::new(Resumable {
        observed: observed.clone(),
        fail: AtomicBool::new(false),
    });
    let budget = || Budget::new(Duration::from_secs(1), 256, 65536);
    let scheduler = Scheduler::new(fixture.database.clone(), 1, 2).unwrap();
    assert!(matches!(
        scheduler
            .submit(job.clone(), budget(), None)
            .unwrap()
            .status,
        JobStatus::Continue { .. }
    ));
    scheduler.shutdown_and_join();
    drop(scheduler);
    let scheduler = Scheduler::new(fixture.database.clone(), 1, 2).unwrap();
    fixture.database.transaction(|c| {
        c.execute("UPDATE coordinator_maintenance SET lease_until_ms=$1 WHERE job_key='fixture-cursor'",[now_ms()+60_000])?;
        Ok(())
    }).unwrap();
    assert!(matches!(
        scheduler
            .submit(job.clone(), budget(), None)
            .unwrap()
            .status,
        JobStatus::Deferred { .. }
    ));
    assert_eq!(
        observed.lock().unwrap().len(),
        1,
        "another live lease must never be reported as completed work"
    );
    fixture.database.transaction(|c|{c.execute("UPDATE coordinator_maintenance SET lease_until_ms=0,due_at_ms=0 WHERE job_key='fixture-cursor'",[])?;Ok(())}).unwrap();
    job.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        scheduler
            .submit(job.clone(), budget(), None)
            .unwrap()
            .status,
        JobStatus::Retry { .. }
    ));
    assert!(
        matches!(
            scheduler
                .submit(job.clone(), budget(), None)
                .unwrap()
                .status,
            JobStatus::Deferred { .. }
        ),
        "explicit jobs must honor persisted retry backoff"
    );
    fixture
        .database
        .transaction(|c| {
            c.execute(
                "UPDATE coordinator_maintenance SET due_at_ms=0 WHERE job_key='fixture-cursor'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        scheduler
            .submit(job.clone(), budget(), None)
            .unwrap()
            .status,
        JobStatus::Complete
    );
    scheduler.shutdown_and_join();
    assert_eq!(
        *observed.lock().unwrap(),
        vec![None, Some("second-page".into()), Some("second-page".into())]
    );
    let remaining: u64 = fixture
        .database
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM coordinator_maintenance WHERE job_key='fixture-cursor'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        remaining, 0,
        "completed explicit jobs must not grow the scheduler state indefinitely"
    );

    // Restart recovery must use a worker promptly even when the previous
    // periodic observation scheduled its next visit a minute in the future.
    fixture.database.transaction(|c| {
        c.execute("INSERT INTO coordinator_maintenance(job_key,due_at_ms,cursor) VALUES('fixture-cursor',$1,'\"second-page\"'::jsonb)", [now_ms()+60_000])?;
        Ok(())
    }).unwrap();
    let scheduler = Scheduler::new(fixture.database.clone(), 1, 2).unwrap();
    let mut completed = scheduler.retry_events();
    let (stop, receiver) = watch::channel(false);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let worker = scheduler.clone();
        let service = tokio::spawn(async move {
            worker
                .serve(
                    vec![PeriodicJob {
                        job,
                        run_on_start: true,
                        interval: Duration::from_secs(60),
                        budget: Duration::from_secs(1),
                        max_rows: 256,
                        max_bytes: 65536,
                    }],
                    receiver,
                )
                .await;
        });
        let result = tokio::time::timeout(Duration::from_secs(2), completed.changed()).await;
        stop.send(true).unwrap();
        service.await.unwrap();
        result
            .expect("restart-sensitive job waited for the old periodic deadline")
            .unwrap();
    });
    scheduler.shutdown_and_join();
    assert_eq!(
        observed.lock().unwrap().last(),
        Some(&Some("second-page".into()))
    );
    assert_eq!(observed.lock().unwrap().len(), 4);
}
