//! Finalize and validate changed visual-review decisions for the Node verifier.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::audit_findings::strict_json_object;
use crate::audit_ledger::{
    create_directory_all_nofollow, read_bytes_nofollow, write_new_bytes_nofollow,
};

pub const SCHEMA_VERSION: u64 = 1;
pub const KIND: &str = "formal-web-ui-manual-review";
pub const QUEUE_KIND: &str = "formal-web-ui-review-queue";

#[derive(Clone, Debug)]
struct RunEvidence {
    report: Map<String, Value>,
    queue: Map<String, Value>,
    cells: Vec<Map<String, Value>>,
    report_sha256: String,
    queue_sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

fn load_object(path: &Path, label: &str) -> Result<(Map<String, Value>, Vec<u8>), String> {
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "{label} must be a regular non-symlink JSON file: {}",
                path.display()
            )
        })?;
    let value = strict_json_object(&bytes, label)?;
    Ok((value, bytes))
}

fn load_decisions(path: Option<&Path>) -> Result<Vec<Map<String, Value>>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("decision input is missing: {}", path.display()))?;
    let mut value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decision input is not valid JSON: {error}"))?;
    if let Value::Object(object) = &mut value {
        value = object.remove("decisions").unwrap_or(Value::Null);
    }
    value
        .as_array()
        .ok_or_else(|| {
            "decision input must be a JSON list or an object with a decisions list".to_owned()
        })?
        .iter()
        .enumerate()
        .map(|(index, item)| {
            item.as_object()
                .cloned()
                .ok_or_else(|| format!("decision {index} must be an object"))
        })
        .collect()
}

fn screenshot_hashes(cell: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let key = cell
        .get("reviewCellKey")
        .and_then(Value::as_str)
        .unwrap_or("");
    let screenshots = cell
        .get("screenshots")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("review cell {key} is missing screenshots"))?;
    let mut result = Map::new();
    for (input, output) in [
        ("viewport", "viewportSha256"),
        ("fullPage", "fullPageSha256"),
    ] {
        let item = screenshots
            .get(input)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("review cell {key} is missing {input} screenshot evidence"))?;
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| format!("review cell {key} has no {input} screenshot path"))?;
        let bytes = read_bytes_nofollow(&path, None)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "review screenshot is unavailable or symlinked: {}",
                    path.display()
                )
            })?;
        let actual = sha256_hex(&bytes);
        if item.get("sha256").and_then(Value::as_str) != Some(&actual) {
            return Err(format!(
                "review screenshot hash mismatch: {}",
                path.display()
            ));
        }
        result.insert(output.to_owned(), Value::String(actual));
    }
    Ok(result)
}

