use super::*;
use devcoordinator2_control::database::Database;
use devcoordinator2_control::deployment_state::{
    DeploymentStore, ObservedContainerInput, ObservedDeploymentInput,
};

pub(super) fn engine_cache_cleanup(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    let socket = world.base.join("builder.sock");
    let config = world.base.join("builder.json");
    write_private_json(&config, &json!({}))?;
    let unit = format!("{}-builder.service", world.unit_prefix);
    run_status(
        "systemd-run",
        &[
            "--quiet",
            "--collect",
            &format!("--unit={unit}"),
            "--property=Type=notify",
            "--property=TimeoutStartSec=90",
            "--property=TimeoutStopSec=15",
            "--property=PrivateNetwork=yes",
            "/usr/sbin/dockerd",
            "--config-file",
            config.to_str().unwrap(),
            "--host",
            &format!("unix://{}", socket.display()),
            "--data-root",
            world.base.join("builder-data").to_str().unwrap(),
            "--exec-root",
            world.base.join("builder-exec").to_str().unwrap(),
            "--pidfile",
            world.base.join("builder.pid").to_str().unwrap(),
            "--bridge=none",
            "--iptables=false",
            "--ip-masq=false",
            "--storage-driver=vfs",
        ],
    )?;
    write_private_json(
        &world.state.join("storage-fixture-engine.json"),
        &json!(socket),
    )?;
    let registration = world.call("repository.register", json!({"path":world.repo}))?;
    let repository = data(&registration)?["repository_id"]
        .as_str()
        .ok_or("fixture repository missing")?
        .to_owned();
    let host = format!("unix://{}", socket.display());
    let label = format!("devcoordinator2.instance={}", world.unit_prefix);
    let network = format!("{}-network", world.unit_prefix);
    run_status(
        "docker",
        &[
            "--host", &host, "network", "create", "--label", &label, &network,
        ],
    )?;
    let mut tags = Vec::new();
    for name in ["selected", "retained"] {
        let context = world.repo.join(name);
        fs::create_dir(&context).map_err(|e| e.to_string())?;
        fs::write(
            context.join("Dockerfile"),
            "FROM scratch\nCOPY payload /payload\n",
        )
        .map_err(|e| e.to_string())?;
        fs::write(
            context.join("payload"),
            format!("{}-{name}", world.unit_prefix),
        )
        .map_err(|e| e.to_string())?;
        let tag = format!("{}-{name}:fixture", world.unit_prefix);
        run_status(
            "docker",
            &[
                "--host",
                &host,
                "build",
                "--network=none",
                "--label",
                &label,
                "--tag",
                &tag,
                context.to_str().unwrap(),
            ],
        )?;
        tags.push(tag);
    }
    let scan = world.call(
        "storage.scan",
        json!({"idempotency_key":"engine-cache-initial"}),
    )?;
    ensure!(
        wait_job(world, data(&scan)?["job_id"].as_str().unwrap())?["state"] == "completed",
        "Engine API cache discovery failed"
    );
    let inventory = mcp(
        world,
        "storage_inventory",
        json!({"kind":"build_cache","limit":100}),
    )?;
    let networks = mcp(
        world,
        "storage_inventory",
        json!({"kind":"network","query":network,"limit":20}),
    )?;
    let network_row = networks["artifacts"]
        .as_array()
        .and_then(|r| r.first())
        .ok_or("isolated network was not inventoried")?;
    let network_plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[network_row["artifact_id"]]}),
    )?;
    ensure!(
        network_plan["ready"] == true,
        "unused isolated network was not removable"
    );
    let network_job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":network_plan["plan_id"],"idempotency_key":"remove-isolated-network"}),
    )?;
    ensure!(
        wait_job(world, network_job["job_id"].as_str().unwrap())?["state"] == "completed",
        "exact network cleanup did not complete"
    );
    let initial = inventory["artifacts"]
        .as_array()
        .ok_or("cache rows missing")?;
    ensure!(
        !initial.is_empty(),
        "old Buildx formatter prevented structured Engine cache discovery"
    );
    ensure!(
        initial.iter().any(|r| r["reasons"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("shared_image_layers")))),
        "shared image cache was not protected"
    );
    let shared = initial
        .iter()
        .find(|r| {
            r["reasons"]
                .as_array()
                .is_some_and(|v| v.contains(&json!("shared_image_layers")))
        })
        .unwrap();
    let refusal = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[shared["artifact_id"]]}),
    )?;
    ensure!(
        refusal["ready"] == false,
        "a shared cache entry acquired a removal plan"
    );
    // Release only our selected image through the normal storage contract.
    let images = mcp(
        world,
        "storage_inventory",
        json!({"kind":"image","query":tags[0],"limit":100}),
    )?;
    let image = images["artifacts"]
        .as_array()
        .and_then(|v| v.first())
        .ok_or("owned fixture image missing")?;
    mcp(
        world,
        "storage_register",
        json!({"artifact_id":image["artifact_id"],"expected_revision":image["revision"],"repository_id":repository,"effect":"rebuildable","reason":"Owned isolated builder fixture image"}),
    )?;
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[image["artifact_id"]]}),
    )?;
    ensure!(
        plan["ready"] == true,
        "fixture image removal was blocked: {}",
        bounded_json(&plan)
    );
    let job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"remove-cache-image"}),
    )?;
    ensure!(
        wait_job(world, job["job_id"].as_str().unwrap())?["state"] == "completed",
        "fixture image was not removed"
    );
    let scan = world.call(
        "storage.scan",
        json!({"idempotency_key":"engine-cache-after-image"}),
    )?;
    ensure!(
        wait_job(world, data(&scan)?["job_id"].as_str().unwrap())?["state"] == "completed",
        "cache refresh failed"
    );
    let inventory = mcp(
        world,
        "storage_inventory",
        json!({"kind":"build_cache","limit":100}),
    )?;
    let rows = inventory["artifacts"].as_array().unwrap();
    let selected = rows
        .iter()
        .find(|r| r["deletable"] == true)
        .ok_or("no unused isolated cache record became removable")?;
    let protected = rows
        .iter()
        .find(|r| r["deletable"] == false)
        .ok_or("retained image cache disappeared")?;
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[selected["artifact_id"]]}),
    )?;
    ensure!(plan["ready"] == true, "unused cache plan was blocked");
    let job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"remove-one-engine-cache"}),
    )?;
    let result = wait_job(world, job["job_id"].as_str().unwrap())?;
    ensure!(
        result["state"] == "completed",
        "exact Engine cache removal failed: {}",
        bounded_json(&result)
    );
    let kept = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":protected["artifact_id"]}),
    )?;
    ensure!(
        kept["removed_at_ms"].is_null(),
        "exact cache pruning removed an unselected shared record"
    );
    ensure!(
        result["unmeasured_items"] == 0,
        "cache removal did not retain its filesystem measurement"
    );
    run_status(
        "docker",
        &[
            "--host", &host, "image", "inspect", "--format", "{{.Id}}", &tags[1],
        ],
    )?;
    Ok(())
}

