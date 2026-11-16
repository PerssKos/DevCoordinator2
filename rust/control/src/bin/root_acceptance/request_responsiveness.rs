//! Observe real governed units while finite actions occupy every authority
//! writer, then prove completion, cancellation and successor acceptance.
use super::*;

const READ_BOUND: Duration = Duration::from_secs(1);

fn read_surfaces(world: &mut World, run: &str, state: &str) -> Vec<String> {
    let deadline = (time::OffsetDateTime::now_utc() + time::Duration::milliseconds(150))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut queries = vec![
        ("test.status", json!({"path":world.repo})),
        ("test.log.catalog", json!({"path":world.repo,"run_id":run})),
        (
            "test.log.tail",
            json!({
                "path":world.repo,"run_id":run,"check":"main",
                "phase":"check","stream":"stdout","lines":1
            }),
        ),
        ("health.summary", json!({})),
    ];
    if state == "running" {
        queries.push((
            "test.wait",
            json!({
                "path":world.repo,"run_id":run,"deadline_at":deadline
            }),
        ));
    }
    let observations = thread::scope(|scope| {
        let pending = queries
            .into_iter()
            .map(|(operation, params)| {
                let socket = world.socket.clone();
                scope.spawn(move || {
                    let began = Instant::now();
                    let result = request_over_socket_with_timeout(
                        &socket,
                        &request(operation, params, "other", None),
                        READ_BOUND,
                    );
                    (operation, began.elapsed(), result)
                })
            })
            .collect::<Vec<_>>();
        pending
            .into_iter()
            .map(|pending| pending.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut failures = Vec::new();
    for (operation, elapsed, response) in observations {
        world
            .measurements
            .entry(format!("{state}_{}_ms", operation.replace('.', "_")))
            .and_modify(|maximum| *maximum = (*maximum).max(elapsed.as_millis()))
            .or_insert(elapsed.as_millis());
        let valid = match &response {
            Ok(value) if operation == "test.wait" => {
                error_code(value) == Some("wait_deadline_reached")
                    && value.pointer("/error/recovery/run_id") == Some(&json!(run))
            }
            Ok(value) if operation == "test.status" => {
                data(value).is_ok_and(|status| status["run_id"] == run && status["status"] == state)
            }
            Ok(value) => data(value).is_ok(),
            Err(_) => false,
        };
        if !valid || elapsed > READ_BOUND + Duration::from_millis(250) {
            failures.push(format!("{operation} while {state}: {response:?}"));
        }
    }
    failures
}

pub(super) fn reads_survive_busy_writers_and_run_transitions(
    world: &mut World,
) -> Result<(), String> {
    use std::os::unix::net::UnixListener;
    // Startup publishes the socket before its first scheduled host sample.
    // Require a real sample before measuring reads under contention.
    let warmup = Instant::now();
    loop {
        let sampled = request_over_socket_with_timeout(
            &world.socket,
            &request("health.summary", json!({}), "other", None),
            READ_BOUND,
        )?;
        if data(&sampled).is_ok() {
            break;
        }
        ensure!(
            warmup.elapsed() < Duration::from_secs(10),
            "isolated daemon never produced its first health sample"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let gate = world.base.join("responsiveness-ready.sock");
    let listener = UnixListener::bind(&gate).map_err(|error| error.to_string())?;
    chown_path(&gate, world.harness.caller_uid, world.harness.caller_gid)?;
    let command = fixture_command(
        world,
        &["hold-socket", gate.to_str().ok_or("invalid gate")?],
    );
    world.write_config(&unit_config(&command, Some(90))?)?;
    let first = world.start_fresh(json!({"path":world.repo}), None)?;
    let first_id = test_run_id(&first)?.to_owned();
    let mut release = await_database_case(&listener)?;
    let mut failures = read_surfaces(world, &first_id, "running");

    let repository_id = ids::repository_id(&world.repo).map_err(|error| error.to_string())?;
    let connection = world.authority_connection()?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .query_row(
            "SELECT repository_id FROM repositories WHERE repository_id=$1 FOR UPDATE",
            [&repository_id],
            |_| Ok(()),
        )
        .map_err(|error| error.to_string())?;
    let mut pending = Vec::new();
    for index in 0..4 {
        let harness = world.harness.clone();
        let socket = world.socket.clone();
        let repository = repository_id.clone();
        pending.push(thread::spawn(move || {
            call_as(
                &harness.executable,
                harness.caller_uid,
                harness.caller_gid,
                &socket,
                request(
                    "task.create",
                    json!({
                        "repository_id":repository,
                        "title":format!("Retain fixture mutation {index}"),
                        "kind":"improvement"
                    }),
                    "other",
                    None,
                ),
            )
        }));
    }
    let observation = world.authority()?;
    let started = Instant::now();
    let saturated = loop {
        let waiting = observation.read(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'",
                [],
                |row| row.get::<_, u32>(0),
            )?)
        }).map_err(|error| error.to_string())?;
        if waiting >= 4 {
            break true;
        }
        if started.elapsed() >= Duration::from_secs(2) {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    if saturated {
        failures.extend(read_surfaces(world, &first_id, "running"));
    } else {
        failures.push("fixture actions did not occupy all four authority writers".into());
    }
    // Release the exact fixture row before joining every accepted mutation,
    // even when an observation failed. No foreign run or state is touched.
    transaction.rollback().map_err(|error| error.to_string())?;
    for operation in pending {
        let result = operation.join().map_err(|_| "fixture mutation panicked")?;
        if result
            .as_ref()
            .map_or(true, |response| data(response).is_err())
        {
            failures.push(format!(
                "accepted fixture mutation did not finish: {result:?}"
            ));
        }
    }
    release.write_all(b"1").map_err(|error| error.to_string())?;
    drop(release);
    let completed =
        world.wait_run_status(&first, &["passed", "failed"], Duration::from_secs(30))?;
    ensure!(
        completed["status"] == "passed",
        "held governed run did not complete"
    );
    failures.extend(read_surfaces(world, &first_id, "passed"));

    let second = world.start_fresh(json!({"path":world.repo}), Some(&first_id))?;
    let second_id = test_run_id(&second)?.to_owned();
    let second_release = await_database_case(&listener)?;
    failures.extend(read_surfaces(world, &second_id, "running"));
    data(&world.call("test.stop", json!({"path":world.repo}))?)?;
    drop(second_release);
    world.wait_run_status(&second, &["cancelled"], Duration::from_secs(30))?;
    failures.extend(read_surfaces(world, &second_id, "cancelled"));

    world.write_config(&unit_config(&["/usr/bin/true".into()], Some(30))?)?;
    let began = Instant::now();
    let successor = world.start_fresh(json!({"path":world.repo}), Some(&second_id))?;
    world.measurements.insert(
        "successor_acceptance_ms".into(),
        began.elapsed().as_millis(),
    );
    ensure!(
        began.elapsed() < Duration::from_secs(5),
        "next run acceptance stalled after cancellation"
    );
    world.wait_run_status(&successor, &["passed"], Duration::from_secs(30))?;
    failures.extend(read_surfaces(world, test_run_id(&successor)?, "passed"));
    ensure!(
        failures.is_empty(),
        "unresponsive user surfaces: {}",
        failures.join("; ")
    );
    Ok(())
}
