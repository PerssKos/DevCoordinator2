//! Reviewed import of legacy state into the current Coordinator database.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use devcoordinator2_api::DATABASE_SCHEMA_VERSION;
use regex::Regex;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use rustix::fs::{Mode, OFlags, open as unix_open};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const SCHEMA: &str = include_str!("../../control/src/schema.sql");
const OBSERVATION_SOURCE: &str = "legacy-current-import";
const NATIVE_OBSERVATION_SOURCE: &str = "legacy-native-route-import";
const MAX_INPUT_BYTES: u64 = 32 * 1024 * 1024;
const REPOSITORY_NAMESPACE: &[u8] = b"devcoordinator2.repository\0";
const OBSERVED_NAMESPACE: &[u8] = b"devcoordinator2.observed-deployment\0";

#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub export: PathBuf,
    pub state_dir: PathBuf,
    pub bugs_dir: PathBuf,
    pub live_containers: Option<PathBuf>,
    pub current_route_map: Option<PathBuf>,
    pub native_routes: Option<PathBuf>,
    pub routes_path: Option<PathBuf>,
    pub base_domain: Option<String>,
    pub prune_missing_install_fixtures: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentPlan {
    pub deployments: Vec<ObservedDeployment>,
    pub containers: Vec<ObservedContainer>,
    pub routes: Vec<ObservedRoute>,
    pub skipped: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedDeployment {
    pub deployment_id: String,
    pub repository_id: String,
    pub name: String,
    pub native_project: String,
    pub state: String,
    pub health: String,
    pub evidence: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedContainer {
    pub container_id: String,
    pub deployment_id: String,
    pub repository_id: String,
    pub name: String,
    pub image: String,
    pub compose_service: String,
    pub status: String,
    pub health: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedRoute {
    pub domain: String,
    pub deployment_id: String,
    pub component: String,
    pub port: u16,
    pub public: bool,
    pub evidence: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRoutesDocument {
    schema: u32,
    routes: Vec<NativeRoute>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRoute {
    domain: String,
    repository_root: String,
    name: String,
    component: String,
    port: u16,
    public: bool,
    state: String,
    evidence: NativeEvidence,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeEvidence {
    repository_root: String,
    port: u16,
    listener_observed: bool,
    observed_at: String,
    pid: Option<u32>,
}

fn plan_native_routes(
    export: &Value,
    document: &Value,
    connection: &Connection,
) -> Result<CurrentPlan, String> {
    let document: NativeRoutesDocument = serde_json::from_value(document.clone())
        .map_err(|error| format!("invalid native routes document: {error}"))?;
    if document.schema != 1 {
        return Err("native routes schema must be 1".into());
    }
    let registered = query_registered(connection)?;
    let labels = Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$").expect("constant regex");
    let mut plan = CurrentPlan {
        deployments: Vec::new(),
        containers: Vec::new(),
        routes: Vec::new(),
        skipped: Vec::new(),
    };
    let mut domains = HashSet::new();
    for route in document.routes {
        if !labels.is_match(&route.domain)
            || !labels.is_match(&route.component)
            || route.name.trim().is_empty()
            || route.port == 0
            || !domains.insert(route.domain.clone())
        {
            return Err("native route has invalid or duplicate identity".into());
        }
        let repository_id = registered
            .get(&route.repository_root)
            .ok_or("native route repository is not registered")?
            .clone();
        let matches = array_or_empty(object_or_empty(export.get("routes")).get("routes"))
            .iter()
            .filter(|legacy| {
                legacy.get("slug").and_then(Value::as_str) == Some(route.domain.as_str())
            })
            .collect::<Vec<_>>();
        if matches.len() != 1
            || matches[0].get("upstream_port").and_then(Value::as_u64)
                != Some(u64::from(route.port))
        {
            return Err("native route does not match one exact exported route and port".into());
        }
        let auth = matches[0]
            .get("auth")
            .and_then(Value::as_str)
            .ok_or("native route legacy authentication is missing")?;
        if !matches!(auth, "public" | "google" | "authenticated")
            || route.public != (auth == "public")
        {
            return Err("native route must preserve legacy authentication".into());
        }
        let evidence = &route.evidence;
        if evidence.repository_root != route.repository_root
            || evidence.port != route.port
            || evidence.observed_at.trim().is_empty()
            || !matches!(route.state.as_str(), "running" | "stopped")
            || evidence.listener_observed != (route.state == "running")
            || (route.state == "running" && evidence.pid.is_none_or(|pid| pid == 0))
            || (route.state == "stopped" && evidence.pid.is_some())
        {
            return Err("native route state requires matching reviewed listener evidence".into());
        }
        let authority_roots =
            array_or_empty(object_or_empty(export.get("authority")).get("port_assignments"))
                .iter()
                .filter(|assignment| {
                    assignment.get("port").and_then(Value::as_u64) == Some(u64::from(route.port))
                })
                .filter_map(|assignment| assignment.get("root").and_then(Value::as_str))
                .collect::<HashSet<_>>();
        if !authority_roots.is_empty()
            && (authority_roots.len() != 1
                || !authority_roots.contains(route.repository_root.as_str()))
        {
            return Err("native route conflicts with exported port ownership".into());
        }
        let project = format!("native-route-{}", route.domain);
        let deployment_id = observed_deployment_id(&repository_id, &project);
        let conflict: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM domain_routes WHERE domain=?1 UNION ALL SELECT 1 FROM observed_routes WHERE domain=?1 AND observed_deployment_id != ?2 UNION ALL SELECT 1 FROM observed_deployments WHERE observed_deployment_id=?2 AND source != ?3)",
            rusqlite::params![route.domain, deployment_id, NATIVE_OBSERVATION_SOURCE], |row| row.get(0)).map_err(sql_error)?;
        if conflict {
            return Err("native route conflicts with an existing deployment route".into());
        }
        let evidence = serde_json::to_value(route.evidence).map_err(|error| error.to_string())?;
        plan.deployments.push(ObservedDeployment {
            deployment_id: deployment_id.clone(),
            repository_id,
            name: route.name,
            native_project: project,
            state: route.state,
            health: "unknown".into(),
            evidence: evidence.clone(),
        });
        plan.routes.push(ObservedRoute {
            domain: route.domain,
            deployment_id,
            component: route.component,
            port: route.port,
            public: route.public,
            evidence,
        });
    }
    Ok(plan)
}

fn import_native_routes(
    connection: &mut Connection,
    plan: &CurrentPlan,
    now: &str,
) -> Result<(), String> {
    let transaction = connection.transaction().map_err(sql_error)?;
    for (deployment, route) in plan.deployments.iter().zip(&plan.routes) {
        let conflict: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM domain_routes WHERE domain=?1 UNION ALL SELECT 1 FROM observed_routes WHERE domain=?1 AND observed_deployment_id != ?2)",
            rusqlite::params![route.domain, deployment.deployment_id], |row| row.get(0)).map_err(sql_error)?;
        if conflict {
            return Err("native route conflicts with an existing deployment route".into());
        }
        transaction.execute(
            "INSERT INTO observed_deployments(observed_deployment_id,repository_id,name,native_project,state,health,source,evidence_json,observed_at,imported_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9) ON CONFLICT(observed_deployment_id) DO UPDATE SET name=excluded.name,state=excluded.state,health=excluded.health,evidence_json=excluded.evidence_json,observed_at=excluded.observed_at",
            rusqlite::params![deployment.deployment_id,deployment.repository_id,deployment.name,deployment.native_project,deployment.state,deployment.health,NATIVE_OBSERVATION_SOURCE,canonical_string(&deployment.evidence)?,now]).map_err(sql_error)?;
        transaction.execute(
            "INSERT INTO observed_routes(domain,observed_deployment_id,component,port,public,evidence_json,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(domain) DO UPDATE SET component=excluded.component,port=excluded.port,public=excluded.public,evidence_json=excluded.evidence_json,observed_at=excluded.observed_at",
            rusqlite::params![route.domain,route.deployment_id,route.component,route.port,i64::from(route.public),canonical_string(&route.evidence)?,now]).map_err(sql_error)?;
    }
    transaction.commit().map_err(sql_error)
}

pub fn run(options: &ImportOptions, now: &str) -> Result<Value, String> {
    let export = read_json(&options.export)?;
    let mut connection = open_database(&options.state_dir.join("authority.sqlite3"))?;
    let native = options
        .native_routes
        .as_deref()
        .map(read_json)
        .transpose()?
        .map(|document| plan_native_routes(&export, &document, &connection))
        .transpose()?;
    let mut report = import_state(
        &export,
        &mut connection,
        &options.bugs_dir,
        options.dry_run,
        now,
    )?;
    if let Some(live_path) = &options.live_containers {
        let live = read_json(live_path)?;
        let route_map = options
            .current_route_map
            .as_deref()
            .map(read_json)
            .transpose()?
            .unwrap_or_else(|| json!({"routes": []}));
        let current = plan_current_observations(&export, &live, &route_map, &connection)?;
        let mut current_report = serde_json::to_value(&current)
            .map_err(|error| format!("cannot encode current import plan: {error}"))?;
        if !options.dry_run {
            let counts = replace_current(&mut connection, &current, now)?;
            current_report["imported"] = counts;
            if let Some(routes_path) = &options.routes_path {
                let base_domain = options
                    .base_domain
                    .as_deref()
                    .ok_or_else(|| "--base-domain is required with --routes-path".to_owned())?;
                let generation = publish_routes(&mut connection, routes_path, base_domain, now)?;
                current_report["route_generation"] = json!(generation);
            }
        }
        report["current_observed"] = current_report;
    }
    if let Some(native) = native {
        if !options.dry_run {
            import_native_routes(&mut connection, &native, now)?;
            if let Some(path) = &options.routes_path {
                let domain = options
                    .base_domain
                    .as_deref()
                    .ok_or("--base-domain is required with --routes-path")?;
                publish_routes(&mut connection, path, domain, now)?;
            }
        }
        report["native_routes"] =
            serde_json::to_value(native).map_err(|error| error.to_string())?;
    }

    let candidates = if options.prune_missing_install_fixtures {
        missing_install_fixtures(&connection)?
    } else {
        Vec::new()
    };
    report["would_prune_install_fixtures"] = if options.dry_run {
        json!(candidates)
    } else {
        json!([])
    };
    report["pruned_install_fixtures"] = if options.dry_run {
        json!([])
    } else {
        json!(prune_missing_install_fixtures(
            &mut connection,
            &candidates
        )?)
    };
    report["pruned_telegram_scopes"] = if options.dry_run {
        json!([])
    } else {
        json!(prune_unregistered_telegram_scopes(&mut connection)?)
    };
    report["deployment_declaration_plan"] = plan_deployments(&export)?;
    report["dry_run"] = json!(options.dry_run);
    Ok(report)
}

pub fn plan_deployments(export: &Value) -> Result<Value, String> {
    let authority = object_or_empty(export.get("authority"));
    let servers = array_or_empty(authority.get("server_definitions"));
    let mut ports = HashMap::new();
    for port in array_or_empty(authority.get("port_assignments")) {
        let Some(port) = port.as_object() else {
            continue;
        };
        if port.get("status").and_then(Value::as_str) == Some("active")
            && let (Some(root), Some(server)) = (
                port.get("root").and_then(Value::as_str),
                port.get("server").and_then(Value::as_str),
            )
        {
            ports.insert(
                (root.to_owned(), server.to_owned()),
                Value::Object(port.clone()),
            );
        }
    }
    let mut plan = Vec::new();
    for server in servers {
        let server = server
            .as_object()
            .ok_or_else(|| "legacy server definition must be an object".to_owned())?;
        let role = string_field(server, "role")?;
        if matches!(role, "temporary" | "validation-port-lease") {
            continue;
        }
        let root = string_field(server, "root")?;
        let name = string_field(server, "name")?;
        let port = ports.get(&(root.to_owned(), name.to_owned()));
        let mut suggested = Map::new();
        suggested.insert(
            format!("deployment.{name}"),
            json!({
                "source": "worktree",
                "components": [name],
                "domain": "<label from legacy route, if any>",
            }),
        );
        suggested.insert(
            format!("deployment.{name}.component.{name}"),
            json!({
                "type": "process",
                "command": server.get("command").cloned().unwrap_or_else(|| json!([])),
                "cwd": server.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()).unwrap_or("."),
                "port": port.is_some(),
                "route": port.is_some(),
                "health": server.get("health_url_template").filter(|value| truthy(Some(value))).map(|_| json!({"path":"/"})),
                "env_names_to_reference_outside_repo": server.get("environment_looks_secret").cloned().unwrap_or_else(|| json!([])),
            }),
        );
        plan.push(json!({
            "repository_root": root,
            "legacy_server": name,
            "role": role,
            "suggested": suggested,
            "legacy_port": port.and_then(|port| port.get("port")).cloned().unwrap_or(Value::Null),
        }));
    }
    for route in array_or_empty(object_or_empty(export.get("routes")).get("routes")) {
        let route = route
            .as_object()
            .ok_or_else(|| "legacy route must be an object".to_owned())?;
        plan.push(json!({
            "legacy_route": route.get("slug").cloned().unwrap_or(Value::Null),
            "auth": route.get("auth").cloned().unwrap_or(Value::Null),
            "upstream_port": route.get("upstream_port").cloned().unwrap_or(Value::Null),
            "note": "map to the deployment serving this upstream; auth=public → public = true; authenticated → grants",
        }));
    }
    Ok(Value::Array(plan))
}

pub fn import_state(
    export: &Value,
    connection: &mut Connection,
    bugs_dir: &Path,
    dry_run: bool,
    now: &str,
) -> Result<Value, String> {
    let pending = object_or_empty(export.get("access_control"))
        .get("pending_requests")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let owners = array_or_empty(object_or_empty(export.get("routes")).get("owners"))
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut chats = Vec::new();
    let mut subscriptions = Vec::new();
    let mut legacy_roots = HashMap::new();
    for repository in array_or_empty(object_or_empty(export.get("authority")).get("repositories")) {
        if let Some(repository) = repository.as_object()
            && let (Some(id), Some(root)) = (
                repository.get("legacy_repo_id").and_then(Value::as_str),
                repository.get("root").and_then(Value::as_str),
            )
        {
            legacy_roots.insert(id.to_owned(), root.to_owned());
        }
    }
    let registered_roots = query_registered(connection)?;
    let telegram = object_or_empty(export.get("telegram"));
    for bot in array_or_empty(telegram.get("bots")) {
        let Some(bot) = bot.as_object() else { continue };
        let owner = bot
            .get("owner")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase)
            .or_else(|| owners.first().cloned())
            .unwrap_or_default();
        let mut scopes = Vec::new();
        if owners.contains(&owner) {
            scopes.push("server".to_owned());
        }
        for legacy_repository in array_or_empty(bot.get("projects")) {
            let Some(root) = legacy_repository
                .as_str()
                .and_then(|id| legacy_roots.get(id))
            else {
                continue;
            };
            if registered_roots.contains_key(root) {
                scopes.push(format!("repository:{}", repository_id(Path::new(root))?));
            }
        }
        for authorization in array_or_empty(telegram.get("authorizations")) {
            let Some(authorization) = authorization.as_object() else {
                continue;
            };
            if authorization.get("status").and_then(Value::as_str) != Some("approved") {
                continue;
            }
            let Some(chat_id) = integer_value(authorization.get("chat_id")).filter(|id| *id != 0)
            else {
                continue;
            };
            chats.push(json!({"chat_id":chat_id,"email":owner}));
            for scope in &scopes {
                subscriptions.push(json!({"chat_id":chat_id,"scope":scope}));
            }
            if !dry_run {
                let transaction = connection.transaction().map_err(sql_error)?;
                transaction
                    .execute(
                        "INSERT OR REPLACE INTO telegram_chats(chat_id,email,label,linked_at) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![chat_id, owner, authorization.get("username").and_then(Value::as_str), now],
                    )
                    .map_err(sql_error)?;
                for scope in &scopes {
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO telegram_subscriptions(chat_id,scope,created_at) VALUES(?1,?2,?3)",
                            rusqlite::params![chat_id, scope, now],
                        )
                        .map_err(sql_error)?;
                }
                transaction.commit().map_err(sql_error)?;
            }
        }
    }
    if !dry_run {
        let transaction = connection.transaction().map_err(sql_error)?;
        for email in &owners {
            let exists = transaction
                .query_row("SELECT 1 FROM users WHERE email=?1", [email], |_| Ok(()))
                .optional()
                .map_err(sql_error)?
                .is_some();
            if !exists {
                transaction
                    .execute(
                        "INSERT INTO users(user_id,email,administrator,created_at,created_by) VALUES(?1,?2,1,?3,'legacy-import')",
                        rusqlite::params![random_id('u', 8)?, email, now],
                    )
                    .map_err(sql_error)?;
            }
        }
        transaction.commit().map_err(sql_error)?;
    }

    let mut bugs = Vec::new();
    for legacy in array_or_empty(export.get("open_bugs")) {
        let Some(legacy) = legacy.as_object() else {
            continue;
        };
        let fields = LegacyBug {
            component: clip_chars(
                string_choice(legacy, &["component", "area"]).unwrap_or("legacy"),
                64,
            ),
            summary: clip_chars(
                string_choice(legacy, &["summary", "title"]).unwrap_or(""),
                200,
            ),
            expected: clip_chars(
                string_choice(legacy, &["expected", "expected_behavior"]).unwrap_or("-"),
                2_000,
            ),
            actual: clip_chars(
                string_choice(legacy, &["actual", "actual_behavior"]).unwrap_or("-"),
                2_000,
            ),
            steps: clip_chars(
                string_choice(legacy, &["steps", "reproduction", "reproduction_steps"])
                    .unwrap_or("-"),
                4_000,
            ),
        };
        if fields.summary.is_empty() {
            continue;
        }
        bugs.push(Value::String(fields.summary.clone()));
        if !dry_run && let Err(error) = import_bug(bugs_dir, &fields, now) {
            *bugs.last_mut().expect("just pushed") =
                Value::String(format!("SKIPPED ({error}): {}", fields.summary));
        }
    }
    Ok(json!({
        "administrators": owners,
        "telegram_chats": chats,
        "subscriptions": subscriptions,
        "bugs": bugs,
        "pending_access_requests": pending,
    }))
}