pub(super) fn retained_evidence_after_run(world: &World, run_id: &str) -> Result<(), String> {
    let repository = ids::repository_id(&world.repo).map_err(|e| e.to_string())?;
    let inventory = scanned(world, &repository, "retained-evidence-inventory")?;
    let evidence = inventory["artifacts"]
        .as_array()
        .ok_or("evidence inventory missing")?
        .iter()
        .filter(|r| r["kind"] == "evidence")
        .collect::<Vec<_>>();
    ensure!(
        evidence.len() == 1,
        "real retained run missing from storage inventory: {}",
        bounded_json(&inventory)
    );
    let row = evidence[0];
    ensure!(
        row["automatic_eligible"] == false && row["eligible_at_ms"].is_null(),
        "storage replaced the existing evidence retention policy"
    );
    let pin = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":row["artifact_id"],"expected_revision":row["revision"],"protected":true}),
    )?;
    let blocked = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[row["artifact_id"]],"include_persistent_data":true}),
    )?;
    ensure!(blocked["ready"] == false, "pinned evidence was removable");
    ensure!(
        !world.log_catalog(run_id, "main")?["entries"]
            .as_array()
            .unwrap()
            .is_empty(),
        "pinning destroyed retained evidence"
    );
    mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":row["artifact_id"],"expected_revision":pin["revision"],"protected":false}),
    )?;
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[row["artifact_id"]],"include_persistent_data":true}),
    )?;
    ensure!(
        plan["ready"] == true,
        "manual evidence plan remained blocked: {}",
        bounded_json(&plan)
    );
    let job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"remove-retained-evidence"}),
    )?;
    let removed = wait_job(
        world,
        job["job_id"]
            .as_str()
            .ok_or("evidence cleanup job missing")?,
    )?;
    ensure!(
        removed["state"] == "completed",
        "existing evidence engine did not remove the run: {}",
        bounded_json(&removed)
    );
    let after = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":row["artifact_id"]}),
    )?;
    ensure!(
        !after["removed_at_ms"].is_null(),
        "evidence receipt did not persist"
    );
    let tail = world.log_tail(run_id, "main", "stdout");
    ensure!(
        tail.is_err()
            || tail
                .as_ref()
                .is_ok_and(|v| !response_text(v).contains("uid=")),
        "retired evidence payload remains available"
    );
    let history = mcp(
        world,
        "storage_history",
        json!({"artifact_id":row["artifact_id"]}),
    )?;
    ensure!(
        history["jobs"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["job_id"] == job["job_id"])),
        "retention lost the cleanup receipt"
    );
    Ok(())
}

pub(super) fn cancellation_receipts(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned(".gitignore", "cache/\n")?;
    for n in 0..16 {
        world.write_owned(&format!("cache/item-{n}/data"), vec![7u8; 4096])?;
    }
    let registered = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registered)?["repository_id"]
        .as_str()
        .ok_or("repository missing")?
        .to_owned();
    data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Cancellation fixture","path":world.repo.join("cache"),"kind":"dependency_cache"}))?)?;
    let inventory = scanned(world, &repo, "cancellation-inventory")?;
    let ids = inventory["artifacts"]
        .as_array()
        .ok_or("cancellation artifacts missing")?
        .iter()
        .filter(|r| r["kind"] == "dependency_cache")
        .map(|r| r["artifact_id"].clone())
        .collect::<Vec<_>>();
    ensure!(ids.len() == 16, "cancellation fixture inventory incomplete");
    let plan = mcp(world, "storage_cleanup_plan", json!({"artifact_ids":ids}))?;
    ensure!(plan["ready"] == true, "cancellation plan was not ready");
    let started = world.call(
        "storage.cleanup.start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"cancel-during-native-cleanup"}),
    )?;
    let id = data(&started)?["job_id"]
        .as_str()
        .ok_or("cancel job identity missing")?
        .to_owned();
    mcp(world, "storage_job_cancel", json!({"job_id":id}))?;
    let result = wait_job(world, &id)?;
    let remaining = (0..16)
        .filter(|n| world.repo.join(format!("cache/item-{n}/data")).exists())
        .count();
    let mut failures = Vec::new();
    if result["state"] != "cancelled" {
        failures.push(format!("cancelled operation ended as {}", result["state"]));
    }
    if result["receipts"]
        .as_array()
        .is_none_or(|rows| rows.len() != 16)
    {
        failures.push("cancellation omitted per-item receipts".into());
    }
    if remaining == 0 {
        failures.push("cancellation did not preserve remaining data".into());
    }
    world.stop_daemon(false)?;
    world.start_daemon(None, None, None)?;
    let retained = mcp(world, "storage_job_status", json!({"job_id":id}))?;
    if retained["state"] != result["state"] || retained["receipts"] != result["receipts"] {
        failures.push("restart changed the cancellation receipt".into());
    }
    if (0..16)
        .filter(|n| world.repo.join(format!("cache/item-{n}/data")).exists())
        .count()
        != remaining
    {
        failures.push("cancelled cleanup resumed after restart".into());
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(())
}

pub(super) fn current_stopped_data(world: &World, volume: &str) -> Result<(), String> {
    let repository = ids::repository_id(&world.repo).map_err(|e| e.to_string())?;
    let inventory = scanned(world, &repository, "current-stopped-deployment-storage")?;
    let data_row = inventory["artifacts"]
        .as_array()
        .ok_or("stopped deployment inventory missing")?
        .iter()
        .find(|r| r["kind"] == "volume" && r["name"] == volume)
        .ok_or("current stopped database volume missing from inventory")?;
    ensure!(
        data_row["deletable"] == false && data_row["automatic_eligible"] == false,
        "a current stopped database was labelled safe"
    );
    ensure!(
        data_row["reasons"]
            .as_array()
            .is_some_and(|r| r.contains(&json!("current_deployment"))),
        "current deployment protection reason missing"
    );
    let pin = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":data_row["artifact_id"],"expected_revision":data_row["revision"],"protected":true}),
    )?;
    let released = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":data_row["artifact_id"],"expected_revision":pin["revision"],"protected":false}),
    )?;
    ensure!(
        released["deletable"] == false,
        "removing an explicit pin overrode current deployment protection"
    );
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[data_row["artifact_id"]],"include_persistent_data":true}),
    )?;
    ensure!(
        plan["ready"] == false,
        "current stopped database acquired a cleanup plan"
    );
    ensure!(
        volume_exists(volume)?,
        "storage inspection changed the current database volume"
    );
    Ok(())
}

