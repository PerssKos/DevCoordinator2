//! Real shared-fixture ownership, retained retry, and crash recovery.

use super::*;
use devcoordinator2_control::docker::{DockerCli, DockerControl, ExactContainerId};

pub(super) fn retained_exit_diagnostics(world: &mut World) -> Result<(), String> {
    let (listener, gate) = database_case_gate(world)?;
    let command = command_json(&fixture_command(
        world,
        &[
            "postgres-query-gate",
            "SELECT 1",
            gate.to_str().ok_or("invalid gate")?,
        ],
    ))?;
    world.write_config(&format!(
        r#"schema=2
[test.database]
timeout_seconds=120
[test.database.postgres]
image="postgres:16-alpine"
user="app"
database="app_test"
[[test.database.check]]
name="database-user"
tier="release"
command={command}
timeout_seconds=90
[[test.database.check]]
name="after-exit"
tier="release"
requires=["database-user"]
command=["/usr/bin/psql","--no-psqlrc","--set=ON_ERROR_STOP=1","-tA","-c","SELECT 1"]
timeout_seconds=10
"#
    ))?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "retained container diagnostic acceptance"])?;
    let started = world.start_fresh(json!({"path":world.repo,"test":"database"}), None)?;
    let run = test_run_id(&started)?.to_owned();
    let mut gate = await_database_case(&listener)?;
    let records: Value = serde_json::from_slice(
        &fs::read(
            world
                .repo
                .join(".devcoordinator/test/containers/pending")
                .join(&run)
                .join("container-lifecycle.json"),
        )
        .map_err(|_| "shared fixture receipt missing")?,
    )
    .map_err(|_| "shared fixture receipt invalid")?;
    let record = records
        .as_array()
        .and_then(|records| records.first())
        .ok_or("shared fixture exact ownership missing")?;
    let id = record["container_id"]
        .as_str()
        .ok_or("fixture ID missing")?;
    let request = json!({"path":world.repo,"run_id":run,"check":record["check"],"container_id":id});
    let live = world.call("test.container.inspect", request.clone())?;
    ensure!(
        data(&live)?["inspection"]["state"] == "running"
            && data(&live)?["record"]["labels"]["devcoordinator2.instance"] == world.unit_prefix
            && data(&live)?["record"].get("diagnostic").is_none(),
        "fixture was not ready, exactly owned, and diagnostic-free before failure"
    );
    // The harness owns this exact container in its isolated namespace. Write
    // non-secret PostgreSQL log lines through its real logging driver, then stop
    // PostgreSQL after readiness without asking governed cleanup to cause it.
    run_status(
        "docker",
        &[
            "exec",
            id,
            "psql",
            "-U",
            "app",
            "-d",
            "app_test",
            "-c",
            "DO $$ BEGIN FOR i IN 1..900 LOOP RAISE LOG 'fixture post-readiness diagnostic %', repeat('x', 180); END LOOP; END $$;",
        ],
    )?;
    run_status("docker", &["kill", "--signal=KILL", id])?;
    let exact = ExactContainerId::parse(id.to_owned()).map_err(|_| "invalid exact fixture")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = DockerCli::default()
            .inspect(&exact)
            .map_err(|_| "fixture state unavailable")?;
        if value["State"]["Status"] == "exited" {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "controlled PostgreSQL exit did not settle"
        );
        thread::sleep(Duration::from_millis(10));
    }
    gate.write_all(b"1").map_err(|error| error.to_string())?;
    world.wait_run_status(&started, &["failed"], Duration::from_secs(30))?;
    ensure!(
        !DockerCli::default()
            .container_present(&exact)
            .map_err(|_| "cannot prove exact fixture absence")?,
        "governed cleanup did not remove the exact exited fixture"
    );
    let cleaned = world.call("test.container.inspect", request.clone())?;
    let record = &data(&cleaned)?["record"];
    ensure!(
        record["cleanup_status"] == "cleanup_complete"
            && record["oom_killed"] == false
            && record["solver_exit_code"]
                .as_i64()
                .is_some_and(|exit| exit != 0),
        "governed cleanup lost PostgreSQL exit or OOM facts"
    );
    ensure!(
        record["diagnostic"]["truncated"] == true
            && record["diagnostic"]["bytes"]
                .as_u64()
                .is_some_and(|bytes| bytes <= 65536),
        "real oversized container logs were not retained with an honest bound"
    );
    let mut read = request.clone();
    read["max_bytes"] = json!(4096);
    read["byte_offset"] = json!(0);
    let mut text = String::new();
    loop {
        let content = world.call("test.container.diagnostic", read.clone())?;
        let content = data(&content)?;
        let chunk = content["text"].as_str().ok_or("diagnostic text missing")?;
        ensure!(
            chunk.len() <= 4096,
            "diagnostic response exceeded its explicit bound"
        );
        text.push_str(chunk);
        match content["next_byte_offset"].as_u64() {
            Some(offset) => read["byte_offset"] = json!(offset),
            None => break,
        }
        ensure!(
            text.len() <= 65536,
            "diagnostic pagination did not remain bounded"
        );
    }
    ensure!(
        text.contains("fixture post-readiness diagnostic")
            && record["diagnostic"]["sha256"] == sha256_hex(text.as_bytes())
            && record["diagnostic"]["bytes"] == text.len(),
        "supported post-cleanup read lost or changed retained container diagnostics"
    );
    let mut foreign = read;
    foreign["check"] = json!("shared-foreign");
    ensure!(
        world.call("test.container.diagnostic", foreign)?["ok"] == false,
        "diagnostic content crossed its exact check ownership"
    );
    let retry = retry_when_idle(world, &request)?;
    ensure!(
        data(&retry)?["record"] == *record,
        "completed cleanup retry changed retained diagnostic evidence"
    );
    Ok(())
}