pub fn plan_current_observations(
    export: &Value,
    live_document: &Value,
    route_map: &Value,
    connection: &Connection,
) -> Result<CurrentPlan, String> {
    if live_document.get("ok") != Some(&Value::Bool(true)) {
        return Err("live container document must be a successful health.containers result".into());
    }
    let result = live_document
        .get("data")
        .or_else(|| live_document.get("result"))
        .and_then(Value::as_object)
        .ok_or_else(|| "live container document has no result.containers list".to_owned())?;
    let live = result
        .get("containers")
        .and_then(Value::as_array)
        .ok_or_else(|| "live container document has no result.containers list".to_owned())?;
    let registered = query_registered(connection)?;
    let mut mappings: HashMap<String, HashSet<(String, String, String)>> = HashMap::new();
    for resource in array_or_empty(object_or_empty(export.get("authority")).get("docker_resources"))
    {
        let Some(resource) = resource.as_object() else {
            continue;
        };
        let values = ["container_id", "root", "compose_project", "compose_service"]
            .map(|name| resource.get(name).and_then(Value::as_str));
        if let [Some(container), Some(root), Some(project), Some(service)] = values
            && values.iter().flatten().all(|value| !value.is_empty())
        {
            mappings.entry(container.to_owned()).or_default().insert((
                root.to_owned(),
                project.to_owned(),
                service.to_owned(),
            ));
        }
    }
    let mut containers = Vec::new();
    let mut skipped = Vec::new();
    let mut groups: BTreeMap<(String, String, String), Vec<ObservedContainer>> = BTreeMap::new();
    for item in live {
        let row = item
            .as_object()
            .ok_or_else(|| "live container entries must be objects".to_owned())?;
        let container_id = row.get("id").and_then(Value::as_str).map(str::to_owned);
        if row.get("state").and_then(Value::as_str) != Some("running") {
            skipped.push(json!({"container_id":container_id,"reason":"not_running"}));
            continue;
        }
        let Some(container_id) = container_id else {
            skipped.push(json!({"container_id":null,"reason":"no_exact_mapping"}));
            continue;
        };
        let matches = mappings.get(&container_id).cloned().unwrap_or_default();
        if matches.len() != 1 {
            skipped.push(json!({
                "container_id": container_id,
                "reason": if matches.is_empty() {"no_exact_mapping"} else {"conflicting_mapping"},
            }));
            continue;
        }
        let (root, project, service) = matches.into_iter().next().expect("one match");
        let Some(repository_id) = registered.get(&root).cloned() else {
            skipped.push(json!({
                "container_id":container_id,
                "reason":"repository_not_registered",
                "repository_root":root,
            }));
            continue;
        };
        let deployment_id = observed_deployment_id(&repository_id, &project);
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let container = ObservedContainer {
            container_id,
            deployment_id: deployment_id.clone(),
            repository_id: repository_id.clone(),
            name: row
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(&service)
                .to_owned(),
            image: row
                .get("image")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown")
                .to_owned(),
            compose_service: service.clone(),
            health: container_health(&status).to_owned(),
            status,
        };
        groups
            .entry((repository_id, root, project))
            .or_default()
            .push(container.clone());
        containers.push(container);
    }

    let mut deployments = Vec::new();
    let mut by_project = HashMap::new();
    for ((repository_id, root, project), items) in groups {
        let healths = items
            .iter()
            .map(|item| item.health.as_str())
            .collect::<HashSet<_>>();
        let health = if healths.contains("unhealthy") {
            "unhealthy"
        } else if healths.len() == 1 && healths.contains("healthy") {
            "healthy"
        } else {
            "unknown"
        };
        let mut container_ids = items
            .iter()
            .map(|item| item.container_id.clone())
            .collect::<Vec<_>>();
        container_ids.sort();
        let deployment = ObservedDeployment {
            deployment_id: observed_deployment_id(&repository_id, &project),
            repository_id: repository_id.clone(),
            name: project.clone(),
            native_project: project.clone(),
            state: if health == "unhealthy" {
                "degraded"
            } else {
                "running"
            }
            .to_owned(),
            health: health.to_owned(),
            evidence: json!({
                "repository_root": root,
                "container_ids": container_ids,
                "legacy_exported_at": export.get("exported_at").cloned().unwrap_or(Value::Null),
            }),
        };
        by_project.insert((root, project), deployment.deployment_id.clone());
        deployments.push(deployment);
    }

    let routes = route_map
        .as_object()
        .and_then(|route_map| route_map.get("routes"))
        .and_then(Value::as_array)
        .ok_or_else(|| "current route map routes must be a list".to_owned())?;
    let expected = [
        "domain",
        "repository_root",
        "native_project",
        "component",
        "port",
        "public",
        "evidence",
    ];
    let domain_regex =
        Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$").expect("constant domain regex");
    let mut planned_routes = Vec::new();
    for item in routes {
        let route = item
            .as_object()
            .ok_or_else(|| "current route entries must be objects".to_owned())?;
        if route.len() != expected.len() || expected.iter().any(|key| !route.contains_key(*key)) {
            return Err("current route fields do not match the reviewed schema".to_owned());
        }
        let domain = string_field(route, "domain")?;
        if !domain_regex.is_match(domain) {
            return Err("current route domain must be a DNS label".to_owned());
        }
        let port = route
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0)
            .ok_or_else(|| "current route port must be 1..65535".to_owned())?;
        let public = route
            .get("public")
            .and_then(Value::as_bool)
            .ok_or_else(|| "current route public must be boolean".to_owned())?;
        let root = string_field(route, "repository_root")?;
        let project = string_field(route, "native_project")?;
        let deployment_id = by_project
            .get(&(root.to_owned(), project.to_owned()))
            .cloned()
            .ok_or_else(|| format!("current route {domain} has no imported live project"))?;
        let component = string_field(route, "component")?;
        if !containers.iter().any(|container| {
            container.deployment_id == deployment_id && container.compose_service == component
        }) {
            return Err(format!("current route {domain} component is not running"));
        }
        planned_routes.push(ObservedRoute {
            domain: domain.to_owned(),
            deployment_id,
            component: component.to_owned(),
            port,
            public,
            evidence: route.get("evidence").cloned().unwrap_or_else(|| json!({})),
        });
    }
    Ok(CurrentPlan {
        deployments,
        containers,
        routes: planned_routes,
        skipped,
    })
}