pub(super) fn shared_alias_protection(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned(".gitignore", "cache/\n")?;
    world.write_owned("cache/shared/data", vec![6u8; 16384])?;
    let alias = world.base.join("cache-alias");
    fs::create_dir(&alias).map_err(|e| e.to_string())?;
    let output = Command::new("systemd-escape")
        .args(["--path", "--suffix=mount"])
        .arg(&alias)
        .output()
        .map_err(|e| e.to_string())?;
    ensure!(output.status.success(), "alias fixture unit unavailable");
    world.cleanup_storage_mount_units.push(
        String::from_utf8(output.stdout)
            .map_err(|e| e.to_string())?
            .trim()
            .into(),
    );
    run_status(
        "systemd-mount",
        &[
            "--collect",
            "--type=none",
            "--options=bind",
            world.repo.join("cache").to_str().unwrap(),
            alias.to_str().unwrap(),
        ],
    )?;
    let registered = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registered)?["repository_id"]
        .as_str()
        .ok_or("alias repository missing")?
        .to_owned();
    for (label, path) in [
        ("Original", world.repo.join("cache")),
        ("Alias", alias.clone()),
    ] {
        data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":label,"path":path,"kind":"dependency_cache"}))?)?;
    }
    let inventory = scanned(world, &repo, "shared-aliases")?;
    let rows = inventory["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"] == "shared")
        .cloned()
        .collect::<Vec<_>>();
    ensure!(rows.len() == 2, "both bind aliases were not inventoried");
    ensure!(
        rows[0]["accounting_id"] == rows[1]["accounting_id"],
        "bind aliases were counted as different data"
    );
    let pin = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":rows[0]["artifact_id"],"expected_revision":rows[0]["revision"],"protected":true}),
    )?;
    let other = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":rows[1]["artifact_id"]}),
    )?;
    ensure!(
        other["deletable"] == false && other["safety"] == "protected",
        "a second alias bypassed protection"
    );
    let blocked = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[rows[1]["artifact_id"]]}),
    )?;
    ensure!(
        blocked["ready"] == false,
        "protected shared data acquired a plan through another alias"
    );
    mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":rows[0]["artifact_id"],"expected_revision":pin["revision"],"protected":false}),
    )?;
    let lease = mcp(
        world,
        "storage_lease_set",
        json!({"artifact_ids":[rows[0]["artifact_id"]],"duration_seconds":300}),
    )?;
    let other = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":rows[1]["artifact_id"]}),
    )?;
    ensure!(
        other["deletable"] == false && other["safety"] == "in_use",
        "a second alias bypassed active use"
    );
    mcp(
        world,
        "storage_lease_release",
        json!({"lease_id":lease["lease_id"]}),
    )?;
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[rows[0]["artifact_id"],rows[1]["artifact_id"]]}),
    )?;
    ensure!(
        plan["ready"] == true,
        "released aliases were not removable: {}",
        bounded_json(&plan)
    );
    ensure!(
        plan["reclaimable_bytes"] == rows[0]["allocated_bytes"],
        "cleanup plan counted shared data twice"
    );
    let start = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"remove-shared-aliases"}),
    )?;
    let done = wait_job(world, start["job_id"].as_str().unwrap())?;
    ensure!(
        done["state"] == "completed",
        "shared reference removal failed: {}",
        bounded_json(&done)
    );
    ensure!(
        !alias.join("shared").exists() && !world.repo.join("cache/shared").exists(),
        "shared data remained visible through an alias"
    );
    ensure!(
        done["receipts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["code"] == "removed_shared_reference")
            .count()
            == 1,
        "cleanup tried to delete the same data twice"
    );
    world.write_owned("cache/tree/leaf/data", vec![8u8; 4096])?;
    world.write_owned("cache/sibling/data", vec![9u8; 4096])?;
    data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Nested cache","path":world.repo.join("cache/tree"),"kind":"dependency_cache"}))?)?;
    let inventory = scanned(world, &repo, "nested-protection")?;
    let all = inventory["artifacts"].as_array().unwrap();
    let leaf = all
        .iter()
        .find(|r| r["name"] == "leaf")
        .ok_or("nested leaf missing")?;
    let tree = all
        .iter()
        .find(|r| r["name"] == "tree")
        .ok_or("parent tree missing")?;
    let sibling = all
        .iter()
        .find(|r| r["name"] == "sibling")
        .ok_or("sibling tree missing")?;
    let pin = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":leaf["artifact_id"],"expected_revision":leaf["revision"],"protected":true}),
    )?;
    let parent = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":tree["artifact_id"]}),
    )?;
    ensure!(
        parent["safety"] == "protected" && parent["deletable"] == false,
        "parent removal bypassed a protected descendant"
    );
    let unrelated = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":sibling["artifact_id"]}),
    )?;
    ensure!(
        unrelated["deletable"] == true,
        "nested protection incorrectly blocked a sibling"
    );
    mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":leaf["artifact_id"],"expected_revision":pin["revision"],"protected":false}),
    )?;
    let lease = mcp(
        world,
        "storage_lease_set",
        json!({"artifact_ids":[leaf["artifact_id"]],"duration_seconds":300}),
    )?;
    let parent = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":tree["artifact_id"]}),
    )?;
    ensure!(
        parent["safety"] == "in_use" && parent["deletable"] == false,
        "parent removal bypassed a descendant lease"
    );
    let released = mcp(
        world,
        "storage_lease_release",
        json!({"lease_id":lease["lease_id"]}),
    )?;
    let parent = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":tree["artifact_id"]}),
    )?;
    ensure!(
        parent["last_used_at_ms"].as_u64() >= released["expires_at_ms"].as_u64(),
        "parent inactivity ignored recent descendant use"
    );
    let mut failures = Vec::new();
    let nested = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[leaf["artifact_id"],tree["artifact_id"]]}),
    )?;
    ensure!(
        nested["ready"] == true,
        "nested cleanup is unexpectedly blocked"
    );
    if nested["reclaimable_bytes"] != tree["allocated_bytes"] {
        failures.push("nested cleanup counted descendant bytes twice");
    }
    let parent_only = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[tree["artifact_id"]]}),
    )?;
    if !parent_only["items"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|r| r["artifact_id"] == leaf["artifact_id"])
    }) {
        failures.push("parent cleanup omitted the nested artifact from its exact targets");
    }
    let job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":nested["plan_id"],"idempotency_key":"remove-nested-once"}),
    )?;
    let result = wait_job(
        world,
        job["job_id"].as_str().ok_or("nested cleanup job missing")?,
    )?;
    if result["state"] != "completed"
        || result["receipts"]
            .as_array()
            .is_none_or(|rows| rows.len() != 2 || rows.iter().any(|r| r["status"] != "removed"))
    {
        failures.push("nested cleanup did not retain one successful receipt per selected identity");
    }
    if world.repo.join("cache/tree").exists() || !world.repo.join("cache/sibling/data").exists() {
        failures.push("nested cleanup left selected data or affected a sibling");
    }
    ensure!(
        failures.is_empty(),
        "nested cleanup failures: {}",
        failures.join("; ")
    );
    Ok(())
}

