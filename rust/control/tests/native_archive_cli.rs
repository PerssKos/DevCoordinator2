//! Actual CLI report publication and failure status over owned PostgreSQL state.
use devcoordinator2_persistence::{Error, Persistence, backup, fixture::FixtureDatabase};
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::process::Command;

#[test]
fn native_archive_cli_retains_private_mismatches_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = FixtureDatabase::for_path(&dir.path().join("source")).unwrap();
    let database = Persistence::connect(&fixture.config).unwrap();
    database.transaction::<_,Error,_>(|tx|tx.execute_batch("CREATE TABLE archive_cli_fixture(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,secret text NOT NULL); INSERT INTO archive_cli_fixture(secret) VALUES('private-original-row')")).unwrap();
    let archive = dir.path().join("authority.pgdump");
    backup::dump(&fixture.config, &archive).unwrap();
    let config = dir.path().join("postgres.json");
    let private = serde_json::json!({"schema":1,"connection":format!("{} password='private-credential-sentinel'",fixture.config.url),"data_directory":fixture.config.data_directory});
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)
        .unwrap()
        .write_all(serde_json::to_string(&private).unwrap().as_bytes())
        .unwrap();
    let invoke = |name: &str| {
        let report = dir.path().join(name);
        let output = Command::new(env!("CARGO_BIN_EXE_devcoordinator2-migrate-authority"))
            .args(["--mode", "verify-backup", "--database-config"])
            .arg(&config)
            .arg("--destination")
            .arg(&archive)
            .arg("--report")
            .arg(&report)
            .output()
            .unwrap();
        let bytes = fs::read(&report).expect("private report is retained on mismatch");
        assert_eq!(
            fs::metadata(&report).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let exposed = [bytes.clone(), output.stdout.clone(), output.stderr.clone()].concat();
        for private in [
            "private-original-row",
            "private-changed-row",
            "private-credential-sentinel",
        ] {
            assert!(!String::from_utf8_lossy(&exposed).contains(private));
        }
        (
            output,
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        )
    };
    let (success, report) = invoke("matching.json");
    assert!(success.status.success());
    assert_eq!(report["verified"], true);
    assert!(
        report.get("comparison").is_none(),
        "successful report shape stays compatible"
    );
    database
        .write::<_, Error, _>(|c| {
            c.execute(
                "UPDATE archive_cli_fixture SET secret='private-changed-row'",
                [],
            )
        })
        .unwrap();
    let (failure, report) = invoke("table-mismatch.json");
    assert_eq!(failure.status.code(), Some(1));
    assert_eq!(report["verified"], false);
    assert_eq!(report["matched_frozen_authority"], false);
    assert_eq!(report["isolated_restore_removed"], true);
    assert_eq!(report["comparison"]["changed_table_count"], 1);
    assert_eq!(
        report["comparison"]["tables"][0]["table"],
        "archive_cli_fixture"
    );
    assert_eq!(report["comparison"]["tables"][0]["rows_equal"], true);
    assert_eq!(report["comparison"]["tables"][0]["hash_equal"], false);
    database.write::<_,Error,_>(|c|c.execute_batch("UPDATE archive_cli_fixture SET secret='private-original-row'; SELECT nextval('archive_cli_fixture_id_seq')")).unwrap();
    let (failure, report) = invoke("sequence-mismatch.json");
    assert_eq!(failure.status.code(), Some(1));
    assert_eq!(report["verified"], false);
    assert_eq!(report["comparison"]["changed_table_count"], 0);
    assert_eq!(report["comparison"]["changed_sequence_count"], 1);
    assert_eq!(report["comparison"]["sequences"][0]["value_equal"], false);
    database.transaction::<_,Error,_>(|tx| {
        for index in 0..40 {tx.execute_batch(&format!("CREATE TABLE comparison_bound_{index}(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY)"))?;}
        Ok(())
    }).unwrap();
    let (failure, report) = invoke("bounded-mismatch.json");
    assert_eq!(failure.status.code(), Some(1));
    assert_eq!(report["verified"], false);
    assert_eq!(report["comparison"]["changed_table_count"], 40);
    assert_eq!(report["comparison"]["tables"].as_array().unwrap().len(), 32);
    assert_eq!(
        report["comparison"]["sequences"].as_array().unwrap().len(),
        32
    );
    assert_eq!(report["comparison"]["truncated"], true);
}