pub fn prune_unregistered_telegram_scopes(
    connection: &mut Connection,
) -> Result<Vec<String>, String> {
    let valid = query_repository_ids(connection)?
        .into_iter()
        .map(|id| format!("repository:{id}"))
        .collect::<HashSet<_>>();
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT scope FROM telegram_subscriptions WHERE scope LIKE 'repository:%'",
        )
        .map_err(sql_error)?;
    let mut stale = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?
        .into_iter()
        .filter(|scope| !valid.contains(scope))
        .collect::<Vec<_>>();
    stale.sort();
    drop(statement);
    if !stale.is_empty() {
        let transaction = connection.transaction().map_err(sql_error)?;
        for scope in &stale {
            transaction
                .execute("DELETE FROM telegram_subscriptions WHERE scope=?1", [scope])
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
    }
    Ok(stale)
}

fn replace_current(
    connection: &mut Connection,
    current: &CurrentPlan,
    observed_at: &str,
) -> Result<Value, String> {
    let transaction = connection.transaction().map_err(sql_error)?;
    transaction
        .execute("DELETE FROM observed_routes WHERE observed_deployment_id IN (SELECT observed_deployment_id FROM observed_deployments WHERE source != ?1)", [NATIVE_OBSERVATION_SOURCE])
        .map_err(sql_error)?;
    transaction
        .execute("DELETE FROM observed_containers WHERE observed_deployment_id IN (SELECT observed_deployment_id FROM observed_deployments WHERE source != ?1)", [NATIVE_OBSERVATION_SOURCE])
        .map_err(sql_error)?;
    transaction
        .execute(
            "DELETE FROM observed_deployments WHERE source != ?1",
            [NATIVE_OBSERVATION_SOURCE],
        )
        .map_err(sql_error)?;
    for deployment in &current.deployments {
        transaction
            .execute(
                "INSERT INTO observed_deployments(observed_deployment_id,repository_id,name,native_project,state,health,source,evidence_json,observed_at,imported_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                rusqlite::params![deployment.deployment_id,deployment.repository_id,deployment.name,deployment.native_project,deployment.state,deployment.health,OBSERVATION_SOURCE,canonical_string(&deployment.evidence)?,observed_at],
            )
            .map_err(sql_error)?;
    }
    for container in &current.containers {
        transaction
            .execute(
                "INSERT INTO observed_containers(container_id,observed_deployment_id,repository_id,name,image,compose_service,state,status,health,observed_at) VALUES(?1,?2,?3,?4,?5,?6,'running',?7,?8,?9)",
                rusqlite::params![container.container_id,container.deployment_id,container.repository_id,container.name,container.image,container.compose_service,container.status,container.health,observed_at],
            )
            .map_err(sql_error)?;
    }
    for route in &current.routes {
        if transaction
            .query_row(
                "SELECT deployment_id FROM domain_routes WHERE domain=?1",
                [&route.domain],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
            .is_some()
        {
            return Err(format!(
                "observed route {} conflicts with a managed route",
                route.domain
            ));
        }
        transaction
            .execute(
                "INSERT INTO observed_routes(domain,observed_deployment_id,component,port,public,evidence_json,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![route.domain,route.deployment_id,route.component,route.port,i64::from(route.public),canonical_string(&route.evidence)?,observed_at],
            )
            .map_err(sql_error)?;
    }
    transaction.commit().map_err(sql_error)?;
    Ok(json!({
        "deployments": current.deployments.len(),
        "containers": current.containers.len(),
        "routes": current.routes.len(),
    }))
}

fn missing_install_fixtures(connection: &Connection) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(
            "SELECT repository_id,root_path FROM repositories WHERE root_path LIKE '/tmp/dc2-installed-%' ORDER BY root_path",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let mut candidates = Vec::new();
    for (repository_id, root) in rows {
        if Path::new(&root).exists() {
            continue;
        }
        if !repository_has_state(connection, &repository_id)? {
            candidates.push(root);
        }
    }
    Ok(candidates)
}

fn prune_missing_install_fixtures(
    connection: &mut Connection,
    candidates: &[String],
) -> Result<Vec<String>, String> {
    let transaction = connection.transaction().map_err(sql_error)?;
    let mut pruned = Vec::new();
    for root in candidates {
        let repository_id = transaction
            .query_row(
                "SELECT repository_id FROM repositories WHERE root_path=?1",
                [root],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?;
        let Some(repository_id) = repository_id else {
            continue;
        };
        if Path::new(root).exists() || repository_has_state(&transaction, &repository_id)? {
            continue;
        }
        transaction
            .execute(
                "DELETE FROM worktrees WHERE repository_id=?1",
                [&repository_id],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM repositories WHERE repository_id=?1",
                [&repository_id],
            )
            .map_err(sql_error)?;
        pruned.push(root.clone());
    }
    transaction.commit().map_err(sql_error)?;
    Ok(pruned)
}

fn publish_routes(
    connection: &mut Connection,
    path: &Path,
    base_domain: &str,
    now: &str,
) -> Result<u64, String> {
    let transaction = connection.transaction().map_err(sql_error)?;
    let generation = transaction
        .query_row(
            "SELECT value FROM meta WHERE key='route_generation'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    transaction
        .execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('route_generation',?1)",
            [generation.to_string()],
        )
        .map_err(sql_error)?;
    let mut routes = Vec::new();
    {
        let mut statement = transaction
            .prepare("SELECT r.domain,r.deployment_id,r.component,r.port,r.generation,d.public FROM domain_routes r JOIN deployments d ON d.deployment_id=r.deployment_id WHERE r.port IS NOT NULL ORDER BY r.domain")
            .map_err(sql_error)?;
        for row in statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                    row.get::<_, i64>(5)? != 0,
                ))
            })
            .map_err(sql_error)?
        {
            routes.push(route_value(row.map_err(sql_error)?, base_domain));
        }
    }
    {
        let mut statement = transaction
            .prepare("SELECT domain,observed_deployment_id,component,port,NULL,public FROM observed_routes WHERE port IS NOT NULL ORDER BY domain")
            .map_err(sql_error)?;
        for row in statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                    row.get::<_, i64>(5)? != 0,
                ))
            })
            .map_err(sql_error)?
        {
            routes.push(route_value(row.map_err(sql_error)?, base_domain));
        }
    }
    routes.sort_by(|left, right| left["label"].as_str().cmp(&right["label"].as_str()));
    let owners = query_strings(
        &transaction,
        "SELECT email FROM users WHERE administrator=1 ORDER BY email",
    )?;
    let mut grants = Vec::new();
    let mut statement = transaction
        .prepare("SELECT u.email,g.deployment_id,g.role FROM grants g JOIN users u ON u.user_id=g.user_id ORDER BY u.email,g.deployment_id")
        .map_err(sql_error)?;
    for row in statement
        .query_map([], |row| {
            Ok(json!({
                "identity": row.get::<_, String>(0)?,
                "deployment_id": row.get::<_, String>(1)?,
                "role": row.get::<_, String>(2)?,
            }))
        })
        .map_err(sql_error)?
    {
        grants.push(row.map_err(sql_error)?);
    }
    drop(statement);
    let access = json!({"owners":owners,"grants":grants});
    let payload = json!({
        "generation":generation,
        "published_at":now,
        "domain":base_domain,
        "routes":routes,
        "access":access,
    });
    let document = json!({
        "schema":1,
        "payload_sha256": sha256_hex(&serde_json::to_vec(&payload).map_err(|error| error.to_string())?),
        "generation":generation,
        "published_at":now,
        "domain":base_domain,
        "routes":payload["routes"],
        "access":payload["access"],
    });
    transaction.commit().map_err(sql_error)?;
    atomic_json(path, &document, 0o644)?;
    Ok(generation)
}