pub(super) fn legacy_docker_consumers(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    let registration = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registration)?["repository_id"]
        .as_str()
        .ok_or("repository id missing")?
        .to_owned();
    let project = format!("{}-legacy", world.unit_prefix);
    let volume = format!("{project}-data");
    world.track_volume(volume.clone());
    let instance = format!("devcoordinator2.instance={}", world.unit_prefix);
    let project_label = format!("com.docker.compose.project={project}");
    run_status(
        "docker",
        &[
            "volume",
            "create",
            "--label",
            &instance,
            "--label",
            &project_label,
            &volume,
        ],
    )?;
    let backing = world.base.join("legacy-backing");
    fs::create_dir(&backing).map_err(|e| e.to_string())?;
    let mountpoint = Command::new("docker")
        .args(["volume", "inspect", "--format", "{{.Mountpoint}}", &volume])
        .output()
        .map_err(|e| e.to_string())?;
    ensure!(
        mountpoint.status.success(),
        "fixture volume mountpoint unavailable"
    );
    let mountpoint = PathBuf::from(
        String::from_utf8(mountpoint.stdout)
            .map_err(|e| e.to_string())?
            .trim(),
    );
    let fstab = format!(
        "# preserve this unrelated configuration\n{} {} none bind,nofail,x-systemd.automount 0 0\n",
        backing.display(),
        mountpoint.display()
    );
    fs::write(world.state.join("storage-fixture.fstab"), &fstab).map_err(|e| e.to_string())?;
    let commit = devcoordinator2_control::SOURCE_COMMIT;
    let binary_hash = sha256_hex(&fs::read(&world.harness.daemon).map_err(|e| e.to_string())?);
    write_private_json(
        &world.state.join("storage-fixture-install.json"),
        &json!({"source_commit":commit,"binaries":[{"name":"devcoordinator2","path":world.harness.daemon,"sha256":binary_hash,"source_commit":commit}]}),
    )?;
    for suffix in ["automount", "mount"] {
        let output = Command::new("systemd-escape")
            .arg("--path")
            .arg(format!("--suffix={suffix}"))
            .arg(&mountpoint)
            .output()
            .map_err(|e| e.to_string())?;
        ensure!(
            output.status.success(),
            "fixture mount unit identity unavailable"
        );
        world.cleanup_storage_mount_units.push(
            String::from_utf8(output.stdout)
                .map_err(|e| e.to_string())?
                .trim()
                .into(),
        );
    }
    run_status(
        "systemd-mount",
        &[
            "--no-block",
            "--collect",
            "--automount=yes",
            "--type=none",
            "--options=bind",
            backing.to_str().ok_or("fixture path invalid")?,
            mountpoint.to_str().ok_or("fixture path invalid")?,
        ],
    )?;
    // Access deliberately activates the real automount before Docker consumes it.
    fs::read_dir(&mountpoint).map_err(|e| e.to_string())?;
    let mount = format!("type=volume,source={volume},target=/data");
    run_status(
        "docker",
        &[
            "run",
            "--rm",
            "--network=none",
            "--label",
            &instance,
            "--mount",
            &mount,
            "--entrypoint",
            "/bin/sh",
            "postgres:16-alpine",
            "-c",
            "printf retained-fixture > /data/storage-fixture",
        ],
    )?;
    let mut ids = Vec::new();
    for name in ["service", "bootstrap"] {
        let output = Command::new("docker")
            .args([
                "create",
                "--network=none",
                "--label",
                &instance,
                "--label",
                &project_label,
                "--name",
                &format!("{project}-{name}"),
                "--mount",
                &mount,
                "--entrypoint",
                "/bin/sh",
                "postgres:16-alpine",
                "-c",
                "exit 0",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        ensure!(
            output.status.success(),
            "cannot create owned legacy fixture"
        );
        let id = String::from_utf8(output.stdout)
            .map_err(|e| e.to_string())?
            .trim()
            .to_owned();
        ensure!(id.len() == 64, "legacy fixture identity is not exact");
        ids.push(id);
    }
    world.stop_daemon(false)?;
    let database =
        Database::open(world.state.join("authority.sqlite3")).map_err(|e| e.to_string())?;
    let store = DeploymentStore::new(database.clone());
    let deployment = "d1234567890abcdef";
    store
        .replace_observed_current(
            &[ObservedDeploymentInput {
                deployment_id: deployment.into(),
                repository_id: repo.clone(),
                name: project.clone(),
                native_project: project.clone(),
                state: "stopped".into(),
                health: "unknown".into(),
                evidence: json!({"fixture":"owned legacy retirement"}),
            }],
            &[ObservedContainerInput {
                container_id: ids[0].clone(),
                deployment_id: deployment.into(),
                repository_id: repo.clone(),
                name: format!("{project}-service"),
                image: "postgres:16-alpine".into(),
                compose_service: "service".into(),
                status: "created".into(),
                health: "none".into(),
            }],
            &[],
            "2026-10-02T00:00:00Z",
        )
        .map_err(|e| e.to_string())?;
    database.close().map_err(|e| e.to_string())?;
    world.start_daemon(None, None, None)?;
    let refusal = world.call(
        "deployment.remove",
        json!({"deployment_id":deployment,"delete_data":true}),
    )?;
    ensure!(
        error_code(&refusal) == Some("observed_only"),
        "ordinary deployment removal lost its observed-only guard"
    );
    let scan = world.call(
        "storage.scan",
        json!({"repository_id":repo,"idempotency_key":"legacy-scan"}),
    )?;
    let job = wait_job(
        world,
        data(&scan)?["job_id"].as_str().ok_or("scan id missing")?,
    )?;
    ensure!(
        job["state"] == "completed",
        "legacy scan failed: {}",
        bounded_json(&job)
    );
    let inventory = world.call(
        "storage.inventory",
        json!({"repository_id":repo,"limit":100}),
    )?;
    let inventory = data(&inventory)?;
    let rows = inventory["artifacts"]
        .as_array()
        .ok_or("legacy artifact rows missing")?;
    ensure!(
        rows.iter().filter(|r| r["kind"] == "container").count() == 2,
        "discovery missed the unrecorded bootstrap consumer"
    );
    let target = rows
        .iter()
        .find(|r| r["kind"] == "volume" && r["name"] == volume)
        .ok_or("legacy volume missing")?;
    ensure!(
        target["deletable"] == false,
        "unreviewed legacy data was labelled safe"
    );
    let registered=world.call("storage.legacy.register",json!({"deployment_id":deployment,"expected_inventory_revision":inventory["revision"],"reason":"Owned acceptance fixture is retired and disposable"}))?;
    let registered = wait_job(
        world,
        data(&registered)?["job_id"]
            .as_str()
            .ok_or("registration job missing")?,
    )?;
    ensure!(
        registered["state"] == "completed",
        "legacy ownership verification failed: {}",
        bounded_json(&registered)
    );
    let plan = world.call(
        "storage.cleanup.plan",
        json!({"artifact_ids":[target["artifact_id"]],"include_persistent_data":true}),
    )?;
    let plan = data(&plan)?;
    ensure!(
        plan["ready"] == true,
        "registered legacy group remained blocked: {}",
        bounded_json(plan)
    );
    ensure!(
        plan["items"].as_array().is_some_and(|items| items
            .iter()
            .filter(|r| r["kind"] == "container")
            .count()
            == 2),
        "cleanup omitted a real consumer"
    );
    fs::write(
        world.state.join("storage-crash-after-remove"),
        b"verify whole-group recovery metadata",
    )
    .map_err(|e| e.to_string())?;
    let start = world.call(
        "storage.cleanup.start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"retire-legacy"}),
    )?;
    let cleanup_id = data(&start)?["job_id"]
        .as_str()
        .ok_or("cleanup job missing")?
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if world
            .daemon
            .as_mut()
            .ok_or("fixture daemon missing")?
            .try_wait()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "legacy cleanup did not reach its interruption boundary"
        );
        thread::sleep(Duration::from_millis(50));
    }
    world.daemon.take();
    let recovery = fs::read_dir(world.state.join("storage-recovery"))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    ensure!(
        recovery.len() == 4,
        "the whole group's recovery metadata was not saved before removal"
    );
    let mut metadata_bytes = 0;
    for item in recovery {
        let metadata = item.metadata().map_err(|e| e.to_string())?;
        ensure!(
            metadata.uid() == 0 && metadata.mode() & 0o077 == 0,
            "recovery metadata was not private"
        );
        metadata_bytes += metadata.len();
    }
    ensure!(
        metadata_bytes < 1024 * 1024,
        "cleanup copied disposable data instead of recovery metadata"
    );
    ensure!(
        volume_exists(&volume)? && backing.join("storage-fixture").is_file(),
        "interruption removed data before retiring its consumers"
    );
    world.start_daemon(None, None, None)?;
    let result = wait_job(world, &cleanup_id)?;
    ensure!(
        result["state"] == "completed",
        "legacy cleanup failed: {}",
        bounded_json(&result)
    );
    ensure!(
        !volume_exists(&volume)?,
        "legacy volume survived successful cleanup"
    );
    ensure!(!backing.exists(), "legacy backing data survived cleanup");
    ensure!(
        fs::read_to_string(world.state.join("storage-fixture.fstab")).map_err(|e| e.to_string())?
            == "# preserve this unrelated configuration\n",
        "retirement changed unrelated configuration"
    );
    ensure!(
        !devcoordinator2_control::storage::fs::mount_targets()
            .map_err(|e| e.message)?
            .contains(&mountpoint),
        "retired mount can still be accessed"
    );
    for id in ids {
        let output = Command::new("docker")
            .args(["container", "inspect", &id])
            .output()
            .map_err(|e| e.to_string())?;
        ensure!(!output.status.success(), "legacy consumer survived cleanup");
    }
    world.forget_volume(&volume);
    Ok(())
}

fn cli(world: &World, args: &[&str]) -> Result<Value, String> {
    let socket = format!("DEVCOORDINATOR2_SOCKET={}", world.socket.display());
    let program = world
        .harness
        .daemon
        .to_str()
        .ok_or("candidate path is not UTF-8")?;
    let mut all = vec![socket.as_str(), program];
    all.extend_from_slice(args);
    let text = command_stdout_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &world.repo,
        "/usr/bin/env",
        &all,
        &world.base,
    )?;
    serde_json::from_str(&text).map_err(|_| "normal client returned invalid JSON".into())
}

fn mcp(world: &World, name: &str, arguments: Value) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    tokio::runtime::Runtime::new().map_err(|e|e.to_string())?.block_on(async {
        tokio::time::timeout(Duration::from_secs(30),async {
            let mut child=tokio::process::Command::new("/usr/bin/setpriv")
                .args([format!("--reuid={}",world.harness.caller_uid),format!("--regid={}",world.harness.caller_gid),"--init-groups".into(),"--".into()])
                .arg(&world.harness.daemon).arg("mcp").env("DEVCOORDINATOR2_SOCKET",&world.socket)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true)
                .spawn().map_err(|e|e.to_string())?;
            let mut input=child.stdin.take().ok_or("MCP input missing")?;
            let mut lines=tokio::io::BufReader::new(child.stdout.take().ok_or("MCP output missing")?).lines();
            let initialize=json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"storage-acceptance","version":"1"}}});
            input.write_all(format!("{initialize}\n").as_bytes()).await.map_err(|e|e.to_string())?;
            let initialized:Value=serde_json::from_str(&lines.next_line().await.map_err(|e|e.to_string())?.ok_or("MCP initialization closed")?).map_err(|e|e.to_string())?;
            ensure!(initialized["id"]==1 && initialized.get("error").is_none(),"MCP initialization failed");
            input.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await.map_err(|e|e.to_string())?;
            let request=json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}});
            input.write_all(format!("{request}\n").as_bytes()).await.map_err(|e|e.to_string())?;
            loop {
                let line=lines.next_line().await.map_err(|e|e.to_string())?.ok_or("MCP tool call closed")?;
                let response:Value=serde_json::from_str(&line).map_err(|e|e.to_string())?;
                if response["id"]!=2 {continue;}
                ensure!(response.get("error").is_none() && response["result"]["isError"]!=true,"MCP storage operation failed: {}",bounded_json(&response));
                let result=response["result"]["structuredContent"].clone();
                ensure!(!result.is_null(),"MCP storage operation omitted its typed result");
                child.kill().await.map_err(|e|e.to_string())?;
                return Ok(result);
            }
        }).await.map_err(|_|"MCP storage operation exceeded its deadline".to_owned())?
    })
}

