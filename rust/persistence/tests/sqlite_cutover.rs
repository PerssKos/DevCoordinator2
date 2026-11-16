use devcoordinator2_persistence::{
    Error, Persistence,
    fixture::FixtureCluster,
    migration::{import_sqlite, inspect_sqlite, verify_import},
};

fn source(path: &std::path::Path) {
    let sqlite = rusqlite::Connection::open(path).unwrap();
    sqlite
        .execute_batch(include_str!("fixtures/sqlite-schema-34/schema.sql"))
        .unwrap();
    sqlite
        .execute_batch(include_str!("fixtures/sqlite-schema-34/tickets.sql"))
        .unwrap();
    sqlite
        .execute_batch(include_str!("fixtures/sqlite-schema-34/storage.sql"))
        .unwrap();
    sqlite
        .execute_batch(include_str!("fixtures/sqlite-schema-34/sketch_history.sql"))
        .unwrap();
    sqlite.execute_batch(r#"
      PRAGMA foreign_keys=ON;
      INSERT INTO meta VALUES('schema_version','34');
      INSERT INTO repositories(rowid,repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(17,'r1','/fixture/source','Source','t',1000,'t');
      INSERT INTO worktrees VALUES('w1','r1','/fixture/worktree','t','t');
      INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1','r1','w1','Preview','fixture','f','{"huge":123456789012345678901234567890,"decimal":1.200e10,"nested":{"z":2,"a":1}}','running','t',1000,'fixture','t');
      INSERT INTO generations VALUES('d1',7,'legacy-source',0,'/fixture/retained','legacy-fingerprint','t','current');
      INSERT INTO port_assignments VALUES(23456,'d1','web',0,'t','lease1');
      INSERT INTO domain_routes VALUES('fixture.invalid','d1','web',23456,0,'t','lease1');
      INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES('p1','r1',19,3,'Preserve tasks','Keep every row','improvement','open','t','fixture','t');
      INSERT INTO plan_events(event_id,repository_id,subject_kind,subject_id,event,actor,at) VALUES(42,'r1','task','p1','created','fixture','t');
      UPDATE sqlite_sequence SET seq=9000 WHERE name='plan_events';
      INSERT INTO decisions VALUES('n1','r1',12,'FIXTURE-EXACT','data','searchable nebula','Preserve history','technical',NULL,'t','fixture');
      INSERT INTO review_records VALUES('review1',1,'r1',1,2,'{"window":[1,2],"result":"kept"}','fixture',3);
      INSERT INTO metric_minutes VALUES('container','c1','cpu_percent','2026-08-01T12:00Z',0.5,1.25,2.0,4);
      INSERT INTO metric_minutes VALUES('repository','r1','cpu_percent','2026-11-05T12:00Z',0.0,1.0,2.0,3);
      INSERT INTO storage_artifacts VALUES('artifact1','r1','build',1,NULL,'{"state":"ready","nested":{"b":2,"a":1}}',2);
      INSERT INTO feature_tickets VALUES('ticket1','https://fixture.invalid','local','Fixture','Ticket title','Body',0,0,1,'request1','fingerprint','t','t');
      INSERT INTO ticket_attachments VALUES('attachment1','ticket1',NULL,0,'fixture.bin','application/octet-stream',4,'fixture-hash',x'0001feff');
      INSERT INTO ticket_history VALUES('ticket1',1,'created','fixture','Fixture','t');
      INSERT INTO agent_hub_agents(agent_id,handle,space,display_name,token_hash,lease_expires_at,created_at,last_seen_at) VALUES('a1','fixture-agent','private','Fixture','token-digest','future','t','t');
      INSERT INTO agent_hub_rooms VALUES('room1','private','Fixture room','a1','t',NULL);
      INSERT INTO agent_hub_room_members VALUES('room1','a1','t');
      INSERT INTO agent_hub_messages VALUES('message1','room1','a1','Preserved message','request1','t');
      INSERT INTO agent_hub_attachments VALUES('hubfile1','message1',0,'fixture.bin','application/octet-stream',4,'fixture-hash',x'0001feff');
      INSERT INTO agent_hub_deliveries VALUES('delivery1','message1','a1',NULL,0,'t','t',NULL);
      INSERT INTO agent_hub_outbox(delivery_id,callback_url,payload_json,signature,callback_nonce,signed_at,next_attempt_at,created_at) VALUES('delivery1','https://fixture.invalid/callback','{ "z" : 2, "a" : 1 }','opaque-signature','nonce','t','t','t');
    "#).unwrap();
    // A compact record accepted by schema 34 can exceed its former limit
    // solely because PostgreSQL's JSONB display inserts array separators.
    let mut near_limit = serde_json::json!({"items":vec![0;256],"text":""});
    let overhead = serde_json::to_string(&near_limit).unwrap().len();
    near_limit["text"] = serde_json::Value::String("x".repeat(8192 - overhead));
    let near_limit = serde_json::to_string(&near_limit).unwrap();
    assert_eq!(near_limit.len(), 8192);
    sqlite
        .execute(
            "INSERT INTO review_records VALUES('review-size-boundary',1,'r1',1,2,?1,'fixture',4)",
            [near_limit],
        )
        .unwrap();
    sqlite.pragma_update(None, "journal_mode", "WAL").unwrap();
}

#[test]
fn full_schema_copy_preserves_ids_history_json_numbers_blobs_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("frozen.sqlite3");
    source(&path);
    let before = inspect_sqlite(&path).unwrap();
    assert_eq!(before.tables.len(), 95);
    assert_eq!(
        before
            .tables
            .iter()
            .find(|t| t.table == "ticket_attachments")
            .unwrap()
            .blob_bytes,
        4
    );
    let cluster = FixtureCluster::start().unwrap();
    let config = cluster.create_database("cutover").unwrap();
    let database = Persistence::connect(&config).unwrap();
    let receipt =
        import_sqlite(&database, &path).unwrap_or_else(|e| panic!("full source import: {e:?}"));
    assert_eq!(receipt.checks.len(), 95);
    assert!(receipt.checks.iter().all(|c| c.matched));
    assert_eq!(before, receipt.source);
    assert!(receipt.source_retained);
    database
        .read::<_, Error, _>(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM generation_source_origins", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                0
            );
            assert_eq!(
                c.query_row(
                    "SELECT commit_hash FROM generations WHERE deployment_id='d1' AND number=7",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "legacy-source"
            );
            Ok(())
        })
        .unwrap();
    cluster.restart().unwrap();
    assert_eq!(verify_import(&database, &path).unwrap(), receipt);
    assert_eq!(inspect_sqlite(&path).unwrap(), before);
    database
        .read::<_, Error, _>(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM generation_source_origins", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                0
            );
            Ok(())
        })
        .unwrap();
    assert!(!path.with_file_name("frozen.sqlite3-wal").exists());
    assert!(!path.with_file_name("frozen.sqlite3-shm").exists());
    assert!(import_sqlite(&database, &path).is_err());
    let occupied =
        Persistence::connect(&cluster.create_database("unreviewed_internal").unwrap()).unwrap();
    occupied.migrate().unwrap();
    occupied.write::<_,Error,_>(|c|{c.execute_batch("CREATE TABLE dc2_unreviewed(value TEXT PRIMARY KEY);INSERT INTO dc2_unreviewed VALUES('valuable')")?;Ok(())}).unwrap();
    assert!(import_sqlite(&occupied, &path).is_err());
    assert_eq!(
        occupied
            .read::<_, Error, _>(|c| c
                .query_row("SELECT value FROM dc2_unreviewed", [], |r| r
                    .get::<_, String>(0)))
            .unwrap(),
        "valuable"
    );
    database.read::<_,Error,_>(|c| {
        let (compact,formatted):(u64,u64)=c.query_row("SELECT dc2_jsonb_compact_bytes(record_json),octet_length(record_json::text) FROM review_records WHERE record_id='review-size-boundary'",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!(compact,8192);assert!(formatted>8192);
        assert_eq!(c.query_row("SELECT rowid FROM repositories WHERE repository_id='r1'",[],|r|r.get::<_,i64>(0))?,17);
        assert_eq!(c.query_row("SELECT spec_json->>'huge' FROM deployments WHERE deployment_id='d1'",[],|r|r.get::<_,String>(0))?,"123456789012345678901234567890");
        assert_eq!(c.query_row("SELECT payload_json FROM agent_hub_outbox",[],|r|r.get::<_,String>(0))?,r#"{ "z" : 2, "a" : 1 }"#);
        assert_eq!(c.query_row("SELECT content FROM ticket_attachments",[],|r|r.get::<_,Vec<u8>>(0))?,[0,1,254,255]);
        assert_eq!(c.query_row("SELECT count(*) FROM decisions WHERE search_vector @@ plainto_tsquery('simple','nebula')",[],|r|r.get::<_,i64>(0))?,1);
        Ok(())
    }).unwrap();
    database.write::<_,Error,_>(|c|{
        let id:i64=c.query_row("INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,actor,at) VALUES('r1','task','p1','checked','fixture','t') RETURNING event_id",[],|r|r.get(0))?;
        assert!(id>9000);
        assert!(c.execute("UPDATE review_records SET actor='changed'",[]).is_err());
        assert!(c.execute("UPDATE port_assignments SET port=23457",[]).is_err());
        Ok(())
    }).unwrap();
    assert!(path.is_file());
}

#[test]
fn inspection_keeps_ordinary_fts_named_tables_and_copy_rolls_back() {
    let directory = tempfile::tempdir().unwrap();
    let toy = directory.path().join("ordinary.sqlite3");
    let connection = rusqlite::Connection::open(&toy).unwrap();
    connection.execute_batch("CREATE TABLE audit_fts_notes(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO audit_fts_notes VALUES(9,'ordinary'); CREATE VIRTUAL TABLE derived_fts USING fts5(value);").unwrap();
    drop(connection);
    let report = inspect_sqlite(&toy).unwrap();
    assert_eq!(report.tables.len(), 1);
    assert_eq!(report.tables[0].table, "audit_fts_notes");
    let path = directory.path().join("bad.sqlite3");
    source(&path);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE metric_minutes SET samples='not-an-integer'", [])
        .unwrap();
    drop(connection);
    let cluster = FixtureCluster::start().unwrap();
    let config = cluster.create_database("rollback").unwrap();
    let database = Persistence::connect(&config).unwrap();
    assert!(import_sqlite(&database, &path).is_err());
    assert_eq!(
        database
            .read::<_, Error, _>(
                |c| c.query_row("SELECT count(*) FROM repositories", [], |r| r
                    .get::<_, i64>(0))
            )
            .unwrap(),
        0
    );
    assert_eq!(
        database
            .read::<_, Error, _>(
                |c| c.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0))
            )
            .unwrap(),
        2
    );
    database
        .write::<_, Error, _>(|c| {
            c.execute(
                "INSERT INTO meta(key,value) VALUES('valuable','retained')",
                [],
            )
        })
        .unwrap();
    assert!(import_sqlite(&database, &path).is_err());
    assert_eq!(
        database
            .read::<_, Error, _>(|c| c.query_row(
                "SELECT value FROM meta WHERE key='valuable'",
                [],
                |r| r.get::<_, String>(0)
            ))
            .unwrap(),
        "retained"
    );
}

