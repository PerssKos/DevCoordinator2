//! Explicit recovery of one private PostgreSQL/process preview; never start it here.
use super::*;
use devcoordinator2_api::runtime_recovery::{Port, Receipt, Request};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Seek, SeekFrom};

type Row = BTreeMap<String, Value>;
type Rows = BTreeMap<String, Vec<Row>>;
const TABLES: &[&str] = &[
    "deployments",
    "generations",
    "components",
    "port_assignments",
    "domain_routes",
];

fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}
fn text<'a>(row: &'a Row, field: &str) -> Result<&'a str, ProtocolError> {
    row.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("saved ownership metadata is incomplete"))
}
fn number(row: &Row, field: &str) -> Result<u32, ProtocolError> {
    row.get(field)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid("saved ownership number is invalid"))
}
fn digest(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
fn read_rows(connection: &Connection, deployment: &str) -> Result<Rows, DatabaseError> {
    let mut result = Rows::new();
    for table in TABLES {
        let mut statement = connection.prepare(&format!(
            "SELECT * FROM {table} WHERE deployment_id=?1 ORDER BY rowid LIMIT 1025"
        ))?;
        let columns: Vec<String> = statement
            .column_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut query = statement.query([deployment])?;
        let mut rows = Vec::new();
        while let Some(row) = query.next()? {
            let mut values = Row::new();
            for (i, name) in columns.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Text(bytes) => json!(
                        std::str::from_utf8(bytes)
                            .map_err(|_| invalid("invalid ownership text"))?
                    ),
                    _ => return Err(invalid("unsupported ownership value").into()),
                };
                values.insert(name.clone(), value);
            }
            rows.push(values);
        }
        if rows.len() > 1024
            || serde_json::to_vec(&rows)
                .map_err(|_| invalid("cannot fingerprint ownership"))?
                .len()
                > 8 * 1024 * 1024
        {
            return Err(invalid("saved ownership exceeds recovery bounds").into());
        }
        result.insert((*table).into(), rows);
    }
    Ok(result)
}
fn row_insert(connection: &Connection, table: &str, row: &Row) -> Result<(), DatabaseError> {
    let columns = row
        .keys()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",");
    let placeholders = std::iter::repeat_n("?", row.len())
        .collect::<Vec<_>>()
        .join(",");
    let values = row
        .values()
        .map(|v| match v {
            Value::Null => Ok(rusqlite::types::Value::Null),
            Value::String(s) => Ok(rusqlite::types::Value::Text(s.clone())),
            Value::Number(n) => n
                .as_i64()
                .map(rusqlite::types::Value::Integer)
                .ok_or_else(|| invalid("invalid ownership number")),
            _ => Err(invalid("invalid ownership value")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    connection.execute(
        &format!("INSERT INTO {table} ({columns}) VALUES ({placeholders})"),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}
fn fingerprint(rows: &Rows, proof: &str) -> Result<String, ProtocolError> {
    Ok(digest(serde_json::to_vec(&(rows, proof)).map_err(
        |_| invalid("cannot fingerprint runtime recovery"),
    )?))
}
fn saved_components(row: &Row) -> Result<Vec<ComponentSpec>, ProtocolError> {
    let value: Value = serde_json::from_str(text(row, "spec_json")?)
        .map_err(|_| invalid("invalid saved specification"))?;
    let components: Vec<ComponentSpec> = serde_json::from_value(
        value
            .get("components")
            .cloned()
            .ok_or_else(|| invalid("saved component specifications are missing"))?,
    )
    .map_err(|_| invalid("invalid saved component specifications"))?;
    if components.is_empty() || components.len() > 64 {
        return Err(invalid("saved component count is invalid"));
    }
    Ok(components)
}

fn require_stale_ownership(current: &Rows, saved: &Rows) -> Result<(), ProtocolError> {
    let old = current["deployments"]
        .first()
        .ok_or_else(|| invalid("current deployment is missing"))?;
    let prior = saved["deployments"]
        .first()
        .ok_or_else(|| invalid("saved deployment is missing"))?;
    let selected = number(prior, "current_generation")?;
    let current_selected = old
        .get("current_generation")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if selected == 0
        || current_selected > u64::from(selected)
        || current["generations"].iter().any(|r| {
            r.get("number")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > u64::from(selected))
        })
        || matches!(
            old.get("state").and_then(Value::as_str),
            Some("applying" | "starting" | "stopping")
        )
    {
        return Err(invalid(
            "current deployment is newer or transitioning; saved ownership was not applied",
        ));
    }
    Ok(())
}

fn database_blockers(
    connection: &Connection,
    request: &Request,
    saved: &Rows,
) -> Result<Vec<String>, DatabaseError> {
    let mut blockers = Vec::new();
    for table in TABLES {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        for row in &saved[*table] {
            if row.get("deployment_id").and_then(Value::as_str) != Some(&request.deployment_id)
                || row.keys().any(|column| !columns.contains(column))
            {
                return Err(invalid(
                    "saved rows exceed the selected deployment or supported schema",
                )
                .into());
            }
        }
    }
    if saved["deployments"]
        .first()
        .and_then(|r| r.get("repository_id"))
        .and_then(Value::as_str)
        != Some(&request.repository_id)
    {
        return Err(invalid("saved deployment belongs to another repository").into());
    }
    for row in &saved["port_assignments"] {
        let port = number(row, "port")?;
        let owner: Option<String> = connection
            .query_row(
                "SELECT deployment_id FROM port_assignments WHERE port=?1",
                [port],
                |r| r.get(0),
            )
            .optional()?;
        if owner
            .as_deref()
            .is_some_and(|owner| owner != request.deployment_id)
        {
            blockers.push(format!("port:{port}:assigned_to_another_deployment"));
        }
        let observed: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM observed_routes WHERE port=?1)",
            [port],
            |r| r.get(0),
        )?;
        if observed {
            blockers.push(format!("port:{port}:observed_route_owns_port"));
        }
    }
    for row in &saved["domain_routes"] {
        let domain = text(row, "domain")?;
        let owner: Option<String> = connection
            .query_row(
                "SELECT deployment_id FROM domain_routes WHERE domain=?1",
                [domain],
                |r| r.get(0),
            )
            .optional()?;
        if owner
            .as_deref()
            .is_some_and(|owner| owner != request.deployment_id)
        {
            blockers.push("route:assigned_to_another_deployment".into());
        }
        let observed: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM observed_routes WHERE domain=?1)",
            [domain],
            |r| r.get(0),
        )?;
        if observed {
            blockers.push("route:observed_deployment_owns_domain".into());
        }
    }
    Ok(blockers)
}