fn wait_job(world: &World, id: &str) -> Result<Value, String> {
    let deadline = time::OffsetDateTime::now_utc() + time::Duration::minutes(5);
    let deadline = deadline
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| e.to_string())?;
    let mut cursor = 0;
    loop {
        let response = world.call("storage.job.status", json!({"job_id":id}))?;
        let job = data(&response)?.clone();
        if matches!(
            job["state"].as_str(),
            Some("completed" | "partial" | "failed" | "cancelled")
        ) {
            return Ok(job);
        }
        let events=world.call("event.wait",json!({"cursor":cursor,"filters":[{"filter_id":"storage","categories":["other"],"kinds":["storage.job.finished","storage.job.failed"],"deadline_at":deadline}]}))?;
        let events = data(&events)?;
        cursor = events["cursor"].as_u64().ok_or("event cursor missing")?;
        ensure!(
            events["heartbeat_due"]
                .as_array()
                .is_none_or(|v| v.is_empty()),
            "storage job missed its completion deadline"
        );
    }
}

fn set_clock(world: &World, now: u64) -> Result<(), String> {
    let path = world.state.join("storage-test-clock-ms");
    fs::write(path.with_extension("next"), now.to_string()).map_err(|e| e.to_string())?;
    fs::rename(path.with_extension("next"), path).map_err(|e| e.to_string())
}

fn scanned(world: &World, repo: &str, key: &str) -> Result<Value, String> {
    let scan = world.call(
        "storage.scan",
        json!({"repository_id":repo,"idempotency_key":key}),
    )?;
    let finished = wait_job(
        world,
        data(&scan)?["job_id"]
            .as_str()
            .ok_or("scan identity missing")?,
    )?;
    ensure!(
        finished["state"] == "completed",
        "controlled-clock scan failed: {}",
        bounded_json(&finished)
    );
    let inventory = world.call(
        "storage.inventory",
        json!({"repository_id":repo,"limit":100}),
    )?;
    Ok(data(&inventory)?.clone())
}

fn wait_automatic_removal(world: &World, id: &str, path: &Path) -> Result<(), String> {
    let deadline = (time::OffsetDateTime::now_utc() + time::Duration::seconds(90))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| e.to_string())?;
    let mut cursor = 0;
    loop {
        let history = world.call("storage.history", json!({"artifact_id":id,"limit":20}))?;
        for job in data(&history)?["jobs"]
            .as_array()
            .ok_or("cleanup history missing")?
        {
            ensure!(
                job["state"] != "failed" && job["state"] != "partial",
                "automatic cleanup failed: {}",
                bounded_json(job)
            );
            if job["state"] == "completed" {
                ensure!(!path.exists(), "automatic receipt did not remove real data");
                return Ok(());
            }
        }
        let response=world.call("event.wait",json!({"cursor":cursor,"filters":[{"filter_id":"automatic-storage","categories":["other"],"kinds":["storage.job.finished","storage.job.failed"],"deadline_at":deadline}]}))?;
        let events = data(&response)?;
        cursor = events["cursor"].as_u64().ok_or("event cursor missing")?;
        ensure!(
            events["heartbeat_due"]
                .as_array()
                .is_none_or(|r| r.is_empty()),
            "automatic deletion missed its deadline"
        );
    }
}

pub(super) fn automatic_policy_boundaries(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned(".gitignore", "cache/\nretired/\n")?;
    let registered = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registered)?["repository_id"]
        .as_str()
        .ok_or("repository identity missing")?
        .to_owned();
    let start = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
    set_clock(world, start)?;
    for name in ["due", "leased", "pinned"] {
        world.write_owned(&format!("cache/{name}/output"), vec![42u8; 16384])?;
    }
    world.write_owned("retired/data/output", vec![51u8; 16384])?;
    data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Policy cache","path":world.repo.join("cache"),"kind":"dependency_cache"}))?)?;
    data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Retired data","path":world.repo.join("retired"),"kind":"unknown"}))?)?;
    let inventory = scanned(world, &repo, "policy-initial")?;
    let rows = inventory["artifacts"]
        .as_array()
        .ok_or("policy rows missing")?;
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .cloned()
            .ok_or_else(|| format!("policy artifact {name} missing"))
    };
    let due = row("due")?;
    let leased = row("leased")?;
    let pinned = row("pinned")?;
    let data_row = row("data")?;
    for r in [&due, &leased, &pinned, &data_row] {
        ensure!(
            r["automatic_eligible"] == false,
            "historical timestamps bypassed observation"
        );
    }
    ensure!(
        due["eligible_at_ms"].as_u64() == Some(start + 3 * 86_400_000),
        "cache default is not three days"
    );
    data(&world.call("storage.protection.set",json!({"artifact_id":pinned["artifact_id"],"expected_revision":pinned["revision"],"protected":true}))?)?;
    data(&world.call("storage.register",json!({"artifact_id":data_row["artifact_id"],"expected_revision":data_row["revision"],"repository_id":repo,"effect":"permanent_data","reason":"Disposable isolated policy fixture"}))?)?;
    set_clock(world, start + 3 * 86_400_000 - 1)?;
    let lease = world.call(
        "storage.lease.set",
        json!({"artifact_ids":[leased["artifact_id"]],"duration_seconds":86_400}),
    )?;
    data(&lease)?;
    scanned(world, &repo, "before-three-days")?;
    let history = mcp(world, "storage_history", json!({"limit":50}))?;
    let pending_scans = history["jobs"]
        .as_array()
        .ok_or("scan history missing")?
        .iter()
        .filter(|job| {
            job["kind"] == "scan" && matches!(job["state"].as_str(), Some("queued" | "running"))
        })
        .count();
    ensure!(
        pending_scans <= 1,
        "the storage clock queued duplicate background scans before eligibility"
    );
    ensure!(
        world.repo.join("cache/due/output").exists(),
        "cache was deleted before the three-day boundary"
    );
    set_clock(world, start + 3 * 86_400_000)?;
    scanned(world, &repo, "at-three-days")?;
    wait_automatic_removal(
        world,
        due["artifact_id"].as_str().unwrap(),
        &world.repo.join("cache/due"),
    )?;
    ensure!(
        world.repo.join("cache/leased/output").exists()
            && world.repo.join("cache/pinned/output").exists(),
        "active lease or pin did not protect data"
    );
    set_clock(world, start + 14 * 86_400_000 - 1)?;
    scanned(world, &repo, "before-fourteen-days")?;
    ensure!(
        world.repo.join("retired/data/output").exists(),
        "persistent data was deleted before fourteen days"
    );
    set_clock(world, start + 14 * 86_400_000)?;
    scanned(world, &repo, "at-fourteen-days")?;
    wait_automatic_removal(
        world,
        data_row["artifact_id"].as_str().unwrap(),
        &world.repo.join("retired/data"),
    )?;
    ensure!(
        world.repo.join("cache/pinned/output").exists(),
        "an explicit pin expired with the policy"
    );
    // A project override changes only that project's next observation deadline.
    data(&world.call("storage.policy.set",json!({"repository_id":repo,"expected_revision":0,"automatic":true,"cache_idle_seconds":5*86_400,"data_idle_seconds":14*86_400,"minimum_verified_backups":2}))?)?;
    world.write_owned("cache/override/output", vec![19u8; 8192])?;
    let observed = scanned(world, &repo, "override-observation")?;
    let overridden = observed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "override")
        .ok_or("override artifact missing")?
        .clone();
    ensure!(
        overridden["eligible_at_ms"].as_u64() == Some(start + 19 * 86_400_000),
        "project override was not applied"
    );
    set_clock(world, start + 19 * 86_400_000 - 1)?;
    scanned(world, &repo, "before-override-deadline")?;
    ensure!(
        world.repo.join("cache/override/output").exists(),
        "project override deleted early"
    );
    set_clock(world, start + 19 * 86_400_000)?;
    scanned(world, &repo, "at-override-deadline")?;
    wait_automatic_removal(
        world,
        overridden["artifact_id"].as_str().unwrap(),
        &world.repo.join("cache/override"),
    )?;
    Ok(())
}

