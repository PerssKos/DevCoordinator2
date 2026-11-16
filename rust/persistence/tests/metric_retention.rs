use devcoordinator2_persistence::{
    Error, Persistence,
    fixture::FixtureCluster,
    maintenance::{ensure_metric_partitions, retain_metrics},
};
use std::time::Instant;
use time::macros::datetime;

#[test]
fn retention_is_bounded_and_partition_drop_is_separate_from_row_receipts() {
    let cluster = FixtureCluster::start().unwrap();
    let config = cluster.create_database("metrics").unwrap();
    let database = Persistence::connect(&config).unwrap();
    database.migrate().unwrap();
    let now = datetime!(2026-11-05 12:00 UTC);
    let provision_deadline = Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let report = match database
            .maintenance_transaction::<_, Error, _>(|tx| ensure_metric_partitions(tx, now))
        {
            Ok(report) => report,
            Err(error) if error.is_timeout() && Instant::now() < provision_deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            Err(error) => panic!("bounded partition provisioning did not recover: {error:?}"),
        };
        assert!(report.created <= 1);
        if report.complete {
            break;
        }
    }
    database.maintenance_transaction::<_,Error,_>(|tx|{
        tx.execute_batch("INSERT INTO metric_minutes(subject_kind,subject_id,metric,minute_utc,min_value,avg_value,max_value,samples) SELECT 'container','expired-'||n,'cpu_percent','2026-10-20T12:00Z',1,1,1,1 FROM generate_series(1,600) n;
          INSERT INTO metric_minutes(subject_kind,subject_id,metric,minute_utc,min_value,avg_value,max_value,samples) VALUES('container','boundary-seven','cpu_percent','2026-10-29T12:00Z',1,1,1,1),('repository','boundary-thirty','cpu_percent','2026-10-06T12:00Z',1,1,1,1),('repository','retained','cpu_percent','2026-10-20T12:00Z',1,1,1,1),('host','old-default','cpu_percent','2026-08-01T12:00Z',1,1,1,1)")?;Ok(())
    }).unwrap();
    let mut total = 0;
    let mut batches = 0;
    loop {
        let report = database
            .maintenance_transaction::<_, Error, _>(|tx| retain_metrics(tx, now, 256))
            .unwrap();
        assert!(report.rows_deleted <= 256);
        assert!(report.bytes_selected <= 256 * 1024);
        total += report.rows_deleted;
        batches += 1;
        if report.complete {
            break;
        }
        assert!(batches < 8);
    }
    assert_eq!(total, 601);
    assert!(batches >= 3);
    assert_eq!(
        database
            .read::<_, Error, _>(|c| c
                .query_row("SELECT count(*) FROM metric_minutes", [], |r| r
                    .get::<_, i64>(0)))
            .unwrap(),
        3
    );
    let later = now + time::Duration::days(40);
    let reader = database
        .acquire(devcoordinator2_persistence::Lane::Read)
        .unwrap();
    let held = reader.unchecked_transaction().unwrap();
    held.query_row(
        "SELECT count(*) FROM metric_minutes WHERE minute_utc='2026-10-06T12:00Z'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap();
    let worker_database = database.clone();
    let (started, ready) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_database.maintenance_transaction::<_, Error, _>(|tx| {
            started.send(()).unwrap();
            retain_metrics(tx, later, 256)
        })
    });
    ready.recv().unwrap();
    let read_start = Instant::now();
    let cheap = database.read::<_, Error, _>(|c| {
        c.query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
    });
    let read_time = read_start.elapsed();
    let blocked = worker.join().unwrap();
    held.rollback().unwrap();
    assert_eq!(cheap.unwrap(), "35");
    assert!(read_time < std::time::Duration::from_millis(250));
    assert!(blocked.as_ref().err().is_some_and(Error::is_timeout));
    let report = database
        .maintenance_transaction::<_, Error, _>(|tx| retain_metrics(tx, later, 256))
        .unwrap();
    assert_eq!(report.partitions_dropped, 1);
    cluster.stop().unwrap();
}

/// A real retained-data distribution catches regressions that tiny fixtures
/// cannot: six million low-cardinality rows must not be rescanned per batch.
#[test]
#[ignore = "explicit large PostgreSQL fixture; run once for acceptance"]
fn six_million_retained_rows_do_not_make_expiry_scan_the_retained_history() {
    let cluster = FixtureCluster::start().unwrap();
    let mut config = cluster.create_database("metrics_scale").unwrap();
    config.statement_timeout = std::time::Duration::from_secs(180);
    let database = Persistence::connect(&config).unwrap();
    database.migrate().unwrap();
    database.write::<_,Error,_>(|c|{
        c.execute_batch("ALTER TABLE metric_minutes_default SET(autovacuum_enabled=false); INSERT INTO metric_minutes(subject_kind,subject_id,metric,minute_utc,min_value,avg_value,max_value,samples) SELECT 'repository','retained-'||n,'cpu_percent','2026-10-15T12:00Z',1,1,1,1 FROM generate_series(1,6000000) n;
          INSERT INTO metric_minutes(subject_kind,subject_id,metric,minute_utc,min_value,avg_value,max_value,samples) SELECT 'container','expired-'||n,'cpu_percent','2026-10-20T12:00Z',1,1,1,1 FROM generate_series(1,600) n;")?;Ok(())
    }).unwrap();
    let mut bounded = config.clone();
    bounded.statement_timeout = std::time::Duration::from_millis(750);
    let bounded = Persistence::connect(&bounded).unwrap();
    let start = Instant::now();
    let mut total = 0;
    let mut batches = 0;
    loop {
        let report = bounded
            .maintenance_transaction::<_, Error, _>(|tx| {
                retain_metrics(tx, datetime!(2026-11-05 12:00 UTC), 256)
            })
            .unwrap();
        total += report.rows_deleted;
        batches += 1;
        if report.complete {
            break;
        }
        assert!(batches < 8);
    }
    assert_eq!(total, 600);
    assert_eq!(batches, 3);
    assert!(start.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(
        bounded
            .read::<_, Error, _>(|c| c.query_row(
                "SELECT count(*) FROM metric_minutes WHERE subject_kind='container'",
                [],
                |r| r.get::<_, i64>(0)
            ))
            .unwrap(),
        0
    );
    cluster.stop().unwrap();
}
