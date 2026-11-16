//! Conservative scan reconciliation, committed in bounded, revision-checked batches.
use super::{
    StorageService, db_error, json,
    model::{Context, Discovery, Record, atom},
};
use devcoordinator2_api::{ProtocolError, storage as api};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

pub(super) struct ReconcileCursor {
    discovery: Discovery,
    sequence: u64,
    now: u64,
    existing: BTreeMap<String, Record>,
    existing_page: VecDeque<Record>,
    after_existing: Option<String>,
    absence_cursor: Option<String>,
    live_finished: bool,
    context: Context,
    ids: Vec<String>,
    seen: BTreeSet<String>,
    missing: BTreeSet<String>,
    unavailable: BTreeSet<String>,
    recovery: BTreeMap<String, Vec<usize>>,
    lineages: VecDeque<Vec<usize>>,
    phase: u8,
    index: usize,
}

impl ReconcileCursor {
    pub(super) fn new(
        service: &StorageService,
        discovery: Discovery,
        sequence: u64,
        manual_caller_uid: Option<u32>,
    ) -> Result<Self, ProtocolError> {
        Ok(Self {
            discovery,
            sequence,
            now: service.now_ms(),
            ids: Vec::new(),
            existing: BTreeMap::new(),
            existing_page: VecDeque::new(),
            after_existing: None,
            absence_cursor: None,
            live_finished: false,
            context: service.context_for_uid(manual_caller_uid)?,
            seen: BTreeSet::new(),
            missing: BTreeSet::new(),
            unavailable: BTreeSet::new(),
            recovery: BTreeMap::new(),
            lineages: VecDeque::new(),
            phase: 0,
            index: 0,
        })
    }