pub(super) fn worktrees_and_backup_floor(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned("source.txt", "authoritative fixture source\n")?;
    world.write_owned(".gitignore", "backups/\n")?;
    world.git(&["add", "."])?;
    world.git(&[
        "-c",
        "user.name=Storage fixture",
        "-c",
        "user.email=storage@example.invalid",
        "commit",
        "-qm",
        "source baseline",
    ])?;
    let origin = world.base.join("origin.git");
    fs::create_dir(&origin).map_err(|e| e.to_string())?;
    chown_path(&origin, world.harness.caller_uid, world.harness.caller_gid)?;
    run_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &world.repo,
        "/usr/bin/git",
        &["init", "--bare", "-q", origin.to_str().unwrap()],
        &world.base,
    )?;
    run_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &origin,
        "/usr/bin/git",
        &["symbolic-ref", "HEAD", "refs/heads/main"],
        &world.base,
    )?;
    world.git(&["remote", "add", "origin", origin.to_str().unwrap()])?;
    world.git(&["push", "-q", "origin", "HEAD:main"])?;
    let registration = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registration)?["repository_id"]
        .as_str()
        .ok_or("repository missing")?
        .to_owned();
    let mut trees = Vec::new();
    let worktrees = world.base.join("worktrees");
    fs::create_dir(&worktrees).map_err(|e| e.to_string())?;
    chown_path(
        &worktrees,
        world.harness.caller_uid,
        world.harness.caller_gid,
    )?;
    for name in ["clean-worktree", "dirty-worktree", "unique-worktree"] {
        let path = worktrees.join(name);
        fs::create_dir(&path).map_err(|e| e.to_string())?;
        chown_path(&path, world.harness.caller_uid, world.harness.caller_gid)?;
        world.git(&[
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ])?;
        if name != "clean-worktree" {
            data(&world.call("repository.register", json!({"path":path}))?)?;
        }
        trees.push(path);
    }
    fs::write(trees[1].join("source.txt"), "valuable uncommitted changes")
        .map_err(|e| e.to_string())?;
    fs::write(trees[2].join("source.txt"), "valuable unique commit").map_err(|e| e.to_string())?;
    run_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &trees[2],
        "/usr/bin/git",
        &[
            "-c",
            "user.name=Storage fixture",
            "-c",
            "user.email=storage@example.invalid",
            "commit",
            "-am",
            "unique work",
            "-q",
        ],
        &world.base,
    )?;
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
    for (index, name) in ["old", "recent", "latest", "required-rollback"]
        .iter()
        .enumerate()
    {
        let bytes = format!("verified recovery generation {name}");
        world.write_owned(&format!("backups/{name}/data"), &bytes)?;
        let manifest = json!({"lineage":"fixture-database","created_at_ms":now-10000+(if index==3 {0} else {index as u64+1})*1000,"files":[{"file":"data","sha256":sha256_hex(bytes.as_bytes())}]});
        world.write_owned(
            &format!("backups/{name}/backup-manifest.json"),
            manifest.to_string(),
        )?;
    }
    world.write_owned("backups/invalid/data", "damaged backup")?;
    world.write_owned("backups/invalid/backup-manifest.json",json!({"lineage":"fixture-database","created_at_ms":now,"files":[{"file":"data","sha256":"0".repeat(64)}]}).to_string())?;
    data(&world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Recovery generations","path":world.repo.join("backups"),"kind":"backup"}))?)?;
    let inventory = scanned(world, &repo, "worktree-and-backup-inventory")?;
    let rows = inventory["artifacts"]
        .as_array()
        .ok_or("storage inventory missing")?;
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .cloned()
            .ok_or_else(|| format!("missing storage artifact {name}"))
    };
    let clean = row("clean-worktree")?;
    let dirty = row("dirty-worktree")?;
    let unique = row("unique-worktree")?;
    ensure!(
        dirty["deletable"] == false && unique["deletable"] == false,
        "valuable worktree was labelled safe"
    );
    let reason_codes = |r: &Value| r["reasons"].as_array().cloned().unwrap_or_default();
    ensure!(
        reason_codes(&dirty).contains(&json!("dirty_worktree")),
        "dirty worktree reason missing"
    );
    ensure!(
        reason_codes(&unique).contains(&json!("unique_worktree_commits")),
        "unique commit reason missing"
    );
    let registered = mcp(
        world,
        "storage_register",
        json!({"artifact_id":clean["artifact_id"],"expected_revision":clean["revision"],"repository_id":repo,"effect":"source_worktree","reason":"Verified disposable fixture worktree has no remaining owner"}),
    )?;
    ensure!(
        registered["deletable"] == true,
        "clean retired worktree remained blocked: {}",
        bounded_json(&registered)
    );
    let old = row("old")?;
    let recent = row("recent")?;
    let latest = row("latest")?;
    let rollback = row("required-rollback")?;
    ensure!(
        recent["safety"] == "protected" && latest["safety"] == "protected",
        "two verified generations were not protected"
    );
    ensure!(
        row("invalid")?["deletable"] == false,
        "corrupt backup was counted as verified"
    );
    let pin = mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":rollback["artifact_id"],"expected_revision":rollback["revision"],"protected":true}),
    )?;
    ensure!(pin["deletable"] == false, "required rollback pin failed");
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[old["artifact_id"],clean["artifact_id"]],"include_persistent_data":true}),
    )?;
    ensure!(
        plan["ready"] == true,
        "verified source and backup cleanup plan failed: {}",
        bounded_json(&plan)
    );
    let job = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"remove-worktree-and-old-backup"}),
    )?;
    let result = wait_job(
        world,
        job["job_id"].as_str().ok_or("MCP cleanup job missing")?,
    )?;
    ensure!(
        result["state"] == "completed",
        "real worktree/backup cleanup failed: {}",
        bounded_json(&result)
    );
    ensure!(
        !trees[0].exists() && !world.repo.join("backups/old").exists(),
        "real worktree or backup remained after cleanup"
    );
    for name in ["recent", "latest", "required-rollback", "invalid"] {
        ensure!(
            world.repo.join("backups").join(name).join("data").is_file(),
            "cleanup removed a protected or unverified backup"
        );
    }
    ensure!(
        trees[1].join("source.txt").is_file() && trees[2].join("source.txt").is_file(),
        "cleanup removed valuable work"
    );
    ensure!(
        fs::read_to_string(world.repo.join("source.txt")).map_err(|e| e.to_string())?
            == "authoritative fixture source\n",
        "cleanup changed the source repository"
    );
    Ok(())
}

