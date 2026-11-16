//! Production socket and control-plane regressions over an isolated real
//! PostgreSQL authority. Run state is fixture-owned; actual-unit acceptance
//! remains the responsibility of the root acceptance suite.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use devcoordinator2_api::{ClientContext, ErrorCode, ResponseEnvelope};
use devcoordinator2_executor_protocol::{ProofKind, ValidationTier};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::automation_test_support::{Fixture, FixtureClock};
use crate::control_plane::ControlPlane;
use crate::daemon::{App, DaemonEndpoint};
use crate::database::{Database, Lane};
use crate::test_state::{TestRunStore, initial_summary};

const RUN: &str = "t20260904T000000Z-aabbcc";
const RESPONSE_BOUND: Duration = Duration::from_secs(1);

struct World {
    fixture: Fixture,
    plane: ControlPlane,
}

impl World {
    fn new() -> Self {
        let mut fixture = Fixture::new();
        // The automation fixture's synthetic project remains intact. A real
        // Git registration needs its own worktree and canonical identity.
        fixture.repository = fixture._temporary.path().join("observed-repository");
        std::fs::create_dir(&fixture.repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&fixture.repository)
                .status()
                .unwrap()
                .success()
        );
        let plane = ControlPlane::with_adapters(
            fixture.config.clone(),
            fixture.database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(FixtureClock),
        )
        .unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        plane
            .registry()
            .register(&fixture.repository, uid, gid)
            .unwrap();
        let mut prepared = TestRunStore
            .prepare(&fixture.repository, RUN, uid, gid)
            .unwrap();
        let summary = initial_summary(
            RUN,
            "all",
            "2026-09-04T00:00:00Z",
            uid,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        TestRunStore
            .write_summary(&prepared.current, &summary, uid, gid)
            .unwrap();
        TestRunStore
            .prepare_log_metadata(&fixture.repository, &summary, uid, gid)
            .unwrap();
        prepared
            .executor_stdout
            .write_all(b"fixture run is still active\n")
            .unwrap();
        plane.health().sampler().tick().unwrap();
        Self { fixture, plane }
    }

    fn query(&self, operation: &str) -> Value {
        match operation {
            "health.summary" | "ping" => json!({}),
            "test.log.tail" => json!({
                "path":self.fixture.repository,"run_id":RUN,
                "phase":"executor","stream":"stdout","lines":1
            }),
            "test.wait" => json!({
                "path":self.fixture.repository,"run_id":RUN,
                "deadline_at": (time::OffsetDateTime::now_utc() + time::Duration::milliseconds(150))
                    .format(&time::format_description::well_known::Rfc3339).unwrap()
            }),
            "test.log.catalog" => json!({"path":self.fixture.repository,"run_id":RUN}),
            "test.status" => json!({"path":self.fixture.repository}),
            "task.create" => json!({
                "repository_id":crate::ids::repository_id(&self.fixture.repository).unwrap(),
                "title":"Retain the accepted responsiveness fixture mutation",
                "kind":"improvement"
            }),
            _ => panic!("unexpected regression operation"),
        }
    }
}

/// Hold only the fixture's existing connections. This reproduces contention
/// from in-flight writes or maintenance without another worker pool.
pub(crate) struct HeldLane {
    release: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HeldLane {
    pub(crate) fn new(database: Database, lane: Lane) -> Self {
        let (ready, receiver) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let held = (0..database.persistence().pool(lane).max_size())
                .map(|_| database.persistence().acquire(lane).unwrap())
                .collect::<Vec<_>>();
            ready.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(10));
            drop(held);
        });
        receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            release: Some(release),
            worker: Some(worker),
        }
    }

    fn locked_transaction(database: Database, query: &'static str, key: String) -> Self {
        let (ready, receiver) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let connection = database.persistence().acquire(Lane::Write).unwrap();
            let transaction = connection.unchecked_transaction().unwrap();
            transaction.query_row(query, [key], |_| Ok(())).unwrap();
            ready.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(10));
            transaction.rollback().unwrap();
        });
        receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            release: Some(release),
            worker: Some(worker),
        }
    }
}

