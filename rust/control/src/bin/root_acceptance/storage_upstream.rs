use super::*;

fn git_in(world: &World, path: &Path, arguments: &[&str]) -> Result<(), String> {
    run_as(
        world.harness.caller_uid,
        world.harness.caller_gid,
        path,
        "/usr/bin/git",
        arguments,
        &world.base,
    )
}

fn scanned(world: &World, repository: &str, key: &str) -> Result<Value, String> {
    let scan = world.call(
        "storage.scan",
        json!({"repository_id":repository,"idempotency_key":key}),
    )?;
    let job_id = data(&scan)?["job_id"]
        .as_str()
        .ok_or("scan identity missing")?;
    let deadline = (time::OffsetDateTime::now_utc() + time::Duration::minutes(5))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| error.to_string())?;
    let mut cursor = 0;
    loop {
        let response = world.call("storage.job.status", json!({"job_id":job_id}))?;
        let job = data(&response)?;
        if matches!(
            job["state"].as_str(),
            Some("completed" | "partial" | "failed" | "cancelled")
        ) {
            ensure!(
                job["state"] == "completed",
                "storage scan failed: {}",
                bounded_json(job)
            );
            break;
        }
        let response = world.call(
            "event.wait",
            json!({"cursor":cursor,"filters":[{"filter_id":"storage-upstream","categories":["other"],"kinds":["storage.job.finished","storage.job.failed"],"deadline_at":deadline}]}),
        )?;
        let events = data(&response)?;
        cursor = events["cursor"].as_u64().ok_or("event cursor missing")?;
        ensure!(
            events["heartbeat_due"]
                .as_array()
                .is_none_or(|values| values.is_empty()),
            "storage scan missed its completion deadline"
        );
    }
    let inventory = world.call(
        "storage.inventory",
        json!({"repository_id":repository,"limit":100}),
    )?;
    Ok(data(&inventory)?.clone())
}

fn row<'a>(inventory: &'a Value, name: &str) -> Result<&'a Value, String> {
    inventory["artifacts"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .ok_or_else(|| format!("storage artifact {name} missing"))
}

fn reasons(row: &Value) -> Vec<Value> {
    row["reasons"].as_array().cloned().unwrap_or_default()
}