pub(super) fn real_files_and_protection(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned(".gitignore", "cache/\n")?;
    world.write_owned("source.txt", "valuable tracked source\n")?;
    world.git(&["add", "."])?;
    world.git(&[
        "-c",
        "user.name=Storage fixture",
        "-c",
        "user.email=storage@example.invalid",
        "commit",
        "-qm",
        "storage fixture",
    ])?;
    let registration = world.call("repository.register", json!({"path":world.repo}))?;
    let repo = data(&registration)?["repository_id"]
        .as_str()
        .ok_or("repository id missing")?
        .to_owned();
    world.write_owned("cache/disposable/output.bin", vec![73_u8; 16384])?;
    world.write_owned("cache/changed/output.bin", vec![91_u8; 16384])?;
    world.write_owned("cache/interrupted/output.bin", vec![33_u8; 16384])?;
    let root=world.call("storage.roots.set",json!({"repository_id":repo,"expected_revision":0,"label":"Isolated build cache","path":world.repo.join("cache"),"kind":"dependency_cache"}))?;
    data(&root)?;
    let began = Instant::now();
    let started = cli(
        world,
        &[
            "storage",
            "scan",
            "--repository-id",
            &repo,
            "--idempotency-key",
            "storage-initial",
        ],
    )?;
    world
        .measurements
        .insert("scan_submission_ms".into(), began.elapsed().as_millis());
    let scan_id = data(&started)?["job_id"]
        .as_str()
        .ok_or("scan id missing")?
        .to_owned();
    let scan = wait_job(world, &scan_id)?;
    ensure!(
        scan["state"] == "completed",
        "scan did not complete: {}",
        bounded_json(&scan)
    );
    let began = Instant::now();
    let inventory = mcp(world, "storage_inventory", json!({"repository_id":repo}))?;
    world
        .measurements
        .insert("inventory_read_ms".into(), began.elapsed().as_millis());
    let rows = inventory["artifacts"]
        .as_array()
        .ok_or("artifact rows missing")?;
    let target = rows
        .iter()
        .find(|r| r["name"] == "disposable")
        .ok_or_else(|| format!("generated artifact missing: {}", bounded_json(&inventory)))?
        .clone();
    ensure!(
        target["safety"] == "safe",
        "unused isolated cache was not safe: {}",
        bounded_json(&target)
    );
    ensure!(
        target["allocated_bytes"].as_u64().unwrap_or(0) >= 16384,
        "size was not measured"
    );
    ensure!(
        target["automatic_eligible"] == false,
        "unknown historic activity skipped the observation period"
    );
    let id = target["artifact_id"]
        .as_str()
        .ok_or("artifact id missing")?;
    let protected = world.call(
        "storage.protection.set",
        json!({"artifact_id":id,"expected_revision":target["revision"],"protected":true}),
    )?;
    let protected = data(&protected)?;
    ensure!(
        protected["safety"] == "protected" && protected["deletable"] == false,
        "protection was not persisted"
    );
    let blocked = world.call("storage.cleanup.plan", json!({"artifact_ids":[id]}))?;
    ensure!(
        data(&blocked)?["ready"] == false,
        "protected data acquired a ready plan"
    );
    let released = world.call(
        "storage.protection.set",
        json!({"artifact_id":id,"expected_revision":protected["revision"],"protected":false}),
    )?;
    ensure!(
        data(&released)?["deletable"] == true,
        "removing a pin did not restore eligible cache state"
    );
    let plan = world.call("storage.cleanup.plan", json!({"artifact_ids":[id]}))?;
    let plan = data(&plan)?;
    ensure!(plan["ready"] == true, "cache cleanup plan was blocked");
    let plan_id = plan["plan_id"].as_str().ok_or("plan id missing")?;
    let started = cli(
        world,
        &[
            "storage",
            "cleanup",
            "start",
            "--plan-id",
            plan_id,
            "--idempotency-key",
            "remove-cache",
        ],
    )?;
    let job_id = data(&started)?["job_id"]
        .as_str()
        .ok_or("cleanup id missing")?
        .to_owned();
    let completed = wait_job(world, &job_id)?;
    ensure!(
        completed["state"] == "completed",
        "cleanup did not complete: {}",
        bounded_json(&completed)
    );
    ensure!(
        !world.repo.join("cache/disposable").exists(),
        "cleanup did not remove the real files"
    );
    ensure!(
        fs::read_to_string(world.repo.join("source.txt")).map_err(|e| e.to_string())?
            == "valuable tracked source\n",
        "cleanup changed tracked source"
    );
    let retried = cli(
        world,
        &[
            "storage",
            "cleanup",
            "start",
            "--plan-id",
            plan_id,
            "--idempotency-key",
            "remove-cache",
        ],
    )?;
    ensure!(
        data(&retried)?["job_id"] == job_id,
        "retry did not return the original receipt"
    );

    let changed = rows
        .iter()
        .find(|r| r["name"] == "changed")
        .ok_or("changed fixture missing")?;
    let changed_id = changed["artifact_id"]
        .as_str()
        .ok_or("changed id missing")?;
    let plan = world.call("storage.cleanup.plan", json!({"artifact_ids":[changed_id]}))?;
    let plan = data(&plan)?;
    ensure!(plan["ready"] == true, "unchanged fixture was blocked");
    fs::rename(
        world.repo.join("cache/changed"),
        world.repo.join("saved-original"),
    )
    .map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(
        world.repo.join("saved-original"),
        world.repo.join("cache/changed"),
    )
    .map_err(|e| e.to_string())?;
    let started = world.call(
        "storage.cleanup.start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"reject-substitution"}),
    )?;
    let failed = wait_job(
        world,
        data(&started)?["job_id"]
            .as_str()
            .ok_or("substitution job id missing")?,
    )?;
    ensure!(
        failed["state"] == "failed",
        "substituted target was not refused"
    );
    ensure!(
        world.repo.join("saved-original/output.bin").is_file(),
        "cleanup followed a substituted link"
    );
    let row = world.call("storage.artifact.get", json!({"artifact_id":changed_id}))?;
    ensure!(
        data(&row)?["deletable"] == false,
        "failed safety evidence remained labelled safe"
    );
    world.stop_daemon(false)?;
    world.start_daemon(None, None, None)?;
    let retained = world.call("storage.job.status", json!({"job_id":job_id}))?;
    ensure!(
        data(&retained)?["state"] == "completed",
        "cleanup receipt did not survive restart"
    );

    let interrupted = rows
        .iter()
        .find(|r| r["name"] == "interrupted")
        .ok_or("restart fixture missing")?;
    let plan = world.call(
        "storage.cleanup.plan",
        json!({"artifact_ids":[interrupted["artifact_id"]]}),
    )?;
    ensure!(
        data(&plan)?["ready"] == true,
        "restart fixture plan is not ready"
    );
    fs::write(
        world.state.join("storage-crash-after-remove"),
        b"isolated acceptance failpoint",
    )
    .map_err(|e| e.to_string())?;
    let started = world.call(
        "storage.cleanup.start",
        json!({"plan_id":data(&plan)?["plan_id"],"idempotency_key":"interrupted-real-removal"}),
    )?;
    let interrupted_id = data(&started)?["job_id"]
        .as_str()
        .ok_or("interrupted job missing")?
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if world
            .daemon
            .as_mut()
            .ok_or("fixture daemon missing")?
            .try_wait()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "isolated cleanup did not reach crash boundary"
        );
        thread::sleep(Duration::from_millis(50));
    }
    world.daemon.take();
    ensure!(
        !world.repo.join("cache/interrupted").exists(),
        "crash happened before real removal"
    );
    world.start_daemon(None, None, None)?;
    let recovered = wait_job(world, &interrupted_id)?;
    ensure!(
        recovered["state"] == "completed",
        "interrupted cleanup did not reconcile: {}",
        bounded_json(&recovered)
    );
    ensure!(
        recovered["receipts"]
            .as_array()
            .is_some_and(|r| r.len() == 1),
        "restart duplicated the removal receipt"
    );
    ensure!(
        recovered["unmeasured_items"] == 1 && recovered["reclaimed_bytes"] == 0,
        "restart invented a reclaimed-space measurement"
    );
    stale_discovery_order(world, &repo)?;
    generated_output_safety(world, &repo)?;
    Ok(())
}