impl Drop for HeldLane {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn observe(
    world: &World,
    holds: Vec<HeldLane>,
    operations: &[&str],
    client: ClientContext,
) -> Vec<(String, Result<ResponseEnvelope, String>)> {
    observe_with_executor(
        world,
        holds,
        operations,
        client,
        Arc::new(world.plane.clone()),
    )
}

fn observe_with_executor(
    world: &World,
    holds: Vec<HeldLane>,
    operations: &[&str],
    client: ClientContext,
    executor: Arc<dyn crate::daemon::OperationExecutor>,
) -> Vec<(String, Result<ResponseEnvelope, String>)> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let socket = &world.fixture.config.socket_path;
        let endpoint = DaemonEndpoint::bind(socket).await.unwrap();
        let app = Arc::new(App::with_executor(None, executor));
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, app).await });
        let mut queries = tokio::task::JoinSet::new();
        for &operation in operations {
            let params = world.query(operation);
            let socket = socket.clone();
            let client = client.clone();
            let operation = operation.to_owned();
            queries.spawn(async move {
                let began = Instant::now();
                let response = tokio::time::timeout(
                    RESPONSE_BOUND,
                    crate::client::call(&socket, &operation, params, client),
                )
                .await
                .map_err(|_| "caller response bound exceeded".to_owned())
                .and_then(|result| result.map_err(|error| error.code.to_string()));
                eprintln!(
                    "{operation}: {} ms, {response:?}",
                    began.elapsed().as_millis()
                );
                (operation, response)
            });
        }
        let mut observations = Vec::new();
        while let Some(result) = queries.join_next().await {
            observations.push(result.unwrap());
        }
        // Release contention before assertions so every finite accepted
        // request can finish and the server can drain after a failed check.
        tokio::task::spawn_blocking(move || drop(holds))
            .await
            .unwrap();
        let recovered = crate::client::call(
            socket,
            "test.status",
            world.query("test.status"),
            ClientContext::default(),
        )
        .await
        .unwrap();
        assert!(matches!(recovered, ResponseEnvelope::Success { data, .. }
            if data["run_id"] == RUN && data["status"] == "running"));
        shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        observations
    })
}

#[test]
fn observation_reads_do_not_queue_behind_actions_or_maintenance() {
    let world = World::new();
    let holds = vec![
        HeldLane::new(world.fixture.database.clone(), Lane::Write),
        HeldLane::new(world.fixture.database.clone(), Lane::Maintenance),
    ];
    let observations = observe(
        &world,
        holds,
        &[
            "test.status",
            "test.log.catalog",
            "test.log.tail",
            "health.summary",
            "test.wait",
        ],
        ClientContext::default(),
    );
    let failures = observations
        .into_iter()
        .filter(
            |(operation, response)| match (operation.as_str(), response) {
                ("test.wait", Ok(ResponseEnvelope::Failure { error, .. })) => {
                    error.code != ErrorCode::WaitDeadlineReached
                        || error
                            .recovery
                            .as_ref()
                            .and_then(|recovery| recovery.run_id.as_deref())
                            != Some(RUN)
                }
                (_, Ok(ResponseEnvelope::Success { .. })) => false,
                _ => true,
            },
        )
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "blocked read surfaces: {failures:?}");
}

#[test]
fn test_wait_deadline_includes_slow_status_preparation() {
    let world = World::new();
    let holds = vec![HeldLane::new(world.fixture.database.clone(), Lane::Read)];
    let observations = observe(
        &world,
        holds,
        &["test.wait", "ping"],
        ClientContext::default(),
    );
    assert!(
        observations.iter().all(|(operation, response)| {
            match (operation.as_str(), response) {
                ("test.wait", Ok(ResponseEnvelope::Failure { error, .. })) => {
                    error.code == ErrorCode::WaitDeadlineReached
                        && error
                            .recovery
                            .as_ref()
                            .and_then(|recovery| recovery.run_id.as_deref())
                            == Some(RUN)
                }
                ("ping", Ok(ResponseEnvelope::Success { .. })) => true,
                _ => false,
            }
        }),
        "bounded wait responses: {observations:?}"
    );
}

#[test]
fn direct_test_wait_keeps_its_deadline_with_scheduler_heartbeats() {
    use crate::daemon::OperationExecutor;
    let world = World::new();
    let caller = crate::access::Caller::from_client(
        std::process::id(),
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
        ClientContext::default(),
        None,
    )
    .unwrap();
    let wait = crate::daemon::WaitContext::new(Arc::new(tokio::sync::Semaphore::new(2)), None);
    let deferred = world
        .plane
        .defer("test.wait", world.query("test.wait"), &caller, wait)
        .unwrap()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let error = tokio::time::timeout(RESPONSE_BOUND, deferred)
            .await
            .expect("the state observer must retain its own logical deadline")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::WaitDeadlineReached);
        assert_eq!(error.recovery.unwrap().run_id.as_deref(), Some(RUN));
    });
}