fn route_value(row: (String, String, String, u16, Option<u32>, bool), base_domain: &str) -> Value {
    let (label, deployment_id, component, port, generation, public) = row;
    json!({
        "deployment_id":deployment_id,
        "component":component,
        "label":label,
        "domain":if base_domain.is_empty() { label.clone() } else { format!("{label}.{base_domain}") },
        "port":port,
        "scheme":"http",
        "auth":if public {"public"} else {"authenticated"},
        "generation":generation,
    })
}

fn open_database(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create state directory: {error}"))?;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .map_err(sql_error)?;
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(sql_error)?;
    let version = connection
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    match version {
        Some(version) => {
            let parsed = version.parse::<u32>().ok();
            if !parsed.is_some_and(|value| [15, 16, 17, DATABASE_SCHEMA_VERSION].contains(&value)) {
                return Err(format!(
                    "legacy import target must use a supported database schema (15, 16, 17, {DATABASE_SCHEMA_VERSION}), found {version}"
                ));
            }
        }
        None => {
            let tables: u32 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [], |row| row.get(0),
            ).map_err(sql_error)?;
            if tables != 0 {
                return Err(
                    "legacy import target has existing tables without a schema version".into(),
                );
            }
            connection.execute_batch(SCHEMA).map_err(sql_error)?;
            connection
                .execute(
                    "INSERT INTO meta(key,value) VALUES('schema_version',?1)",
                    [DATABASE_SCHEMA_VERSION.to_string()],
                )
                .map_err(sql_error)?;
        }
    }
    Ok(connection)
}

