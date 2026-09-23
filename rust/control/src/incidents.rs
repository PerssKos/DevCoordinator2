//! Durable responses to health observations. Observation is not escalation.
use std::collections::BTreeMap;
use std::sync::Arc;

use devcoordinator2_api::incidents::{
    Incident, IncidentCategory, IncidentList, IncidentListParams, IncidentStatus, IncidentUpdate,
    IncidentView,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::database::{Database, DatabaseError};
use crate::platform::Clock;

#[derive(Clone)]
pub struct IncidentService {
    database: Database,
    clock: Arc<dyn Clock>,
}

impl IncidentService {
    pub fn new(database: Database, clock: Arc<dyn Clock>) -> Self {
        Self { database, clock }
    }

    pub fn list(&self, params: IncidentListParams) -> Result<IncidentList, ProtocolError> {
        if !(1..=50).contains(&params.limit) {
            return Err(invalid("limit must be between 1 and 50"));
        }
        let mut all = self.records()?;
        let attention_count = all
            .iter()
            .filter(|i| i.status == IncidentStatus::Escalated)
            .count();
        let dismissed_count = all
            .iter()
            .filter(|i| i.status == IncidentStatus::Dismissed)
            .count();
        all.retain(|i| match params.view {
            IncidentView::Attention => i.status == IncidentStatus::Escalated,
            IncidentView::Dismissed => i.status == IncidentStatus::Dismissed,
            IncidentView::All => true,
        });
        let total = all.len();
        all.sort_by_key(|i| std::cmp::Reverse(cursor(i)));
        if let Some(before) = params.before {
            all.retain(|i| cursor(i) < before);
        }
        let more = all.len() > usize::from(params.limit);
        all.truncate(usize::from(params.limit));
        let next_before = if more { all.last().map(cursor) } else { None };
        Ok(IncidentList {
            incidents: all,
            attention_count,
            dismissed_count,
            total,
            next_before,
        })
    }

    pub fn update(&self, params: IncidentUpdate, actor: &str) -> Result<Incident, ProtocolError> {
        let mut incident = self
            .records()?
            .into_iter()
            .find(|i| i.incident_id == params.incident_id)
            .ok_or_else(|| {
                invalid("Incident occurrence is no longer available; refresh observations")
            })?;
        if params.expected_revision != incident.revision {
            return Err(conflict());
        }
        if params.status == IncidentStatus::Unreviewed || params.status == IncidentStatus::Resolved
        {
            return Err(invalid(
                "Use handling, suppressed, escalated or dismissed; recovery follows the observed condition",
            ));
        }
        if !incident.condition_active && params.status != IncidentStatus::Dismissed {
            return Err(invalid(
                "This occurrence has recovered; it cannot be escalated or handled again",
            ));
        }
        if params.status == IncidentStatus::Dismissed
            && !matches!(
                incident.status,
                IncidentStatus::Escalated | IncidentStatus::Dismissed | IncidentStatus::Resolved
            )
        {
            return Err(invalid("Only a reported occurrence may be dismissed"));
        }
        for (value, maximum, name) in [
            (&params.summary, 120, "summary"),
            (&params.what_happened, 600, "what_happened"),
            (&params.agent_response, 1200, "agent_response"),
            (&params.escalation_reason, 600, "escalation_reason"),
            (&params.next_step, 600, "next_step"),
        ] {
            if let Some(value) = value
                && (value.trim().is_empty()
                    || value.chars().count() > maximum
                    || value.chars().any(|c| c.is_control() && c != '\n'))
            {
                return Err(invalid(&format!(
                    "{name} must contain 1 to {maximum} readable characters"
                )));
            }
        }
        if let Some(v) = params.summary {
            incident.summary = v.trim().to_owned();
        }
        if let Some(v) = params.what_happened {
            incident.what_happened = v.trim().to_owned();
        }
        if let Some(v) = params.agent_response {
            incident.agent_response = Some(v.trim().to_owned());
        }
        if let Some(v) = params.escalation_reason {
            incident.escalation_reason = Some(v.trim().to_owned());
        }
        if let Some(v) = params.next_step {
            incident.next_step = Some(v.trim().to_owned());
        }
        if matches!(
            params.status,
            IncidentStatus::Handling | IncidentStatus::Suppressed
        ) && incident.agent_response.is_none()
        {
            return Err(invalid(
                "Record what is being handled or why this condition is not being reported",
            ));
        }
        if params.status == IncidentStatus::Escalated
            && (incident.agent_response.is_none()
                || incident.escalation_reason.is_none()
                || incident.next_step.is_none())
        {
            return Err(invalid(
                "Escalation requires agent_response, escalation_reason and next_step",
            ));
        }
        if let Some(category) = params.category {
            incident.category = category;
        }
        if params.status == IncidentStatus::Escalated
            && incident.category == IncidentCategory::Development
        {
            return Err(invalid(
                "Development observations cannot be escalated to the owner queue",
            ));
        }
        incident.status = params.status;
        incident.revision += 1;
        incident.updated_by = Some(actor.to_owned());
        incident.updated_at = Some(
            self.clock
                .now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| invalid("Cannot record response time"))?,
        );
        let saved = incident.clone();
        self.database.transaction(move |tx| {
            let revision = tx.query_row("SELECT revision FROM health_incident_reviews WHERE incident_id=?1", [&saved.incident_id], |r| r.get::<_,u32>(0)).optional()?.unwrap_or(0);
            if revision != params.expected_revision { return Err(DatabaseError::Domain(conflict())); }
            if saved.status != IncidentStatus::Dismissed {
                let still_current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM alerts WHERE alert_key=?1 AND opened_at=?2)", rusqlite::params![saved.alert_key,saved.opened_at], |r| r.get(0))?;
                if !still_current { return Err(DatabaseError::Domain(conflict())); }
            }
            let json = serde_json::to_string(&saved).map_err(|_| DatabaseError::Domain(invalid("Cannot encode incident")))?;
            tx.execute("INSERT INTO health_incident_reviews(incident_id,alert_key,opened_at,revision,record_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(incident_id) DO UPDATE SET revision=excluded.revision,record_json=excluded.record_json", rusqlite::params![saved.incident_id,saved.alert_key,saved.opened_at,saved.revision,json])?;
            tx.execute("INSERT INTO health_incident_history(incident_id,revision,record_json) VALUES(?1,?2,?3)", rusqlite::params![saved.incident_id,saved.revision,json])?;
            Ok(())
        }).map_err(db_error)?;
        Ok(incident)
    }

    fn records(&self) -> Result<Vec<Incident>, ProtocolError> {
        self.database.call(|c| {
            let mut all = BTreeMap::<String,Incident>::new();
            let mut saved = c.prepare("SELECT record_json FROM health_incident_reviews")?;
            for json in saved.query_map([], |r| r.get::<_,String>(0))? {
                let mut incident: Incident = serde_json::from_str(&json?).map_err(|_| DatabaseError::Domain(invalid("Stored incident is invalid")))?;
                incident.condition_active = false;
                all.insert(incident.incident_id.clone(), incident);
            }
            let mut stmt = c.prepare("SELECT alert_key,kind,subject_kind,subject_id,severity,message,opened_at,last_seen_at FROM alerts")?;
            let alerts = stmt.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?)))?.collect::<Result<Vec<_>,_>>()?;
            for (key,kind,subject_kind,subject,severity,message,opened,last_seen) in alerts {
                let id = occurrence_id(&key,&opened);
                if let Some(incident) = all.get_mut(&id) {
                    incident.condition_active = true;
                    incident.last_seen_at = last_seen;
                    incident.severity = severity;
                    continue;
                }
                let (dep,component) = if subject_kind == "component" {
                    subject.split_once('/').map(|(d,n)|(Some(d.to_owned()),Some(n.to_owned()))).unwrap_or_default()
                } else if subject_kind == "deployment" { (Some(subject.clone()),None) } else { (None,None) };
                let context = if let Some(dep) = &dep {
                    c.query_row("SELECT d.repository_id,COALESCE(p.display_name,r.display_name),d.name FROM deployments d JOIN repositories r USING(repository_id) LEFT JOIN repository_presentation p USING(repository_id) WHERE d.deployment_id=?1 UNION ALL SELECT d.repository_id,COALESCE(p.display_name,r.display_name),d.name FROM observed_deployments d JOIN repositories r USING(repository_id) LEFT JOIN repository_presentation p USING(repository_id) WHERE d.observed_deployment_id=?1 LIMIT 1", [dep], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?
                } else { None };
                let state = if let (Some(dep),Some(component)) = (&dep,&component) {
                    c.query_row("SELECT state FROM components WHERE deployment_id=?1 AND name=?2",rusqlite::params![dep,component],|r|r.get::<_,String>(0)).optional()?
                } else { None };
                let summary = match kind.as_str() {
                    "host_disk" => "Host storage is nearly full".to_owned(),
                    "host_cpu" => "Host CPU usage remains high".to_owned(),
                    "host_memory" => "Host memory is running low".to_owned(),
                    "crash_loop" => format!("{} is repeatedly restarting",component.as_deref().unwrap_or("Service")),
                    "test_scratch" => "Test scratch storage needs review".to_owned(),
                    _ => format!("{} is {}",component.as_deref().unwrap_or("Service"),state.as_deref().unwrap_or("unavailable")),
                };
                let what_happened = if subject_kind == "host" || kind == "test_scratch" { message }
                    else if kind == "crash_loop" { "The service restart count exceeded its monitored threshold.".to_owned() }
                    else { format!("The {} component is {} while its recorded desired state is running.", component.as_deref().unwrap_or("monitored"),state.as_deref().unwrap_or("unavailable")) };
                all.insert(id.clone(), Incident {
                    category: if kind == "test_scratch" || matches!(subject_kind.as_str(), "test" | "worktree") { IncidentCategory::Development } else { IncidentCategory::Operational },
                    incident_id:id, alert_key:key,opened_at:opened,last_seen_at:last_seen,
                    repository_id:context.as_ref().map(|v|v.0.clone()),repository_name:context.as_ref().map(|v|v.1.clone()),
                    deployment_id:dep,deployment_name:context.map(|v|v.2),component,severity,
                    status:IncidentStatus::Unreviewed,condition_active:true,summary,what_happened,
                    agent_response:None,escalation_reason:None,next_step:None,revision:0,updated_at:None,updated_by:None,
                });
            }
            for incident in all.values_mut() {
                if !incident.condition_active && matches!(incident.status,IncidentStatus::Handling|IncidentStatus::Escalated) {
                    incident.status=IncidentStatus::Resolved;
                }
            }
            Ok(all.into_values().collect())
        }).map_err(db_error)
    }
}

fn occurrence_id(key: &str, opened: &str) -> String {
    let digest = Sha256::digest(format!("{key}\0{opened}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("i{}", &digest[..24])
}
fn cursor(i: &Incident) -> String {
    format!("{}|{}", i.opened_at, i.incident_id)
}
fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}
fn conflict() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::ConfigurationConflict,
        "Incident changed; refresh before applying the response",
    )
}
fn db_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(e) => e,
        _ => ProtocolError::new(ErrorCode::InternalError, "Incident store unavailable"),
    }
}