#[test]
fn test_wait_deadline_includes_observation_after_preparation() {
    use crate::access::Caller;
    use crate::daemon::{DeferredOperation, OperationExecutor};
    use devcoordinator2_api::ProtocolError;

    struct SlowObservation {
        plane: ControlPlane,
        database: Database,
    }
    impl OperationExecutor for SlowObservation {
        fn execute(
            &self,
            operation: &str,
            params: Value,
            caller: &Caller,
        ) -> Result<Value, ProtocolError> {
            self.plane.execute(operation, params, caller)
        }

        fn defer(
            &self,
            operation: &str,
            params: Value,
            caller: &Caller,
            context: crate::daemon::WaitContext,
        ) -> Option<Result<DeferredOperation, ProtocolError>> {
            self.plane
                .defer(operation, params, caller, context)
                .map(|prepared| {
                    prepared.map(|wait| {
                        // Preparation used the production status and repository
                        // path successfully. Its later subscription/status work
                        // encounters genuine read-pool contention.
                        let held = HeldLane::new(self.database.clone(), Lane::Read);
                        Box::pin(async move {
                            let result = wait.await;
                            drop(held);
                            result
                        }) as DeferredOperation
                    })
                })
        }
    }
    let world = World::new();
    let observations = observe_with_executor(
        &world,
        vec![],
        &["test.wait", "ping"],
        ClientContext::default(),
        Arc::new(SlowObservation {
            plane: world.plane.clone(),
            database: world.fixture.database.clone(),
        }),
    );
    assert!(
        observations.iter().all(|(operation, response)| {
            match (operation.as_str(), response) {
                ("test.wait", Ok(ResponseEnvelope::Failure { error, .. })) => {
                    error.code == ErrorCode::WaitDeadlineReached
                }
                ("ping", Ok(ResponseEnvelope::Success { .. })) => true,
                _ => false,
            }
        }),
        "observation escaped the original wait deadline: {observations:?}"
    );
}

