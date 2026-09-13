use super::*;
use devcoordinator2_api::work_context::{WorkAttribution, WorkContext, WorkSource};

#[test]
fn work_context_receipt_retention_preserves_whole_newest_bindings_within_existing_bytes() {
    let context = WorkContext {
        version: 1,
        native_project_id: "n".repeat(256),
        thread_id: "t".repeat(256),
        turn_id: Some("u".repeat(256)),
        operation_id: Some("o".repeat(256)),
        workstream_id: Some("w".repeat(256)),
        outcome_id: Some("d".repeat(256)),
        experiment_ref: Some("e".repeat(256)),
    };
    context.validate().unwrap();
    let work = Some(WorkAttribution {
        context: Some(context),
        source: WorkSource::Environment,
        diagnostic: None,
    });
    let mut rows = (0..1000)
        .map(|index| TestHistoryEntry {
            work: work.clone(),
            run_id: format!("t20260904T000000Z-{index:06x}"),
            test: "focused".into(),
            status: TestStatus::Passed,
            started_at: "2026-09-04T00:00:00Z".into(),
            finished_at: Some("2026-09-04T00:00:01Z".into()),
            duration_seconds: Some(1.0),
            exit_code: Some(0),
            termination_reason: None,
        })
        .collect::<Vec<_>>();
    let newest = rows.last().unwrap().clone();
    bound_receipt_rows(&mut rows).unwrap();
    assert!(rows.len() < 1000);
    assert_eq!(rows.last(), Some(&newest));
    assert_eq!(rows.last().unwrap().work, work);
    assert!(
        serde_json::to_vec(&HistoryDocument {
            schema: Schema2,
            runs: rows
        })
        .unwrap()
        .len()
            < JSON_LIMIT as usize
    );
    let mut oversized = vec!["x".repeat(JSON_LIMIT as usize + 1)];
    assert!(bound_receipt_rows(&mut oversized).is_err());
    assert_eq!(oversized.len(), 1);
}
