//! Extend the existing usage fixture through a real localUsage websocket peer.
use super::*;
use crate::platform::FixedClock;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use time::OffsetDateTime;

fn response(repository: Option<&str>, start: u64, end: u64) -> Value {
    let key = "b".repeat(64);
    json!({"generatedAt":end,"report":{
        "schemaVersion":1,"kind":"usageSummary","databaseSchemaVersion":99,"taxonomyVersion":1,
        "scope":{"type":if repository.is_some(){"repository"}else{"all"},"id":repository},
        "timeRange":{"startMs":start,"endMs":end},"coverage":{"state":"complete","hasGaps":false},
        "counts":{"operations":2,"modelRequests":1,"tools":1},
        "providerTokens":[
            {"category":"total_tokens","repositoryBucket":key,"measurementProvenance":"provider_reported","measuredTokens":120,"exactTokens":120,"unknownObservations":0,"observationCount":1},
            {"category":"input_tokens","repositoryBucket":key,"measurementProvenance":"provider_reported","measuredTokens":100,"exactTokens":100,"unknownObservations":0,"observationCount":1},
            {"category":"input_tokens_details.cached_tokens","repositoryBucket":key,"measurementProvenance":"provider_reported","measuredTokens":80,"exactTokens":80,"unknownObservations":0,"observationCount":1},
            {"category":"output_tokens","repositoryBucket":key,"measurementProvenance":"provider_reported","measuredTokens":20,"exactTokens":20,"unknownObservations":0,"observationCount":1},
            {"category":"output_tokens_details.reasoning_tokens","repositoryBucket":key,"measurementProvenance":"provider_reported","measuredTokens":5,"exactTokens":5,"unknownObservations":0,"observationCount":1}
        ],
        "providerTokensByActivity":[{"phase":"implementation","activity":"coding","attributionProvenance":"agent_declared","measuredTokens":120,"exactTokens":120,"unknownObservations":0}],
        "account":"private-account-must-not-escape","extraPrivate":"private-payload-must-not-escape"
    }})
}
fn peer(
    path: &Path,
    home: &Path,
    count: usize,
    mutate: fn(&mut Value),
) -> (std::thread::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = UnixListener::bind(path).unwrap();
    let home = home.to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..count {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let init: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(init["method"], "initialize");
            assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
            socket
                .send(Message::Text(
                    json!({"id":1,"result":{"codexHome":home}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            let initialized: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            let request: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "localUsage/summary");
            observed.fetch_add(1, Ordering::SeqCst);
            let mut result = response(
                request["params"]["repositoryKey"].as_str(),
                request["params"]["fromAt"].as_u64().unwrap(),
                request["params"]["toAt"].as_u64().unwrap(),
            );
            mutate(&mut result);
            let _ = socket.send(Message::Text(
                json!({"id":2,"result":result}).to_string().into(),
            ));
        }
    });
    (worker, calls)
}

#[test]
fn source_api_serves_whole_collection_once_and_keeps_missing_counts_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("collector");
    let (_, now) = super::super::tests::source_database(&home, 5);
    let mut config = super::super::tests::config(dir.path(), home.clone());
    let socket = dir.path().join("api.sock");
    config.codex_usage_sources[0].api_socket = Some(socket.clone());
    let uid = config.codex_usage_sources[0].uid;
    let db = Database::open(dir.path().join("authority.sqlite3")).unwrap();
    let key = "b".repeat(64);
    db.transaction(move |tx|{
        for id in ["project-alpha","project-beta"] {
            tx.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,?2,?1,'t',1,'t')",rusqlite::params![id,format!("/{id}")])?;
        }
        tx.execute("INSERT INTO codex_usage_repository_links VALUES(?1,'project-alpha',?2,5,1,'t')",rusqlite::params![uid,key])?;Ok(())
    }).unwrap();
    let usage = CodexUsage::with_probe(
        config,
        db,
        Arc::new(FixedClock(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000).unwrap(),
        )),
        Arc::new(HostRepositoryProbe),
    );
    let repositories = [
        RepositoryRecord {
            repository_id: "project-alpha".into(),
            display_name: "Alpha".into(),
            root_path: "/alpha".into(),
        },
        RepositoryRecord {
            repository_id: "project-beta".into(),
            display_name: "Beta".into(),
            root_path: "/beta".into(),
        },
    ];
    let (server, calls) = peer(&socket, &home, 1, |_| {});
    let began = Instant::now();
    let first = usage
        .repositories(&repositories, UsageRange::Hours24)
        .unwrap();
    assert!(began.elapsed() < Duration::from_secs(1));
    let warm = usage
        .repositories(&repositories, UsageRange::Hours24)
        .unwrap();
    assert_eq!(first, warm);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.repositories[0].total_tokens, Some(120));
    assert_eq!(first.repositories[0].model_requests, None);
    assert_eq!(first.repositories[0].tool_calls, None);
    assert_eq!(first.repositories[0].execution_wall_ms, None);
    assert_eq!(first.repositories[1].total_tokens, None);
    let wire = serde_json::to_string(&first).unwrap();
    for private in [
        "private-account",
        "private-payload",
        home.to_str().unwrap(),
        &"b".repeat(64),
    ] {
        assert!(!wire.contains(private));
    }
    // A new producer database schema is acceptable only through its stable API;
    // the fallback's explicit SQLite schema gate is unchanged.
    assert_eq!(first.repositories[0].coverage.database_schemas, vec![99]);
    server.join().unwrap();
}

