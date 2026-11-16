//! Native deployment source and startup-evidence acceptance.

use super::*;

fn commit_candidate(world: &World, candidate: &Path, version: &str) -> Result<String, String> {
    fs::write(candidate.join("marker.txt"), format!("{version}\n")).map_err(|e| e.to_string())?;
    chown_path(
        &candidate.join("marker.txt"),
        world.harness.caller_uid,
        world.harness.caller_gid,
    )?;
    for args in [&["add", "marker.txt"][..], &["commit", "-qm", version]] {
        run_as(
            world.harness.caller_uid,
            world.harness.caller_gid,
            candidate,
            "/usr/bin/git",
            args,
            &world.base,
        )?;
    }
    Ok(command_stdout_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        candidate,
        "/usr/bin/git",
        &["rev-parse", "HEAD"],
        &world.base,
    )?
    .trim()
    .to_owned())
}

pub(super) fn assert_frozen_worktree_candidate(world: &mut World) -> Result<(), String> {
    setup_web(world, "v1", false)?;
    world.git(&["add", "."])?;
    world.git(&[
        "commit",
        "--allow-empty",
        "-qm",
        "worktree candidate fixture",
    ])?;
    let first = data(&world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"web@worktree"}),
    )?)?
    .clone();
    let id = first["deployment_id"]
        .as_str()
        .ok_or("missing worktree deployment")?
        .to_owned();
    let volume = format!("devcoordinator2-{id}-db-pgdata");
    world.track_volume(&volume);
    let database = component(&first, "db")?["binding"]["identity"]
        .as_str()
        .ok_or("missing worktree database")?
        .to_owned();
    docker_exec(
        &database,
        &[
            "psql",
            "-U",
            "app",
            "-d",
            "app",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "CREATE TABLE frozen_keep(x int); INSERT INTO frozen_keep VALUES(11)",
        ],
    )?;
    let candidate = world
        .base
        .join("candidate-worktrees/frozen-worktree-candidate");
    let original_declaration = fs::read_to_string(world.repo.join(".devcoordinator.toml"))
        .map_err(|error| error.to_string())?;
    world.git(&[
        "worktree",
        "add",
        "--detach",
        candidate.to_str().ok_or("invalid candidate path")?,
        "HEAD",
    ])?;
    let commit = commit_candidate(world, &candidate, "frozen-v2")?;
    let request = json!({"deployment_id":id,"candidate":{"path":candidate,"commit":commit}});
    ensure!(
        data(&world.call("deployment.preflight", request.clone())?)?["ready"] == true,
        "clean worktree candidate preflight did not pass"
    );
    let second = data(&world.call("deployment.apply", request)?)?.clone();
    let port = component(&second, "api")?["port"]
        .as_u64()
        .ok_or("missing candidate port")? as u16;
    let unit = component(&second, "api")?["binding"]["identity"]
        .as_str()
        .ok_or("missing candidate unit")?
        .to_owned();
    ensure!(
        http_get_json(port)?["version"] == "frozen-v2",
        "worktree candidate did not serve its exact commit"
    );
    fs::write(candidate.join("marker.txt"), "unreviewed after apply\n")
        .map_err(|e| e.to_string())?;
    world.write_config("schema = [unfinished declaration\n")?;
    let logs = world.call(
        "deployment.logs",
        json!({"deployment_id":id,"component":"api","generation":2,"tail_lines":5}),
    )?;
    ensure!(
        data(&logs)?["process_log"]["generation"] == 2,
        "saved process diagnostics depended on the editable declaration"
    );
    for operation in ["deployment.stop", "deployment.start"] {
        data(&world.call(operation, json!({"deployment_id":id,"component":"api"}))?)?;
    }
    let restarted = data(&world.call(
        "deployment.restart",
        json!({"deployment_id":id,"component":"api"}),
    )?)?
    .clone();
    ensure!(
        component(&restarted, "api")?["state"] == "running",
        "candidate process did not restart"
    );
    ensure!(
        http_get_json(port)?["version"] == "frozen-v2",
        "editing candidate source changed the restarted deployed worktree candidate"
    );
    ensure!(
        world
            .state
            .join("deployments")
            .join(&id)
            .join("gen-2/.git")
            .is_file(),
        "worktree candidate has no retained detached checkout"
    );
    run_status("systemctl", &["stop", &unit])?;
    world.stop_daemon(true)?;
    world.start_daemon(None, None, None)?;
    wait_for_value(
        "frozen candidate process recovery",
        Duration::from_secs(20),
        || match http_get_json(port) {
            Ok(value) if value["version"] == "frozen-v2" => Ok(Some(value)),
            _ => Ok(None),
        },
    )?;
    world.write_config(&original_declaration)?;
    let recovered = data(&world.call("deployment.status", json!({"deployment_id":id}))?)?.clone();
    ensure!(
        recovered["current_generation"] == 2 && recovered["readiness"]["pending_apply"] == true,
        "recovery lost the exact generation or stopped detecting candidate edits"
    );
    fs::write(candidate.join("marker.txt"), "frozen-v2\n").map_err(|e| e.to_string())?;
    let commit = commit_candidate(world, &candidate, "frozen-v3")?;
    world.write_config("schema = [unfinished declaration\n")?;
    let request = json!({"deployment_id":id,"candidate":{"path":candidate,"commit":commit}});
    ensure!(
        data(&world.call("deployment.preflight", request.clone())?)?["ready"] == true,
        "clean candidate preflight depended on the editable declaration"
    );
    let third = data(&world.call("deployment.apply", request)?)?.clone();
    ensure!(
        third["current_generation"] == 3 && third["previous_generation"] == 2,
        "worktree candidate did not retain its frozen predecessor"
    );
    fs::write(candidate.join("marker.txt"), "unreviewed before rollback\n")
        .map_err(|e| e.to_string())?;
    let rollback = data(&world.call("deployment.rollback", json!({"deployment_id":id}))?)?.clone();
    ensure!(
        http_get_json(port)?["version"] == "frozen-v2",
        "rollback replayed mutable candidate source"
    );
    ensure!(
        rollback["deployment_id"] == id
            && rollback["domain"] == first["domain"]
            && rollback["public"] == first["public"]
            && component(&rollback, "api")?["port"] == component(&first, "api")?["port"]
            && component(&rollback, "db")?["binding"]["identity"] == database,
        "frozen worktree candidate changed deployment, access, database or stable port identity"
    );
    ensure!(
        docker_exec(
            &database,
            &[
                "psql",
                "-U",
                "app",
                "-d",
                "app",
                "-At",
                "-c",
                "SELECT x FROM frozen_keep"
            ]
        )?
        .trim()
            == "11",
        "worktree candidate or rollback lost stored data"
    );
    ensure!(
        fs::read_to_string(world.repo.join("marker.txt")).map_err(|e| e.to_string())? == "v1\n",
        "worktree candidate changed unrelated original source"
    );
    data(&world.call(
        "deployment.remove",
        json!({"deployment_id":id,"delete_data":true}),
    )?)?;
    world.write_config(&original_declaration)?;
    world.forget_volume(&volume);
    ensure!(
        !world.state.join("deployments").join(&id).exists(),
        "removal left frozen generation files"
    );
    world.git(&[
        "worktree",
        "remove",
        "--force",
        candidate.to_str().ok_or("invalid candidate path")?,
    ])?;
    Ok(())
}

