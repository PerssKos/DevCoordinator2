//! Evidence protection is shared with the existing retained-log owner.
use super::{
    db_error,
    model::{Locator, Record},
    parse, unavailable,
};
use crate::database::{Database, DatabaseError};
use devcoordinator2_api::ProtocolError;
use std::collections::BTreeSet;
use std::path::Path;

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
