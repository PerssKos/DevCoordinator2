//! State changes before subscription and without a publication stay observable.
use super::*;
use crate::events::{EventService, EventVisibility, NewEvent};
use crate::request_deadline::{WaitContext, WaitDeadline};

fn prepare(
    fixture: &RouteWorld,
    id: &str,
    state: devcoordinator2_api::params::DeploymentWaitState,
    events: &EventService,
) -> crate::daemon::DeferredOperation {
    let request = devcoordinator2_api::params::DeploymentWait {
        path: None,
        name: None,
        deployment_id: Some(id.into()),
        state,
        deadline_at: (time::OffsetDateTime::now_utc() + time::Duration::seconds(4))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
    };
    let params = serde_json::to_value(&request).unwrap();
    let context = WaitContext::new(
        Arc::new(tokio::sync::Semaphore::new(2)),
        WaitDeadline::for_operation("deployment.wait", &params).unwrap(),
    );
    fixture
        .deployments
        .prepare_wait(
            request,
            fixture.caller.clone(),
            events.clone(),
            Arc::new(|| Ok(EventVisibility::unrestricted())),
            context,
        )
        .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn deployment_wait_rechecks_after_subscribing_when_initial_status_races_change() {
    let fixture = RouteWorld::new("worktree");
    let first = fixture.apply().unwrap();
    fixture.change();
    let git = fixture.git.clone();
    *fixture.git.snapshot_hook.lock().unwrap() = Some(Box::new(move || {
        git.snapshot.lock().unwrap().commit = Some("a".repeat(40));
    }));
    let events = EventService::new(fixture.world.database.clone()).unwrap();
    let observed = prepare(
        &fixture,
        &first.deployment_id,
        devcoordinator2_api::params::DeploymentWaitState::SourceCurrent,
        &events,
    );
    runtime().block_on(async {
        let result = tokio::time::timeout(Duration::from_millis(800), observed)
            .await
            .expect("state must be rechecked before waiting for the heartbeat")
            .unwrap();
        assert_eq!(result["completed"], true);
        assert_eq!(result["status"]["readiness"]["pending_apply"], false);
    });
}

#[test]
fn deployment_wait_heartbeat_observes_source_changes_without_owned_events() {
    let fixture = RouteWorld::new("worktree");
    let first = fixture.apply().unwrap();
    fixture.change();
    let events = EventService::new(fixture.world.database.clone()).unwrap();
    let observed = prepare(
        &fixture,
        &first.deployment_id,
        devcoordinator2_api::params::DeploymentWaitState::SourceCurrent,
        &events,
    );
    let (checked, received) = tokio::sync::oneshot::channel();
    *fixture.git.snapshot_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = checked.send(());
    }));
    runtime().block_on(async {
        let pending = tokio::spawn(observed);
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        // The preceding check captured the changed snapshot before signaling;
        // no declaration or runtime action publishes an event for this edit.
        fixture.git.snapshot.lock().unwrap().commit = Some("a".repeat(40));
        let result = pending.await.unwrap().unwrap();
        assert_eq!(result["completed"], true);
        assert_eq!(result["status"]["readiness"]["pending_apply"], false);
    });
}

#[test]
fn recovery_publishes_after_commits_and_optional_notification_failure_is_safe() {
    for fail_notification in [false, true] {
        let fixture = RouteWorld::new("checkout");
        let first = fixture.apply().unwrap();
        let events = EventService::new(fixture.world.database.clone()).unwrap();
        let callbacks = Arc::new(Mutex::new(Vec::new()));
        let observed_callbacks = callbacks.clone();
        let database = fixture.world.database.clone();
        let route_path = fixture.deployments.config.routes_path();
        let published = events.clone();
        fixture.deployments.set_event_sink(Arc::new(move |event| {
            // A writer can enter: no state transaction is held by delivery.
            database
                .transaction({
                    let id = event.deployment_id.clone();
                    move |connection| {
                        connection.execute(
                            "UPDATE deployments SET updated_at=updated_at WHERE deployment_id=$1",
                            [id],
                        )?;
                        Ok(())
                    }
                })
                .unwrap();
            let route: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&route_path).unwrap()).unwrap();
            assert_eq!(route["routes"][0]["deployment_id"], event.deployment_id);
            observed_callbacks.lock().unwrap().push(event.clone());
            if fail_notification {
                return Err(ProtocolError::new(
                    ErrorCode::InternalError,
                    "fixture notification unavailable",
                ));
            }
            published
                .publish(NewEvent {
                    occurred_at: time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                    event: devcoordinator2_api::results::OwnedEvent::Deployment(event),
                    dedupe_key: None,
                })
                .map(|_| ())
        }));
        let unit = first.components[0].binding.identity.as_ref().unwrap();
        fixture.systemd.states.lock().unwrap().remove(unit);
        let observed = prepare(
            &fixture,
            &first.deployment_id,
            devcoordinator2_api::params::DeploymentWaitState::Ready,
            &events,
        );
        let (checked, received) = tokio::sync::oneshot::channel();
        *fixture.git.snapshot_hook.lock().unwrap() = Some(Box::new(move || {
            let _ = checked.send(());
        }));
        runtime().block_on(async {
            let pending = tokio::spawn(observed);
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap();
            fixture.deployments.reconcile_routes().unwrap();
            let result = pending.await.unwrap().unwrap();
            assert_eq!(result["completed"], true);
            assert_eq!(result["status"]["readiness"]["ready"], true);
        });
        let callbacks = callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0].kind, "deployment.recovered");
        assert_eq!(callbacks[0].state.as_deref(), Some("running"));
    }
}