fn query_registered(connection: &Connection) -> Result<HashMap<String, String>, String> {
    let mut statement = connection
        .prepare("SELECT root_path,repository_id FROM repositories")
        .map_err(sql_error)?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(sql_error)
}

fn query_repository_ids(connection: &Connection) -> Result<Vec<String>, String> {
    query_strings(connection, "SELECT repository_id FROM repositories")
}

fn query_strings(connection: &Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut statement = connection.prepare(sql).map_err(sql_error)?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn repository_has_state(connection: &Connection, repository_id: &str) -> Result<bool, String> {
    for table in ["deployments", "observed_deployments"] {
        let sql = format!("SELECT 1 FROM {table} WHERE repository_id=?1");
        if connection
            .query_row(&sql, [repository_id], |_| Ok(()))
            .optional()
            .map_err(sql_error)?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn container_health(status: &str) -> &'static str {
    let lower = status.to_lowercase();
    if lower.contains("unhealthy") {
        "unhealthy"
    } else if lower.contains("health: starting") {
        "starting"
    } else if lower.contains("healthy") {
        "healthy"
    } else {
        "unknown"
    }
}

fn repository_id(root: &Path) -> Result<String, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve imported repository: {error}"))?;
    let mut digest = Sha256::new();
    digest.update(REPOSITORY_NAMESPACE);
    digest.update(root.as_os_str().as_encoded_bytes());
    Ok(format!("r{}", first_hex(&digest.finalize(), 8)))
}

fn observed_deployment_id(repository_id: &str, project: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(OBSERVED_NAMESPACE);
    digest.update(repository_id.as_bytes());
    digest.update([0]);
    digest.update(project.as_bytes());
    format!("d{}", first_hex(&digest.finalize(), 8))
}

fn random_id(prefix: char, bytes: usize) -> Result<String, String> {
    let mut random = vec![0u8; bytes];
    getrandom::fill(&mut random).map_err(|error| format!("cannot create identifier: {error}"))?;
    Ok(format!("{prefix}{}", first_hex(&random, bytes)))
}

fn first_hex(bytes: &[u8], maximum: usize) -> String {
    let mut result = String::with_capacity(maximum * 2);
    for byte in bytes.iter().take(maximum) {
        use std::fmt::Write as _;
        write!(result, "{byte:02x}").expect("string write");
    }
    result
}

fn sha256_hex(bytes: &[u8]) -> String {
    first_hex(&Sha256::digest(bytes), 32)
}

fn canonical_string(value: &Value) -> Result<String, String> {
    serde_json::to_string(&canonical_json(value.clone()))
        .map_err(|error| format!("cannot encode imported evidence: {error}"))
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonical_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

fn read_json(path: &Path) -> Result<Value, String> {
    let file = unix_open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open import input: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect import input: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES {
        return Err("import input must be a regular file no larger than 32 MiB".to_owned());
    }
    let mut raw = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|error| format!("cannot read import input: {error}"))?;
    serde_json::from_slice(&raw).map_err(|error| format!("import input is invalid JSON: {error}"))
}

fn object_or_empty(value: Option<&Value>) -> &Map<String, Value> {
    static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
    value.and_then(Value::as_object).unwrap_or(&EMPTY)
}

fn array_or_empty(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn string_field<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("legacy field {name} must be a string"))
}

