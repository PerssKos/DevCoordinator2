use super::*;
use crate::plan::{PlanService, SqliteDeploymentEvidence};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

const REPO: &str = "r1111111111111111";
const OTHER: &str = "r9999999999999999";
const OLD: &str = "p1111111111111111";
const CHILD: &str = "p2222222222222222";
const LIVE: &str = "p4444444444444444";
const DATE: &str = "2026-09-15T12:00:00Z";

struct Fixture {
    _root: tempfile::TempDir,
    live: Database,
    request: Request,
}

fn seed_repositories(database: &Database) {
    database.call(|c| {
        c.execute_batch("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES
          ('r1111111111111111','/tmp/recovery-project','Project','2026-09-01T00:00:00Z',1000,'2026-09-01T00:00:00Z'),
          ('r9999999999999999','/tmp/recovery-other','Other','2026-09-01T00:00:00Z',1000,'2026-09-01T00:00:00Z');")?;
        Ok(())
    }).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let transaction = root.path().join("saved");
        fs::create_dir(&transaction).unwrap();
        let source = Database::open(root.path().join("source.sqlite3")).unwrap();
        seed_repositories(&source);
        source.call(|c| {
            c.execute_batch("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,requested_at,created_at,created_by,updated_at) VALUES
              ('v1111111111111111','r1111111111111111',1,'Historical preview','preview','requested','2026-09-02T00:00:00Z','2026-09-02T00:00:00Z','old-owner','2026-09-02T00:00:00Z');
              INSERT INTO tasks(task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,kind,status,estimated_loc,created_at,created_by,updated_at) VALUES
              ('p1111111111111111','r1111111111111111',NULL,'v1111111111111111',1,1,'Original goal','Preserve the original full outcome','goal','in_progress',300,'2026-09-02T00:00:00Z','old-owner','2026-09-02T00:00:00Z'),
              ('p2222222222222222','r1111111111111111','p1111111111111111','v1111111111111111',2,1,'Verified historical child','Preserve completed behavior and evidence','stub','done',120,'2026-09-02T00:00:00Z','old-owner','2026-09-03T00:00:00Z'),
              ('p9999999999999999','r9999999999999999',NULL,NULL,1,1,'UNRELATED_PRIVATE_SENTINEL','Never import another repository','goal','planned',100,'2026-09-02T00:00:00Z','old-owner','2026-09-02T00:00:00Z');
              INSERT INTO decisions(decision_id,repository_id,seq,ref,aspect,title,body,created_at,created_by) VALUES
              ('n1111111111111111','r1111111111111111',1,'ORIGINAL-DIRECTION','architecture','Original decision','Keep all original domain meaning','2026-09-02T00:00:00Z','old-owner'),
              ('n2222222222222222','r1111111111111111',2,'CORRECTION','testing','Standing correction','Require real rendered proof','2026-09-03T00:00:00Z','old-owner');
              UPDATE decisions SET superseded_by='n2222222222222222' WHERE decision_id='n1111111111111111';
              INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,to_value,actor,at,note) VALUES
              ('r1111111111111111','task','p1111111111111111','created','goal','old-owner','2026-09-02T00:00:00Z',NULL),
              ('r1111111111111111','task','p2222222222222222','status','done','old-owner','2026-09-03T00:00:00Z','original proof');
              INSERT INTO decision_summaries VALUES('r1111111111111111',2,'HISTORICAL_SUMMARY','2026-09-03T00:00:00Z','old-owner');
              INSERT INTO release_evidence VALUES('original-receipt','v1111111111111111','r1111111111111111','{\"original\":true}');
              INSERT INTO tasks(task_id,repository_id,parent_task_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES
              ('p6666666666666666','r1111111111111111','p1111111111111111',3,2,'Keep the owner feedback','Preserve the review conversation','user_feedback','planned','2026-09-03T00:00:00Z','old-owner','2026-09-03T00:00:00Z');
              INSERT INTO visual_feedback(feedback_id,task_id,repository_id,worktree_id,run_id,check_name,phase,formal_run_id,cell_id,screenshot_kind,screenshot_sha256,image_id,geometry_json,root_comment_id,created_at,created_by,updated_at) VALUES
              ('feedback-old','p6666666666666666','r1111111111111111','w-old','t-old','rendered','run','formal-old','cell-old','initial','sha-old','image-old','{}','comment-old','2026-09-03T00:00:00Z','old-owner','2026-09-03T00:00:00Z');
              INSERT INTO visual_feedback_comments(comment_id,feedback_id,seq,body,created_at,created_by,updated_at) VALUES
              ('comment-old','feedback-old',1,'Original owner correction','2026-09-03T00:00:00Z','old-owner','2026-09-03T00:00:00Z');
              INSERT INTO visual_feedback_events(feedback_id,event,comment_id,actor,at) VALUES
              ('feedback-old','created','comment-old','old-owner','2026-09-03T00:00:00Z');
              UPDATE meta SET value='20' WHERE key='schema_version';")?;
            Ok(())
        }).unwrap();
        let backup = transaction.join("authority-before.sqlite3");
        source.backup(backup.clone()).unwrap();
        source.close().unwrap();
        let header = transaction.join("installation-snapshot.json");
        fs::write(
            &header,
            serde_json::to_vec(
                &serde_json::json!({"schema":1,"status":"committed","transaction_dir":transaction}),
            )
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(header, fs::Permissions::from_mode(0o600)).unwrap();
        let live = Database::open(root.path().join("live.sqlite3")).unwrap();
        seed_repositories(&live);
        live.call(|c| {
            c.execute_batch("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at,delivered_at) VALUES
              ('v4444444444444444','r1111111111111111',1,'Current working preview','preview','delivered','2026-09-14T00:00:00Z','new-owner','2026-09-14T00:00:00Z','2026-09-14T00:00:00Z');
              INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES
              ('p4444444444444444','r1111111111111111',1,1,'Newer work','Preserve newer accepted work','improvement','in_progress','2026-09-14T00:00:00Z','new-owner','2026-09-14T00:00:00Z'),
              ('p8888888888888888','r9999999999999999',1,1,'Other current work','Preserve the other repository','goal','planned','2026-09-14T00:00:00Z','other-owner','2026-09-14T00:00:00Z');
              INSERT INTO decisions(decision_id,repository_id,seq,ref,aspect,title,body,created_at,created_by) VALUES
              ('n4444444444444444','r1111111111111111',1,'LATEST-DIRECTION','process','Current direction','Current approved scope remains authoritative','2026-09-14T00:00:00Z','new-owner');
              INSERT INTO decision_summaries VALUES('r1111111111111111',1,'CURRENT_SUMMARY','2026-09-14T00:00:00Z','new-owner');
              INSERT INTO release_evidence VALUES('current-receipt','v4444444444444444','r1111111111111111','{\"current\":true}');")?;
            Ok(())
        }).unwrap();
        let request = Request {
            repository_id: REPO.into(),
            transaction_dir: transaction.to_string_lossy().into_owned(),
            backup_sha256: digest(fs::read(backup).unwrap()),
            expected_live_sha256: None,
            apply: false,
        };
        Self {
            _root: root,
            live,
            request,
        }
    }

    fn prepare(&self) -> Receipt {
        recover(&self.live, self.request.clone(), "new-owner", DATE).unwrap()
    }
    fn application(&self) -> Request {
        let mut request = self.request.clone();
        request.expected_live_sha256 = Some(self.prepare().live_sha256);
        request.apply = true;
        request
    }
    fn plan(&self) -> PlanService {
        PlanService::new(
            self.live.clone(),
            Arc::new(SqliteDeploymentEvidence::new(
                self.live.clone(),
                "example.test",
            )),
        )
    }
    fn mutate_backup(&mut self, sql: &str) {
        let path = PathBuf::from(&self.request.transaction_dir).join("authority-before.sqlite3");
        let connection = Connection::open(&path).unwrap();
        // Only this disposable backup fixture is intentionally made inconsistent.
        connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        self.request.backup_sha256 = digest(fs::read(path).unwrap());
    }

    fn copy_saved_into_live(&self) {
        let path = PathBuf::from(&self.request.transaction_dir).join("authority-before.sqlite3");
        let source =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let tables = read_tables(&source, REPO).unwrap();
        self.live.transaction(move |connection| {
            connection.execute_batch("PRAGMA defer_foreign_keys=ON; UPDATE tasks SET seq=100 WHERE repository_id='r1111111111111111'; UPDATE releases SET seq=100 WHERE repository_id='r1111111111111111'; UPDATE decisions SET seq=100 WHERE repository_id='r1111111111111111';")?;
            for &(table, _) in TABLES {
                if table == "decision_summaries" {
                    continue;
                }
                for row in &tables[table] {
                    insert_row(connection, table, row)?;
                }
            }
            Ok(())
        }).unwrap();
    }
}

#[test]
fn identical_overlap_keeps_live_rows_and_current_release_eligible() {
    let fixture = Fixture::new();
    fixture.copy_saved_into_live();
    let before = fixture
        .live
        .call(|connection| read_tables(connection, REPO))
        .unwrap();
    let preview = fixture.prepare();
    let encoded = serde_json::to_value(&preview).unwrap();
    assert_eq!(preview.counts["tasks"], 0);
    assert_eq!(encoded["existing_counts"]["tasks"], 3);
    assert_eq!(encoded["existing_counts"]["plan_events"], 2);
    let request = fixture.application();
    let applied = recover(&fixture.live, request.clone(), "owner", DATE).unwrap();
    assert_eq!(applied.status, "applied");
    assert_eq!(
        fixture
            .live
            .call(|connection| read_tables(connection, REPO))
            .unwrap(),
        before
    );
    fixture.live.call(|connection| {
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM planning_recovery_records WHERE record_kind='releases'", [], |row| row.get::<_,i64>(0))?, 0);
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM planning_recovery_records WHERE record_kind='existing:releases'", [], |row| row.get::<_,i64>(0))?, 1);
        Ok(())
    }).unwrap();
    assert_eq!(
        recover(&fixture.live, request, "owner", DATE)
            .unwrap()
            .status,
        "already_applied"
    );
}

#[test]
fn differing_overlap_reports_private_conflicts_and_blocks_application() {
    let fixture = Fixture::new();
    fixture.copy_saved_into_live();
    fixture
        .live
        .call(|connection| {
            connection.execute(
                "UPDATE tasks SET title='PRIVATE_LIVE_VALUE',status='planned' WHERE task_id=?1",
                [OLD],
            )?;
            Ok(())
        })
        .unwrap();
    let before = fixture
        .live
        .call(|connection| fingerprint(connection, REPO))
        .unwrap();
    let preview = fixture.prepare();
    let encoded = serde_json::to_value(&preview).unwrap();
    assert_eq!(preview.status, "conflicted");
    assert_eq!(encoded["conflict_count"], 1);
    assert_eq!(encoded["conflicts"][0]["id"], OLD);
    assert_eq!(
        encoded["conflicts"][0]["fields"],
        serde_json::json!(["status", "title"])
    );
    assert!(!encoded.to_string().contains("PRIVATE_LIVE_VALUE"));
    assert!(!encoded.to_string().contains("Original goal"));
    let mut request = fixture.request.clone();
    request.apply = true;
    request.expected_live_sha256 = Some(preview.live_sha256);
    assert!(recover(&fixture.live, request, "owner", DATE).is_err());
    assert_eq!(
        fixture
            .live
            .call(|connection| fingerprint(connection, REPO))
            .unwrap(),
        before
    );
    fixture
        .live
        .call(|connection| {
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM planning_recoveries", [], |row| row
                        .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn missing_child_is_restored_without_reimporting_its_existing_parent_or_events() {
    let fixture = Fixture::new();
    fixture.copy_saved_into_live();
    fixture
        .live
        .call(|connection| {
            connection.execute("DELETE FROM tasks WHERE task_id=?1", [CHILD])?;
            Ok(())
        })
        .unwrap();
    let parent = fixture.plan().task_history(OLD).unwrap();
    let preview = fixture.prepare();
    assert_eq!(preview.counts["tasks"], 1);
    assert_eq!(preview.counts["plan_events"], 0);
    assert_eq!(preview.existing_counts["tasks"], 2);
    let applied = recover(&fixture.live, fixture.application(), "owner", DATE).unwrap();
    assert_eq!(
        applied
            .mappings
            .iter()
            .filter(|mapping| mapping.kind == "tasks")
            .count(),
        1
    );
    assert_eq!(fixture.plan().task_history(OLD).unwrap(), parent);
    assert_eq!(
        fixture
            .plan()
            .task_history(CHILD)
            .unwrap()
            .task
            .parent_task_id
            .as_deref(),
        Some(OLD)
    );
}

#[test]
fn event_overlap_preserves_distinct_occurrences_with_the_same_payload() {
    let fixture = Fixture::new();
    fixture.copy_saved_into_live();
    let mut source = fixture
        .live
        .call(|connection| read_tables(connection, REPO))
        .unwrap();
    let live = source.clone();
    let mut repeated = source["plan_events"][0].clone();
    repeated.insert("event_id".into(), Value::from(100));
    source.get_mut("plan_events").unwrap().push(repeated);
    let overlap = overlap::classify(&source, &live).unwrap();
    assert_eq!(overlap.existing["plan_events"].len(), 2);
    assert!(!overlap.existing["plan_events"].contains("100"));
}

#[test]
fn conflict_samples_are_bounded_without_losing_the_total() {
    let fixture = Fixture::new();
    let mut source = fixture
        .live
        .call(|connection| read_tables(connection, REPO))
        .unwrap();
    let template = source["tasks"][0].clone();
    let mut live = source.clone();
    source.get_mut("tasks").unwrap().clear();
    live.get_mut("tasks").unwrap().clear();
    for index in 0..80 {
        let mut row = template.clone();
        row.insert("task_id".into(), Value::from(format!("p{index:016x}")));
        source.get_mut("tasks").unwrap().push(row.clone());
        row.insert("title".into(), Value::from("PRIVATE_CHANGED_TITLE"));
        live.get_mut("tasks").unwrap().push(row);
    }
    let overlap = overlap::classify(&source, &live).unwrap();
    assert_eq!(overlap.conflict_count, 80);
    assert_eq!(overlap.conflicts.len(), 64);
    assert!(
        !serde_json::to_string(&overlap.conflicts)
            .unwrap()
            .contains("PRIVATE_CHANGED_TITLE")
    );
}

#[test]
fn dry_run_is_private_scoped_and_does_not_change_either_database() {
    let fixture = Fixture::new();
    let before = fixture.live.call(|c| fingerprint(c, REPO)).unwrap();
    let preview = fixture.prepare();
    assert_eq!(preview.counts["tasks"], 3);
    assert_eq!(preview.counts["decisions"], 2);
    assert_eq!(preview.counts["plan_events"], 2);
    assert_eq!(preview.status, "prepared");
    assert_eq!(preview.live_sha256, before);
    assert_eq!(fixture.live.call(|c| fingerprint(c, REPO)).unwrap(), before);
    let encoded = serde_json::to_string(&preview).unwrap();
    assert!(!encoded.contains("PRIVATE_SENTINEL"));
    assert!(!encoded.contains("Preserve completed behavior"));
    assert!(fixture.plan().task_history(OLD).is_err());
}

#[test]
fn apply_preserves_identity_history_newer_state_and_current_release_and_is_repeatable() {
    let fixture = Fixture::new();
    let before = fixture.plan().task_history(LIVE).unwrap();
    let other = fixture.live.call(|c| fingerprint(c, OTHER)).unwrap();
    let request = fixture.application();
    let receipt = recover(&fixture.live, request.clone(), "new-owner", DATE).unwrap();
    assert_eq!(receipt.status, "applied");
    let goal = fixture.plan().task_history(OLD).unwrap();
    let child = fixture.plan().task_history(CHILD).unwrap();
    assert_eq!(goal.task.seq, 2);
    assert_eq!(child.task.seq, 3);
    assert_eq!(child.task.parent_task_id.as_deref(), Some(OLD));
    assert_eq!(
        child.task.status,
        devcoordinator2_api::params::TaskStatus::Done
    );
    assert_eq!(child.events[0].at, "2026-09-03T00:00:00Z");
    assert_eq!(fixture.plan().task_history(LIVE).unwrap(), before);
    assert_eq!(fixture.live.call(|c| fingerprint(c, OTHER)).unwrap(), other);
    let collection = serde_json::to_value(fixture.plan().overview(None).unwrap()).unwrap();
    let project = collection["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repository_id"] == REPO)
        .unwrap();
    assert_eq!(
        project["current_release"]["name"],
        "Current working preview"
    );
    assert_eq!(project["preview_requested"], false);
    fixture.live.call(|c| {
        assert_eq!(c.query_row("SELECT body FROM decision_summaries WHERE repository_id=?1",[REPO],|r|r.get::<_,String>(0))?,"CURRENT_SUMMARY");
        assert_eq!(c.query_row("SELECT superseded_by FROM decisions WHERE decision_id='n1111111111111111'",[],|r|r.get::<_,String>(0))?,"n2222222222222222");
        assert_eq!(c.query_row("SELECT COUNT(*) FROM decisions_fts WHERE decisions_fts MATCH 'rendered'",[],|r|r.get::<_,i64>(0))?,1);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM release_evidence WHERE repository_id=?1",[REPO],|r|r.get::<_,i64>(0))?,2);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM planning_recovery_records WHERE record_kind='decision_summaries'",[],|r|r.get::<_,i64>(0))?,1);
        Ok(())
    }).unwrap();
    fixture
        .live
        .call(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT body FROM visual_feedback_comments WHERE comment_id='comment-old'",
                    [],
                    |row| row.get::<_, String>(0)
                )?,
                "Original owner correction"
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM visual_feedback_events WHERE feedback_id='feedback-old'",
                    [],
                    |row| row.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
    let post = fixture.live.call(|c| fingerprint(c, REPO)).unwrap();
    assert_eq!(
        recover(&fixture.live, request, "new-owner", DATE)
            .unwrap()
            .status,
        "already_applied"
    );
    assert_eq!(fixture.live.call(|c| fingerprint(c, REPO)).unwrap(), post);
}

#[test]
fn concurrent_edit_rejects_stale_application_without_overwriting_it() {
    let fixture = Fixture::new();
    let request = fixture.application();
    fixture
        .live
        .call(|c| {
            c.execute(
                "UPDATE tasks SET title='New concurrent user edit' WHERE task_id=?1",
                [LIVE],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(recover(&fixture.live, request, "owner", DATE).is_err());
    assert_eq!(
        fixture.plan().task_history(LIVE).unwrap().task.title,
        "New concurrent user edit"
    );
    assert!(fixture.plan().task_history(OLD).is_err());
}

#[test]
fn failure_after_inserts_rolls_back_every_recovered_row_and_receipt() {
    let fixture = Fixture::new();
    let request = fixture.application();
    fixture.live.call(|c| { c.execute_batch("CREATE TRIGGER fail_recovery BEFORE INSERT ON decisions WHEN new.ref='CORRECTION' BEGIN SELECT RAISE(ABORT,'fixture failure after task import'); END;")?; Ok(()) }).unwrap();
    let before = fixture.live.call(|c| fingerprint(c, REPO)).unwrap();
    assert!(recover(&fixture.live, request, "owner", DATE).is_err());
    assert_eq!(fixture.live.call(|c| fingerprint(c, REPO)).unwrap(), before);
    fixture
        .live
        .call(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM planning_recoveries", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn changed_backup_and_missing_live_fingerprint_are_rejected() {
    let fixture = Fixture::new();
    let mut request = fixture.request.clone();
    request.backup_sha256 = "0".repeat(64);
    assert!(recover(&fixture.live, request, "owner", DATE).is_err());
    let mut request = fixture.request.clone();
    request.apply = true;
    assert!(recover(&fixture.live, request, "owner", DATE).is_err());
    assert!(fixture.plan().task_history(OLD).is_err());
}

#[test]
fn duplicate_identity_in_another_repository_is_not_reassigned() {
    let fixture = Fixture::new();
    fixture
        .live
        .call(|c| {
            c.execute(
                "UPDATE tasks SET task_id=?1 WHERE task_id='p8888888888888888'",
                [OLD],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(recover(&fixture.live, fixture.request.clone(), "owner", DATE).is_err());
    assert_eq!(
        fixture.plan().task_history(OLD).unwrap().task.repository_id,
        OTHER
    );
}

#[test]
fn dangling_relationship_and_parent_cycle_are_rejected() {
    for sql in [
        "UPDATE tasks SET parent_task_id='p7777777777777777' WHERE task_id='p2222222222222222';",
        "UPDATE tasks SET parent_task_id='p2222222222222222' WHERE task_id='p1111111111111111';",
    ] {
        let mut fixture = Fixture::new();
        fixture.mutate_backup(sql);
        assert!(recover(&fixture.live, fixture.request.clone(), "owner", DATE).is_err());
        assert!(fixture.plan().task_history(OLD).is_err());
    }
}

#[test]
fn colliding_decision_reference_is_rejected_before_import() {
    let fixture = Fixture::new();
    fixture
        .live
        .call(|c| {
            c.execute(
                "UPDATE decisions SET ref='ORIGINAL-DIRECTION' WHERE repository_id=?1",
                [REPO],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(recover(&fixture.live, fixture.request.clone(), "owner", DATE).is_err());
    assert!(fixture.plan().task_history(OLD).is_err());
}

#[test]
fn normal_protocol_dispatch_authorizes_and_returns_recovered_task_history() {
    check_normal_protocol(Fixture::new());
}

#[test]
fn normal_protocol_dispatch_handles_identical_overlap_and_missing_child() {
    let fixture = Fixture::new();
    fixture.copy_saved_into_live();
    fixture
        .live
        .call(|connection| {
            connection.execute("DELETE FROM tasks WHERE task_id=?1", [CHILD])?;
            Ok(())
        })
        .unwrap();
    check_normal_protocol(fixture);
}

#[test]
fn stored_receipts_without_overlap_fields_remain_readable() {
    let fixture = Fixture::new();
    let preview = fixture.prepare();
    let mut legacy = serde_json::to_value(&preview).unwrap();
    for field in ["existing_counts", "conflict_count", "conflicts"] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let parsed: Receipt = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.mappings, preview.mappings);
    assert!(parsed.existing_counts.is_empty());
    assert_eq!(parsed.conflict_count, 0);
    assert!(parsed.conflicts.is_empty());
}

fn check_normal_protocol(fixture: Fixture) {
    use crate::access::Caller;
    use crate::control_plane::ControlPlane;
    use crate::daemon::OperationExecutor;
    let configuration = crate::automation_test_support::Fixture::new();
    let plane = ControlPlane::with_adapters(
        configuration.config.clone(),
        fixture.live.clone(),
        Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
        Arc::new(crate::platform::FixedClock(
            time::macros::datetime!(2026-09-15 12:00 UTC),
        )),
    )
    .unwrap();
    let local = Caller::from_client(1, 1000, 1000, Default::default(), None).unwrap();
    let mut unauthorized = local.clone();
    unauthorized.uid = 999;
    unauthorized.identity = Some("untrusted@example.test".to_owned());
    let rejected = plane.execute(
        "plan.recovery",
        serde_json::to_value(&fixture.request).unwrap(),
        &unauthorized,
    );
    assert_eq!(rejected.unwrap_err().code, ErrorCode::PermissionDenied);
    let prepared = plane
        .execute(
            "plan.recovery",
            serde_json::to_value(&fixture.request).unwrap(),
            &local,
        )
        .unwrap();
    let mut request = fixture.request.clone();
    request.expected_live_sha256 = Some(prepared["live_sha256"].as_str().unwrap().to_owned());
    request.apply = true;
    let applied = plane
        .execute(
            "plan.recovery",
            serde_json::to_value(request).unwrap(),
            &local,
        )
        .unwrap();
    assert_eq!(applied["status"], "applied");
    let history = plane
        .execute("task.history", serde_json::json!({"task_id":CHILD}), &local)
        .unwrap();
    assert_eq!(history["task"]["parent_task_id"], OLD);
    assert_eq!(history["task"]["status"], "done");
    assert_eq!(history["events"][0]["at"], "2026-09-03T00:00:00Z");
    let operation = devcoordinator2_api::operation("plan.recovery").unwrap();
    assert_eq!(operation.policy.scope, devcoordinator2_api::Scope::Server);
    assert_eq!(
        operation.policy.role,
        devcoordinator2_api::Role::Administrator
    );
    assert!(operation.policy.idempotent);
}