#[test]
fn api_exact_windows_validate_and_unsupported_or_failed_sources_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("collector");
    let (_, now) = super::super::tests::source_database(&home, 5);
    let mut config = super::super::tests::config(dir.path(), home.clone());
    let socket = dir.path().join("api.sock");
    config.codex_usage_sources[0].api_socket = Some(socket.clone());
    let source = config.codex_usage_sources[0].clone();
    let db = Database::open(dir.path().join("authority.sqlite3")).unwrap();
    let usage = CodexUsage::new(config, db);
    let (server, _) = peer(&socket, &home, 1, |v| {
        v["report"]["timeRange"]["endMs"] = json!(0)
    });
    let report = usage
        .read_source(
            &source,
            &"b".repeat(64),
            now - 86_400_000,
            now,
            86_400_000,
            1,
            None,
            Projection::PerformanceFast,
            &[],
        )
        .unwrap();
    assert_eq!(report.tokens["total_tokens"], 100);
    server.join().unwrap();
    // Failed source cooldown is bounded and does not repeat the socket request.
    let source_report = usage
        .read_source(
            &source,
            &"b".repeat(64),
            now - 86_400_000,
            now,
            86_400_000,
            1,
            None,
            Projection::PerformanceFast,
            &[],
        )
        .unwrap();
    assert_eq!(source_report.tokens["total_tokens"], 100);
}

#[test]
fn api_contract_preserves_subsets_cost_basis_and_optional_metadata() {
    let mut value = response(Some(&"b".repeat(64)), 10, 20);
    let summary: Summary = serde_json::from_value(value.clone()).unwrap();
    validate(&summary, Some(&"b".repeat(64)), 10, 20).unwrap();
    let report = summary.source_report(None, 1);
    assert_eq!(report.tokens["total_tokens"], 120);
    assert_eq!(report.tokens["input_tokens_details.cached_tokens"], 80);
    assert_eq!(report.tokens["output_tokens_details.reasoning_tokens"], 5);
    assert!(validate(&summary, Some(&"b".repeat(64)), 10, 21).is_err());
    value["snapshot"] =
        json!({"source_watermark":"42","generated_at":19,"freshness":"stale","refresh_id":null});
    let mut cost = UsageCost {
        status: "complete".into(),
        basis: "api_equivalent".into(),
        currency: "USD".into(),
        processing_tier: "standard".into(),
        estimated_usd_micros: Some(20),
        input_usd_micros: Some(4),
        cached_input_usd_micros: Some(2),
        cache_write_usd_micros: Some(0),
        output_usd_micros: Some(14),
        rate_card_refs: vec!["fixture@1".into()],
        ..Default::default()
    };
    value["cost"] = serde_json::to_value(&cost).unwrap();
    let priced: Summary = serde_json::from_value(value.clone()).unwrap();
    validate(&priced, Some(&"b".repeat(64)), 10, 20).unwrap();
    assert_eq!(
        priced
            .source_report(None, 1)
            .supplied_cost
            .unwrap()
            .estimated_usd_micros,
        Some(20)
    );
    cost.basis = "subscription".into();
    value["cost"] = serde_json::to_value(cost).unwrap();
    assert!(
        validate(
            &serde_json::from_value(value).unwrap(),
            Some(&"b".repeat(64)),
            10,
            20
        )
        .is_err()
    );
}

#[test]
fn slow_api_is_cancelled_within_the_shared_read_budget() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("collector");
    let socket = dir.path().join("api.sock");
    let source = CodexUsageSource {
        uid: rustix::process::getuid().as_raw(),
        codex_home: home.clone(),
        executable: "/unused".into(),
        api_socket: Some(socket.clone()),
    };
    let (server, calls) = peer(&socket, &home, 1, |_| {
        std::thread::sleep(Duration::from_millis(300))
    });
    let began = Instant::now();
    let result = CollectorApi::default().summary(
        &source,
        None,
        10,
        20,
        Instant::now() + Duration::from_millis(100),
    );
    assert!(result.is_err());
    assert!(began.elapsed() < Duration::from_millis(650));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.join().unwrap();
}