    pub(super) fn step(
        &mut self,
        service: &StorageService,
        budget: Duration,
    ) -> Result<(bool, u64, u64), ProtocolError> {
        let started = Instant::now();
        let mut rows = 0u64;
        let mut bytes = 0u64;
        match self.phase {
            0 => {
                // Exact matches include removed tombstones: a newer scan must
                // never be resurrected by an older discovery result.
                let ids = self
                    .discovery
                    .records
                    .iter()
                    .skip(self.index)
                    .take(32)
                    .map(|record| record.artifact.artifact_id.clone())
                    .collect::<Vec<_>>();
                rows = ids.len() as u64;
                self.existing.extend(service.reconcile_matches(&ids)?);
                self.index += ids.len();
                if self.index >= self.discovery.records.len() {
                    self.discovery.records.retain(|record| {
                        self.existing
                            .get(&record.artifact.artifact_id)
                            .is_none_or(|old| old.update_sequence < self.sequence)
                    });
                    self.seen = self
                        .discovery
                        .records
                        .iter()
                        .map(|record| record.artifact.artifact_id.clone())
                        .collect();
                    self.existing.retain(|id, _| self.seen.contains(id));
                    self.phase = 1;
                    self.index = 0;
                }
            }
            1 => {
                if self.existing_page.is_empty() && !self.live_finished {
                    let page = service.reconcile_live_page(
                        self.discovery.repository_id.as_deref(),
                        self.after_existing.as_deref(),
                    )?;
                    self.live_finished = page.len() < 32;
                    self.existing_page = page.into();
                }
                while let Some(old) = self.existing_page.front() {
                    if rows >= 32 || (rows > 0 && started.elapsed() >= budget) {
                        break;
                    }
                    let id = old.artifact.artifact_id.clone();
                    rows += 1;
                    if old.update_sequence >= self.sequence || self.seen.contains(&id) {
                        self.after_existing = Some(id);
                        self.absence_cursor = None;
                        self.existing_page.pop_front();
                        continue;
                    }
                    let tracked_source_directory = old.artifact.kind == api::Kind::BuildOutput
                        && matches!(
                            &old.locator,
                            super::model::Locator::Directory { path, .. }
                                if self.discovery.tracked_source_directories.contains(path)
                        )
                        && !old.artifact.protected
                        && !self.context.protected_resources.contains(&old.resource_key)
                        && !self.context.protected_ancestors.contains(&old.resource_key)
                        && !self.context.leased_artifacts.contains(&id)
                        && !self.context.leased_resources.contains(&old.resource_key)
                        && !self.context.leased_ancestors.contains(&old.resource_key);
                    if tracked_source_directory {
                        // This row was emitted by a prior scan before the
                        // source-folder classifier consulted Git. The path is
                        // still present, so ordinary absence probing cannot
                        // retire it; the fresh scan's tracked-source evidence
                        // is the authoritative replacement observation.
                        self.missing.insert(id.clone());
                    } else if !self.discovery.complete_kinds.contains(&old.artifact.kind) {
                        if old.artifact.kind != api::Kind::BuildCache
                            || self.discovery.repository_id.is_none()
                        {
                            self.unavailable.insert(id.clone());
                        }
                    } else {
                        match service.backend.absent_step(
                            old,
                            &self.context,
                            &mut self.absence_cursor,
                            super::native::DiscoveryBudget {
                                entries: 16,
                                elapsed: budget.saturating_sub(started.elapsed()),
                            },
                        ) {
                            Ok(Some(true)) => {
                                self.missing.insert(id.clone());
                            }
                            Ok(Some(false)) => {}
                            Ok(None) => return Ok((false, rows, bytes)),
                            Err(_) => {
                                self.unavailable.insert(id.clone());
                            }
                        }
                    }
                    self.after_existing = Some(id.clone());
                    self.absence_cursor = None;
                    let old = self.existing_page.pop_front().expect("page front exists");
                    if self.missing.contains(&id) || self.unavailable.contains(&id) {
                        self.ids.push(id.clone());
                        self.existing.insert(id, old);
                    }
                }
                if self.live_finished && self.existing_page.is_empty() {
                    self.phase = 2;
                    self.index = 0;
                }
            }
            2 => {
                for record in self.discovery.records.iter_mut().skip(self.index) {
                    if rows >= 32 || (rows > 0 && started.elapsed() >= budget) {
                        break;
                    }
                    rows += 1;
                    record.update_sequence = self.sequence;
                    if record.blockers.iter().any(|reason| {
                        matches!(
                            reason.as_str(),
                            "active_process"
                                | "active_consumer"
                                | "active_worktree"
                                | "active_evidence"
                                | "active_lease"
                        )
                    }) {
                        record.artifact.last_used_at_ms = Some(self.now);
                    }
                    if let Some(old) = self.existing.get(&record.artifact.artifact_id) {
                        if old.artifact.removed_at_ms.is_some()
                            || old.resource_key != record.resource_key
                        {
                            record.artifact.revision = old.artifact.revision + 1;
                            record.artifact.observed_since_ms = self.now;
                            record.artifact.protected = old.artifact.protected;
                            continue;
                        }
                        record.artifact.observed_since_ms = old.artifact.observed_since_ms;
                        record.artifact.protected = old.artifact.protected;
                        if record.ignored_review.is_none() {
                            record.ignored_review = old.ignored_review.clone();
                        }
                        record.disposal_approved = old.disposal_approved;
                        record.disposal_reason = old.disposal_reason.clone();
                        if old.disposal_approved {
                            record.artifact.effect = old.artifact.effect;
                            record.artifact.ownership = "cleanup_owned".into();
                            record.owner_deployment = record
                                .owner_deployment
                                .clone()
                                .or(old.owner_deployment.clone());
                            record.artifact.repository_id = record
                                .artifact
                                .repository_id
                                .clone()
                                .or(old.artifact.repository_id.clone());
                            record.artifact.repository_name = record
                                .artifact
                                .repository_name
                                .clone()
                                .or(old.artifact.repository_name.clone());
                            record.blockers.retain(|s| {
                                s != "ownership_unknown" && s != "disposal_not_authorized"
                            });
                        }
                        if old.last_activity_signature != record.last_activity_signature {
                            record.artifact.last_used_at_ms = Some(self.now);
                        } else {
                            record.artifact.last_used_at_ms = record
                                .artifact
                                .last_used_at_ms
                                .max(old.artifact.last_used_at_ms);
                        }
                        if record.source_owner_observation.is_some() {
                            record.blockers.extend(old.blockers.iter().cloned());
                            record.artifact.verified_at_ms = old.artifact.verified_at_ms;
                        }
                        record.blockers.sort();
                        record.blockers.dedup();
                        let mut prior_blockers = old.blockers.clone();
                        prior_blockers.sort();
                        prior_blockers.dedup();
                        let changed = old.fingerprint != record.fingerprint
                            || old.ancestor_keys != record.ancestor_keys
                            || old.last_activity_signature != record.last_activity_signature
                            || prior_blockers != record.blockers
                            || old.artifact.dependencies != record.artifact.dependencies;
                        record.artifact.revision = old.artifact.revision + u64::from(changed);
                    }
                }
                self.index += rows as usize;
                if self.index >= self.discovery.records.len() {
                    self.phase = 3;
                    self.index = 0;
                }
            }
            3 => {
                for (index, record) in self.discovery.records.iter().enumerate().skip(self.index) {
                    if rows >= 128 || (rows > 0 && started.elapsed() >= budget) {
                        break;
                    }
                    rows += 1;
                    if record.recovery_verified
                        && let Some(lineage) = &record.recovery_lineage
                    {
                        self.recovery
                            .entry(lineage.clone())
                            .or_default()
                            .push(index);
                    }
                }
                self.index += rows as usize;
                if self.index >= self.discovery.records.len() {
                    self.lineages = std::mem::take(&mut self.recovery).into_values().collect();
                    self.phase = 4;
                    self.index = 0;
                }
            }
            4 => {
                if let Some(mut group) = self.lineages.pop_front() {
                    group.sort_by_key(|i| {
                        std::cmp::Reverse(self.discovery.records[*i].recovery_created_at_ms)
                    });
                    let policy = service.policy(api::Scope {
                        repository_id: self.discovery.records[group[0]]
                            .artifact
                            .repository_id
                            .clone(),
                    })?;
                    for index in group.iter().take(policy.minimum_verified_backups as usize) {
                        self.discovery.records[*index]
                            .blockers
                            .push("required_recovery".into());
                        rows += 1;
                    }
                } else {
                    self.phase = 5;
                }
            }
            5 => {
                let mut batch = Vec::new();
                for record in self.discovery.records.iter().skip(self.index) {
                    if rows >= 64 || (rows > 0 && started.elapsed() >= budget) {
                        break;
                    }
                    let encoded = json(record)?;
                    if rows > 0 && bytes.saturating_add(encoded.len() as u64) > 256 * 1024 {
                        break;
                    }
                    bytes = bytes.saturating_add(encoded.len() as u64);
                    rows += 1;
                    batch.push((
                        record,
                        encoded,
                        self.existing
                            .get(&record.artifact.artifact_id)
                            .map(|old| old.artifact.revision),
                    ));
                }
                service.database.maintenance_transaction(|connection| {
                    for (record,encoded,expected) in batch {
                        match super::locks::ensure_editable(connection,&record.resource_key,None) {
                            Ok(())=>{},Err(crate::database::DatabaseError::Domain(error)) if error.code==devcoordinator2_api::ErrorCode::Busy=>continue,Err(error)=>return Err(error),
                        }
                        connection.execute("INSERT INTO storage_artifacts VALUES($1,$2,$3,$4,NULL,$5,$6) ON CONFLICT(artifact_id) DO UPDATE SET repository_id=excluded.repository_id,kind=excluded.kind,revision=excluded.revision,removed_at_ms=NULL,record_json=excluded.record_json,updated_at_ms=excluded.updated_at_ms WHERE storage_artifacts.revision=$7 AND COALESCE((storage_artifacts.record_json::jsonb->>'update_sequence')::bigint,0)<$8",crate::database::params![record.artifact.artifact_id,record.artifact.repository_id,atom(record.artifact.kind),record.artifact.revision as i64,encoded,self.now as i64,expected.map(|v|v as i64),self.sequence as i64])?;
                    }
                    Ok(())
                }).map_err(db_error)?;
                self.index += rows as usize;
                if self.index >= self.discovery.records.len() {
                    self.phase = 6;
                    self.index = 0;
                }
            }
            6 => {
                let mut batch = Vec::new();
                for id in self.ids.iter().skip(self.index) {
                    if rows >= 64 || (rows > 0 && started.elapsed() >= budget) {
                        break;
                    }
                    let removal = self.missing.contains(id);
                    let unavailable = self.unavailable.contains(id);
                    if !removal && !unavailable {
                        rows += 1;
                        continue;
                    }
                    let mut old = self.existing[id].clone();
                    old.artifact.revision += 1;
                    old.update_sequence = self.sequence;
                    if removal {
                        old.artifact.removed_at_ms = Some(self.now);
                    } else {
                        old.artifact.verified_at_ms = None;
                        if !old.blockers.iter().any(|r| r == "observation_unavailable") {
                            old.blockers.push("observation_unavailable".into());
                        }
                    }
                    let encoded = json(&old)?;
                    if rows > 0 && bytes.saturating_add(encoded.len() as u64) > 256 * 1024 {
                        break;
                    }
                    bytes = bytes.saturating_add(encoded.len() as u64);
                    rows += 1;
                    batch.push((old, encoded, removal));
                }
                service.database.maintenance_transaction(|connection| {
                    for (record,encoded,removal) in batch {
                        match super::locks::ensure_editable(connection,&record.resource_key,None) {
                            Ok(())=>{},Err(crate::database::DatabaseError::Domain(error)) if error.code==devcoordinator2_api::ErrorCode::Busy=>continue,Err(error)=>return Err(error),
                        }
                        if removal {
                            connection.execute("UPDATE storage_artifacts SET removed_at_ms=$1,revision=$2,record_json=$3,updated_at_ms=$1 WHERE artifact_id=$4 AND revision=$5 AND COALESCE((record_json::jsonb->>'update_sequence')::bigint,0)<$6",crate::database::params![self.now as i64,record.artifact.revision as i64,encoded,record.artifact.artifact_id,(record.artifact.revision-1) as i64,self.sequence as i64])?;
                        } else {
                            connection.execute("UPDATE storage_artifacts SET revision=$1,record_json=$2,updated_at_ms=$3 WHERE artifact_id=$4 AND revision=$5 AND COALESCE((record_json::jsonb->>'update_sequence')::bigint,0)<$6",crate::database::params![record.artifact.revision as i64,encoded,self.now as i64,record.artifact.artifact_id,(record.artifact.revision-1) as i64,self.sequence as i64])?;
                        }
                    }
                    Ok(())
                }).map_err(db_error)?;
                self.index += rows as usize;
                if self.index >= self.ids.len() {
                    self.phase = 7;
                }
            }
            _ => {
                let filesystems = json(&self.discovery.filesystems)?;
                let coverage = json(&self.discovery.coverage_gaps)?;
                bytes = (filesystems.len() + coverage.len()) as u64;
                service.database.maintenance_transaction(|connection| {
                    connection.execute("UPDATE storage_scan_state SET last_scan_at_ms=$1,filesystem_json=$2,coverage_json=$3 WHERE singleton=1 AND revision=$4",crate::database::params![self.now as i64,filesystems,coverage,self.sequence as i64])?;
                    Ok(())
                }).map_err(db_error)?;
                return Ok((true, 1, bytes));
            }
        }
        Ok((false, rows, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_test_support::Fixture;
    use crate::storage::read_tests::{record, save, service};

    #[test]
    fn paged_reconciliation_preserves_newer_tombstones_and_ignores_removed_history() {
        let fixture = Fixture::new();
        let service = service(&fixture);
        let now = service.now_ms();
        let mut removed = record("newer-tombstone", now);
        removed.update_sequence = 20;
        removed.artifact.removed_at_ms = Some(now);
        save(&service, &removed);
        let old_json = json(&removed).unwrap();
        fixture.database.call(|connection| {
            connection.execute("INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) SELECT 'history-'||n,'project-alpha','build_cache',1,$1,jsonb_set($2::jsonb,'{artifact,artifact_id}',to_jsonb('history-'||n)),$1 FROM generate_series(1,2000) n",crate::database::params![now,old_json])?;
            Ok(())
        }).unwrap();
        for index in 0..70 {
            save(&service, &record(&format!("live-{index:03}"), now));
        }
        let mut rediscovered = removed.clone();
        rediscovered.artifact.removed_at_ms = None;
        let fresh = record("fresh-discovery", now);
        let mut cursor = ReconcileCursor::new(
            &service,
            Discovery {
                records: vec![rediscovered, fresh],
                ..Discovery::default()
            },
            10,
            None,
        )
        .unwrap();
        assert!(cursor.existing.is_empty());
        let mut total_rows = 0;
        let mut turns = 0;
        loop {
            let (complete, rows, _) = cursor.step(&service, Duration::from_millis(100)).unwrap();
            total_rows += rows;
            turns += 1;
            assert!(
                rows <= 128,
                "one reconciliation phase exceeded its fixed batch"
            );
            assert!(
                cursor.existing.len() <= 71,
                "removed history entered reconciliation state"
            );
            assert!(turns < 40, "historical tombstones extended the scan");
            if complete {
                break;
            }
        }
        assert!(total_rows < 200);
        assert_eq!(
            service.record("newer-tombstone").unwrap().update_sequence,
            20
        );
        assert_eq!(
            service
                .record("newer-tombstone")
                .unwrap()
                .artifact
                .removed_at_ms,
            Some(now)
        );
        assert_eq!(
            service.record("fresh-discovery").unwrap().update_sequence,
            10
        );
        assert!(
            service
                .record("live-000")
                .unwrap()
                .blockers
                .iter()
                .any(|reason| reason == "observation_unavailable")
        );
        // A later legitimate observation can revive the same identity; its
        // exact old revision still participates in the optimistic write.
        let mut rediscovered = removed;
        rediscovered.artifact.removed_at_ms = None;
        let mut newer = ReconcileCursor::new(
            &service,
            Discovery {
                repository_id: Some("project-alpha".into()),
                records: vec![rediscovered],
                ..Discovery::default()
            },
            30,
            None,
        )
        .unwrap();
        for _ in 0..40 {
            if newer.step(&service, Duration::from_millis(100)).unwrap().0 {
                break;
            }
        }
        let revived = service.record("newer-tombstone").unwrap();
        assert_eq!(revived.update_sequence, 30);
        assert!(revived.artifact.removed_at_ms.is_none());
        assert_eq!(revived.artifact.revision, 2);
    }

    #[test]
    fn tracked_source_directory_retires_persisted_build_output() {
        let fixture = Fixture::new();
        let service = service(&fixture);
        let source = fixture.repository.join("tools/build");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("source.txt"), "tracked source\n").unwrap();

        let mut stale = record("stale-tracked-build", service.now_ms());
        stale.artifact.kind = api::Kind::BuildOutput;
        stale.artifact.name = "tools/build".into();
        let (device, inode) = crate::storage::fs::identity(&source).unwrap();
        stale.locator = super::super::model::Locator::Directory {
            path: source.clone(),
            device,
            inode,
            root: fixture.repository.clone(),
            git_root: Some(fixture.repository.clone()),
        };
        stale.resource_key = format!("fs:{device}:{inode}");
        stale.private_aliases = vec![source.clone()];
        save(&service, &stale);

        let mut cursor = ReconcileCursor::new(
            &service,
            Discovery {
                repository_id: Some("project-alpha".into()),
                complete_kinds: vec![api::Kind::BuildOutput],
                tracked_source_directories: std::iter::once(source).collect(),
                ..Discovery::default()
            },
            2,
        )
        .unwrap();
        for _ in 0..32 {
            if cursor.step(&service, Duration::from_millis(100)).unwrap().0 {
                break;
            }
        }

        let retired = service.record("stale-tracked-build").unwrap();
        assert!(retired.artifact.removed_at_ms.is_some());
        assert_eq!(retired.update_sequence, 2);
    }

    #[test]
    fn tracked_source_retirement_preserves_explicit_protection() {
        let fixture = Fixture::new();
        let service = service(&fixture);
        let source = fixture.repository.join("third_party/crashpad/build");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("BUILD.gn"), "tracked source\n").unwrap();

        let mut protected = record("protected-tracked-build", service.now_ms());
        protected.artifact.kind = api::Kind::BuildOutput;
        protected.artifact.name = "third_party/crashpad/build".into();
        protected.artifact.protected = true;
        let (device, inode) = crate::storage::fs::identity(&source).unwrap();
        protected.locator = super::super::model::Locator::Directory {
            path: source.clone(),
            device,
            inode,
            root: fixture.repository.clone(),
            git_root: Some(fixture.repository.clone()),
        };
        protected.resource_key = format!("fs:{device}:{inode}");
        protected.private_aliases = vec![source.clone()];
        save(&service, &protected);

        let mut cursor = ReconcileCursor::new(
            &service,
            Discovery {
                repository_id: Some("project-alpha".into()),
                complete_kinds: vec![api::Kind::BuildOutput],
                tracked_source_directories: std::iter::once(source).collect(),
                ..Discovery::default()
            },
            2,
        )
        .unwrap();
        for _ in 0..32 {
            if cursor.step(&service, Duration::from_millis(100)).unwrap().0 {
                break;
            }
        }

        let retained = service.record("protected-tracked-build").unwrap();
        assert!(retained.artifact.removed_at_ms.is_none());
        assert!(retained.artifact.protected);
    }
}