pub(super) fn shared_ownership_and_recovery(world: &mut World) -> Result<(), String> {
    let (listener, gate) = database_case_gate(world)?;
    let command = command_json(&fixture_command(
        world,
        &[
            "postgres-query-gate",
            "SELECT 1",
            gate.to_str().ok_or("invalid gate path")?,
        ],
    ))?;
    world.write_config(&format!(
        r#"schema=2
[test.database]
timeout_seconds=120
[test.database.postgres]
image="postgres:16-alpine"
[[test.database.check]]
name="database-user"
tier="release"
command={command}
timeout_seconds=90
"#
    ))?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "shared fixture ownership acceptance"])?;
    let mut previous = None;
    let mut first_request: Option<Value> = None;
    for scenario in ["normal", "legacy", "name-reuse", "restart"] {
        let started = world.start_fresh(
            json!({"path":world.repo,"test":"database"}),
            previous.as_deref(),
        )?;
        let run_id = test_run_id(&started)?.to_owned();
        let mut socket = await_database_case(&listener)?;
        let receipt_path = world
            .repo
            .join(".devcoordinator/test/containers/pending")
            .join(&run_id)
            .join("container-lifecycle.json");
        let records: Value = serde_json::from_slice(
            &fs::read(&receipt_path).map_err(|_| "fixture ownership receipt missing")?,
        )
        .map_err(|_| "fixture ownership receipt invalid")?;
        let records = records
            .as_array()
            .ok_or("fixture records must be an array")?;
        ensure!(
            records.len() == 1,
            "shared fixture did not retain exactly one owned container"
        );
        let record = &records[0];
        let id = record["container_id"]
            .as_str()
            .ok_or("fixture exact ID missing")?
            .to_owned();
        let name = record["name"]
            .as_str()
            .ok_or("fixture name missing")?
            .to_owned();
        let request =
            json!({"path":world.repo,"run_id":run_id,"check":record["check"],"container_id":id});
        if let Some(prior) = &first_request {
            let old = world.call("test.container.inspect", prior.clone())?;
            ensure!(
                data(&old)?["record"]["cleanup_status"] == "cleanup_complete",
                "replacement lost the earlier run's ownership receipt"
            );
        }
        if scenario == "legacy" {
            // Recreate the prior persisted contract: containers.json contains
            // the exact ID, but no typed receipt exists. Labels alone are not
            // sufficient for adoption.
            fs::remove_file(&receipt_path).map_err(|_| "cannot prepare legacy fixture receipt")?;
        }
        let live = world.call("test.container.inspect", request.clone())?;
        ensure!(
            data(&live)?["inspection"]["container_id"] == id,
            "supported inspection did not find the shared fixture"
        );
        let refused = world.call("test.container.retry", request.clone())?;
        ensure!(
            refused["ok"] == false,
            "retry deleted a container with an active fixture lease"
        );
        let mut replacement = None;
        if scenario == "name-reuse" {
            run_status("docker", &["rm", "-f", &id])?;
            let unrelated = command_stdout(
                "docker",
                &[
                    "create",
                    "--name",
                    &name,
                    "--label",
                    &format!("devcoordinator2.instance={}", world.unit_prefix),
                    "--label",
                    "devcoordinator2.purpose=test",
                    "postgres:16-alpine",
                ],
            )?;
            let unrelated = unrelated.trim().to_owned();
            ensure!(
                unrelated.len() == 64 && unrelated != id,
                "name replacement must have its own exact identity"
            );
            replacement = Some(unrelated);
        }
        let finished = if scenario == "restart" {
            world.stop_daemon(true)?;
            world.start_daemon(None, None, None)?;
            world.wait_run_status(&started, &["interrupted"], Duration::from_secs(30))?
        } else {
            socket.write_all(b"1").map_err(|error| error.to_string())?;
            world.wait_run_status(&started, &["passed", "failed"], Duration::from_secs(30))?
        };
        let observed = world.call("test.container.inspect", request.clone())?;
        let observed = data(&observed)?;
        ensure!(
            observed.get("inspection").is_none_or(Value::is_null),
            "removed exact fixture still has a live inspection"
        );
        if let Some(replacement) = replacement {
            ensure!(
                finished["status"] == "failed"
                    && observed["record"]["cleanup_status"] != "cleanup_complete",
                "reused name falsely completed cleanup"
            );
            let failed_retry = retry_when_idle(world, &request)?;
            ensure!(
                data(&failed_retry)?["record"]["cleanup_status"] != "cleanup_complete",
                "retry ignored the reused name"
            );
            command_stdout("docker", &["inspect", "--format", "{{.Id}}", &replacement])?;
            run_status("docker", &["rm", "-f", &replacement])?;
        } else {
            ensure!(
                observed["record"]["cleanup_status"] == "cleanup_complete",
                "fixture cleanup did not retain exact absence proof"
            );
            ensure!(
                finished["status"]
                    == if scenario == "restart" {
                        "interrupted"
                    } else {
                        "passed"
                    },
                "normal fixture result changed during cleanup"
            );
        }
        let retry = retry_when_idle(world, &request)?;
        ensure!(
            data(&retry)?["record"]["cleanup_status"] == "cleanup_complete",
            "retained fixture cleanup could not be retried"
        );
        let repeated = retry_when_idle(world, &request)?;
        ensure!(
            data(&repeated)?["record"] == data(&retry)?["record"],
            "repeating successful cleanup changed its durable receipt"
        );
        if first_request.is_none() {
            first_request = Some(request);
        }
        previous = Some(run_id);
        drop(socket);
    }
    Ok(())
}