pub(super) fn case_process_startup_attempts_remain_attributable_and_bounded(
    world: &mut World,
) -> Result<(), String> {
    let configure = |command: Vec<String>| -> Result<(), String> {
        world.write_config(&format!("schema=2\n[deployment.diagnostics]\nsource='worktree'\ndomain='diagnostics'\ncomponents=['api']\n[deployment.diagnostics.component.api]\ntype='process'\ncommand={}\nport=true\nroute=true\nhealth={{path='/healthz',timeout_seconds=15}}\n", command_json(&command)?))
    };
    configure(fixture_command(world, &["http-server", "healthy"]))?;
    world.git(&["add", "."])?;
    world.git(&["commit", "-qm", "process diagnostics fixture"])?;
    let first = data(&world.call(
        "deployment.apply",
        json!({"path":world.repo,"name":"diagnostics"}),
    )?)?
    .clone();
    let id = first["deployment_id"]
        .as_str()
        .ok_or("missing diagnostic deployment")?
        .to_owned();
    configure(fixture_command(
        world,
        &["stderr-exit", "original-candidate-startup", "23"],
    ))?;
    let failed = world.call("deployment.apply", json!({"deployment_id":id}))?;
    ensure!(
        error_code(&failed) == Some("deployment_apply_failed"),
        "candidate startup did not fail"
    );
    let original = data(&world.call(
        "deployment.logs",
        json!({"deployment_id":id,"component":"api","generation":2,"tail_lines":20}),
    )?)?
    .clone();
    let attempt = original["process_log"]["attempt"]
        .as_u64()
        .ok_or("failed startup has no stable attempt identity")?;
    let selected = json!({"deployment_id":id,"component":"api","generation":2,"attempt":attempt,"tail_lines":20});
    ensure!(
        original["tail"]
            .as_str()
            .is_some_and(|v| v.contains("original-candidate-startup"))
            && original["process_log"]["generation"] == 2
            && attempt > 0,
        "failed startup has no exact supported generation/attempt diagnostic"
    );
    configure(fixture_command(world, &["http-server", "healthy"]))?;
    for _ in 0..12 {
        data(&world.call(
            "deployment.restart",
            json!({"deployment_id":id,"component":"api"}),
        )?)?;
    }
    world.stop_daemon(false)?;
    world.start_daemon(None, None, None)?;
    let retained = data(&world.call("deployment.logs", selected)?)?.clone();
    ensure!(
        retained["tail"] == original["tail"] && retained["process_log"]["state"] == "available",
        "recovery replaced the failed candidate startup diagnostic"
    );
    let recent = data(&world.call(
        "deployment.logs",
        json!({"deployment_id":id,"component":"api","generation":1}),
    )?)?
    .clone();
    ensure!(
        recent["process_log"]["retained_attempts"]
            .as_array()
            .is_some_and(|v| v.len() == 8)
            && recent["process_log"]["max_attempts_per_generation"] == 8
            && recent["process_log"]["max_generations"] == 8,
        "process attempt retention is not explicit and bounded"
    );
    let pruned = data(&world.call(
        "deployment.logs",
        json!({"deployment_id":id,"component":"api","generation":1,"attempt":1}),
    )?)?
    .clone();
    ensure!(
        pruned["tail"] == "" && pruned["process_log"]["state"] == "unavailable",
        "pruned startup attempt was replaced with another attempt"
    );
    fs::write(
        world
            .state
            .join("deployments")
            .join(&id)
            .join("logs/api.log"),
        "unrelated legacy startup\n",
    )
    .map_err(|e| e.to_string())?;
    let absent = data(&world.call(
        "deployment.logs",
        json!({"deployment_id":id,"component":"api","generation":99}),
    )?)?
    .clone();
    ensure!(
        absent["tail"] == "" && absent["process_log"]["state"] == "unavailable",
        "missing historical generation returned a legacy log"
    );
    data(&world.call("deployment.remove", json!({"deployment_id":id}))?)?;
    Ok(())
}