fn string_choice<'a>(object: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        object
            .get(*name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    })
}

fn integer_value(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(value)) if value.is_empty() => false,
        Some(Value::Array(value)) if value.is_empty() => false,
        Some(Value::Object(value)) if value.is_empty() => false,
        Some(Value::Number(value)) if value.as_i64() == Some(0) => false,
        Some(_) => true,
    }
}

fn clip_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn sql_error(error: rusqlite::Error) -> String {
    format!("legacy import database operation failed: {error}")
}

#[derive(Clone)]
struct LegacyBug {
    component: String,
    summary: String,
    expected: String,
    actual: String,
    steps: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBug {
    component: String,
    summary: String,
    expected: String,
    actual: String,
    steps: String,
    correlations: Value,
    bug_id: String,
    opened_at: String,
    last_seen_at: String,
    occurrences: u32,
    reporter: String,
}

fn import_bug(directory: &Path, bug: &LegacyBug, now: &str) -> Result<(), String> {
    validate_bug(bug)?;
    if directory.is_symlink() {
        return Err("bug directory must not be a symlink".to_owned());
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create bug directory: {error}"))?;
    let mut names = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot read bug directory: {error}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    names.sort();
    for path in names {
        if path.is_symlink() {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Ok(mut existing) = serde_json::from_slice::<StoredBug>(&raw) else {
            continue;
        };
        if existing.component == bug.component && existing.summary == bug.summary {
            existing.occurrences = existing
                .occurrences
                .checked_add(1)
                .ok_or_else(|| "bug occurrence count overflow".to_owned())?;
            existing.last_seen_at = now.to_owned();
            return atomic_json(&path, &serde_json::to_value(existing).unwrap(), 0o666);
        }
    }
    let record = StoredBug {
        component: bug.component.clone(),
        summary: bug.summary.clone(),
        expected: bug.expected.clone(),
        actual: bug.actual.clone(),
        steps: bug.steps.clone(),
        correlations: json!({}),
        bug_id: random_id('b', 6)?,
        opened_at: now.to_owned(),
        last_seen_at: now.to_owned(),
        occurrences: 1,
        reporter: "legacy-import".to_owned(),
    };
    atomic_json(
        &directory.join(format!("{}.json", record.bug_id)),
        &serde_json::to_value(record).unwrap(),
        0o666,
    )
}

fn validate_bug(bug: &LegacyBug) -> Result<(), String> {
    let forbidden = Regex::new(r"(?i)(password|secret|token|api[_-]?key|authorization:\s*bearer)")
        .expect("constant secret regex");
    let private = Regex::new(r"/home/[^/\s]+/|/etc/devcoordinator2/|/var/lib/devcoordinator2/")
        .expect("constant path regex");
    for value in [
        &bug.component,
        &bug.summary,
        &bug.expected,
        &bug.actual,
        &bug.steps,
    ] {
        if value.trim().is_empty() || forbidden.is_match(value) || private.is_match(value) {
            return Err("legacy bug contains invalid private text".to_owned());
        }
    }
    Ok(())
}

fn atomic_json(path: &Path, value: &Value, mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "output path has no parent".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| format!("cannot create output: {error}"))?;
    let temporary = parent.join(format!(".legacy-{}.tmp", random_id('x', 6)?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("cannot create temporary output: {error}"))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot set output permissions: {error}"))?;
        let mut payload = serde_json::to_vec_pretty(value)
            .map_err(|error| format!("cannot encode output: {error}"))?;
        payload.push(b'\n');
        file.write_all(&payload)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("cannot persist output: {error}"))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("cannot replace output: {error}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("cannot sync output directory: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_export(repository: &Path) -> Value {
        json!({
            "exported_at":"2026-08-23T00:00:00Z",
            "authority":{
                "repositories":[{"legacy_repo_id":"L1","root":repository,"display_name":"r","state":"active"}],
                "port_assignments":[{"root":repository,"server":"web","port":3003,"status":"active"}],
                "server_definitions":[
                    {"root":repository,"name":"web","role":"web","cwd":".","command":["npm","start"],"health_url_template":"http://x/","environment_names":["API_TOKEN"],"environment_looks_secret":["API_TOKEN"]},
                    {"root":repository,"name":"old-preview","role":"temporary","cwd":".","command":["npm","start"],"health_url_template":null,"environment_names":[],"environment_looks_secret":[]}
                ],
                "docker_resources":[
                    {"root":repository,"container_id":"a".repeat(64),"name":"app-1","image":"app:current","compose_project":"app-current","compose_service":"app"},
                    {"root":repository,"container_id":"b".repeat(64),"name":"old-1","image":"app:old","compose_project":"app-old","compose_service":"app"}
                ]
            },
            "routes":{"owners":["Owner@Example.test"],"routes":[{"slug":"news","auth":"google","upstream_port":3003}]},
            "access_control":{"pending_requests":[{"email":"p@example.test"}]},
            "telegram":{"bots":[{"owner":"owner@example.test","projects":["L1"]}],"authorizations":[{"chat_id":4242,"status":"approved","username":"u"}]},
            "open_bugs":[{"component":"api","summary":"legacy bug","expected":"a","actual":"b","steps":"c"}]
        })
    }

    fn native_fixture(repository: &Path) -> Value {
        json!({"schema":1,"routes":[{
            "domain":"news","repository_root":repository,"name":"News","component":"web",
            "port":3003,"public":false,"state":"running",
            "evidence":{"repository_root":repository,"port":3003,"listener_observed":true,
                "observed_at":"2026-09-07T00:00:00Z","pid":1234}
        }]})
    }

    #[test]
    fn native_import_preserves_supported_schema_and_rejects_future() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        for version in [15, 16, 17, DATABASE_SCHEMA_VERSION] {
            let path = temporary.path().join(format!("schema-{version}.sqlite3"));
            let connection = registered_database(&path, &repository);
            connection
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='schema_version'",
                    [version.to_string()],
                )
                .unwrap();
            let before: String = connection.query_row("SELECT group_concat(sql, ';') FROM (SELECT sql FROM sqlite_master ORDER BY name)", [], |row| row.get(0)).unwrap();
            drop(connection);
            for _ in 0..2 {
                let mut connection = open_database(&path).unwrap();
                let plan = plan_native_routes(
                    &fixture_export(&repository),
                    &native_fixture(&repository),
                    &connection,
                )
                .unwrap();
                import_native_routes(&mut connection, &plan, "t").unwrap();
                let actual: String = connection
                    .query_row(
                        "SELECT value FROM meta WHERE key='schema_version'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(actual, version.to_string());
                let after: String = connection.query_row("SELECT group_concat(sql, ';') FROM (SELECT sql FROM sqlite_master ORDER BY name)", [], |row| row.get(0)).unwrap();
                assert_eq!(before, after);
            }
        }
        let path = temporary.path().join("future.sqlite3");
        let connection = open_database(&path).unwrap();
        connection
            .execute(
                "UPDATE meta SET value=?1 WHERE key='schema_version'",
                [(DATABASE_SCHEMA_VERSION + 1).to_string()],
            )
            .unwrap();
        drop(connection);
        assert!(open_database(&path).is_err());
        let connection = Connection::open(&path).unwrap();
        let version: String = connection
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, (DATABASE_SCHEMA_VERSION + 1).to_string());
    }

    #[test]
    fn native_routes_preserve_identity_auth_and_truthful_state() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let connection =
            registered_database(&temporary.path().join("authority.sqlite3"), &repository);
        let export = fixture_export(&repository);
        let document = native_fixture(&repository);
        let current = plan_native_routes(&export, &document, &connection).unwrap();
        assert_eq!(current.deployments[0].state, "running");
        assert!(current.containers.is_empty());
        assert!(!current.routes[0].public);
        let mut stopped = document.clone();
        stopped["routes"][0]["state"] = json!("stopped");
        stopped["routes"][0]["evidence"]["listener_observed"] = json!(false);
        stopped["routes"][0]["evidence"]["pid"] = Value::Null;
        assert_eq!(
            plan_native_routes(&export, &stopped, &connection)
                .unwrap()
                .deployments[0]
                .state,
            "stopped"
        );
        for (key, value) in [
            ("public", json!(true)),
            ("port", json!(3004)),
            ("domain", json!("other")),
            ("repository_root", json!("/missing")),
            ("state", json!("stopped")),
        ] {
            let mut invalid = document.clone();
            invalid["routes"][0][key] = value;
            assert!(
                plan_native_routes(&export, &invalid, &connection).is_err(),
                "accepted {key}"
            );
        }
        let mut invalid = document.clone();
        invalid["routes"][0]["evidence"]["repository_root"] = json!("/other");
        assert!(plan_native_routes(&export, &invalid, &connection).is_err());
        let mut invalid_export = export.clone();
        invalid_export["authority"]["port_assignments"][0]["root"] = json!("/other");
        assert!(plan_native_routes(&invalid_export, &document, &connection).is_err());
    }

