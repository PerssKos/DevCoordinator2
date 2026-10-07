//! Durable harness routing policy and concise current-work instructions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use devcoordinator2_api::agent_routing::{Action, Capabilities, Role, Rule, RuleInput, Scope};
use devcoordinator2_api::{ClientKind, ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;

use crate::config::Config;
use crate::database::{Database, DatabaseError};

const DEFAULT_EXPIRY_MS: u64 = 24 * 60 * 60 * 1000;
const DISCOVERY_REFRESH_INTERVAL_MS: u64 = 60 * 60 * 1000;

const DEFAULT_ROLES: &[(&str, &str)] = &[
    ("ui_design", "UI design"),
    ("ui_implementation", "UI implementation"),
    ("ui_audit", "UI audit"),
    ("backend_implementation", "Back-end implementation"),
    ("other_coding", "Other coding"),
    ("debugging", "Debugging"),
    ("testing", "Testing"),
    ("deploying", "Deploying"),
    ("architecture", "Architecture"),
    ("coordination", "Coordination"),
    ("project_management", "Project management"),
    ("verification", "Verification"),
    ("small_corrections", "Small corrections"),
];

#[derive(Clone)]
pub struct AgentRoutingService {
    database: Database,
    discovery_lock: Arc<Mutex<()>>,
    last_discovery_attempt_ms: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl AgentRoutingService {
    pub fn new(database: Database) -> Result<Self, ProtocolError> {
        let service = Self {
            database,
            discovery_lock: Arc::new(Mutex::new(())),
            last_discovery_attempt_ms: Arc::new(Mutex::new(BTreeMap::new())),
        };
        service.seed_roles()?;
        Ok(service)
    }

    fn seed_roles(&self) -> Result<(), ProtocolError> {
        let now = now_text();
        self.database
            .transaction(move |tx| {
                let count: i64 = tx.query_row("SELECT COUNT(*) FROM agent_roles", [], |r| r.get(0))?;
                if count == 0 {
                    for (position, (role_id, title)) in DEFAULT_ROLES.iter().enumerate() {
                        tx.execute(
                            "INSERT INTO agent_roles(role_id,title,position,retired,revision,created_at,updated_at) VALUES(?1,?2,?3,0,1,?4,?4)",
                            rusqlite::params![role_id, title, position as i64, now],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(db_error)
    }

    pub fn ensure_capability(
        &self,
        config: &Config,
        harness: ClientKind,
        now_ms: u64,
    ) -> Result<Capabilities, ProtocolError> {
        let current = self
            .database
            .call(move |c| read_capability(c, harness, now_ms))
            .map_err(db_error)?;
        if current.reported_at_ms.is_some() {
            return Ok(current);
        }
        let guard = self
            .discovery_lock
            .lock()
            .map_err(|_| invalid("agent capability discovery lock unavailable"))?;
        let latest = self
            .database
            .call(move |c| read_capability(c, harness, now_ms))
            .map_err(db_error)?;
        if latest.reported_at_ms.is_some() {
            drop(guard);
            return Ok(latest);
        }
        let key = harness_text(harness).to_owned();
        {
            let attempts = self
                .last_discovery_attempt_ms
                .lock()
                .map_err(|_| invalid("agent capability discovery state unavailable"))?;
            if attempts.get(&key).is_some_and(|attempt| {
                now_ms.saturating_sub(*attempt) < DISCOVERY_REFRESH_INTERVAL_MS
            }) {
                drop(guard);
                return Ok(latest);
            }
        }
        self.last_discovery_attempt_ms
            .lock()
            .map_err(|_| invalid("agent capability discovery state unavailable"))?
            .insert(key.clone(), now_ms);
        let Ok(discovered) = crate::capability_discovery::discover(config, harness, now_ms) else {
            drop(guard);
            return Ok(latest);
        };
        self.store_discovered(&discovered, harness_text(harness), now_ms)?;
        self.last_discovery_attempt_ms
            .lock()
            .map_err(|_| invalid("agent capability discovery state unavailable"))?
            .remove(&key);
        drop(guard);
        self.database
            .call(move |c| read_capability(c, harness, now_ms))
            .map_err(db_error)
    }

    pub fn refresh_capabilities(&self, config: &Config, now_ms: u64) -> Result<(), ProtocolError> {
        let _guard = self
            .discovery_lock
            .lock()
            .map_err(|_| invalid("agent capability discovery lock unavailable"))?;
        for harness in [ClientKind::Codex, ClientKind::Antigravity] {
            let current = self
                .database
                .call(move |c| read_capability(c, harness, now_ms))
                .map_err(db_error)?;
            let due = current.reported_at_ms.is_none_or(|reported| {
                now_ms.saturating_sub(reported) >= DISCOVERY_REFRESH_INTERVAL_MS
            });
            if !due {
                continue;
            }
            let key = harness_text(harness).to_owned();
            let attempted_recently = self
                .last_discovery_attempt_ms
                .lock()
                .map_err(|_| invalid("agent capability discovery state unavailable"))?
                .get(&key)
                .is_some_and(|attempt| {
                    now_ms.saturating_sub(*attempt) < DISCOVERY_REFRESH_INTERVAL_MS
                });
            if attempted_recently {
                continue;
            }
            self.last_discovery_attempt_ms
                .lock()
                .map_err(|_| invalid("agent capability discovery state unavailable"))?
                .insert(key.clone(), now_ms);
            if let Ok(discovered) = crate::capability_discovery::discover(config, harness, now_ms) {
                self.store_discovered(&discovered, harness_text(harness), now_ms)?;
                self.last_discovery_attempt_ms
                    .lock()
                    .map_err(|_| invalid("agent capability discovery state unavailable"))?
                    .remove(&key);
            }
        }
        Ok(())
    }

    fn store_discovered(
        &self,
        capabilities: &Capabilities,
        source: &str,
        now_ms: u64,
    ) -> Result<(), ProtocolError> {
        let models_json = serde_json::to_string(&capabilities.models)
            .map_err(|_| invalid("cannot encode discovered models"))?;
        let efforts_json = serde_json::to_string(&capabilities.efforts)
            .map_err(|_| invalid("cannot encode discovered efforts"))?;
        let pairs_json = serde_json::to_string(&capabilities.pairs)
            .map_err(|_| invalid("cannot encode discovered capability pairs"))?;
        let harness = harness_text(capabilities.harness);
        let expires_at_ms = capabilities.expires_at_ms;
        let source = source.to_owned();
        self.database
            .call(move |c| {
                c.execute(
                    "INSERT INTO agent_capabilities(harness,models_json,efforts_json,pairs_json,reported_at_ms,expires_at_ms,reported_by) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(harness) DO UPDATE SET models_json=excluded.models_json,efforts_json=excluded.efforts_json,pairs_json=excluded.pairs_json,reported_at_ms=excluded.reported_at_ms,expires_at_ms=excluded.expires_at_ms,reported_by=excluded.reported_by",
                    rusqlite::params![harness, models_json, efforts_json, pairs_json, now_ms as i64, expires_at_ms.map(|value| value as i64), source],
                )?;
                Ok(())
            })
            .map_err(db_error)
    }

    pub fn settings(
        &self,
        scope: Scope,
        repository_id: Option<String>,
        harness: ClientKind,
        now_ms: u64,
    ) -> Result<devcoordinator2_api::agent_routing::Settings, ProtocolError> {
        let repo = normalize_repo(scope, repository_id)?;
        let repo_for_query = repo.clone();
        let harness_text = harness_text(harness);
        let (
            roles,
            roles_revision,
            global_rules,
            repository_rules,
            global_revision,
            repository_revision,
            capability,
        ) = self
            .database
            .call(move |c| {
                let roles = read_roles(c)?;
                let roles_revision: u32 = c.query_row(
                    "SELECT COALESCE(MAX(revision),0) FROM agent_roles",
                    [],
                    |r| r.get(0),
                )?;
                let global = read_rules(c, "global", "", harness_text)?;
                let repository = if repo_for_query.is_empty() {
                    None
                } else {
                    Some(read_rules(c, "repository", &repo_for_query, harness_text)?)
                };
                let capability = read_capability(c, harness, now_ms)?;
                Ok((
                    roles,
                    roles_revision,
                    global.0,
                    repository.as_ref().map(|value| value.0.clone()),
                    global.1,
                    repository.map(|value| value.1),
                    capability,
                ))
            })
            .map_err(db_error)?;
        let mut effective = BTreeMap::new();
        for rule in global_rules {
            effective.insert(rule.role_id.clone(), (rule, true));
        }
        if let Some(repository_rules) = repository_rules {
            for rule in repository_rules {
                effective.insert(rule.role_id.clone(), (rule, false));
            }
        }
        let rules = roles
            .iter()
            .filter(|role| !role.retired)
            .map(|role| {
                if let Some((rule, inherited)) = effective.remove(&role.role_id) {
                    let stale = rule.model.as_ref().is_some_and(|model| {
                        !capability.fresh
                            || !capability.models.iter().any(|candidate| candidate == model)
                            || rule.effort.as_ref().is_none_or(|effort| {
                                !capability
                                    .efforts
                                    .iter()
                                    .any(|candidate| candidate == effort)
                            })
                            || (!capability.pairs.is_empty()
                                && rule.effort.as_ref().is_none_or(|effort| {
                                    !capability
                                        .pairs
                                        .iter()
                                        .any(|pair| &pair.model == model && &pair.effort == effort)
                                }))
                    });
                    Rule {
                        role_id: rule.role_id,
                        action: rule.action,
                        model: rule.model,
                        effort: rule.effort,
                        inherited,
                        stale,
                    }
                } else {
                    Rule {
                        role_id: role.role_id.clone(),
                        action: Action::NeverSpawn,
                        model: None,
                        effort: None,
                        inherited: false,
                        stale: false,
                    }
                }
            })
            .collect();
        let revision = if scope == Scope::Repository {
            repository_revision.unwrap_or(0)
        } else {
            global_revision
        };
        Ok(devcoordinator2_api::agent_routing::Settings {
            scope,
            repository_id: (!repo.is_empty()).then_some(repo),
            harness,
            revision,
            roles_revision,
            roles,
            rules,
            capabilities: capability,
        })
    }

    pub fn save_settings(
        &self,
        input: devcoordinator2_api::params::AgentSettingsSave,
        actor: &str,
        now_ms: u64,
    ) -> Result<devcoordinator2_api::agent_routing::Settings, ProtocolError> {
        let repo = normalize_repo(input.scope, input.repository_id)?;
        let harness_text = harness_text(input.harness);
        let capability = self
            .database
            .call(move |c| read_capability(c, input.harness, now_ms))
            .map_err(db_error)?;
        validate_rules(&input.rules, &capability, &self.roles()?)?;
        let rules_json = serde_json::to_string(&input.rules)
            .map_err(|_| invalid("cannot encode routing rules"))?;
        let scope_text = scope_text(input.scope);
        let repo_for_write = repo.clone();
        let actor = actor.to_owned();
        let now = now_text();
        self.database
            .transaction(move |tx| {
                let current: Option<u32> = tx
                    .query_row(
                        "SELECT revision FROM agent_routing_settings WHERE scope=?1 AND repository_id=?2 AND harness=?3",
                        rusqlite::params![scope_text, repo_for_write, harness_text],
                        |r| r.get(0),
                    )
                    .optional()?;
                if current.unwrap_or(0) != input.expected_revision {
                    return Err(invalid("agent settings revision conflict").into());
                }
                let revision = input.expected_revision + 1;
                tx.execute(
                    "INSERT INTO agent_routing_settings(scope,repository_id,harness,revision,rules_json,updated_at,updated_by) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(scope,repository_id,harness) DO UPDATE SET revision=excluded.revision,rules_json=excluded.rules_json,updated_at=excluded.updated_at,updated_by=excluded.updated_by",
                    rusqlite::params![scope_text, repo_for_write, harness_text, revision, rules_json, now, actor],
                )?;
                tx.execute(
                    "INSERT INTO agent_routing_history(scope,repository_id,harness,revision,rules_json,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![scope_text, repo_for_write, harness_text, revision, rules_json, actor, now],
                )?;
                Ok(())
            })
            .map_err(db_error)?;
        self.settings(
            input.scope,
            (!repo.is_empty()).then_some(repo),
            input.harness,
            now_ms,
        )
    }

    pub fn roles(&self) -> Result<Vec<Role>, ProtocolError> {
        self.database.call(|c| read_roles(c)).map_err(db_error)
    }

    pub fn save_roles(
        &self,
        input: devcoordinator2_api::params::AgentRolesSave,
        actor: &str,
    ) -> Result<(u32, Vec<Role>), ProtocolError> {
        let mut ids = BTreeSet::new();
        for role in &input.roles {
            if !valid_role_id(&role.role_id)
                || role.title.trim().is_empty()
                || role.title.len() > 200
            {
                return Err(invalid(
                    "role identifiers and titles must be non-empty and bounded",
                ));
            }
            if !ids.insert(role.role_id.clone()) {
                return Err(invalid("role identifiers must be unique"));
            }
        }
        let actor = actor.to_owned();
        let now = now_text();
        let result = self.database.transaction(move |tx| {
            let current: u32 = tx.query_row("SELECT COALESCE(MAX(revision),0) FROM agent_roles", [], |r| r.get(0))?;
            if current != input.expected_revision {
                return Err(invalid("agent role revision conflict").into());
            }
            let revision = current + 1;
            for role in &input.roles {
                tx.execute(
                    "INSERT INTO agent_roles(role_id,title,position,retired,revision,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?6) ON CONFLICT(role_id) DO UPDATE SET title=excluded.title,position=excluded.position,retired=excluded.retired,revision=excluded.revision,updated_at=excluded.updated_at",
                    rusqlite::params![role.role_id, role.title, role.position, role.retired, revision, now],
                )?;
                tx.execute(
                    "INSERT INTO agent_role_history(role_id,title,position,retired,revision,actor,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![role.role_id, role.title, role.position, role.retired, revision, actor, now],
                )?;
            }
            Ok(revision)
        }).map_err(db_error)?;
        Ok((result, self.roles()?))
    }

    pub fn report_capabilities(
        &self,
        input: devcoordinator2_api::params::AgentCapabilitiesReport,
        caller_kind: ClientKind,
        actor: &str,
        now_ms: u64,
    ) -> Result<Capabilities, ProtocolError> {
        if input.harness != caller_kind && caller_kind != ClientKind::Other {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "a harness may report only its own capabilities",
            ));
        }
        let models = unique_bounded(input.models, 256, "models")?;
        let efforts = unique_bounded(input.efforts, 64, "efforts")?;
        for pair in &input.pairs {
            if !models.contains(&pair.model) || !efforts.contains(&pair.effort) {
                return Err(invalid(
                    "capability pairs must reference reported models and efforts",
                ));
            }
        }
        let expires = input.expires_at_ms.or(Some(now_ms + DEFAULT_EXPIRY_MS));
        let models_json =
            serde_json::to_string(&models).map_err(|_| invalid("cannot encode models"))?;
        let efforts_json =
            serde_json::to_string(&efforts).map_err(|_| invalid("cannot encode efforts"))?;
        let pairs_json = serde_json::to_string(&input.pairs)
            .map_err(|_| invalid("cannot encode capability pairs"))?;
        let harness = harness_text(input.harness);
        let actor = actor.to_owned();
        self.database.call(move |c| {
            c.execute("INSERT INTO agent_capabilities(harness,models_json,efforts_json,pairs_json,reported_at_ms,expires_at_ms,reported_by) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(harness) DO UPDATE SET models_json=excluded.models_json,efforts_json=excluded.efforts_json,pairs_json=excluded.pairs_json,reported_at_ms=excluded.reported_at_ms,expires_at_ms=excluded.expires_at_ms,reported_by=excluded.reported_by", rusqlite::params![harness,models_json,efforts_json,pairs_json,now_ms as i64,expires.map(|value| value as i64),actor])?;
            Ok(())
        }).map_err(db_error)?;
        self.database
            .call(move |c| read_capability(c, input.harness, now_ms))
            .map_err(db_error)
    }

    pub fn instruction(
        &self,
        repository_id: Option<String>,
        harness: ClientKind,
        model: Option<String>,
        effort: Option<String>,
        now_ms: u64,
    ) -> Result<devcoordinator2_api::results::AgentInstruction, ProtocolError> {
        let settings = self
            .settings(Scope::Repository, repository_id.clone(), harness, now_ms)
            .or_else(|_| self.settings(Scope::Global, None, harness, now_ms))?;
        let current_model = model;
        let current_effort = effort;
        let mut assignments = Vec::new();
        for role in &settings.roles {
            let rule = settings
                .rules
                .iter()
                .find(|r| r.role_id == role.role_id)
                .cloned()
                .unwrap_or(Rule {
                    role_id: role.role_id.clone(),
                    action: Action::NeverSpawn,
                    model: None,
                    effort: None,
                    inherited: false,
                    stale: false,
                });
            let (resolved, reason) = match rule.action {
                Action::NeverSpawn => (true, Some("owned".to_owned())),
                Action::AlwaysSpawn => (true, Some("spawn".to_owned())),
                Action::SkipIfModelMatches => match (&current_model, &rule.model) {
                    (Some(current), Some(target)) if current == target => {
                        (true, Some("owned".to_owned()))
                    }
                    (Some(_), Some(_)) => (true, Some("spawn".to_owned())),
                    _ => (false, Some("current model is unavailable".to_owned())),
                },
                Action::SkipIfModelAndEffortMatches => {
                    match (&current_model, &current_effort, &rule.model, &rule.effort) {
                        (Some(cm), Some(ce), Some(tm), Some(te)) if cm == tm && ce == te => {
                            (true, Some("owned".to_owned()))
                        }
                        (Some(_), Some(_), Some(_), Some(_)) => (true, Some("spawn".to_owned())),
                        _ => (
                            false,
                            Some("current model and effort are unavailable".to_owned()),
                        ),
                    }
                }
            };
            assignments.push(devcoordinator2_api::results::AgentAssignment {
                role_id: role.role_id.clone(),
                title: role.title.clone(),
                action: rule.action,
                model: rule.model,
                effort: rule.effort,
                resolved,
                reason,
            });
        }
        let owned: Vec<_> = assignments
            .iter()
            .filter(|a| a.resolved && a.reason.as_deref() == Some("owned"))
            .map(|a| a.title.clone())
            .collect();
        let unresolved: Vec<_> = assignments
            .iter()
            .filter(|a| !a.resolved)
            .map(|a| a.title.clone())
            .collect();
        let text = if current_model.is_some() && current_effort.is_some() {
            let prefix = format!(
                "You ({} {}) own: {}.",
                current_model.as_deref().unwrap_or("current model"),
                current_effort.as_deref().unwrap_or("current effort"),
                if owned.is_empty() {
                    "no roles".to_owned()
                } else {
                    owned.join(", ")
                }
            );
            let mut spawn_groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
            for assignment in assignments.iter().filter(|assignment| {
                assignment.resolved && assignment.reason.as_deref() == Some("spawn")
            }) {
                if let (Some(model), Some(effort)) = (&assignment.model, &assignment.effort) {
                    spawn_groups
                        .entry((model.clone(), effort.clone()))
                        .or_default()
                        .push(assignment.title.clone());
                }
            }
            let spawn = if spawn_groups.is_empty() {
                String::new()
            } else {
                spawn_groups
                    .into_iter()
                    .map(|((model, effort), roles)| {
                        format!(
                            " Spawn {} {} agent for: {}.",
                            model,
                            effort,
                            roles.join(", ")
                        )
                    })
                    .collect::<String>()
            };
            let pending = if unresolved.is_empty() {
                String::new()
            } else {
                format!(
                    " Conditional roles unresolved until current metadata is available: {}.",
                    unresolved.join(", ")
                )
            };
            format!("{}{}{}", prefix, spawn, pending)
        } else {
            format!(
                "Follow the configured routing rules. Current model and effort are unavailable; conditional ownership remains unresolved{}.",
                if unresolved.is_empty() {
                    String::new()
                } else {
                    format!(" for {}", unresolved.join(", "))
                }
            )
        };
        Ok(devcoordinator2_api::results::AgentInstruction {
            repository_id,
            harness,
            current_model,
            current_effort,
            assignments,
            text,
            cache_expires_at_ms: settings.capabilities.expires_at_ms,
        })
    }
}

fn validate_rules(
    rules: &[RuleInput],
    capability: &Capabilities,
    roles: &[Role],
) -> Result<(), ProtocolError> {
    let ids: BTreeSet<_> = roles
        .iter()
        .filter(|r| !r.retired)
        .map(|r| r.role_id.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for rule in rules {
        if !ids.contains(rule.role_id.as_str()) || !seen.insert(rule.role_id.clone()) {
            return Err(invalid(
                "routing rules must reference each active role at most once",
            ));
        }
        let needs_target = !matches!(rule.action, Action::NeverSpawn);
        if needs_target && (rule.model.is_none() || rule.effort.is_none()) {
            return Err(invalid("spawn rules require both model and effort"));
        }
        if !needs_target && (rule.model.is_some() || rule.effort.is_some()) {
            return Err(invalid("never-spawn rules cannot carry a target"));
        }
        if let (Some(model), Some(effort)) = (&rule.model, &rule.effort)
            && (!capability.fresh
                || !capability.models.contains(model)
                || !capability.efforts.contains(effort)
                || (!capability.pairs.is_empty()
                    && !capability
                        .pairs
                        .iter()
                        .any(|pair| &pair.model == model && &pair.effort == effort)))
        {
            return Err(invalid(
                "target model and effort are not present in a fresh capability report",
            ));
        }
    }
    Ok(())
}

fn unique_bounded(
    values: Vec<String>,
    max: usize,
    name: &str,
) -> Result<Vec<String>, ProtocolError> {
    if values.len() > max
        || values.iter().any(|value| {
            value.trim().is_empty() || value.len() > 160 || value.chars().any(char::is_control)
        })
    {
        return Err(invalid(format!("{name} are empty or exceed the limit")));
    }
    let mut seen = BTreeSet::new();
    for value in &values {
        if !seen.insert(value.clone()) {
            return Err(invalid(format!("{name} must be unique")));
        }
    }
    Ok(values)
}

fn read_roles(c: &rusqlite::Connection) -> Result<Vec<Role>, DatabaseError> {
    let mut q = c.prepare(
        "SELECT role_id,title,position,retired FROM agent_roles ORDER BY position,role_id",
    )?;
    Ok(q.query_map([], |r| {
        Ok(Role {
            role_id: r.get(0)?,
            title: r.get(1)?,
            position: r.get(2)?,
            retired: r.get::<_, i64>(3)? != 0,
        })
    })?
    .collect::<Result<Vec<_>, _>>()?)
}

fn read_rules(
    c: &rusqlite::Connection,
    scope: &str,
    repository_id: &str,
    harness: &str,
) -> Result<(Vec<RuleInput>, u32), DatabaseError> {
    let row: Option<(u32,String)> = c.query_row("SELECT revision,rules_json FROM agent_routing_settings WHERE scope=?1 AND repository_id=?2 AND harness=?3", rusqlite::params![scope,repository_id,harness], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    match row {
        Some((revision, json)) => Ok((
            serde_json::from_str(&json)
                .map_err(|_| DatabaseError::Domain(invalid("stored routing rules are invalid")))?,
            revision,
        )),
        None => Ok((Vec::new(), 0)),
    }
}

fn read_capability(
    c: &rusqlite::Connection,
    harness: ClientKind,
    now_ms: u64,
) -> Result<Capabilities, DatabaseError> {
    let row: Option<(String,String,String,i64,Option<i64>)> = c.query_row("SELECT models_json,efforts_json,pairs_json,reported_at_ms,expires_at_ms FROM agent_capabilities WHERE harness=?1", [harness_text(harness)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((models, efforts, pairs, reported_at_ms, expires_at_ms)) = row else {
        return Ok(Capabilities {
            harness,
            models: Vec::new(),
            efforts: Vec::new(),
            pairs: Vec::new(),
            reported_at_ms: None,
            expires_at_ms: None,
            fresh: false,
        });
    };
    let reported_at_ms = u64::try_from(reported_at_ms).unwrap_or(0);
    let expires_at_ms = expires_at_ms.map(|value| u64::try_from(value).unwrap_or(0));
    Ok(Capabilities {
        harness,
        models: serde_json::from_str(&models)
            .map_err(|_| DatabaseError::Domain(invalid("stored models are invalid")))?,
        efforts: serde_json::from_str(&efforts)
            .map_err(|_| DatabaseError::Domain(invalid("stored efforts are invalid")))?,
        pairs: serde_json::from_str(&pairs)
            .map_err(|_| DatabaseError::Domain(invalid("stored capability pairs are invalid")))?,
        reported_at_ms: Some(reported_at_ms),
        expires_at_ms,
        fresh: expires_at_ms.is_none_or(|expires| expires > now_ms),
    })
}

fn scope_text(scope: Scope) -> &'static str {
    if scope == Scope::Global {
        "global"
    } else {
        "repository"
    }
}
fn normalize_repo(scope: Scope, repository_id: Option<String>) -> Result<String, ProtocolError> {
    match (scope, repository_id) {
        (Scope::Global, None) => Ok(String::new()),
        (Scope::Global, Some(_)) => Err(invalid("global settings cannot include a repository")),
        (Scope::Repository, Some(id)) if !id.trim().is_empty() => Ok(id),
        (Scope::Repository, Some(_)) => Err(invalid("repository settings require a repository")),
        (Scope::Repository, None) => Err(invalid("repository settings require a repository")),
    }
}
fn harness_text(harness: ClientKind) -> &'static str {
    match harness {
        ClientKind::Codex => "codex",
        ClientKind::Claude => "claude",
        ClientKind::Cursor => "cursor",
        ClientKind::Antigravity => "antigravity",
        ClientKind::Other => "other",
        ClientKind::Human => "human",
        ClientKind::Edge => "edge",
    }
}
fn now_text() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ConfigurationInvalid, message)
}
fn db_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, other.to_string()),
    }
}

fn valid_role_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
        && value.len() <= 80
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn capability_report_save_and_instruction_resolve_conditional_rules() {
        let temp = tempdir().unwrap();
        let database = Database::open(temp.path().join("authority.sqlite3")).unwrap();
        let service = AgentRoutingService::new(database).unwrap();
        let caps = service
            .report_capabilities(
                devcoordinator2_api::params::AgentCapabilitiesReport {
                    harness: ClientKind::Codex,
                    models: vec!["gpt-6-astra".into(), "gpt-6.1-sol".into()],
                    efforts: vec!["high".into(), "max".into()],
                    pairs: vec![devcoordinator2_api::agent_routing::Pair {
                        model: "gpt-6.1-sol".into(),
                        effort: "max".into(),
                    }],
                    expires_at_ms: Some(10_000),
                },
                ClientKind::Codex,
                "uid:1",
                1,
            )
            .unwrap();
        assert!(caps.fresh);
        let saved = service
            .save_settings(
                devcoordinator2_api::params::AgentSettingsSave {
                    scope: Scope::Global,
                    repository_id: None,
                    harness: ClientKind::Codex,
                    expected_revision: 0,
                    rules: vec![RuleInput {
                        role_id: "backend_implementation".into(),
                        action: Action::AlwaysSpawn,
                        model: Some("gpt-6.1-sol".into()),
                        effort: Some("max".into()),
                    }],
                },
                "uid:1",
                1,
            )
            .unwrap();
        assert_eq!(saved.revision, 1);
        let instruction = service
            .instruction(
                None,
                ClientKind::Codex,
                Some("gpt-6-astra".into()),
                Some("high".into()),
                1,
            )
            .unwrap();
        assert!(instruction.text.contains("Spawn gpt-6.1-sol max agent"));
        assert!(
            instruction
                .assignments
                .iter()
                .any(|assignment| assignment.role_id == "backend_implementation"
                    && assignment.resolved)
        );
    }

    #[test]
    fn stale_capabilities_block_new_spawn_targets() {
        let temp = tempdir().unwrap();
        let database = Database::open(temp.path().join("authority.sqlite3")).unwrap();
        let service = AgentRoutingService::new(database).unwrap();
        let error = service
            .save_settings(
                devcoordinator2_api::params::AgentSettingsSave {
                    scope: Scope::Global,
                    repository_id: None,
                    harness: ClientKind::Codex,
                    expected_revision: 0,
                    rules: vec![RuleInput {
                        role_id: "testing".into(),
                        action: Action::AlwaysSpawn,
                        model: Some("gpt-6.1-sol".into()),
                        effort: Some("max".into()),
                    }],
                },
                "uid:1",
                1,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConfigurationInvalid);
    }

    #[test]
    fn conditional_rules_stay_unresolved_without_current_metadata_and_revisions_conflict() {
        let temp = tempdir().unwrap();
        let database = Database::open(temp.path().join("authority.sqlite3")).unwrap();
        let service = AgentRoutingService::new(database).unwrap();
        service
            .report_capabilities(
                devcoordinator2_api::params::AgentCapabilitiesReport {
                    harness: ClientKind::Codex,
                    models: vec!["gpt-6-astra".into()],
                    efforts: vec!["high".into()],
                    pairs: vec![devcoordinator2_api::agent_routing::Pair {
                        model: "gpt-6-astra".into(),
                        effort: "high".into(),
                    }],
                    expires_at_ms: Some(10_000),
                },
                ClientKind::Codex,
                "uid:1",
                1,
            )
            .unwrap();
        service
            .save_settings(
                devcoordinator2_api::params::AgentSettingsSave {
                    scope: Scope::Global,
                    repository_id: None,
                    harness: ClientKind::Codex,
                    expected_revision: 0,
                    rules: vec![RuleInput {
                        role_id: "testing".into(),
                        action: Action::SkipIfModelMatches,
                        model: Some("gpt-6-astra".into()),
                        effort: Some("high".into()),
                    }],
                },
                "uid:1",
                1,
            )
            .unwrap();
        let instruction = service
            .instruction(None, ClientKind::Codex, None, None, 1)
            .unwrap();
        let assignment = instruction
            .assignments
            .iter()
            .find(|assignment| assignment.role_id == "testing")
            .unwrap();
        assert!(!assignment.resolved);
        assert!(
            assignment
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("unavailable")
        );
        let conflict = service.save_settings(
            devcoordinator2_api::params::AgentSettingsSave {
                scope: Scope::Global,
                repository_id: None,
                harness: ClientKind::Codex,
                expected_revision: 0,
                rules: vec![],
            },
            "uid:1",
            1,
        );
        assert_eq!(conflict.unwrap_err().code, ErrorCode::ConfigurationInvalid);
    }
}
