//! Evidence protection is shared with the existing retained-log owner.
use super::{
    db_error,
    model::{Locator, Record},
    parse, unavailable,
};
use crate::database::{Database, DatabaseError, OptionalExtension};
use devcoordinator2_api::ProtocolError;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
pub(crate) type ProtectionSnapshot = BTreeMap<PathBuf, BTreeSet<String>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunRetentionState {
    pub generation: String,
    pub payload_leaves: u64,
    pub active: bool,
    pub eligible: bool,
}

pub(crate) fn published_run(
    database: &Database,
    worktree: &Path,
    run_id: &str,
) -> Result<Option<RunRetentionState>, ProtocolError> {
    retention_state(database, worktree, run_id, None, None)
}
pub(crate) fn run_retention_state(
    database: &Database,
    worktree: &Path,
    run_id: &str,
    age: u64,
    depth: u64,
) -> Result<Option<RunRetentionState>, ProtocolError> {
    retention_state(database, worktree, run_id, Some(age), Some(depth))
}
fn retention_state(
    database: &Database,
    worktree: &Path,
    run_id: &str,
    age: Option<u64>,
    depth: Option<u64>,
) -> Result<Option<RunRetentionState>, ProtocolError> {
    let path = worktree.to_string_lossy().into_owned();
    let run = run_id.to_owned();
    database.read(move|c|{
        Ok(c.query_row("SELECT h.generation,coalesce(r.payload_leaves,0),coalesce(r.active_count>0,false),coalesce(r.payload_leaves>0 AND r.checked_count=r.total_count AND r.ineligible_count=0 AND r.active_count=0,false) FROM worktrees w JOIN dc2_test_log_retention_heads h ON h.worktree_id=w.worktree_id JOIN test_log_retention_state s ON s.singleton=1 AND s.max_age_seconds=h.max_age_seconds AND s.case_depth=h.case_depth LEFT JOIN dc2_test_log_retention_runs r ON r.worktree_id=h.worktree_id AND r.generation=h.generation AND r.run_id=$2 WHERE w.worktree_path=$1 AND h.status='published' AND ($3::bigint IS NULL OR h.max_age_seconds=$3) AND ($4::bigint IS NULL OR h.case_depth=$4)",crate::database::params![path,run,age,depth],|r|Ok(RunRetentionState{generation:r.get(0)?,payload_leaves:r.get(1)?,active:r.get(2)?,eligible:r.get(3)?})).optional()?)
    }).map_err(db_error)
}

pub(crate) fn published_run_observation(
    database: &Database,
    worktree: &Path,
    run_id: &str,
) -> Result<Option<devcoordinator2_executor_core::log_query::RetentionObservation>, ProtocolError> {
    let path = worktree.to_string_lossy().into_owned();
    let run = run_id.to_owned();
    let observation=database.read(|c|{
        Ok(c.query_row("SELECT CASE WHEN octet_length(p.location_json::text)<=16384 THEN p.location_json ELSE NULL END FROM worktrees w JOIN dc2_test_log_retention_heads h ON h.worktree_id=w.worktree_id JOIN test_log_retention_state s ON s.singleton=1 AND s.max_age_seconds=h.max_age_seconds AND s.case_depth=h.case_depth JOIN test_log_retention_projection p ON p.worktree_id=h.worktree_id AND p.generation=h.generation AND p.run_id=$2 AND p.removed=0 WHERE w.worktree_path=$1 AND h.status='published' ORDER BY p.relative_path LIMIT 1",crate::database::params![path,run],|r|r.get::<_,Option<String>>(0)).optional()?)
    }).map_err(db_error)?;
    let Some(value) = observation else {
        return Ok(None);
    };
    let value = value.ok_or_else(|| unavailable("retention_observation_too_large"))?;
    let observation: devcoordinator2_executor_core::log_query::RetentionObservation =
        parse(&value)?;
    if observation.run_id != run_id
        || observation
            .components
            .first()
            .is_none_or(|first| first != run_id)
    {
        return Err(unavailable("retention_observation_invalid"));
    }
    Ok(Some(observation))
}