fn retry_when_idle(world: &World, request: &Value) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = world.call("test.container.retry", request.clone())?;
        let transient_busy = reply["ok"] == false
            && reply
                .pointer("/error/recovery/class")
                .and_then(Value::as_str)
                == Some("transient")
            && reply.pointer("/error/recovery/retryable") == Some(&json!(true))
            && matches!(
                reply
                    .pointer("/error/recovery/state")
                    .and_then(Value::as_str),
                Some("container_operation_in_progress" | "container_metadata_busy")
            );
        if !transient_busy {
            return Ok(reply);
        }
        ensure!(
            Instant::now() < deadline,
            "typed container cleanup contention did not settle within 30 seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn assert_live_template(world: &World, id: &str) -> Result<Value, String> {
    let labels: Value = serde_json::from_str(&command_stdout(
        "docker",
        &["inspect", "--format", "{{json .Config.Labels}}", id],
    )?)
    .map_err(|_| "template ownership labels invalid")?;
    let request = json!({"path":world.repo,"run_id":labels["devcoordinator2.run"],"check":labels["devcoordinator2.check"],"container_id":id});
    let inspected = world.call("test.container.inspect", request.clone())?;
    ensure!(
        data(&inspected)?["record"]["container_id"] == id,
        "template lacks supported durable ownership"
    );
    let refused = world.call("test.container.retry", request.clone())?;
    ensure!(
        refused["ok"] == false,
        "retry removed a template still owned by the pool"
    );
    Ok(request)
}