fn load_run(report_path: &Path, queue_path: &Path) -> Result<RunEvidence, String> {
    let (report, report_bytes) = load_object(report_path, "report")?;
    let (queue, queue_bytes) = load_object(queue_path, "review queue")?;
    if report.get("schemaVersion") != Some(&json!(2)) {
        return Err("report schemaVersion must be 2".to_owned());
    }
    if report
        .get("formal")
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        != Some("passed")
    {
        return Err("manual review requires formal.result == passed".to_owned());
    }
    if report
        .get("formal")
        .and_then(|value| value.get("manifestSha256"))
        .and_then(Value::as_str)
        .is_none_or(|value| {
            value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("formal receipt requires its evidence manifest digest".to_owned());
    }
    if queue.get("schemaVersion") != Some(&json!(1))
        || queue.get("kind") != Some(&json!(QUEUE_KIND))
    {
        return Err("review queue has an unsupported schema or kind".to_owned());
    }
    if report.get("runId") != queue.get("runId") {
        return Err("report and review queue run ids differ".to_owned());
    }
    let review = report
        .get("review")
        .and_then(Value::as_object)
        .ok_or_else(|| "report is missing changed visual-review cells".to_owned())?;
    let cells = review
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| "report is missing changed visual-review cells".to_owned())?
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            cell.as_object()
                .cloned()
                .ok_or_else(|| format!("report review cell {index} must be an object"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let queue_sha256 = sha256_hex(&queue_bytes);
    if review.get("queueSha256").and_then(Value::as_str) != Some(&queue_sha256) {
        return Err("report does not bind the supplied review queue bytes".to_owned());
    }
    let keys = cells
        .iter()
        .map(|cell| {
            cell.get("reviewCellKey")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    "report review cells require unique non-empty reviewCellKey values".to_owned()
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
        return Err("report review cells require unique non-empty reviewCellKey values".to_owned());
    }
    let entries = queue
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "review queue entries must be a list".to_owned())?;
    let queued = entries
        .iter()
        .map(|entry| {
            entry
                .get("reviewCellKey")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    "review queue entries require unique non-empty reviewCellKey values".to_owned()
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let queued_set = queued.iter().cloned().collect::<BTreeSet<_>>();
    if queued_set.len() != queued.len() {
        return Err(
            "review queue entries require unique non-empty reviewCellKey values".to_owned(),
        );
    }
    let expected = cells
        .iter()
        .filter(|cell| cell.get("status") == Some(&json!("review-required")))
        .filter_map(|cell| cell.get("reviewCellKey"))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if queued_set != expected {
        return Err(
            "review queue entries do not exactly match report review-required cells".to_owned(),
        );
    }
    Ok(RunEvidence {
        report,
        queue,
        cells,
        report_sha256: sha256_hex(&report_bytes),
        queue_sha256,
    })
}

fn normalize_decisions(
    raw: &[Map<String, Value>],
    required: &BTreeSet<String>,
) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut decisions = BTreeMap::new();
    for (index, item) in raw.iter().enumerate() {
        let key = item
            .get("reviewCellKey")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("decision {index} requires reviewCellKey"))?;
        let decision = item
            .get("decision")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "pass" | "gap" | "blocked"))
            .ok_or_else(|| format!("decision for {key} must be pass, gap, or blocked"))?;
        let note = item
            .get("note")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_default();
        if decision != "pass" && note.trim().is_empty() {
            return Err(format!("{decision} decision for {key} requires a note"));
        }
        if decisions
            .insert(
                key.to_owned(),
                (decision.to_owned(), note.trim().to_owned()),
            )
            .is_some()
        {
            return Err(format!("duplicate decision for {key}"));
        }
    }
    let actual = decisions.keys().cloned().collect::<BTreeSet<_>>();
    if &actual != required {
        return Err(format!(
            "decision keys must exactly match the review queue; missing={:?}; extra={:?}",
            required.difference(&actual).collect::<Vec<_>>(),
            actual.difference(required).collect::<Vec<_>>()
        ));
    }
    Ok(decisions)
}

pub fn finalize(
    report_path: &Path,
    queue_path: &Path,
    decisions_path: Option<&Path>,
    reviewed_at: &str,
) -> Result<Value, String> {
    let run = load_run(report_path, queue_path)?;
    let queued = run
        .queue
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("reviewCellKey"))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let supplied = normalize_decisions(&load_decisions(decisions_path)?, &queued)?;
    let mut decisions: Vec<Value> = Vec::new();
    for cell in &run.cells {
        let key = cell["reviewCellKey"]
            .as_str()
            .expect("validated review key");
        let status = cell.get("status").and_then(Value::as_str).unwrap_or("");
        let (decision, note, basis) = if status == "review-required" {
            let selected = supplied.get(key).expect("exact supplied decision set");
            (
                selected.0.clone(),
                selected.1.clone(),
                "agent-reviewed-current-screenshots",
            )
        } else if status.starts_with("carried-") {
            let decision = cell
                .get("decision")
                .and_then(Value::as_str)
                .filter(|value| matches!(*value, "pass" | "gap" | "blocked"))
                .ok_or_else(|| format!("carried review cell {key} has no valid prior decision"))?;
            (
                decision.to_owned(),
                cell.get("note")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                "carried-unchanged-ui-inputs-and-intent",
            )
        } else {
            return Err(format!("review cell {key} has unsupported status {status}"));
        };
        decisions.push(json!({
            "reviewCellKey":key,
            "targetName":cell.get("targetName").cloned().unwrap_or(Value::Null),
            "requestedPath":cell.get("requestedPath").cloned().unwrap_or(Value::Null),
            "stateName":cell.get("stateName").cloned().unwrap_or(Value::Null),
            "theme":cell.get("theme").cloned().unwrap_or(Value::Null),
            "viewport":cell.get("viewport").cloned().unwrap_or(Value::Null),
            "decision":decision,"note":note,"basis":basis,
            "sourceFingerprint":cell.get("sourceFingerprint").cloned().unwrap_or(Value::Null),
            "intentFingerprint":cell.get("intentFingerprint").cloned().unwrap_or(Value::Null),
            "screenshots":Value::Object(screenshot_hashes(cell)?),
        }));
    }
    Ok(json!({
        "schemaVersion":SCHEMA_VERSION,"kind":KIND,
        "reviewedRunId":run.report["runId"],"reviewedAt":reviewed_at,
        "reportSha256":run.report_sha256,"reviewQueueSha256":run.queue_sha256,
        "manual": {
            "result": if decisions.iter().all(|item| item["decision"] == "pass") { "passed" } else { "failed" },
            "formalRunId":run.report["runId"],
            "formalManifestSha256":run.report["formal"]["manifestSha256"],
            "reviewer":"agent", "reviewedAt":reviewed_at, "cells":decisions,
        },
        "decisions":decisions,
    }))
}