#[test]
fn busy_writers_preserve_durable_declarations_and_report_uncaptured_context() {
    use crate::automation_test_support::{START, WEEK};
    use crate::daemon::OperationExecutor;
    use crate::usage_api_fixture::{Peer, response};
    use devcoordinator2_api::work_context::WorkContext;

    let mut world = World::new();
    let root = world.fixture._temporary.path();
    let peer = Peer::start(root, &root.join("usage-source"), |params| {
        response(params, START + WEEK, 135)
    });
    world.fixture.config.codex_usage_sources = vec![peer.source.clone()];
    world.plane = ControlPlane::with_adapters(
        world.fixture.config.clone(),
        world.fixture.database.clone(),
        Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
        Arc::new(crate::platform::FixedClock(
            time::OffsetDateTime::from_unix_timestamp_nanos(
                i128::from(START + WEEK - 1) * 1_000_000,
            )
            .unwrap(),
        )),
    )
    .unwrap();
    let uid = rustix::process::getuid().as_raw();
    let repository = crate::ids::repository_id(&world.fixture.repository).unwrap();
    let caller = crate::access::Caller::from_client(
        std::process::id(),
        uid,
        rustix::process::getgid().as_raw(),
        ClientContext::default(),
        None,
    )
    .unwrap();
    world
        .plane
        .execute(
            "review.policy.set",
            json!({
                "repository_id":repository,"workstream_id":"response-scope","active":true
            }),
            &caller,
        )
        .unwrap();
    let context = |operation: &str| ClientContext {
        work: Some(WorkContext {
            version: 1,
            alarm: Some(devcoordinator2_api::review_policy::AlarmCapability {
                alarm_namespace: "response-alarm".into(),
                capability_revision: 1,
                lease_expires_at: START + WEEK + 60_000,
            }),
            native_project_id: "response-project".into(),
            thread_id: "response-thread".into(),
            turn_id: None,
            operation_id: Some(operation.into()),
            workstream_id: Some("response-scope".into()),
            outcome_id: None,
            experiment_ref: None,
        }),
        ..ClientContext::default()
    };
    let initial = observe(&world, vec![], &["test.status"], context("already-durable"));
    assert!(
        initial
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. })))
    );
    let repeated = observe(
        &world,
        vec![HeldLane::new(world.fixture.database.clone(), Lane::Write)],
        &["test.status", "test.log.tail"],
        context("already-durable"),
    );
    assert!(
        repeated
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. })))
    );
    assert!(!world.plane.usage().binding_capture_failed(uid, &repository));

    let dictionary = observe(
        &world,
        vec![HeldLane::locked_transaction(
            world.fixture.database.clone(),
            "SELECT c.context_seq FROM codex_usage_contexts c JOIN codex_usage_operation_contexts b USING(context_seq) WHERE b.operation_id=$1 FOR NO KEY UPDATE OF c",
            "already-durable".into(),
        )],
        &["test.status"],
        context("same-context-new-operation"),
    );
    assert!(
        dictionary
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. })))
    );
    assert!(
        !world.plane.usage().binding_capture_failed(uid, &repository),
        "an existing context dictionary row must not be needlessly updated"
    );
    let alarm = observe(
        &world,
        vec![HeldLane::locked_transaction(
            world.fixture.database.clone(),
            "SELECT repository_id FROM review_policies WHERE repository_id=$1 FOR UPDATE",
            repository.clone(),
        )],
        &["test.status"],
        context("already-durable"),
    );
    assert!(
        alarm
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. }))),
        "an optional alarm refresh row lock held the useful read: {alarm:?}"
    );

    let fresh = observe(
        &world,
        vec![HeldLane::new(world.fixture.database.clone(), Lane::Write)],
        &["test.status", "test.log.tail", "task.create"],
        context("fresh-during-contention"),
    );
    assert!(
        fresh.iter().all(|(operation, result)| {
            if operation == "task.create" {
                // A domain mutation still needs its writer. Disconnecting its
                // observer must not cancel the already accepted operation.
                result.is_err()
            } else {
                matches!(result, Ok(ResponseEnvelope::Success { .. }))
            }
        }),
        "fresh declarations blocked useful reads: {fresh:?}"
    );
    assert!(world.plane.usage().binding_capture_failed(uid, &repository));
    let (declarations, mutations) = world.fixture.database.read(|connection| {
        Ok((
            connection.query_row("SELECT count(*) FROM codex_usage_operation_contexts WHERE source_uid=$1 AND repository_id=$2", crate::database::params![uid,repository], |row| row.get::<_, u32>(0))?,
            connection.query_row("SELECT count(*) FROM tasks WHERE repository_id=$1", [&repository], |row| row.get::<_, u32>(0))?,
        ))
    }).unwrap();
    assert_eq!(
        declarations, 2,
        "existing declaration survives; uncaptured context is not invented"
    );
    assert_eq!(
        mutations, 1,
        "optional capture failure does not reject an accepted mutation"
    );
    let review = crate::review::ReviewService::new(
        world.fixture.database.clone(),
        world.plane.usage().clone(),
    );
    let prepared = review
        .prepare(
            devcoordinator2_api::review::Prepare {
                repository_id: repository.clone(),
                workstream_id: Some("response-scope".into()),
                window_start_ms: START,
                window_end_ms: START + WEEK,
                offset: 0,
                limit: 10,
                before_decision_seq: None,
                outcome_cursor: None,
                outcome_limit: None,
            },
            START + WEEK,
        )
        .unwrap();
    assert_eq!(
        prepared
            .usage
            .outcomes
            .unattributed_reasons
            .get("work_context_capture_failed"),
        Some(&1)
    );
    assert_ne!(prepared.usage.outcomes.coverage, "complete");

    let mut conflict = context("already-durable");
    conflict.work.as_mut().unwrap().outcome_id = Some("different-outcome".into());
    let locked = observe(
        &world,
        vec![HeldLane::locked_transaction(
            world.fixture.database.clone(),
            "SELECT operation_id FROM codex_usage_operation_contexts WHERE operation_id=$1 FOR UPDATE",
            "already-durable".into(),
        )],
        &["test.status"],
        conflict.clone(),
    );
    assert!(
        locked
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. }))),
        "a single locked declaration held the useful read: {locked:?}"
    );
    let poisoned = || {
        world.fixture.database.read(|connection| {
        Ok(connection.query_row("SELECT rejected FROM codex_usage_operation_contexts WHERE operation_id='already-durable'", [], |row| row.get::<_, bool>(0))?)
    }).unwrap()
    };
    assert!(
        !poisoned(),
        "an unavailable capture must not invent a completed conflict write"
    );
    let retried = observe(&world, vec![], &["test.status"], conflict);
    assert!(
        retried
            .iter()
            .all(|(_, result)| matches!(result, Ok(ResponseEnvelope::Success { .. })))
    );
    assert!(
        poisoned(),
        "a true conflict remains permanently unattributed once captured"
    );
}