    #[test]
    fn native_routes_are_idempotent_and_survive_docker_reimport() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let mut connection =
            registered_database(&temporary.path().join("authority.sqlite3"), &repository);
        let plan = plan_native_routes(
            &fixture_export(&repository),
            &native_fixture(&repository),
            &connection,
        )
        .unwrap();
        import_native_routes(&mut connection, &plan, "t1").unwrap();
        import_native_routes(&mut connection, &plan, "t2").unwrap();
        replace_current(
            &mut connection,
            &CurrentPlan {
                deployments: Vec::new(),
                containers: Vec::new(),
                routes: Vec::new(),
                skipped: Vec::new(),
            },
            "t3",
        )
        .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM observed_routes", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM observed_containers", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT observed_at FROM observed_deployments", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "t2"
        );
        let path = temporary.path().join("routes.json");
        publish_routes(&mut connection, &path, "example.test", "t4").unwrap();
        let published = read_json(&path).unwrap();
        assert_eq!(published["routes"][0]["label"], "news");
        assert_eq!(published["routes"][0]["auth"], "authenticated");
    }

    fn registered_database(path: &Path, repository: &Path) -> Connection {
        let connection = open_database(path).unwrap();
        let repository_id = repository_id(repository).unwrap();
        let worktree_id = format!("w{}", &repository_id[1..]);
        connection
            .execute(
                "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,?2,'repo','t',1000,'t')",
                rusqlite::params![repository_id,repository.display().to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO worktrees VALUES(?1,?2,?3,'t','t')",
                rusqlite::params![worktree_id, repository_id, repository.display().to_string()],
            )
            .unwrap();
        connection
    }

    #[test]
    fn current_plan_excludes_historical_and_requires_exact_routes() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let connection =
            registered_database(&temporary.path().join("authority.sqlite3"), &repository);
        let export = fixture_export(&repository);
        let live = json!({"ok":true,"result":{"containers":[
            {"id":"a".repeat(64),"name":"app-1","image":"app:current","state":"running","status":"Up 1 hour (healthy)"},
            {"id":"b".repeat(64),"name":"old-1","image":"app:old","state":"exited","status":"Exited (0)"}
        ]}});
        let routes = json!({"routes":[{
            "domain":"app","repository_root":repository,"native_project":"app-current",
            "component":"app","port":3003,"public":true,"evidence":{"probe":"ok"}
        }]});
        let current = plan_current_observations(&export, &live, &routes, &connection).unwrap();
        assert_eq!(current.deployments.len(), 1);
        assert_eq!(current.containers.len(), 1);
        assert_eq!(current.routes.len(), 1);
        assert_eq!(current.skipped[0]["reason"], "not_running");
        let declaration = plan_deployments(&export).unwrap();
        assert_eq!(declaration.as_array().unwrap().len(), 2);
        assert_eq!(declaration[0]["legacy_port"], 3003);
        assert_eq!(
            declaration[0]["suggested"]["deployment.web.component.web"]["command"],
            json!(["npm", "start"])
        );
    }

    #[test]
    fn dry_run_is_non_mutating_and_apply_replaces_current_state() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let state = temporary.path().join("state");
        let database_path = state.join("authority.sqlite3");
        let connection = registered_database(&database_path, &repository);
        connection
            .execute(
                "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('rfixture','/tmp/dc2-installed-missing-rust-fixture','fixture','t',1000,'t')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO worktrees VALUES('wfixture','rfixture','/tmp/dc2-installed-missing-rust-fixture','t','t')",
                [],
            )
            .unwrap();
        drop(connection);
        let export = fixture_export(&repository);
        let export_path = temporary.path().join("export.json");
        std::fs::write(&export_path, serde_json::to_vec(&export).unwrap()).unwrap();
        let live_path = temporary.path().join("live.json");
        std::fs::write(
            &live_path,
            serde_json::to_vec(&json!({"ok":true,"result":{"containers":[
                {"id":"a".repeat(64),"name":"app-1","image":"app:current","state":"running","status":"Up (healthy)"}
            ]}}))
            .unwrap(),
        )
        .unwrap();
        let route_path = temporary.path().join("route-map.json");
        std::fs::write(
            &route_path,
            serde_json::to_vec(&json!({"routes":[{
                "domain":"app","repository_root":repository,"native_project":"app-current",
                "component":"app","port":3003,"public":true,"evidence":{"probe":"ok"}
            }]}))
            .unwrap(),
        )
        .unwrap();
        let mut options = ImportOptions {
            export: export_path,
            state_dir: state,
            bugs_dir: temporary.path().join("bugs"),
            live_containers: Some(live_path),
            current_route_map: Some(route_path),
            native_routes: None,
            routes_path: Some(temporary.path().join("routes.json")),
            base_domain: Some("example.test".to_owned()),
            prune_missing_install_fixtures: true,
            dry_run: true,
        };
        let dry = run(&options, "2026-09-04T00:00:00Z").unwrap();
        assert_eq!(dry["dry_run"], true);
        assert_eq!(
            dry["would_prune_install_fixtures"],
            json!(["/tmp/dc2-installed-missing-rust-fixture"])
        );
        let connection = Connection::open(&database_path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            0
        );
        drop(connection);
        options.dry_run = false;
        let applied = run(&options, "2026-09-04T00:00:00Z").unwrap();
        assert_eq!(
            applied["current_observed"]["imported"],
            json!({"deployments":1,"containers":1,"routes":1})
        );
        assert_eq!(
            applied["pruned_install_fixtures"],
            json!(["/tmp/dc2-installed-missing-rust-fixture"])
        );
        let connection = Connection::open(&database_path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM observed_deployments", [], |row| {
                    row.get::<_, u32>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT email FROM users", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "owner@example.test"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM repositories WHERE repository_id='rfixture'",
                    [],
                    |row| row.get::<_, u32>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                &std::fs::read(options.routes_path.as_ref().unwrap()).unwrap()
            )
            .unwrap()["routes"][0]["domain"],
            "app.example.test"
        );
        assert_eq!(std::fs::read_dir(&options.bugs_dir).unwrap().count(), 1);
    }
}