fn generated_output_safety(world: &World, repo: &str) -> Result<(), String> {
    world.write_owned(
        "program.c",
        "#include <unistd.h>\nint main(void) { for (;;) pause(); }\n",
    )?;
    world.write_owned("generated/compiler/.keep", "")?;
    for directory in ["generated", "generated/compiler"] {
        chown_path(
            &world.repo.join(directory),
            world.harness.caller_uid,
            world.harness.caller_gid,
        )?;
    }
    let metadata = [
        ".ssh/key",
        ".aws/credentials",
        ".gnupg/key",
        "sessions/chat",
        "archived_sessions/chat",
        ".env",
        ".netrc",
        ".npmrc",
        "auth.json",
        "credentials.json",
        "id_rsa",
        "id_ed25519",
    ];
    for (index, name) in metadata.iter().enumerate() {
        world.write_owned(
            &format!("generated/protected-{index}/{name}"),
            "fixture protected metadata",
        )?;
    }
    for name in [
        ".env.example",
        ".ssh-example/config",
        "credentials.md",
        "id_rsa.pub",
    ] {
        world.write_owned(
            &format!("generated/compiler/{name}"),
            "public generated example",
        )?;
    }
    world.write_owned("build/unclassified", "unknown historical output")?;
    run_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        &world.repo,
        "/usr/bin/cc",
        &["program.c", "-o", "generated/compiler/program"],
        &world.base,
    )?;
    data(&world.call("storage.roots.set", json!({"repository_id":repo,"expected_revision":0,"label":"Declared compiler outputs","path":world.repo.join("generated"),"kind":"build_output"}))?)?;
    struct RunningOutput(Child);
    impl Drop for RunningOutput {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let executable = world.repo.join("generated/compiler/program");
    let mut running = RunningOutput(
        Command::new("setpriv")
            .args([
                format!("--reuid={}", world.harness.caller_uid),
                format!("--regid={}", world.harness.caller_gid),
                "--clear-groups".into(),
                "--".into(),
            ])
            .arg(&executable)
            .current_dir(&world.repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_link(format!("/proc/{}/exe", running.0.id()))
        .ok()
        .as_ref()
        != Some(&executable)
    {
        ensure!(
            running.0.try_wait().map_err(|e| e.to_string())?.is_none() && Instant::now() < deadline,
            "compiled fixture did not start"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let active = scanned(world, repo, "generated-active")?;
    let rows = active["artifacts"]
        .as_array()
        .ok_or("generated rows missing")?;
    let find = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .cloned()
            .ok_or_else(|| format!("generated {name} missing"))
    };
    let build = find("compiler")?;
    let unknown = find("build")?;
    let mut failures = Vec::new();
    if build["deletable"] != false
        || !build["reasons"]
            .as_array()
            .is_some_and(|r| r.contains(&json!("active_process")))
    {
        failures.push("a running generated executable was removable");
    }
    for (index, _) in metadata.iter().enumerate() {
        let protected = find(&format!("protected-{index}"))?;
        if protected["deletable"] != false
            || !protected["reasons"]
                .as_array()
                .is_some_and(|r| r.contains(&json!("protected_source_or_credentials")))
        {
            failures
                .push("declaring generated output made nested credentials or history removable");
        }
    }
    if unknown["deletable"] != false {
        failures.push("unrecognized output was removable without ownership evidence");
    }
    drop(running);
    scanned(world, repo, "generated-inactive")?;
    let request = cli(
        world,
        &[
            "storage",
            "cleanup",
            "plan",
            "--artifact-id",
            build["artifact_id"].as_str().ok_or("build id missing")?,
        ],
    )?;
    let plan = data(&request)?;
    ensure!(
        plan["ready"] == true,
        "unused compiled output did not become removable"
    );
    let started = cli(
        world,
        &[
            "storage",
            "cleanup",
            "start",
            "--plan-id",
            plan["plan_id"].as_str().ok_or("build plan missing")?,
            "--idempotency-key",
            "remove-compiled-output",
        ],
    )?;
    let done = wait_job(
        world,
        data(&started)?["job_id"]
            .as_str()
            .ok_or("build job missing")?,
    )?;
    if done["state"] != "completed"
        || executable.exists()
        || !world.repo.join("program.c").is_file()
        || metadata.iter().enumerate().any(|(index, name)| {
            !world
                .repo
                .join(format!("generated/protected-{index}/{name}"))
                .is_file()
        })
        || !world.repo.join("build/unclassified").is_file()
    {
        failures.push("compiled output cleanup did not preserve source and unrelated data");
    }
    ensure!(
        failures.is_empty(),
        "generated output safety failures: {}",
        failures.join("; ")
    );
    Ok(())
}

fn stale_discovery_order(world: &World, repo: &str) -> Result<(), String> {
    for name in ["ordering-delete", "ordering-keep", "ordering-invalid"] {
        world.write_owned(&format!("cache/{name}/data"), vec![77_u8; 4096])?;
    }
    scanned(world, repo, "ordering-baseline")?;
    let barrier = world.state.join("storage-pause-scan");
    fs::write(&barrier, b"ordering-old-global").map_err(|e| e.to_string())?;
    let old = world.call(
        "storage.scan",
        json!({"idempotency_key":"ordering-old-global"}),
    )?;
    let old_id = data(&old)?["job_id"]
        .as_str()
        .ok_or("old scan id missing")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !barrier.with_extension("ready").exists() {
        ensure!(
            Instant::now() < deadline,
            "old scan did not reach its observation barrier"
        );
        thread::sleep(Duration::from_millis(25));
    }
    set_clock(
        world,
        (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64 + 5000,
    )?;
    let fresh = scanned(world, repo, "ordering-new-project")?;
    let rows = fresh["artifacts"]
        .as_array()
        .ok_or("new scan rows missing")?;
    let row = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .cloned()
            .ok_or_else(|| format!("{name} missing"))
    };
    let removed = row("ordering-delete")?;
    let kept = row("ordering-keep")?;
    let invalid = row("ordering-invalid")?;
    mcp(
        world,
        "storage_protection_set",
        json!({"artifact_id":kept["artifact_id"],"expected_revision":kept["revision"],"protected":true}),
    )?;
    let plan = mcp(
        world,
        "storage_cleanup_plan",
        json!({"artifact_ids":[removed["artifact_id"],invalid["artifact_id"]]}),
    )?;
    ensure!(plan["ready"] == true, "ordering cleanup was not ready");
    fs::rename(
        world.repo.join("cache/ordering-invalid"),
        world.repo.join("ordering-saved"),
    )
    .map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(
        world.repo.join("ordering-saved"),
        world.repo.join("cache/ordering-invalid"),
    )
    .map_err(|e| e.to_string())?;
    let cleanup = mcp(
        world,
        "storage_cleanup_start",
        json!({"plan_id":plan["plan_id"],"idempotency_key":"ordering-cleanup"}),
    )?;
    let receipt = wait_job(
        world,
        cleanup["job_id"]
            .as_str()
            .ok_or("ordering cleanup id missing")?,
    )?;
    ensure!(
        receipt["state"] == "partial",
        "ordering cleanup did not preserve its partial result: {}",
        bounded_json(&receipt)
    );
    fs::remove_file(&barrier).map_err(|e| e.to_string())?;
    ensure!(
        wait_job(world, old_id)?["state"] == "completed",
        "older scan did not finish"
    );
    let mut failures = Vec::new();
    let removed_after = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":removed["artifact_id"]}),
    )?;
    if removed_after["removed_at_ms"].is_null() || world.repo.join("cache/ordering-delete").exists()
    {
        failures.push("older discovery resurrected removed data");
    }
    let kept_after = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":kept["artifact_id"]}),
    )?;
    if kept_after["verified_at_ms"] != kept["verified_at_ms"] || kept_after["protected"] != true {
        failures.push("older discovery replaced newer verification or protection");
    }
    let invalid_after = mcp(
        world,
        "storage_artifact_get",
        json!({"artifact_id":invalid["artifact_id"]}),
    )?;
    if invalid_after["deletable"] != false || !world.repo.join("ordering-saved/data").is_file() {
        failures.push("older discovery cleared a failed identity check");
    }
    // A genuinely later observation must still admit a replacement as a new
    // resource; the ordering guard must not permanently hide a reused path.
    world.write_owned("cache/ordering-delete/replacement", vec![88_u8; 4096])?;
    let replacement = scanned(world, repo, "ordering-legitimate-replacement")?;
    if !replacement["artifacts"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|r| r["name"] == "ordering-delete" && r["removed_at_ms"].is_null())
    }) {
        failures.push("new discovery could not observe a legitimate replacement");
    }
    ensure!(
        failures.is_empty(),
        "scan ordering failures: {}",
        failures.join("; ")
    );
    Ok(())
}