impl Deployments {
    pub fn recover_saved_preview(
        &self,
        request: Request,
        caller: &Caller,
        actor: &str,
        now: &str,
    ) -> Result<Receipt, ProtocolError> {
        let target =
            self.resolve_target_readonly(None, None, Some(&request.deployment_id), caller)?;
        if target.repository_id != request.repository_id {
            return Err(invalid(
                "deployment does not belong to the requested repository",
            ));
        }
        let _busy = self.acquire_busy(&request.deployment_id)?;
        let inspection = crate::planning_backup::InspectRequest {
            transaction_dir: request.transaction_dir.clone().into(),
            repository_id: request.repository_id.clone(),
            task_ids: vec![],
            include_identities: false,
        };
        let id = request.deployment_id.clone();
        let (backup, saved) = crate::planning_backup::inspect_with(&inspection, |c| {
            read_rows(c, &id).map_err(|_| "cannot inspect scoped runtime rows".into())
        })
        .map_err(|_| invalid("saved runtime backup failed inspection"))?;
        if backup.backup_sha256 != request.backup_sha256 {
            return Err(invalid("saved runtime backup hash changed"));
        }
        let id = request.deployment_id.clone();
        let hash = request.backup_sha256.clone();
        let old_receipt: Option<String> = self.database.call(move |c|c.query_row("SELECT receipt_json FROM deployment_recoveries WHERE deployment_id=?1 AND backup_sha256=?2",params![id,hash],|r|r.get(0)).optional().map_err(DatabaseError::from)).map_err(database_error)?;
        if let Some(old) = old_receipt {
            let mut receipt: Receipt = serde_json::from_str(&old)
                .map_err(|_| invalid("invalid saved runtime recovery receipt"))?;
            let identity = ExactContainerId::parse(receipt.preserved_database_identity.clone())
                .map_err(|_| invalid("saved recovery database identity is invalid"))?;
            if self.docker.container_state(&identity).state != RuntimeState::Running {
                return Err(invalid(
                    "recovered database is not running; inspect its current ownership",
                ));
            }
            receipt.status = "already_applied".into();
            return Ok(receipt);
        }
        let id = request.deployment_id.clone();
        let current = self
            .database
            .call(move |c| read_rows(c, &id))
            .map_err(database_error)?;
        let old = current["deployments"]
            .first()
            .ok_or_else(|| invalid("current deployment is missing"))?;
        let prior = saved["deployments"]
            .first()
            .ok_or_else(|| invalid("saved deployment is missing"))?;
        require_stale_ownership(&current, &saved)?;
        for field in [
            "repository_id",
            "worktree_id",
            "name",
            "source",
            "created_at",
            "created_by_uid",
            "public",
        ] {
            if old.get(field) != prior.get(field) {
                return Err(invalid(
                    "saved deployment identity or access boundary differs",
                ));
            }
        }
        if saved["deployments"].len() != 1 || saved["domain_routes"].len() != 1 {
            return Err(invalid(
                "select one saved deployment with its exact routed component",
            ));
        }
        if saved["generations"]
            .iter()
            .any(|row| row.get("path").and_then(Value::as_str) != target.worktree.to_str())
        {
            return Err(invalid(
                "saved worktree generation points outside the current deployment worktree",
            ));
        }
        if prior.get("public") != Some(&json!(0)) || text(prior, "source")? != "worktree" {
            return Err(invalid(
                "runtime recovery supports private worktree previews only",
            ));
        }
        let components = saved_components(prior)?;
        if components
            .iter()
            .any(|c| !matches!(c.kind, ComponentKind::Process | ComponentKind::Postgres))
        {
            return Err(invalid(
                "runtime recovery supports process and PostgreSQL components only",
            ));
        }
        let databases: Vec<_> = components
            .iter()
            .filter(|c| c.kind == ComponentKind::Postgres)
            .collect();
        if databases.len() != 1 {
            return Err(invalid(
                "select a preview with one owned persistent PostgreSQL component",
            ));
        }
        let db = databases[0];
        if db.shared_from.is_some() {
            return Err(invalid(
                "shared PostgreSQL recovery requires its owning deployment",
            ));
        }
        let desired_db = target
            .specification
            .component(&db.name)
            .ok_or_else(|| invalid("current source omits the saved database"))?;
        if DeploymentStore::component_fingerprint(db)
            != DeploymentStore::component_fingerprint(desired_db)
        {
            return Err(invalid("current and saved database declarations differ"));
        }
        let db_row = saved["components"]
            .iter()
            .find(|row| row.get("name").and_then(Value::as_str) == Some(&db.name))
            .ok_or_else(|| invalid("saved database binding is missing"))?;
        if text(db_row, "binding_kind")? != "container"
            || text(db_row, "type")? != "postgres"
            || text(db_row, "spec_fingerprint")? != DeploymentStore::component_fingerprint(db)
        {
            return Err(invalid(
                "saved database component does not match its declaration",
            ));
        }
        let identity = ExactContainerId::parse(text(db_row, "binding_identity")?.to_owned())
            .map_err(|_| invalid("saved database identity is invalid"))?;
        let credentials = self
            .files
            .read_postgres_credentials(&request.deployment_id, &db.name)
            .map_err(file_apply_error)?
            .ok_or_else(|| invalid("original database credentials are unavailable"))?;
        let database_proof =
            self.recovery_database_proof(&request, &identity, db, &credentials, &saved)?;
        let observed_components = self.recovery_component_states(&saved)?;
        let proof = digest(
            serde_json::to_vec(&(&database_proof, &observed_components))
                .map_err(|_| invalid("cannot fingerprint observed ownership"))?,
        );
        let live_sha256 = fingerprint(&current, &proof)?;
        let request_copy = request.clone();
        let saved_copy = saved.clone();
        let mut blockers = self
            .database
            .call(move |c| database_blockers(c, &request_copy, &saved_copy))
            .map_err(database_error)?;
        let mut ports = Vec::new();
        for row in &saved["port_assignments"] {
            let component = text(row, "component")?;
            let port = u16::try_from(number(row, "port")?)
                .map_err(|_| invalid("saved port is invalid"))?;
            let generation = number(row, "generation")?;
            let binding = saved["components"]
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(component));
            let owned = if component == db.name {
                true
            } else {
                binding.is_some_and(|r| {
                    r.get("binding_kind").and_then(Value::as_str) == Some("unit")
                        && r.get("binding_identity")
                            .and_then(Value::as_str)
                            .is_some_and(|unit| self.systemd.owns_tcp_listener(unit, port))
                })
            };
            if !owned && !self.port_availability.bindable(port) {
                blockers.push(format!("port:{port}:foreign_listener"));
            }
            ports.push(Port {
                component: component.into(),
                port,
                generation,
            });
        }
        let mut receipt = Receipt {
            recovery_id: None,
            repository_id: request.repository_id.clone(),
            deployment_id: request.deployment_id.clone(),
            backup_sha256: backup.backup_sha256,
            live_sha256,
            provenance: backup.provenance.into(),
            status: if blockers.is_empty() {
                "prepared"
            } else {
                "blocked"
            }
            .into(),
            current_generation: old
                .get("current_generation")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            saved_generation: number(prior, "current_generation")?,
            preserved_database_identity: identity.to_string(),
            ports,
            blockers,
            observed_components,
            database_backup_sha256: None,
        };
        if !request.apply {
            return Ok(receipt);
        }
        if !receipt.blockers.is_empty() {
            return Err(invalid("saved preview ownership has unresolved conflicts"));
        }
        if request.expected_live_sha256.as_deref() != Some(&receipt.live_sha256) {
            return Err(invalid(
                "runtime recovery needs the exact current dry-run fingerprint",
            ));
        }
        let recovery_id = recovery_identity()?;
        receipt.database_backup_sha256 =
            Some(self.backup_recovery_database(&recovery_id, &identity, &credentials)?);
        if self.recovery_database_proof(&request, &identity, db, &credentials, &saved)?
            != database_proof
            || self.recovery_component_states(&saved)? != receipt.observed_components
        {
            return Err(invalid("database ownership changed during backup"));
        }
        receipt.recovery_id = Some(recovery_id);
        receipt.status = "applied".into();
        let result = receipt.clone();
        let actor = actor.to_owned();
        let now = now.to_owned();
        self.database
            .transaction(move |connection| {
                apply_metadata(
                    connection,
                    &request,
                    &saved,
                    &current,
                    &proof,
                    &receipt,
                    (&actor, &now),
                )
            })
            .map_err(database_error)?;
        self.routes.publish_current()?;
        Ok(result)
    }

    fn recovery_component_states(
        &self,
        saved: &Rows,
    ) -> Result<BTreeMap<String, String>, ProtocolError> {
        saved["components"]
            .iter()
            .map(|row| {
                let name = text(row, "name")?.to_owned();
                if text(row, "type")? == "postgres" {
                    return Ok((name, "running".into()));
                }
                if text(row, "binding_kind")? != "unit" {
                    return Err(invalid("saved process binding is invalid"));
                }
                let unit = text(row, "binding_identity")?;
                let expected = process_unit_name(
                    &self.config.deploy_unit_prefix(),
                    text(row, "deployment_id")?,
                    &name,
                    number(row, "generation")?,
                );
                if unit != expected {
                    return Err(invalid(
                        "saved unit identity does not match its deployment and generation",
                    ));
                }
                let state = self
                    .systemd
                    .process_state(unit)
                    .map_err(|_| invalid("cannot observe saved process binding"))?;
                let state = match state.active_state.as_str() {
                    "active" => "running",
                    "failed" => "failed",
                    "inactive" => "stopped",
                    _ => return Err(invalid("saved process is transitioning; inspect it again")),
                };
                Ok((name, state.into()))
            })
            .collect()
    }

    fn recovery_database_proof(
        &self,
        request: &Request,
        identity: &ExactContainerId,
        db: &ComponentSpec,
        credentials: &PostgresCredentials,
        saved: &Rows,
    ) -> Result<String, ProtocolError> {
        let info = self
            .docker
            .inspect(identity)
            .map_err(|_| invalid("cannot inspect original database container"))?;
        for (key, value) in [
            ("repository", request.repository_id.as_str()),
            ("deployment", request.deployment_id.as_str()),
            ("component", db.name.as_str()),
            ("data", "persistent"),
        ] {
            if info
                .pointer(&format!("/Config/Labels/devcoordinator2.{key}"))
                .and_then(Value::as_str)
                != Some(value)
            {
                return Err(invalid("original database labels do not prove ownership"));
            }
        }
        if info.pointer("/State/Running").and_then(Value::as_bool) != Some(true)
            || info.pointer("/Config/Image").and_then(Value::as_str) != db.image.as_deref()
        {
            return Err(invalid(
                "original database is not running with the saved image",
            ));
        }
        if credentials.user != db.user.as_deref().unwrap_or("app")
            || credentials.database != db.database.as_deref().unwrap_or("app")
        {
            return Err(invalid(
                "original database credentials do not match its declaration",
            ));
        }
        let env = info
            .pointer("/Config/Env")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("original database environment is unavailable"))?;
        for (key, value) in [
            ("POSTGRES_USER", &credentials.user),
            ("POSTGRES_DB", &credentials.database),
            ("POSTGRES_PASSWORD", &credentials.password),
        ] {
            if !env.iter().any(|v| {
                v.as_str().is_some_and(|v| {
                    v.strip_prefix(&format!("{key}="))
                        .is_some_and(|v| v == value)
                })
            }) {
                return Err(invalid("original database credential binding differs"));
            }
        }
        let expected = managed_volume_name(&request.deployment_id, &db.name, "pgdata").to_string();
        let mounts = info
            .get("Mounts")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("original database volume is unavailable"))?;
        if !mounts.iter().any(|m| {
            m["Name"] == expected
                && m["Destination"] == "/var/lib/postgresql/data"
                && m["Type"] == "volume"
        }) {
            return Err(invalid("original persistent volume binding differs"));
        }
        let db_port = saved["port_assignments"]
            .iter()
            .find(|r| r.get("component").and_then(Value::as_str) == Some(&db.name))
            .ok_or_else(|| invalid("saved database port is missing"))?;
        let port = number(db_port, "port")?.to_string();
        let bound = info
            .pointer("/HostConfig/PortBindings/5432~1tcp")
            .and_then(Value::as_array)
            .is_some_and(|ports| {
                ports
                    .iter()
                    .any(|p| p["HostIp"] == "127.0.0.1" && p["HostPort"] == port)
            });
        if !bound {
            return Err(invalid("original database port binding differs"));
        }
        Ok(digest(
            serde_json::to_vec(&(
                identity.as_str(),
                &info["Config"]["Env"],
                &info["Mounts"],
                &info["HostConfig"]["PortBindings"],
            ))
            .map_err(|_| invalid("cannot fingerprint database ownership"))?,
        ))
    }

    fn backup_recovery_database(
        &self,
        recovery: &str,
        identity: &ExactContainerId,
        credentials: &PostgresCredentials,
    ) -> Result<String, ProtocolError> {
        use rustix::fs::{Mode, OFlags};
        let root = crate::deployment_files::open_directory_path(
            &self.config.state_dir.join("recovery"),
            true,
            0o700,
        )
        .map_err(file_apply_error)?
        .ok_or_else(|| invalid("recovery directory unavailable"))?;
        let directory = crate::deployment_files::open_child_directory(&root, recovery, true, 0o700)
            .map_err(file_apply_error)?
            .ok_or_else(|| invalid("recovery directory unavailable"))?;
        let descriptor = rustix::fs::openat(
            &directory,
            "postgres.pending",
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| invalid("cannot create private database backup"))?;
        let mut file = File::from(descriptor);
        let invocation = crate::docker::DockerInvocation::new(
            vec![
                "exec".into(),
                identity.as_str().into(),
                "pg_dump".into(),
                "--format=custom".into(),
                "--username".into(),
                credentials.user.clone().into(),
                "--dbname".into(),
                credentials.database.clone().into(),
            ],
            Duration::from_secs(600),
        )
        .map_err(|_| invalid("cannot prepare database backup"))?
        .with_private_stdout(
            file.try_clone()
                .map_err(|_| invalid("cannot open private backup output"))?,
        );
        let output = self
            .docker
            .invoke(invocation)
            .map_err(|_| invalid("database backup failed; original data preserved"))?;
        if !output.success() {
            return Err(invalid("database backup failed; original data preserved"));
        }
        file.sync_all()
            .map_err(|_| invalid("cannot seal private database backup"))?;
        directory
            .sync_all()
            .map_err(|_| invalid("cannot seal private backup directory"))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| invalid("cannot read private database backup"))?;
        let mut magic = [0u8; 5];
        file.read_exact(&mut magic)
            .map_err(|_| invalid("database backup is empty"))?;
        if &magic != b"PGDMP" {
            return Err(invalid("database backup is not a PostgreSQL archive"));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| invalid("cannot validate private database backup"))?;
        let verify = crate::docker::DockerInvocation::new(
            vec![
                "exec".into(),
                "-i".into(),
                identity.as_str().into(),
                "pg_restore".into(),
                "--file=/dev/null".into(),
            ],
            Duration::from_secs(600),
        )
        .map_err(|_| invalid("cannot validate database backup"))?
        .with_input(
            file.try_clone()
                .map_err(|_| invalid("cannot open backup verification input"))?,
        );
        if !self
            .docker
            .invoke(verify)
            .map_err(|_| invalid("database backup validation failed"))?
            .success()
        {
            return Err(invalid("database backup validation failed"));
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| invalid("cannot fingerprint private database backup"))?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| invalid("cannot fingerprint private database backup"))?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let digest = hash.finalize().iter().map(|v| format!("{v:02x}")).collect();
        rustix::fs::renameat(&directory, "postgres.pending", &directory, "postgres.dump")
            .map_err(|_| invalid("cannot publish verified database backup"))?;
        directory
            .sync_all()
            .map_err(|_| invalid("cannot seal verified backup directory"))?;
        Ok(digest)
    }
}