pub(super) fn retain_start_response(
    world: &World,
    stage: &str,
    response: &Value,
) -> Result<(), String> {
    ensure!(
        stage
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid database start evidence stage"
    );
    let run_id = test_run_id(response)?;
    let root = world.harness.work_root.join("evidence");
    match fs::create_dir(&root) {
        Ok(()) => fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&root).map_err(|error| error.to_string())?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "database evidence directory changed"
            );
        }
        Err(error) => return Err(error.to_string()),
    }
    let case = world
        .base
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("invalid database case identity")?;
    let payload = serde_json::to_vec_pretty(
        &json!({"schema":1,"stage":stage,"run_id":run_id,"response":response}),
    )
    .map_err(|error| error.to_string())?;
    ensure!(
        payload.len() <= 64 * 1024,
        "database start evidence exceeded its bound"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.join(format!("{case}-{stage}.json")))
        .map_err(|error| error.to_string())?;
    file.write_all(&payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

pub(super) fn completed_receipt_and_renamed_intent(world: &mut World) -> Result<(), String> {
    let (listener, gate) = database_case_gate(world)?;
    let command = command_json(&fixture_command(
        world,
        &["hold-socket", gate.to_str().ok_or("invalid gate path")?],
    ))?;
    world.write_config(&format!("schema=2\n[test.owned]\ntimeout_seconds=120\n[[test.owned.check]]\nname=\"unit\"\ntier=\"release\"\ncommand={command}\ntimeout_seconds=90\n"))?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "owned receipt recovery fixture"])?;
    DockerCli::default()
        .ensure_image("postgres:16-alpine")
        .map_err(|_| "container fixture image is unavailable")?;
    let started = world.start_fresh(json!({"path":world.repo,"test":"owned"}), None)?;
    retain_start_response(world, "start-before-recovery", &started)?;
    let _socket = await_database_case(&listener)?;
    let run = test_run_id(&started)?.to_owned();
    let create = json!({"path":world.repo,"run_id":run,"check":"unit","image":"postgres:16-alpine","command":["true"]});
    let created = world.call("test.container.create", create.clone())?;
    let first = data(&created)?["record"]["container_id"]
        .as_str()
        .ok_or("created container identity missing")?
        .to_owned();
    let reference = json!({"path":world.repo,"run_id":run,"check":"unit","container_id":first});
    let removed = world.call("test.container.remove", reference.clone())?;
    let before = data(&removed)?["record"].clone();
    let mut failures = Vec::new();
    let refused = world.call("test.container.start", reference.clone())?;
    if refused["ok"] != false {
        failures.push("removed container could be started");
    }
    let after = world.call("test.container.inspect", reference.clone())?;
    if data(&after)?["record"] != before {
        failures.push("refused start rewrote completed cleanup evidence");
    }
    data(&world.call("test.container.retry", reference)?)?;

    let created = world.call("test.container.create", create)?;
    let created = data(&created)?["record"].clone();
    let id = created["container_id"]
        .as_str()
        .ok_or("second container identity missing")?
        .to_owned();
    let name = created["name"]
        .as_str()
        .ok_or("second container name missing")?
        .to_owned();
    world.stop_daemon(true)?;
    let path = world
        .repo
        .join(".devcoordinator/test/containers/pending")
        .join(&run)
        .join("container-lifecycle.json");
    let mut records: Value =
        serde_json::from_slice(&fs::read(&path).map_err(|_| "pending receipt missing")?)
            .map_err(|_| "pending receipt invalid")?;
    let record = records
        .as_array_mut()
        .ok_or("pending receipt must be an array")?
        .iter_mut()
        .find(|record| record["container_id"] == id)
        .ok_or("pending exact identity missing")?;
    record["container_id"] = json!("0".repeat(64));
    record["cleanup_status"] = json!("cleanup_pending");
    record["cleanup_attempts"] = json!([]);
    fs::write(
        &path,
        serde_json::to_vec(&records).map_err(|_| "cannot encode pending intent")?,
    )
    .map_err(|_| "cannot retain pending intent")?;
    run_status("docker", &["rename", &id, &format!("{name}-renamed")])?;
    world.start_daemon(None, None, None)?;
    world.wait_run_status(&started, &["interrupted"], Duration::from_secs(30))?;
    let intent =
        json!({"path":world.repo,"run_id":run,"check":"unit","container_id":"0".repeat(64)});
    let inspected = world.call("test.container.inspect", intent.clone())?;
    let exact =
        ExactContainerId::parse(id.clone()).map_err(|_| "invalid exact fixture identity")?;
    let alive = DockerCli::default()
        .container_present(&exact)
        .map_err(|_| "cannot inspect exact fixture")?;
    if !alive || data(&inspected)?["record"]["cleanup_status"] == "cleanup_complete" {
        failures.push("renamed labelled container lost its unresolved ownership intent");
    }
    run_status("docker", &["rename", &id, &name])?;
    let retried = world.call("test.container.retry", intent)?;
    let absent = !DockerCli::default()
        .container_present(&exact)
        .map_err(|_| "cannot verify exact fixture absence")?;
    if data(&retried)?["record"]["container_id"] != id
        || data(&retried)?["record"]["cleanup_status"] != "cleanup_complete"
        || !absent
    {
        failures.push("restored pending intent could not finish exact cleanup");
    }
    if !absent {
        remove_fixture_container(&world.unit_prefix, &id)?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