/// Full offline migration boundary, including real multi-million-row COPY and
/// post-restart verification under the explicit finite migration allowance.
#[test]
#[ignore = "explicit six-million-row offline SQLite migration acceptance"]
fn six_million_metric_rows_copy_and_verify_after_postgres_restart() {
    let cluster = FixtureCluster::start().unwrap();
    let base =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/postgres-fixtures");
    let directory = tempfile::Builder::new()
        .prefix("sqlite-scale-")
        .tempdir_in(base)
        .unwrap();
    let path = directory.path().join("frozen.sqlite3");
    source(&path);
    let sqlite = rusqlite::Connection::open(&path).unwrap();
    sqlite.execute_batch("WITH RECURSIVE rows(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM rows WHERE n<6000000) INSERT INTO metric_minutes SELECT 'repository',printf('scale-%08d',n),'cpu_percent','2026-11-01T12:00Z',1.0,1.5,2.0,3 FROM rows; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    drop(sqlite);
    let config = cluster.create_database("cutover_scale").unwrap();
    let database = Persistence::connect(&config).unwrap();
    let started = std::time::Instant::now();
    let receipt = import_sqlite(&database, &path).unwrap();
    let metric = receipt
        .checks
        .iter()
        .find(|row| row.source.table == "metric_minutes")
        .unwrap();
    assert_eq!(metric.source.rows, 6000002);
    assert_eq!(metric.target.rows, 6000002);
    assert!(metric.matched);
    cluster.restart().unwrap();
    assert_eq!(verify_import(&database, &path).unwrap(), receipt);
    assert!(path.is_file());
    eprintln!(
        "six-million-row offline COPY, restart and typed revalidation completed in {} ms; original source retained",
        started.elapsed().as_millis()
    );
    cluster.stop().unwrap();
}