pub fn validate(
    review_path: &Path,
    report_path: &Path,
    queue_path: &Path,
) -> Result<Value, String> {
    let run = load_run(report_path, queue_path)?;
    let (actual, _) = load_object(review_path, "manual review")?;
    if actual.get("schemaVersion") != Some(&json!(SCHEMA_VERSION))
        || actual.get("kind") != Some(&json!(KIND))
    {
        return Err("manual review has an unsupported schema or kind".to_owned());
    }
    for (field, expected) in [
        ("reviewedRunId", run.report["runId"].clone()),
        ("reportSha256", Value::String(run.report_sha256)),
        ("reviewQueueSha256", Value::String(run.queue_sha256)),
    ] {
        if actual.get(field) != Some(&expected) {
            return Err(format!(
                "manual review {field} does not match the supplied run evidence"
            ));
        }
    }
    let decisions = actual
        .get("decisions")
        .and_then(Value::as_array)
        .ok_or_else(|| "manual review decisions must be a list".to_owned())?;
    let expected = run
        .cells
        .iter()
        .map(|cell| {
            let key = cell["reviewCellKey"].as_str().expect("validated key").to_owned();
            Ok((
                key,
                json!({
                    "sourceFingerprint":cell.get("sourceFingerprint").cloned().unwrap_or(Value::Null),
                    "intentFingerprint":cell.get("intentFingerprint").cloned().unwrap_or(Value::Null),
                    "screenshots":Value::Object(screenshot_hashes(cell)?),
                }),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut seen = BTreeSet::new();
    for (index, decision) in decisions.iter().enumerate() {
        let item = decision
            .as_object()
            .ok_or_else(|| format!("manual review decision {index} references an unknown cell"))?;
        let key = item
            .get("reviewCellKey")
            .and_then(Value::as_str)
            .filter(|key| expected.contains_key(*key))
            .ok_or_else(|| format!("manual review decision {index} references an unknown cell"))?;
        if !seen.insert(key.to_owned()) {
            return Err(format!("manual review repeats cell {key}"));
        }
        let decision = item.get("decision").and_then(Value::as_str);
        if !matches!(decision, Some("pass" | "gap" | "blocked")) {
            return Err(format!("manual review decision for {key} is invalid"));
        }
        if decision != Some("pass")
            && item
                .get("note")
                .and_then(Value::as_str)
                .is_none_or(|note| note.trim().is_empty())
        {
            return Err(format!(
                "manual review {} for {key} requires a note",
                decision.unwrap_or("invalid")
            ));
        }
        for field in ["sourceFingerprint", "intentFingerprint", "screenshots"] {
            if item.get(field) != expected[key].get(field) {
                return Err(format!(
                    "manual review decision for {key} does not bind current {field}"
                ));
            }
        }
    }
    let missing = expected
        .keys()
        .filter(|key| !seen.contains(*key))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("manual review omits current cells: {missing:?}"));
    }
    let result = if decisions.iter().all(|item| item["decision"] == "pass") {
        "passed"
    } else {
        "failed"
    };
    if actual
        .get("manual")
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        != Some(result)
        || actual
            .get("manual")
            .and_then(|value| value.get("formalRunId"))
            != run.report.get("runId")
        || actual
            .get("manual")
            .and_then(|value| value.get("formalManifestSha256"))
            != run
                .report
                .get("formal")
                .and_then(|value| value.get("manifestSha256"))
        || actual.get("manual").and_then(|value| value.get("cells")) != actual.get("decisions")
    {
        return Err("manual receipt does not bind the complete reviewed cell set".to_owned());
    }
    Ok(Value::Object(actual))
}

pub fn write_new_review(path: &Path, review: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_directory_all_nofollow(parent, 0o700).map_err(|error| error.to_string())?;
    }
    let mut bytes = serde_json::to_vec_pretty(review).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    write_new_bytes_nofollow(path, &bytes, 0o600).map_err(|error| error.to_string())
}

pub fn summary(review: &Value) -> Result<Value, String> {
    let decisions = review
        .get("decisions")
        .and_then(Value::as_array)
        .ok_or_else(|| "manual review decisions must be a list".to_owned())?;
    let blocking = decisions
        .iter()
        .filter(|item| item.get("decision").and_then(Value::as_str) != Some("pass"))
        .count();
    Ok(json!({
        "ok":blocking == 0,
        "reviewedRunId":review.get("reviewedRunId").cloned().unwrap_or(Value::Null),
        "decisionCount":decisions.len(),"blockingCount":blocking,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let viewport = directory.path().join("viewport.png");
        let full = directory.path().join("full.png");
        std::fs::write(&viewport, b"viewport").unwrap();
        std::fs::write(&full, b"full-page").unwrap();
        let queue = json!({
            "schemaVersion":1,"kind":QUEUE_KIND,"runId":"run-1",
            "entries":[{"reviewCellKey":"cell-1"}],
        });
        let queue_path = directory.path().join("review-queue.json");
        std::fs::write(&queue_path, serde_json::to_vec(&queue).unwrap()).unwrap();
        let report = json!({
            "schemaVersion":2,"runId":"run-1","formal":{"result":"passed","manifestSha256":"c".repeat(64)},"review":{
                "queueSha256":sha256_hex(&std::fs::read(&queue_path).unwrap()),
                "cells":[{
                    "reviewCellKey":"cell-1","status":"review-required",
                    "targetName":"fixture","requestedPath":"/","stateName":"default",
                    "viewport":{"name":"desktop","width":1280,"height":800},
                    "sourceFingerprint":"a".repeat(64),"intentFingerprint":"b".repeat(64),
                    "screenshots":{
                        "viewport":{"path":viewport,"sha256":sha256_hex(b"viewport")},
                        "fullPage":{"path":full,"sha256":sha256_hex(b"full-page")}
                    }
                }]
            }
        });
        let report_path = directory.path().join("report.json");
        std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        let decisions_path = directory.path().join("decisions.json");
        std::fs::write(
            &decisions_path,
            br#"[{"reviewCellKey":"cell-1","decision":"pass","note":""}]"#,
        )
        .unwrap();
        (directory, report_path, queue_path, decisions_path, viewport)
    }

    #[test]
    fn finalizes_validates_and_summarizes_exact_current_decisions() {
        let (directory, report, queue, decisions, _) = fixture();
        let review = finalize(&report, &queue, Some(&decisions), "2026-09-04T00:00:00Z").unwrap();
        assert_eq!(summary(&review).unwrap()["ok"], true);
        assert_eq!(review["manual"]["result"], "passed");
        let output = directory.path().join("manual-review.json");
        write_new_review(&output, &review).unwrap();
        assert_eq!(validate(&output, &report, &queue).unwrap(), review);
        assert!(write_new_review(&output, &review).is_err());
        let mut invalid: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        for result in ["failed", "blocked", "incomplete"] {
            invalid["formal"]["result"] = json!(result);
            std::fs::write(&report, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(
                finalize(&report, &queue, Some(&decisions), "now")
                    .unwrap_err()
                    .contains("formal.result")
            );
        }
    }

    #[test]
    fn gaps_require_notes_and_screenshot_tampering_fails_closed() {
        let (_directory, report, queue, decisions, viewport) = fixture();
        std::fs::write(
            &decisions,
            br#"[{"reviewCellKey":"cell-1","decision":"gap","note":""}]"#,
        )
        .unwrap();
        assert!(finalize(&report, &queue, Some(&decisions), "now").is_err());
        std::fs::write(
            &decisions,
            br#"[{"reviewCellKey":"cell-1","decision":"pass","note":""}]"#,
        )
        .unwrap();
        std::fs::write(&viewport, b"tampered").unwrap();
        assert!(
            finalize(&report, &queue, Some(&decisions), "now")
                .unwrap_err()
                .contains("hash mismatch")
        );
    }

    #[test]
    fn carried_cells_need_no_new_decision_but_keep_prior_outcome() {
        let (directory, report_path, queue_path, _decisions, _) = fixture();
        let mut report: Value =
            serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
        report["review"]["cells"][0]["status"] = json!("carried-gap");
        report["review"]["cells"][0]["decision"] = json!("gap");
        report["review"]["cells"][0]["note"] = json!("known unresolved displacement");
        let queue = json!({"schemaVersion":1,"kind":QUEUE_KIND,"runId":"run-1","entries":[]});
        std::fs::write(&queue_path, serde_json::to_vec(&queue).unwrap()).unwrap();
        report["review"]["queueSha256"] = json!(sha256_hex(&std::fs::read(&queue_path).unwrap()));
        std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        let review = finalize(&report_path, &queue_path, None, "now").unwrap();
        assert_eq!(review["decisions"][0]["decision"], "gap");
        assert_eq!(
            review["decisions"][0]["basis"],
            "carried-unchanged-ui-inputs-and-intent"
        );
        assert_eq!(summary(&review).unwrap()["blockingCount"], 1);
        drop(directory);
    }
}