pub(super) fn upstream_worktree_baselines(world: &mut World) -> Result<(), String> {
    world.write_config("schema = 2\n")?;
    world.write_owned("source.txt", "upstream baseline\n")?;
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

    let origin = world.base.join("upstream-origin.git");
    fs::create_dir(&origin).map_err(|error| error.to_string())?;
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
    let origin_text = origin.to_str().ok_or("upstream origin path is not UTF-8")?;
    world.git(&["remote", "add", "origin", origin_text])?;
    world.git(&["push", "-q", "origin", "HEAD:refs/heads/main"])?;
    let repository =
        data(&world.call("repository.register", json!({"path":world.repo}))?)?["repository_id"]
            .as_str()
            .ok_or("repository missing")?
            .to_owned();

    let worktrees = world.base.join("upstream-worktrees");
    fs::create_dir(&worktrees).map_err(|error| error.to_string())?;
    chown_path(
        &worktrees,
        world.harness.caller_uid,
        world.harness.caller_gid,
    )?;
    let candidate = worktrees.join("published-upstream");
    let candidate_text = candidate
        .to_str()
        .ok_or("upstream worktree path is not UTF-8")?;
    world.git(&["worktree", "add", "--detach", candidate_text, "HEAD"])?;
    git_in(world, &candidate, &["switch", "-c", "published-upstream"])?;
    let published_file = candidate.join("published.txt");
    fs::write(&published_file, "published branch\n").map_err(|error| error.to_string())?;
    chown_path(
        &published_file,
        world.harness.caller_uid,
        world.harness.caller_gid,
    )?;
    git_in(world, &candidate, &["add", "published.txt"])?;
    git_in(
        world,
        &candidate,
        &[
            "-c",
            "user.name=Storage fixture",
            "-c",
            "user.email=storage@example.invalid",
            "commit",
            "-qm",
            "published upstream",
        ],
    )?;
    git_in(
        world,
        &candidate,
        &[
            "push",
            "-q",
            "-u",
            "origin",
            "HEAD:refs/heads/published-upstream",
        ],
    )?;

    let inventory = scanned(world, &repository, "upstream-published")?;
    let published = row(&inventory, "published-upstream")?;
    let published_reasons = reasons(published);
    ensure!(
        !published_reasons.contains(&json!("git_baseline_unverified"))
            && !published_reasons.contains(&json!("unique_worktree_commits")),
        "published nondefault upstream was not accepted: {}",
        bounded_json(published)
    );

    let dirty_file = candidate.join("dirty.txt");
    fs::write(&dirty_file, "uncommitted\n").map_err(|error| error.to_string())?;
    chown_path(
        &dirty_file,
        world.harness.caller_uid,
        world.harness.caller_gid,
    )?;
    let inventory = scanned(world, &repository, "upstream-dirty")?;
    let dirty = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(dirty).contains(&json!("dirty_worktree")),
        "dirty upstream worktree was not blocked: {}",
        bounded_json(dirty)
    );
    git_in(world, &candidate, &["add", "dirty.txt"])?;
    git_in(
        world,
        &candidate,
        &[
            "-c",
            "user.name=Storage fixture",
            "-c",
            "user.email=storage@example.invalid",
            "commit",
            "-qm",
            "unpublished work",
        ],
    )?;
    let inventory = scanned(world, &repository, "upstream-unique")?;
    let unique = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(unique).contains(&json!("unique_worktree_commits")),
        "unpublished upstream commit was labelled safe: {}",
        bounded_json(unique)
    );

    git_in(
        world,
        &candidate,
        &["reset", "--hard", "origin/published-upstream"],
    )?;
    git_in(
        world,
        &candidate,
        &["push", "-q", "origin", "--delete", "published-upstream"],
    )?;
    let inventory = scanned(world, &repository, "upstream-deleted")?;
    let deleted = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(deleted).contains(&json!("git_baseline_unverified")),
        "deleted upstream ref was labelled safe: {}",
        bounded_json(deleted)
    );

    git_in(
        world,
        &candidate,
        &["push", "-q", "origin", "HEAD:refs/heads/published-upstream"],
    )?;
    let unavailable = world.base.join("missing-upstream.git");
    let unavailable_text = unavailable
        .to_str()
        .ok_or("missing upstream path is not UTF-8")?;
    git_in(
        world,
        &candidate,
        &["remote", "set-url", "origin", unavailable_text],
    )?;
    let inventory = scanned(world, &repository, "upstream-unavailable")?;
    let unavailable = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(unavailable).contains(&json!("git_baseline_unverified")),
        "unavailable upstream ref was labelled safe: {}",
        bounded_json(unavailable)
    );

    git_in(
        world,
        &candidate,
        &["config", "branch.published-upstream.remote", "."],
    )?;
    let inventory = scanned(world, &repository, "upstream-local-only")?;
    let local_only = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(local_only).contains(&json!("git_baseline_unverified")),
        "local-only upstream was treated as a remote baseline: {}",
        bounded_json(local_only)
    );

    git_in(
        world,
        &candidate,
        &[
            "config",
            "branch.published-upstream.remote",
            "/tmp/unconfigured-remote",
        ],
    )?;
    let inventory = scanned(world, &repository, "upstream-malformed-remote")?;
    let malformed = row(&inventory, "published-upstream")?;
    ensure!(
        reasons(malformed).contains(&json!("git_baseline_unverified")),
        "unconfigured upstream remote was treated as a published baseline: {}",
        bounded_json(malformed)
    );
    Ok(())
}