/// Historical full-catalog oracle for parity tests of indexed protection.
#[cfg(test)]
pub(crate) fn protection_snapshot(
    database: &Database,
) -> Result<ProtectionSnapshot, ProtocolError> {
    let (worktrees, records, leases, deliveries, reviews, feedback) = database
        .read(|c| {
            let worktrees = {
                let mut query = c.prepare(
                    "SELECT worktree_id,worktree_path FROM worktrees",
                )?;
                query
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, PathBuf::from(row.get::<_, String>(1)?)))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let records = {
                let mut query = c.prepare(
                    "SELECT record_json FROM storage_artifacts WHERE kind='evidence' AND removed_at_ms IS NULL",
                )?;
                query
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
            let leases = {
                let mut query = c.prepare(
                    "SELECT lease_json FROM storage_leases WHERE expires_at_ms>$1",
                )?;
                query
                    .query_map([now], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let deliveries = {
                let mut query = c.prepare(
                    "SELECT receipt_json FROM release_evidence ORDER BY (receipt_json::jsonb->>'checked_at_ms')::bigint DESC",
                )?;
                query
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let reviews = {
                let mut query = c.prepare(
                    "SELECT r.record_json FROM review_records r JOIN review_policies p ON p.last_receipt=r.record_id||'@'||r.revision WHERE p.active=1",
                )?;
                query
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let feedback = {
                let mut query = c.prepare(
                    "SELECT DISTINCT f.worktree_id,f.run_id FROM visual_feedback f JOIN tasks t ON t.task_id=f.task_id WHERE f.deleted_at IS NULL AND t.status NOT IN ('done','dropped')",
                )?;
                query
                    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            Ok((worktrees, records, leases, deliveries, reviews, feedback))
        })
        .map_err(db_error)?;

    let worktree_paths = worktrees.into_iter().collect::<BTreeMap<_, _>>();
    let mut protected = ProtectionSnapshot::new();
    let mut active_artifacts = BTreeSet::new();
    for value in leases {
        let lease: devcoordinator2_api::storage::Lease = parse(&value)?;
        active_artifacts.extend(lease.artifact_ids);
    }
    for value in records {
        let record: Record = parse(&value)?;
        if (record.artifact.protected || active_artifacts.contains(&record.artifact.artifact_id))
            && let Locator::Evidence {
                worktree, run_id, ..
            } = record.locator
        {
            protected.entry(worktree).or_default().insert(run_id);
        }
    }

    let mut latest_targets = BTreeSet::new();
    let mut latest_releases = BTreeSet::new();
    for value in deliveries {
        let receipt: devcoordinator2_api::delivery::Receipt = parse(&value)?;
        let newest = latest_releases.insert(receipt.release_id.clone());
        let current = receipt.qualified
            && latest_targets.insert((receipt.repository_id.clone(), receipt.target.clone()));
        if (current || newest && !receipt.qualified)
            && let Some(worktree) = worktree_paths.get(&receipt.worktree_id)
        {
            protected
                .entry(worktree.clone())
                .or_default()
                .insert(receipt.run_id);
        }
    }

    for (worktree_id, run_id) in feedback {
        if let Some(worktree) = worktree_paths.get(&worktree_id) {
            protected
                .entry(worktree.clone())
                .or_default()
                .insert(run_id);
        }
    }

    // Preserve the existing conservative behavior: malformed active review
    // references remain unavailable whenever at least one registered worktree
    // exists. Unknown owners are ignored just as protected_runs ignores a
    // receipt whose worktree_id does not match the requested worktree.
    if !worktree_paths.is_empty() {
        for value in reviews {
            let record: devcoordinator2_api::review::ReviewRecord = parse(&value)?;
            let experiment = record.experiment;
            let references = experiment
                .evidence_refs
                .iter()
                .chain(&experiment.result_evidence_refs)
                .chain(&experiment.baseline.evidence_refs)
                .chain(
                    experiment
                        .observations
                        .iter()
                        .flat_map(|row| row.evidence_refs.iter()),
                );
            for reference in references {
                if !matches!(
                    reference.kind,
                    devcoordinator2_api::review::EvidenceKind::Run
                ) {
                    continue;
                }
                let Some((owner, run)) = reference.reference.split_once('/') else {
                    return Err(unavailable("review_evidence_reference_unverified"));
                };
                if let Some(worktree) = worktree_paths.get(owner) {
                    protected
                        .entry(worktree.clone())
                        .or_default()
                        .insert(run.into());
                }
            }
        }
    }
    Ok(protected)
}

pub(crate) fn protected_runs(
    database: &Database,
    worktree: &Path,
) -> Result<BTreeSet<String>, ProtocolError> {
    let path = worktree.to_string_lossy().into_owned();
    let (worktree_id,records,leases,deliveries,reviews)=database.read(move|c|{
        use crate::database::OptionalExtension;
        let worktree_id=c.query_row("SELECT worktree_id FROM worktrees WHERE worktree_path=$1",[&path],|r|r.get::<_,String>(0)).optional()?;
        let records={let mut q=c.prepare("SELECT record_json FROM storage_artifacts WHERE kind='evidence' AND removed_at_ms IS NULL")?;q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        let now=time::OffsetDateTime::now_utc().unix_timestamp()*1000;
        let leases={let mut q=c.prepare("SELECT lease_json FROM storage_leases WHERE expires_at_ms>$1")?;q.query_map([now],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        let deliveries={let mut q=c.prepare("SELECT receipt_json FROM release_evidence ORDER BY (receipt_json::jsonb->>'checked_at_ms')::bigint DESC")?;q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        let reviews={let mut q=c.prepare("SELECT r.record_json FROM review_records r JOIN review_policies p ON p.last_receipt=r.record_id||'@'||r.revision WHERE p.active=1")?;q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        Ok((worktree_id,records,leases,deliveries,reviews))
    }).map_err(db_error)?;
    let mut protected = BTreeSet::new();
    let mut active = BTreeSet::new();
    for value in leases {
        let lease: devcoordinator2_api::storage::Lease = parse(&value)?;
        active.extend(lease.artifact_ids);
    }
    for value in records {
        let record: Record = parse(&value)?;
        if (record.artifact.protected || active.contains(&record.artifact.artifact_id))
            && let Locator::Evidence {
                worktree: root,
                run_id,
                ..
            } = record.locator
            && root == worktree
        {
            protected.insert(run_id);
        }
    }
    let mut latest_targets = BTreeSet::new();
    let mut latest_releases = BTreeSet::new();
    for value in deliveries {
        let receipt: devcoordinator2_api::delivery::Receipt = parse(&value)?;
        let newest = latest_releases.insert(receipt.release_id.clone());
        let current = receipt.qualified
            && latest_targets.insert((receipt.repository_id.clone(), receipt.target.clone()));
        if worktree_id.as_ref() == Some(&receipt.worktree_id)
            && (current || newest && !receipt.qualified)
        {
            protected.insert(receipt.run_id);
        }
    }
    if let Some(worktree_id) = worktree_id {
        let id = worktree_id.clone();
        let feedback=database.read(move|c|{let mut q=c.prepare("SELECT DISTINCT f.run_id FROM visual_feedback f JOIN tasks t ON t.task_id=f.task_id WHERE f.worktree_id=$1 AND f.deleted_at IS NULL AND t.status NOT IN ('done','dropped')")?;Ok(q.query_map([id],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)}).map_err(db_error)?;
        protected.extend(feedback);
        for value in reviews {
            let record: devcoordinator2_api::review::ReviewRecord = parse(&value)?;
            let experiment = record.experiment;
            let refs = experiment
                .evidence_refs
                .iter()
                .chain(&experiment.result_evidence_refs)
                .chain(&experiment.baseline.evidence_refs)
                .chain(
                    experiment
                        .observations
                        .iter()
                        .flat_map(|r| r.evidence_refs.iter()),
                );
            for reference in refs {
                if !matches!(
                    reference.kind,
                    devcoordinator2_api::review::EvidenceKind::Run
                ) {
                    continue;
                }
                let Some((owner, run)) = reference.reference.split_once('/') else {
                    return Err(unavailable("review_evidence_reference_unverified"));
                };
                if owner == worktree_id {
                    protected.insert(run.into());
                }
            }
        }
    }
    Ok(protected)
}

/// Indexed protection for one run. Historical receipts and inactive reviews
/// are reduced transactionally by native database projections.
pub(crate) fn run_is_protected(
    database: &Database,
    worktree: &Path,
    run_id: &str,
) -> Result<bool, ProtocolError> {
    let path = worktree.to_string_lossy().into_owned();
    let run = run_id.to_owned();
    let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
    let (invalid_record,artifact,lease,delivery,invalid_review,review,feedback,invalid_delivery)=database.read(move|c|{
        use crate::database::OptionalExtension;
        let owner=c.query_row("SELECT worktree_id FROM worktrees WHERE worktree_path=$1",[&path],|r|r.get::<_,String>(0)).optional()?;
        c.query_row("WITH invalid_delivery AS MATERIALIZED(SELECT source_rowid FROM dc2_delivery_protection_heads WHERE worktree_id IS NULL OR run_id IS NULL OR checked_at_ms IS NULL ORDER BY source_rowid,scope_kind,owner_id,target LIMIT 1),invalid_review AS MATERIALIZED(SELECT repository_id,workstream_key,ordinal FROM dc2_active_review_runs WHERE valid=0 ORDER BY repository_id,workstream_key,ordinal LIMIT 1) SELECT
          EXISTS(SELECT 1 FROM storage_artifacts a WHERE a.kind='evidence' AND a.removed_at_ms IS NULL AND a.record_json#>>'{locator,worktree}'=$1 AND a.record_json#>>'{locator,run_id}'=$2 AND (jsonb_typeof(a.record_json#>'{artifact,protected}') IS DISTINCT FROM 'boolean' OR a.record_json#>>'{locator,type}' IS DISTINCT FROM 'evidence' OR octet_length(a.record_json::text)>262144)),
          EXISTS(SELECT 1 FROM storage_artifacts a WHERE a.kind='evidence' AND a.removed_at_ms IS NULL AND a.record_json#>>'{locator,worktree}'=$1 AND a.record_json#>>'{locator,run_id}'=$2 AND a.record_json#>'{artifact,protected}'='true'::jsonb),
          EXISTS(SELECT 1 FROM storage_artifacts a JOIN dc2_storage_lease_artifacts l ON l.artifact_id=a.artifact_id AND l.expires_at_ms>$4 WHERE a.kind='evidence' AND a.removed_at_ms IS NULL AND a.record_json#>>'{locator,worktree}'=$1 AND a.record_json#>>'{locator,run_id}'=$2),
          EXISTS(SELECT 1 FROM dc2_delivery_protection_heads WHERE worktree_id=$3 AND run_id=$2 AND (scope_kind='target' OR qualified=0)),
          $3::text IS NOT NULL AND EXISTS(SELECT 1 FROM invalid_review),
          EXISTS(SELECT 1 FROM dc2_active_review_runs WHERE valid=1 AND worktree_id=$3 AND run_id=$2),
          EXISTS(SELECT 1 FROM visual_feedback f JOIN tasks t ON t.task_id=f.task_id WHERE f.worktree_id=$3 AND f.run_id=$2 AND f.deleted_at IS NULL AND t.status NOT IN ('done','dropped')),
          EXISTS(SELECT 1 FROM invalid_delivery)",
          crate::database::params![path,run,owner,now],|r|Ok((r.get::<_,bool>(0)?,r.get::<_,bool>(1)?,r.get::<_,bool>(2)?,r.get::<_,bool>(3)?,r.get::<_,bool>(4)?,r.get::<_,bool>(5)?,r.get::<_,bool>(6)?,r.get::<_,bool>(7)?))).map_err(DatabaseError::from)
    }).map_err(db_error)?;
    if invalid_record {
        return Err(unavailable("stored_evidence_record_invalid"));
    }
    if invalid_review {
        return Err(unavailable("review_evidence_reference_unverified"));
    }
    if invalid_delivery {
        return Err(unavailable("delivery_evidence_reference_unverified"));
    }
    Ok(artifact || lease || delivery || review || feedback)
}

/// Record one fully expired run with a stable idempotency key. The indexed
/// candidate set and serialized bytes are bounded before rows are retrieved.
pub(crate) fn record_run_expired(
    database: &Database,
    worktree: &Path,
    run_id: &str,
) -> Result<(), ProtocolError> {
    let root = worktree.to_owned();
    let run = run_id.to_owned();
    let path = root.to_string_lossy().into_owned();
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
    let job_id = super::stable_id("sj", format!("log-retention\0{path}\0{run}").as_bytes());
    database.maintenance_transaction(move|c|{
        let bounded=c.prepare("WITH candidates AS MATERIALIZED(SELECT artifact_id,record_json FROM storage_artifacts WHERE kind='evidence' AND removed_at_ms IS NULL AND record_json#>>'{locator,worktree}'=$1 AND record_json#>>'{locator,run_id}'=$2 ORDER BY artifact_id LIMIT 65 FOR UPDATE), metadata AS MATERIALIZED(SELECT count(*) count,coalesce(sum(octet_length(record_json::text)),0)::bigint bytes FROM candidates) SELECT c.artifact_id,CASE WHEN m.count<=64 AND m.bytes<=262144 THEN c.record_json ELSE NULL END,m.count,m.bytes FROM metadata m LEFT JOIN candidates c ON true ORDER BY c.artifact_id")?.query_map(crate::database::params![path,run],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?)))?.collect::<crate::database::Result<Vec<_>>>()?;
        let mut rows=Vec::new();
        for (id,value,count,bytes) in bounded{
            if count>64||bytes>262144{return Err(DatabaseError::Domain(unavailable("retention_receipt_metadata_too_large")));}
            if let Some(id)=id{rows.push((id,value.ok_or_else(||DatabaseError::Domain(unavailable("retention_receipt_metadata_invalid")))?));}
        }
        let mut identities=Vec::new();
        for (id,value) in rows{
            let mut record:Record=parse(&value).map_err(DatabaseError::Domain)?;
            if !matches!(&record.locator,Locator::Evidence{worktree,run_id,..} if worktree==&root&&run_id==&run){return Err(DatabaseError::Domain(unavailable("evidence_identity_changed")));}
            record.artifact.removed_at_ms=Some(now);record.artifact.deletable=false;record.artifact.automatic_eligible=false;record.artifact.revision+=1;record.update_sequence=super::next_sequence(c)?;
            c.execute("UPDATE storage_artifacts SET removed_at_ms=$1,revision=$2,record_json=$3,updated_at_ms=$1 WHERE artifact_id=$4",crate::database::params![now,record.artifact.revision,super::json(&record).map_err(DatabaseError::Domain)?,id])?;identities.push(id);
        }
        if c.query_row("SELECT 1 FROM storage_jobs WHERE job_id=$1",[&job_id],|_|Ok(())).optional()?.is_some(){return Ok(());}
        if identities.is_empty(){let locator=Locator::Evidence{worktree:root,run_id:run,leaf:"run".into()};identities.push(super::stable_id("sa",super::json(&locator).map_err(DatabaseError::Domain)?.as_bytes()));}
        let receipts=identities.into_iter().map(|artifact_id|devcoordinator2_api::storage::ItemReceipt{artifact_id,status:"removed".into(),code:None,measured_bytes_before:None,removed_at_ms:Some(now),space_change:None}).collect::<Vec<_>>();
        let job=devcoordinator2_api::storage::Job{job_id:job_id.clone(),kind:"retention".into(),state:devcoordinator2_api::storage::JobState::Completed,created_at_ms:now,started_at_ms:None,completed_at_ms:Some(now),actor:"retention".into(),plan_id:None,unmeasured_items:receipts.len() as u32,receipts,reclaimed_bytes:0,error_code:None};
        c.execute("INSERT INTO storage_jobs VALUES($1,'retention','completed',$1,$2,'retention',0,$3,'{}',$4)",crate::database::params![job_id,super::hash(job_id.as_bytes()),super::json(&job).map_err(DatabaseError::Domain)?,now])?;
        for (step,receipt) in job.receipts.iter().enumerate(){c.execute("INSERT INTO storage_item_receipts VALUES($1,$2,$3,$4)",crate::database::params![job_id,receipt.artifact_id,step,super::json(receipt).map_err(DatabaseError::Domain)?])?;}
        Ok(())
    }).map_err(db_error)?;
    crate::test_state::TestRunStore
        .retire_container_run(worktree, run_id)
        .map_err(|_| unavailable("container_ownership_retention_failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use devcoordinator2_api::delivery::{Kind as DeliveryKind, Qualification, Receipt};
    use devcoordinator2_api::review::{
        Baseline, Disposition, EvidenceKind, EvidenceRef, Observation, ObservationKind,
        OptimizationExperiment, ReviewRecord,
    };
    use devcoordinator2_api::storage::{Artifact, Effect, Kind, Lease, Safety};
    use tempfile::TempDir;

    fn artifact_record(path: &Path, run_id: &str, protected: bool) -> String {
        let artifact = Artifact {
            artifact_id: format!("sa-{run_id}"),
            revision: 1,
            repository_id: Some("r1".into()),
            repository_name: Some("fixture".into()),
            group_id: None,
            group_name: None,
            accounting_id: format!("sa-{run_id}"),
            name: run_id.into(),
            kind: Kind::Evidence,
            effect: Effect::RetainedEvidence,
            ownership: "retention_owned".into(),
            filesystem_id: None,
            allocated_bytes: Some(1),
            last_used_at_ms: None,
            observed_since_ms: 1,
            verified_at_ms: Some(1),
            eligible_at_ms: None,
            safety: if protected {
                Safety::Protected
            } else {
                Safety::NeedsReview
            },
            reasons: Vec::new(),
            protected,
            deletable: !protected,
            automatic_eligible: !protected,
            dependencies: Vec::new(),
            aliases: Vec::new(),
            removed_at_ms: None,
        };
        serde_json::to_string(&Record {
            update_sequence: 1,
            artifact,
            locator: Locator::Evidence {
                worktree: path.to_owned(),
                run_id: run_id.into(),
                leaf: "run".into(),
            },
            resource_key: format!("evidence:{run_id}"),
            ancestor_keys: Vec::new(),
            fingerprint: format!("fingerprint:{run_id}"),
            owner_deployment: None,
            source_owner_observation: None,
            last_activity_signature: String::new(),
            discovered_effect: Effect::RetainedEvidence,
            disposal_approved: false,
            disposal_reason: None,
            blockers: Vec::new(),
            recovery_lineage: None,
            recovery_verified: false,
            recovery_created_at_ms: None,
            private_aliases: Vec::new(),
            retention_eligible: true,
            ignored_review: None,
        })
        .unwrap()
    }

    fn review_record(reference: &str) -> String {
        serde_json::to_string(&ReviewRecord {
            version: 1,
            repository_id: "r1".into(),
            project_id: "p1".into(),
            workstream_id: Some("w1".into()),
            window_start_ms: 1,
            window_end_ms: 2,
            outcome_id: None,
            experiment: OptimizationExperiment {
                hypothesis: "fixture".into(),
                evidence_refs: vec![EvidenceRef {
                    kind: EvidenceKind::Run,
                    reference: reference.into(),
                }],
                alternatives: vec!["keep".into()],
                chosen_action: "keep".into(),
                baseline: Baseline {
                    evidence_refs: Vec::new(),
                    interpretation: "fixture".into(),
                    missing_measurements: Vec::new(),
                },
                success_criteria: "fixture".into(),
                rollback_condition: "fixture".into(),
                disposition: Disposition::Unchanged,
                result_evidence_refs: Vec::new(),
                scope_repo_id: "r1".into(),
                preserves_quality: true,
                reason: "fixture".into(),
                observations: vec![Observation {
                    kind: ObservationKind::Other,
                    interpretation: "fixture".into(),
                    evidence_refs: Vec::new(),
                }],
                comparison: None,
            },
        })
        .unwrap()
    }

    fn delivery_receipt(
        receipt_id: &str,
        release_id: &str,
        run_id: &str,
        target: &str,
        qualified: bool,
        checked_at_ms: u64,
    ) -> String {
        serde_json::to_string(&Receipt {
            receipt_id: receipt_id.into(),
            release_id: release_id.into(),
            repository_id: "r1".into(),
            worktree_id: "w1".into(),
            kind: DeliveryKind::NativeConsole,
            target: target.into(),
            source_sha256: "source".into(),
            config_sha256: "config".into(),
            run_id: run_id.into(),
            check: "check".into(),
            artifact: "artifact".into(),
            artifact_sha256: "artifact-sha".into(),
            manifest_sha256: "manifest".into(),
            run_metadata_sha256: "metadata".into(),
            verification_sha256: None,
            qualification: if qualified {
                Qualification::Qualified
            } else {
                Qualification::PendingExternalEvidence
            },
            qualified,
            verified_at_ms: None,
            checked_at_ms,
            delivered_at_ms: None,
            access: None,
            reason: None,
            completion_sha256: None,
            completion_claim: None,
            completion_capabilities: None,
            completion_incomplete: None,
        })
        .unwrap()
    }

    fn fixture() -> (TempDir, Database, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("repo-one");
        let second = temporary.path().join("repo-two");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let first_text = first.to_string_lossy().into_owned();
        let second_text = second.to_string_lossy().into_owned();
        let first_worktree = first.clone();
        let second_worktree = second.clone();
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1',$1,'one','t',1,'t'),('r2',$2,'two','t',1,'t')",
                    crate::database::params![first_text, second_text],
                )?;
                c.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1','r1',$1,'t','t'),('w2','r2',$2,'t','t')",
                    crate::database::params![
                        first_worktree.to_string_lossy(),
                        second_worktree.to_string_lossy()
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        (temporary, database, first, second)
    }

    #[test]
    fn scan_snapshot_maps_multiple_worktrees_and_fresh_revalidation_sees_new_protection() {
        let (_temporary, database, first, second) = fixture();
        let first_record = artifact_record(&first, "run-one", true);
        let second_record = artifact_record(&second, "run-two", true);
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-one','r1','evidence',1,NULL,$1,1),('sa-run-two','r2','evidence',1,NULL,$2,1)",
                    crate::database::params![first_record, second_record],
                )?;
                Ok(())
            })
            .unwrap();

        let snapshot = protection_snapshot(&database).unwrap();
        assert_eq!(snapshot.get(&first).unwrap().len(), 1);
        assert!(snapshot.get(&first).unwrap().contains("run-one"));
        assert_eq!(snapshot.get(&second).unwrap().len(), 1);
        assert!(snapshot.get(&second).unwrap().contains("run-two"));

        // A scan snapshot is intentionally stale. The destructive path still
        // reads current protection metadata before it can remove anything.
        let late_record = artifact_record(&first, "run-late", true);
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-late','r1','evidence',1,NULL,$1,1)",
                    [&late_record],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            protected_runs(&database, &first)
                .unwrap()
                .contains("run-late")
        );
        assert!(run_is_protected(&database, &first, "run-one").unwrap());
        assert!(run_is_protected(&database, &first, "run-late").unwrap());
    }

    #[test]
    fn scan_snapshot_preserves_lease_delivery_and_visual_feedback_protection() {
        let (_temporary, database, first, _second) = fixture();
        let leased_record = artifact_record(&first, "run-lease", false);
        let qualified = delivery_receipt(
            "receipt-qualified",
            "release-qualified",
            "run-qualified",
            "target-a",
            true,
            20,
        );
        let qualified_old = delivery_receipt(
            "receipt-qualified-old",
            "release-qualified-old",
            "run-qualified-old",
            "target-a",
            true,
            10,
        );
        let failed = delivery_receipt(
            "receipt-failed",
            "release-failed",
            "run-failed",
            "target-b",
            false,
            30,
        );
        let failed_old = delivery_receipt(
            "receipt-failed-old",
            "release-failed",
            "run-failed-old",
            "target-b",
            false,
            20,
        );
        let lease = serde_json::to_string(&Lease {
            lease_id: "lease-one".into(),
            artifact_ids: vec!["sa-run-lease".into()],
            expires_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 3_600)
                * 1_000,
        })
        .unwrap();
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-lease','r1','evidence',1,NULL,$1,1)",
                    [&leased_record],
                )?;
                c.execute(
                    "INSERT INTO storage_leases(lease_id,actor,lease_json,expires_at_ms) VALUES('lease-one','fixture',$1,$2)",
                    crate::database::params![lease, (time::OffsetDateTime::now_utc().unix_timestamp() + 3_600) * 1_000],
                )?;
                for (release_id, seq) in [
                    ("release-qualified", 10),
                    ("release-qualified-old", 11),
                    ("release-failed", 12),
                ] {
                    c.execute(
                        "INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES($1,'r1',$2,$1,'native_console','delivered','t','fixture','t')",
                        crate::database::params![release_id, seq],
                    )?;
                }
                for (receipt_id, receipt, release_id) in [
                    ("receipt-qualified", qualified, "release-qualified"),
                    ("receipt-qualified-old", qualified_old, "release-qualified-old"),
                    ("receipt-failed", failed, "release-failed"),
                    ("receipt-failed-old", failed_old, "release-failed"),
                ] {
                    c.execute(
                        "INSERT INTO release_evidence(receipt_id,release_id,repository_id,receipt_json) VALUES($1,$2,'r1',$3)",
                        crate::database::params![receipt_id, release_id, receipt],
                    )?;
                }
                c.execute(
                    "INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES('task-feedback','r1',100,1,'feedback','open','review','open','t','fixture','t')",
                    [],
                )?;
                c.execute(
                    "INSERT INTO visual_feedback(feedback_id,task_id,repository_id,worktree_id,run_id,check_name,phase,formal_run_id,cell_id,screenshot_kind,screenshot_sha256,image_id,geometry_json,root_comment_id,created_at,created_by,updated_at) VALUES('feedback-one','task-feedback','r1','w1','run-feedback','check','formal','formal-run','cell','initial','sha','image','{}','comment','t','fixture','t')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let snapshot = protection_snapshot(&database).unwrap();
        let first_runs = snapshot.get(&first).unwrap();
        for run_id in ["run-lease", "run-qualified", "run-failed", "run-feedback"] {
            assert!(first_runs.contains(run_id), "missing {run_id}");
        }
        for run_id in ["run-qualified-old", "run-failed-old"] {
            assert!(!first_runs.contains(run_id), "stale {run_id} protected");
        }
        // The old per-worktree path must continue to agree with the bulk
        // snapshot for all protection sources, including late destructive
        // revalidation callers.
        assert_eq!(first_runs, &protected_runs(&database, &first).unwrap());
        for run in [
            "run-lease",
            "run-qualified",
            "run-failed",
            "run-feedback",
            "run-qualified-old",
            "run-failed-old",
            "absent",
        ] {
            assert_eq!(
                run_is_protected(&database, &first, run).unwrap(),
                first_runs.contains(run),
                "indexed protection disagrees for {run}"
            );
        }
    }

    #[test]
    fn malformed_active_review_reference_fails_closed_and_unknown_owner_is_ignored() {
        let (_temporary, database, first, _second) = fixture();
        let record = review_record("malformed");
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES('review',1,'r1',1,2,$1,'fixture',2)",
                    [&record],
                )?;
                c.execute(
                    "INSERT INTO review_policies(repository_id,workstream_key,interval_ms,escalation_ms,active,window_start_ms,window_end_ms,last_receipt) VALUES('r1','w1',1,1,1,1,2,'review@1')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            protection_snapshot(&database).unwrap_err().code,
            devcoordinator2_api::ErrorCode::StorageUnavailable
        );
        assert!(run_is_protected(&database, &first, "some-run").is_err());

        let (_temporary, unknown_database, unknown_first, _unknown_second) = fixture();
        unknown_database
            .transaction(|c| {
                c.execute(
                    "INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES('review',1,'r1',1,2,$1,'fixture',2)",
                    [&review_record("unknown-owner/run")],
                )?;
                c.execute(
                    "INSERT INTO review_policies(repository_id,workstream_key,interval_ms,escalation_ms,active,window_start_ms,window_end_ms,last_receipt) VALUES('r1','w1',1,1,1,1,2,'review@1')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            !protection_snapshot(&unknown_database)
                .unwrap()
                .contains_key(&unknown_first)
        );
        assert!(!run_is_protected(&unknown_database, &unknown_first, "run").unwrap());
    }
}