fn apply_metadata(
    connection: &Connection,
    request: &Request,
    saved: &Rows,
    before: &Rows,
    proof: &str,
    receipt: &Receipt,
    audit: (&str, &str),
) -> Result<(), DatabaseError> {
    let (actor, now) = audit;
    require_stale_ownership(before, saved)?;
    if fingerprint(&read_rows(connection, &request.deployment_id)?, proof)? != receipt.live_sha256
        || !database_blockers(connection, request, saved)?.is_empty()
    {
        return Err(invalid("runtime ownership changed; prepare recovery again").into());
    }
    let mut recovered = saved.clone();
    let routed = saved["domain_routes"]
        .first()
        .map(|row| text(row, "component"))
        .transpose()?
        .map(str::to_owned);
    {
        let row = &mut recovered.get_mut("deployments").unwrap()[0];
        row.insert("state".into(), json!("degraded"));
        row.insert("ttl_expires_at".into(), Value::Null);
        row.insert("updated_at".into(), json!(now));
    }
    for component in recovered.get_mut("components").unwrap() {
        let database = component.get("type").and_then(Value::as_str) == Some("postgres");
        let state = receipt
            .observed_components
            .get(text(component, "name")?)
            .ok_or_else(|| invalid("missing observed component state"))?;
        component.insert("state".into(), json!(state));
        component.insert(
            "health".into(),
            json!(if database { "healthy" } else { "none" }),
        );
    }
    for port in recovered.get_mut("port_assignments").unwrap() {
        if port.get("component").and_then(Value::as_str) == routed.as_deref() {
            port.insert("generation".into(), json!(0));
        }
        if port.get("lease_id").and_then(Value::as_str).is_none() {
            port.insert(
                "lease_id".into(),
                json!(
                    crate::ids::lease_id()
                        .map_err(|_| invalid("cannot establish recovered lease identity"))?
                ),
            );
        }
    }
    // Withdraw old route authority. Normal apply must prove listeners before publication.
    for table in [
        "domain_routes",
        "port_assignments",
        "components",
        "generations",
    ] {
        connection.execute(
            &format!("DELETE FROM {table} WHERE deployment_id=?1"),
            [&request.deployment_id],
        )?;
    }
    let saved_deployment = &recovered["deployments"][0];
    let columns = saved_deployment
        .keys()
        .filter(|k| k.as_str() != "deployment_id")
        .cloned()
        .collect::<Vec<_>>();
    let sql = format!(
        "UPDATE deployments SET {} WHERE deployment_id=?",
        columns
            .iter()
            .map(|k| format!("\"{k}\"=?"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut values = columns
        .iter()
        .map(|k| match &saved_deployment[k] {
            Value::Null => Ok(rusqlite::types::Value::Null),
            Value::String(s) => Ok(rusqlite::types::Value::Text(s.clone())),
            Value::Number(n) => n
                .as_i64()
                .map(rusqlite::types::Value::Integer)
                .ok_or_else(|| invalid("invalid recovered number")),
            _ => Err(invalid("invalid recovered value")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    values.push(rusqlite::types::Value::Text(request.deployment_id.clone()));
    connection.execute(&sql, rusqlite::params_from_iter(values))?;
    for table in ["generations", "components", "port_assignments"] {
        for row in &recovered[table] {
            row_insert(connection, table, row)?;
        }
    }
    connection.execute("INSERT INTO deployment_recoveries(recovery_id,repository_id,deployment_id,backup_sha256,before_sha256,at,actor,before_json,saved_json,receipt_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![receipt.recovery_id,request.repository_id,request.deployment_id,request.backup_sha256,receipt.live_sha256,now,actor,serde_json::to_string(before).map_err(|_|invalid("cannot retain current ownership"))?,serde_json::to_string(saved).map_err(|_|invalid("cannot retain saved ownership"))?,serde_json::to_string(receipt).map_err(|_|invalid("cannot retain recovery receipt"))?])?;
    Ok(())
}

fn recovery_identity() -> Result<String, ProtocolError> {
    let nonce = crate::ids::lease_id().map_err(|_| invalid("cannot create recovery identity"))?;
    Ok(format!("dr{}", &digest(nonce)[..30]))
}

#[cfg(test)]
mod tests {
    use super::*;
    const REPO: &str = "r1111111111111111";
    const DEPLOY: &str = "d1111111111111111";
    #[test]
    fn recovery_identities_are_bounded_and_unique() {
        let first = recovery_identity().unwrap();
        let second = recovery_identity().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.starts_with("dr"));
        assert!(first[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    fn fixture() -> (tempfile::TempDir, Database, Rows, Rows, Request, Receipt) {
        let root = tempfile::tempdir().unwrap();
        let database = Database::open(root.path().join("authority.sqlite3")).unwrap();
        database.call(|c| {c.execute_batch("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/tmp/preview','preview','t',1000,'t');
            INSERT INTO worktrees VALUES('w1','r1111111111111111','/tmp/preview','t','t');
            INSERT INTO deployments VALUES('d1111111111111111','r1111111111111111','w1','preview','worktree','preview','fingerprint','{\"components\":[]}', 'running',149,NULL,'t',1000,'other','t',NULL,0,'preview');
            INSERT INTO generations VALUES('d1111111111111111',149,NULL,1,'/tmp/preview','snapshot','t','running');
            INSERT INTO components VALUES('d1111111111111111','db','postgres',0,'db-fingerprint','running','running','healthy',0,'container','original-container',0,NULL,'t');
            INSERT INTO components VALUES('d1111111111111111','api','process',1,'api-fingerprint','running','running','healthy',149,'unit','api-g149',0,NULL,'t');
            INSERT INTO port_assignments VALUES(20023,'d1111111111111111','db',0,'t','lease-db');
            INSERT INTO port_assignments VALUES(20053,'d1111111111111111','api',149,'t','lease-api');
            INSERT INTO domain_routes VALUES('preview','d1111111111111111','api',20053,149,'t','lease-api');")?;Ok(())}).unwrap();
        let mut saved = database.call(|c| read_rows(c, DEPLOY)).unwrap();
        for row in saved.get_mut("port_assignments").unwrap() {
            row.remove("lease_id");
        }
        database.call(|c| {c.execute_batch("UPDATE deployments SET current_generation=15;
            DELETE FROM components WHERE name='db';DELETE FROM port_assignments WHERE component='db';")?;Ok(())}).unwrap();
        let before = database.call(|c| read_rows(c, DEPLOY)).unwrap();
        let expected = fingerprint(&before, "proof").unwrap();
        let request = Request {
            repository_id: REPO.into(),
            deployment_id: DEPLOY.into(),
            transaction_dir: "/private/saved".into(),
            backup_sha256: "a".repeat(64),
            expected_live_sha256: Some(expected.clone()),
            apply: true,
        };
        let receipt = Receipt {
            recovery_id: Some("drfixture".into()),
            repository_id: REPO.into(),
            deployment_id: DEPLOY.into(),
            backup_sha256: request.backup_sha256.clone(),
            live_sha256: expected,
            provenance: "legacy_without_recorded_hash".into(),
            status: "applied".into(),
            current_generation: 15,
            saved_generation: 149,
            preserved_database_identity: "original-container".into(),
            ports: vec![],
            blockers: vec![],
            observed_components: BTreeMap::from([
                ("db".into(), "running".into()),
                ("api".into(), "stopped".into()),
            ]),
            database_backup_sha256: Some("b".repeat(64)),
        };
        (root, database, saved, before, request, receipt)
    }
    #[test]
    fn metadata_recovery_keeps_both_versions_and_adopts_the_original_stable_port() {
        let (_root, db, saved, before, request, receipt) = fixture();
        let saved_copy = saved.clone();
        let before_copy = before.clone();
        db.transaction(move |c| {
            apply_metadata(
                c,
                &request,
                &saved,
                &before,
                "proof",
                &receipt,
                ("owner", "now"),
            )
        })
        .unwrap();
        db.call(move |c| {
            let restored = read_rows(c, DEPLOY)?;
            assert_eq!(restored["deployments"][0]["current_generation"], 149);
            assert_eq!(restored["deployments"][0]["public"], 0);
            assert_eq!(
                restored["components"]
                    .iter()
                    .find(|r| r["name"] == "api")
                    .unwrap()["state"],
                "stopped"
            );
            assert!(
                restored["domain_routes"].is_empty(),
                "recovery must not publish an unverified route"
            );
            let port = restored["port_assignments"]
                .iter()
                .find(|r| r["component"] == "api")
                .unwrap();
            assert_eq!(port["port"], 20053);
            assert_eq!(port["generation"], 0);
            assert!(port["lease_id"].as_str().is_some());
            let (old, new): (String, String) = c.query_row(
                "SELECT before_json,saved_json FROM deployment_recoveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(serde_json::from_str::<Rows>(&old).unwrap(), before_copy);
            assert_eq!(serde_json::from_str::<Rows>(&new).unwrap(), saved_copy);
            assert!(c.execute("DELETE FROM deployment_recoveries", []).is_err());
            Ok(())
        })
        .unwrap();
    }
    #[test]
    fn runtime_conflicts_and_provenance_failure_leave_all_live_rows_intact() {
        let (_root, db, saved, before, request, receipt) = fixture();
        db.call(|c| {c.execute_batch("CREATE TRIGGER fail_fixture BEFORE INSERT ON components BEGIN SELECT RAISE(ABORT,'fixture insertion failure'); END;")?;Ok(())}).unwrap();
        let expected = before.clone();
        assert!(
            db.transaction(move |c| apply_metadata(
                c,
                &request,
                &saved,
                &before,
                "proof",
                &receipt,
                ("owner", "now")
            ))
            .is_err()
        );
        assert_eq!(db.call(|c| read_rows(c, DEPLOY)).unwrap(), expected);
        db.call(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM deployment_recoveries", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
        let (_root, db, saved, before, request, receipt) = fixture();
        db.call(|c| {
            c.execute("UPDATE deployments SET state='changed'", [])?;
            Ok(())
        })
        .unwrap();
        assert!(
            db.transaction(move |c| apply_metadata(
                c,
                &request,
                &saved,
                &before,
                "proof",
                &receipt,
                ("owner", "now")
            ))
            .is_err()
        );
    }
    #[test]
    fn ownership_review_rejects_foreign_ports_and_unexpected_columns() {
        let (_root, db, mut saved, _before, request, _receipt) = fixture();
        db.call(|c|{c.execute_batch("INSERT INTO deployments SELECT 'd2222222222222222',repository_id,worktree_id,'other',source,NULL,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public,NULL FROM deployments;
            INSERT INTO port_assignments VALUES(20023,'d2222222222222222','db',0,'t','other-lease');")?;Ok(())}).unwrap();
        let copy = saved.clone();
        let req = request.clone();
        assert_eq!(
            db.call(move |c| database_blockers(c, &req, &copy)).unwrap(),
            vec!["port:20023:assigned_to_another_deployment"]
        );
        saved.get_mut("components").unwrap()[0]
            .insert("unexpected_column".into(), json!("PRIVATE_SENTINEL"));
        let error = db
            .call(move |c| database_blockers(c, &request, &saved))
            .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_SENTINEL"));
    }

    #[test]
    fn runtime_recovery_never_rolls_back_a_newer_candidate_or_active_transition() {
        let (_root, _db, saved, mut before, _request, _receipt) = fixture();
        assert!(require_stale_ownership(&before, &saved).is_ok());
        before.get_mut("deployments").unwrap()[0].insert("current_generation".into(), json!(150));
        assert!(require_stale_ownership(&before, &saved).is_err());
        before.get_mut("deployments").unwrap()[0].insert("current_generation".into(), json!(15));
        before.get_mut("generations").unwrap()[0].insert("number".into(), json!(150));
        assert!(require_stale_ownership(&before, &saved).is_err());
        before.get_mut("generations").unwrap()[0].insert("number".into(), json!(149));
        before.get_mut("deployments").unwrap()[0].insert("state".into(), json!("applying"));
        assert!(require_stale_ownership(&before, &saved).is_err());
    }
}
