//! Evidence protection is shared with the existing retained-log owner.
use super::{
    db_error,
    model::{Locator, Record},
    parse, unavailable,
};
use crate::database::{Database, DatabaseError};
use devcoordinator2_api::ProtocolError;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) type ProtectionSnapshot = BTreeMap<PathBuf, BTreeSet<String>>;

/// Read shared protection metadata once for a complete storage scan.
///
/// `discover_evidence` visits every registered worktree. The metadata used to
/// decide whether a retained run is protected is global, so repeating these
/// queries and JSON parses once per worktree needlessly monopolizes the
/// Coordinator database actor and the storage worker. Destructive validation
/// and removal intentionally continue to call `protected_runs` below so a
/// stale scan snapshot can never authorize a mutation.
pub(crate) fn protection_snapshot(
    database: &Database,
) -> Result<ProtectionSnapshot, ProtocolError> {
    let (worktrees, records, leases, deliveries, reviews, feedback) = database
        .call(|c| {
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
                    "SELECT lease_json FROM storage_leases WHERE expires_at_ms>?1",
                )?;
                query
                    .query_map([now], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let deliveries = {
                let mut query = c.prepare(
                    "SELECT receipt_json FROM release_evidence ORDER BY CAST(json_extract(receipt_json,'$.checked_at_ms') AS INTEGER) DESC",
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
    let (worktree_id,records,leases,deliveries,reviews)=database.call(move|c|{
        use rusqlite::OptionalExtension;
        let worktree_id=c.query_row("SELECT worktree_id FROM worktrees WHERE worktree_path=?1",[&path],|r|r.get::<_,String>(0)).optional()?;
        let records={let mut q=c.prepare("SELECT record_json FROM storage_artifacts WHERE kind='evidence' AND removed_at_ms IS NULL")?;q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        let now=time::OffsetDateTime::now_utc().unix_timestamp()*1000;
        let leases={let mut q=c.prepare("SELECT lease_json FROM storage_leases WHERE expires_at_ms>?1")?;q.query_map([now],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
        let deliveries={let mut q=c.prepare("SELECT receipt_json FROM release_evidence ORDER BY CAST(json_extract(receipt_json,'$.checked_at_ms') AS INTEGER) DESC")?;q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
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
        let feedback=database.call(move|c|{let mut q=c.prepare("SELECT DISTINCT f.run_id FROM visual_feedback f JOIN tasks t ON t.task_id=f.task_id WHERE f.worktree_id=?1 AND f.deleted_at IS NULL AND t.status NOT IN ('done','dropped')")?;Ok(q.query_map([id],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)}).map_err(db_error)?;
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

/// Retention removes payload leaves; the compact run and cleanup receipts remain.
pub(crate) fn record_expired(
    database: &Database,
    worktree: &Path,
    removed_runs: &[String],
) -> Result<(), ProtocolError> {
    if removed_runs.is_empty() {
        return Ok(());
    }
    let root = worktree.to_owned();
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
    for batch in removed_runs.chunks(100) {
        let root = root.clone();
        let removed = batch.iter().cloned().collect::<BTreeSet<_>>();
        let job_id = super::new_id("sj")?;
        database.transaction(move|c|{
            let mut statement=c.prepare("SELECT artifact_id,record_json FROM storage_artifacts WHERE kind='evidence' AND removed_at_ms IS NULL")?;
            let rows=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;drop(statement);
            let mut known=std::collections::BTreeMap::new();
            for (id,value) in rows {
                let mut record:Record=parse(&value).map_err(DatabaseError::Domain)?;
                let Locator::Evidence{worktree,run_id,..}=&record.locator else{continue;};
                if worktree!=&root||!removed.contains(run_id){continue;}
                known.insert(run_id.clone(),id.clone());
                record.artifact.removed_at_ms=Some(now);record.artifact.deletable=false;record.artifact.automatic_eligible=false;record.artifact.revision+=1;
                record.update_sequence=super::next_sequence(c)?;
                c.execute("UPDATE storage_artifacts SET removed_at_ms=?1,revision=?2,record_json=?3,updated_at_ms=?1 WHERE artifact_id=?4",rusqlite::params![now as i64,record.artifact.revision as i64,super::json(&record).map_err(DatabaseError::Domain)?,id])?;
            }
            let mut receipts=Vec::new();
            for run_id in removed {
                let locator=Locator::Evidence{worktree:root.clone(),run_id:run_id.clone(),leaf:"run".into()};
                let id=known.remove(&run_id).unwrap_or(super::stable_id("sa",super::json(&locator).map_err(DatabaseError::Domain)?.as_bytes()));
                receipts.push(devcoordinator2_api::storage::ItemReceipt{artifact_id:id,status:"removed".into(),code:None,measured_bytes_before:None,removed_at_ms:Some(now),space_change:None});
            }
            let job=devcoordinator2_api::storage::Job{job_id:job_id.clone(),kind:"retention".into(),state:devcoordinator2_api::storage::JobState::Completed,created_at_ms:now,started_at_ms:None,completed_at_ms:Some(now),actor:"retention".into(),plan_id:None,unmeasured_items:receipts.len() as u32,receipts,reclaimed_bytes:0,error_code:None};
            c.execute("INSERT INTO storage_jobs VALUES(?1,'retention','completed',?1,?2,'retention',0,?3,'{}',?4)",rusqlite::params![job_id,super::hash(job_id.as_bytes()),super::json(&job).map_err(DatabaseError::Domain)?,now as i64])?;
            for (step,receipt) in job.receipts.iter().enumerate(){c.execute("INSERT INTO storage_item_receipts VALUES(?1,?2,?3,?4)",rusqlite::params![job_id,receipt.artifact_id,step as i64,super::json(receipt).map_err(DatabaseError::Domain)?])?;}
            Ok(())
        }).map_err(db_error)?;
    }
    Ok(())
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
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1',?1,'one','t',1,'t'),('r2',?2,'two','t',1,'t')",
                    rusqlite::params![first_text, second_text],
                )?;
                c.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1','r1',?1,'t','t'),('w2','r2',?2,'t','t')",
                    rusqlite::params![
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
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-one','r1','evidence',1,NULL,?1,1),('sa-run-two','r2','evidence',1,NULL,?2,1)",
                    rusqlite::params![first_record, second_record],
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
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-late','r1','evidence',1,NULL,?1,1)",
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
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-lease','r1','evidence',1,NULL,?1,1)",
                    [&leased_record],
                )?;
                c.execute(
                    "INSERT INTO storage_leases(lease_id,actor,lease_json,expires_at_ms) VALUES('lease-one','fixture',?1,?2)",
                    rusqlite::params![lease, (time::OffsetDateTime::now_utc().unix_timestamp() + 3_600) * 1_000],
                )?;
                for (release_id, seq) in [
                    ("release-qualified", 10),
                    ("release-qualified-old", 11),
                    ("release-failed", 12),
                ] {
                    c.execute(
                        "INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES(?1,'r1',?2,?1,'native_console','delivered','t','fixture','t')",
                        rusqlite::params![release_id, seq],
                    )?;
                }
                for (receipt_id, receipt, release_id) in [
                    ("receipt-qualified", qualified, "release-qualified"),
                    ("receipt-qualified-old", qualified_old, "release-qualified-old"),
                    ("receipt-failed", failed, "release-failed"),
                    ("receipt-failed-old", failed_old, "release-failed"),
                ] {
                    c.execute(
                        "INSERT INTO release_evidence(receipt_id,release_id,repository_id,receipt_json) VALUES(?1,?2,'r1',?3)",
                        rusqlite::params![receipt_id, release_id, receipt],
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
    }

    #[test]
    fn malformed_active_review_reference_fails_closed_and_unknown_owner_is_ignored() {
        let (_temporary, database, _first, _second) = fixture();
        let record = review_record("malformed");
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES('review',1,'r1',1,2,?1,'fixture',2)",
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

        let (_temporary, unknown_database, unknown_first, _unknown_second) = fixture();
        unknown_database
            .transaction(|c| {
                c.execute(
                    "INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES('review',1,'r1',1,2,?1,'fixture',2)",
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
    }
}
