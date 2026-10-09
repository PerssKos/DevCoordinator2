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
    formal: Map<String, Value>,
    formal_manifest_sha256: String,
}

fn hash_value(value: Option<&Value>, label: &str) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .map(str::to_owned)
        .ok_or_else(|| format!("{label} must be an observed SHA-256 identity"))
}

fn verify_manifest_file(
    manifest: &Map<String, Value>,
    file: &Path,
    kind: &str,
) -> Result<(), String> {
    let bytes = read_bytes_nofollow(file, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("formal {kind} artifact is unavailable"))?;
    let absolute = std::fs::canonicalize(file).map_err(|error| error.to_string())?;
    let identity = sha256_hex(absolute.to_string_lossy().as_bytes());
    let rows = manifest
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "formal artifact manifest has no files".to_owned())?;
    let matching = rows
        .iter()
        .filter(|row| row["kind"] == kind && row["identity"] == identity)
        .collect::<Vec<_>>();
    if matching.len() != 1
        || matching[0]["sha256"] != sha256_hex(&bytes)
        || matching[0]["bytes"] != bytes.len()
    {
        return Err(format!(
            "formal {kind} artifact hash mismatch with the exact retained manifest"
        ));
    }
    Ok(())
}

fn passing_formal(
    report: &Map<String, Value>,
    report_path: &Path,
    queue_path: &Path,
) -> Result<(Map<String, Value>, String), String> {
    let formal = report
        .get("formal")
        .and_then(Value::as_object)
        .ok_or_else(|| "manual review requires a passed formal receipt".to_owned())?;
    if formal.get("result") != Some(&json!("passed"))
        || formal.get("freshComplete") != Some(&json!(true))
        || formal.get("exitCode") != Some(&json!(0))
    {
        return Err("manual review requires fresh complete formal.result == passed".to_owned());
    }
    if formal.get("runId") != report.get("runId") {
        return Err("formal run identity does not match the report".to_owned());
    }
    let coverage = formal
        .get("coverage")
        .and_then(Value::as_object)
        .ok_or("formal coverage is missing")?;
    let required = coverage
        .get("requiredCells")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if required == 0
        || coverage.get("checkedCells").and_then(Value::as_u64) != Some(required)
        || coverage.get("readinessEligible") != Some(&json!(true))
        || coverage.get("status") != Some(&json!("passed"))
    {
        return Err("manual review requires every readiness-eligible formal cell".to_owned());
    }
    for field in [
        "sourceSha256",
        "configSha256",
        "verifierSha256",
        "planSha256",
        "candidateId",
    ] {
        hash_value(formal.get(field), field)?;
    }
    if formal
        .get("sourceDigestScope")
        .and_then(Value::as_str)
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err("formal source digest scope is missing".to_owned());
    }
    for (field, family) in [("configSha256", "config"), ("verifierSha256", "verifier")] {
        if formal.get(field)
            != report
                .get("evidence")
                .and_then(|value| value.get(family))
                .and_then(|value| value.get("sha256"))
        {
            return Err(format!(
                "formal {field} does not bind the observed report evidence"
            ));
        }
    }
    let directory = report_path
        .parent()
        .ok_or("formal report has no evidence directory")?;
    let (receipt, _) = load_object(&directory.join("formal-receipt.json"), "formal receipt")?;
    let retained = receipt
        .get("formal")
        .and_then(Value::as_object)
        .ok_or("retained formal receipt is missing")?;
    for field in [
        "result",
        "runId",
        "candidateId",
        "exitCode",
        "freshComplete",
        "sourceSha256",
        "sourceDigestScope",
        "configSha256",
        "verifierSha256",
        "planSha256",
        "coverage",
    ] {
        if retained.get(field) != formal.get(field) {
            return Err(format!("retained formal receipt has a different {field}"));
        }
    }
    let (manifest, bytes) = load_object(
        &directory.join("formal-artifacts.json"),
        "formal artifact manifest",
    )?;
    let manifest_sha256 = sha256_hex(&bytes);
    if retained
        .get("evidence")
        .and_then(|value| value.get("manifestSha256"))
        .and_then(Value::as_str)
        != Some(&manifest_sha256)
        || manifest.get("kind") != Some(&json!("formal-ui-artifact-manifest"))
        || manifest.get("runId") != report.get("runId")
    {
        return Err("formal receipt does not bind the retained artifact manifest".to_owned());
    }
    verify_manifest_file(&manifest, report_path, "report")?;
    verify_manifest_file(&manifest, queue_path, "review-queue")?;
    let journey = report
        .get("evidence")
        .and_then(|value| value.get("journey"))
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .ok_or("formal journey artifact is missing")?;
    verify_manifest_file(&manifest, Path::new(journey), "journey-evidence")?;
    let cells = report
        .get("review")
        .and_then(|value| value.get("cells"))
        .and_then(Value::as_array)
        .ok_or("formal review cells are missing")?;
    if cells.len() as u64 != required {
        return Err("manual review cell count does not match required formal coverage".to_owned());
    }
    let pages = report
        .get("pages")
        .and_then(Value::as_array)
        .ok_or("formal page evidence is missing")?;
    if pages.len() as u64 != required {
        return Err("formal page count does not match required cells".to_owned());
    }
    let mut ids = BTreeSet::new();
    for cell in cells {
        let id = cell
            .get("cellId")
            .and_then(Value::as_str)
            .ok_or("review cell has no formal cell identity")?;
        if !ids.insert(id) {
            return Err("review repeats a formal cell identity".to_owned());
        }
        let matching = pages
            .iter()
            .filter(|page| page["cellId"] == id)
            .collect::<Vec<_>>();
        if matching.len() != 1
            || matching[0]["outcome"] != "checked"
            || matching[0].pointer("/cache/hit") == Some(&json!(true))
        {
            return Err("review cell is not bound to a fresh checked formal page".to_owned());
        }
        let page = matching[0];
        for (field, actual) in [
            ("targetName", page.pointer("/target/name")),
            ("stateName", page.pointer("/target/stateName")),
            ("theme", page.pointer("/target/theme")),
            ("viewport", page.get("viewport")),
            (
                "sourceFingerprint",
                page.pointer("/review/sourceFingerprint"),
            ),
            (
                "intentFingerprint",
                page.pointer("/review/intentFingerprint"),
            ),
        ] {
            if cell.get(field) != actual {
                return Err(format!("review cell has mismatched formal {field}"));
            }
        }
        for kind in ["viewport", "fullPage"] {
            let filename = cell
                .get("screenshots")
                .and_then(|value| value.get(kind))
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str)
                .ok_or("required formal screenshot is missing")?;
            if cell.pointer(&format!("/screenshots/{kind}/sha256"))
                != page.pointer(&format!("/screenshots/{kind}/sha256"))
            {
                return Err("review screenshot identity differs from the formal page".to_owned());
            }
            verify_manifest_file(&manifest, Path::new(filename), "screenshot")?;
        }
    }
    Ok((retained.clone(), manifest_sha256))
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
    let (formal, formal_manifest_sha256) = passing_formal(&report, report_path, queue_path)?;
    let (queue, queue_bytes) = load_object(queue_path, "review queue")?;
    if report.get("schemaVersion") != Some(&json!(2)) {
        return Err("report schemaVersion must be 2".to_owned());
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
        formal,
        formal_manifest_sha256,
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
    time::OffsetDateTime::parse(reviewed_at, &time::format_description::well_known::Rfc3339)
        .map_err(|_| "manual review timestamp must be RFC3339".to_owned())?;
    #[cfg(unix)]
    let reviewer = format!("uid:{}", unsafe { libc::geteuid() });
    #[cfg(not(unix))]
    let reviewer = return Err("an observed local reviewer identity is unavailable".to_owned());
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
    let mut decisions = Vec::new();
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
            let prior = cell
                .get("carriedFrom")
                .and_then(Value::as_object)
                .ok_or("carried review has no prior evidence binding")?;
            if prior.get("sourceFingerprint") != cell.get("sourceFingerprint")
                || prior.get("intentFingerprint") != cell.get("intentFingerprint")
                || prior
                    .get("reviewEnvironmentIdentity")
                    .filter(|value| !value.is_null())
                    != cell
                        .get("reviewEnvironmentIdentity")
                        .filter(|value| !value.is_null())
                || prior
                    .get("reviewEnvironmentSourceSha256")
                    .filter(|value| !value.is_null())
                    != cell
                        .get("reviewEnvironmentSourceSha256")
                        .filter(|value| !value.is_null())
                || prior.get("manualManifestSha256")
                    != run
                        .report
                        .get("review")
                        .and_then(|review| review.get("priorManifestSha256"))
                || prior.get("formalRunId")
                    != run
                        .report
                        .get("review")
                        .and_then(|review| review.get("priorReviewedRunId"))
            {
                return Err(
                    "carried review does not bind the actual prior source and intent".to_owned(),
                );
            }
            hash_value(prior.get("manualManifestSha256"), "prior manual manifest")?;
            for field in ["viewportSha256", "fullPageSha256"] {
                hash_value(
                    prior
                        .get("screenshots")
                        .and_then(|screenshots| screenshots.get(field)),
                    "prior screenshot",
                )?;
            }
            if prior
                .get("reviewer")
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
                || prior
                    .get("reviewedAt")
                    .and_then(Value::as_str)
                    .is_none_or(|value| {
                        time::OffsetDateTime::parse(
                            value,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .is_err()
                    })
            {
                return Err("carried reviewer evidence is unavailable".to_owned());
            }
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
            "cellId":cell.get("cellId").cloned().unwrap_or(Value::Null),
            "targetName":cell.get("targetName").cloned().unwrap_or(Value::Null),
            "requestedPath":cell.get("requestedPath").cloned().unwrap_or(Value::Null),
            "requestedOrigin":cell.get("requestedOrigin").cloned().unwrap_or(Value::Null),
            "stateName":cell.get("stateName").cloned().unwrap_or(Value::Null),
            "viewport":cell.get("viewport").cloned().unwrap_or(Value::Null),
            "theme":cell.get("theme").cloned().unwrap_or(Value::Null),
            "reviewEnvironmentIdentity":cell.get("reviewEnvironmentIdentity").cloned().unwrap_or(Value::Null),
            "reviewEnvironmentSourceSha256":cell.get("reviewEnvironmentSourceSha256").cloned().unwrap_or(Value::Null),
            "reviewer":reviewer,"reviewedAt":reviewed_at,
            "decision":decision,"note":note,"basis":basis,
            "sourceFingerprint":cell.get("sourceFingerprint").cloned().unwrap_or(Value::Null),
            "intentFingerprint":cell.get("intentFingerprint").cloned().unwrap_or(Value::Null),
            "screenshots":Value::Object(screenshot_hashes(cell)?),
            "carriedFrom":cell.get("carriedFrom").cloned().unwrap_or(Value::Null),
        }));
    }
    let result = if decisions.iter().any(|row| row["decision"] == "blocked") {
        "blocked"
    } else if decisions.iter().any(|row| row["decision"] != "pass") {
        "incomplete"
    } else {
        "passed"
    };
    let manual = json!({
        "result":result,"formalRunId":run.report["runId"],
        "formalManifestSha256":run.formal_manifest_sha256,
        "sourceSha256":run.formal["sourceSha256"],"candidateId":run.formal["candidateId"],
        "reviewer":reviewer,"reviewedAt":reviewed_at,"cells":decisions,
    });
    Ok(json!({
        "schemaVersion":SCHEMA_VERSION,"kind":KIND,
        "reviewedRunId":run.report["runId"],"reviewedAt":reviewed_at,
        "reportSha256":run.report_sha256,"reviewQueueSha256":run.queue_sha256,
        "decisions":decisions,
        "manual":manual,
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
    let manual = actual
        .get("manual")
        .and_then(Value::as_object)
        .ok_or("manual result receipt is missing")?;
    for (field, expected) in [
        ("formalRunId", run.report["runId"].clone()),
        ("formalManifestSha256", json!(run.formal_manifest_sha256)),
        ("sourceSha256", run.formal["sourceSha256"].clone()),
        ("candidateId", run.formal["candidateId"].clone()),
        ("cells", json!(decisions)),
    ] {
        if manual.get(field) != Some(&expected) {
            return Err(format!(
                "manual {field} does not bind the exact passed formal run"
            ));
        }
    }
    let result = if decisions.iter().any(|row| row["decision"] == "blocked") {
        "blocked"
    } else if decisions.iter().any(|row| row["decision"] != "pass") {
        "incomplete"
    } else {
        "passed"
    };
    if manual.get("result") != Some(&json!(result)) {
        return Err("manual result contradicts its required cell decisions".to_owned());
    }
    for field in ["reviewer", "reviewedAt"] {
        if manual
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(format!("manual {field} is missing"));
        }
    }
    let expected = run
        .cells
        .iter()
        .map(|cell| {
            let key = cell["reviewCellKey"].as_str().expect("validated key").to_owned();
            Ok((
                key,
                json!({
                    "cellId":cell.get("cellId").cloned().unwrap_or(Value::Null),
                    "targetName":cell.get("targetName").cloned().unwrap_or(Value::Null),
                    "requestedOrigin":cell.get("requestedOrigin").cloned().unwrap_or(Value::Null),
                    "stateName":cell.get("stateName").cloned().unwrap_or(Value::Null),
                    "theme":cell.get("theme").cloned().unwrap_or(Value::Null),
                    "viewport":cell.get("viewport").cloned().unwrap_or(Value::Null),
                    "reviewEnvironmentIdentity":cell.get("reviewEnvironmentIdentity").cloned().unwrap_or(Value::Null),
                    "reviewEnvironmentSourceSha256":cell.get("reviewEnvironmentSourceSha256").cloned().unwrap_or(Value::Null),
                    "sourceFingerprint":cell.get("sourceFingerprint").cloned().unwrap_or(Value::Null),
                    "intentFingerprint":cell.get("intentFingerprint").cloned().unwrap_or(Value::Null),
                    "screenshots":Value::Object(screenshot_hashes(cell)?),
                    "carriedFrom":cell.get("carriedFrom").cloned().unwrap_or(Value::Null),
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
        let reviewer = item
            .get("reviewer")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or("manual cell reviewer is missing")?;
        let reviewed_at = item
            .get("reviewedAt")
            .and_then(Value::as_str)
            .ok_or("manual cell timestamp is missing")?;
        time::OffsetDateTime::parse(reviewed_at, &time::format_description::well_known::Rfc3339)
            .map_err(|_| "manual cell timestamp is invalid")?;
        if reviewer != manual["reviewer"].as_str().unwrap()
            || reviewed_at != manual["reviewedAt"].as_str().unwrap()
        {
            return Err("manual cell reviewer/time differs from the receipt".to_owned());
        }
        for field in [
            "cellId",
            "targetName",
            "requestedOrigin",
            "stateName",
            "theme",
            "viewport",
            "reviewEnvironmentIdentity",
            "reviewEnvironmentSourceSha256",
            "sourceFingerprint",
            "intentFingerprint",
            "screenshots",
            "carriedFrom",
        ] {
            let optional_origin_field = matches!(
                field,
                "requestedOrigin" | "reviewEnvironmentIdentity" | "reviewEnvironmentSourceSha256"
            );
            let actual_field = item
                .get(field)
                .or_else(|| optional_origin_field.then_some(&Value::Null));
            if actual_field != expected[key].get(field) {
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
        "manual":{
            "result":review.pointer("/manual/result").cloned().ok_or("normalized manual receipt is missing")?,
            "formalRunId":review.pointer("/manual/formalRunId"),
            "formalManifestSha256":review.pointer("/manual/formalManifestSha256"),
            "sourceSha256":review.pointer("/manual/sourceSha256"),
            "candidateId":review.pointer("/manual/candidateId"),
            "requiredCells":decisions.len(),"reviewedCells":decisions.len(),
        },
        "reviewedRunId":review.get("reviewedRunId").cloned().unwrap_or(Value::Null),
        "decisionCount":decisions.len(),"blockingCount":blocking,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal_fixture(report_path: &Path, queue_path: &Path) {
        let directory = report_path.parent().unwrap();
        let mut report: Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        let journey = directory.join("journey-evidence.json");
        std::fs::write(&journey, b"{\"kind\":\"isolated-manual-contract-fixture\"}").unwrap();
        report["evidence"] = json!({"config":{"sha256":"c".repeat(64)},"verifier":{"sha256":"d".repeat(64)},"journey":{"path":journey}});
        report["formal"] = json!({"result":"passed","runId":"run-1","freshComplete":true,"exitCode":0,
            "sourceSha256":"e".repeat(64),"sourceDigestScope":"isolated fixture source identity",
            "configSha256":"c".repeat(64),"verifierSha256":"d".repeat(64),"planSha256":"f".repeat(64),"candidateId":"1".repeat(64),
            "coverage":{"status":"passed","readinessEligible":true,"requiredCells":1,"checkedCells":1}});
        let cell = report["review"]["cells"][0].clone();
        report["pages"] = json!([{"cellId":cell["cellId"],"outcome":"checked","target":{"name":cell["targetName"],"stateName":cell["stateName"],"theme":cell["theme"]},"viewport":cell["viewport"],"review":{"sourceFingerprint":cell["sourceFingerprint"],"intentFingerprint":cell["intentFingerprint"]},"screenshots":cell["screenshots"]}]);
        std::fs::write(report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        let files = [
            (report_path.to_path_buf(), "report"),
            (queue_path.to_path_buf(), "review-queue"),
            (journey, "journey-evidence"),
            (
                PathBuf::from(cell["screenshots"]["viewport"]["path"].as_str().unwrap()),
                "screenshot",
            ),
            (
                PathBuf::from(cell["screenshots"]["fullPage"]["path"].as_str().unwrap()),
                "screenshot",
            ),
        ];
        let manifest = json!({"schemaVersion":1,"kind":"formal-ui-artifact-manifest","runId":"run-1","files":files.iter().map(|(file, kind)| {
            let bytes=std::fs::read(file).unwrap(); json!({"kind":kind,"identity":sha256_hex(std::fs::canonicalize(file).unwrap().to_string_lossy().as_bytes()),"sha256":sha256_hex(&bytes),"bytes":bytes.len()})
        }).collect::<Vec<_>>()});
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(directory.join("formal-artifacts.json"), &manifest_bytes).unwrap();
        report["formal"]["evidence"] = json!({"manifestSha256":sha256_hex(&manifest_bytes)});
        std::fs::write(
            directory.join("formal-receipt.json"),
            serde_json::to_vec(&json!({"formal":report["formal"]})).unwrap(),
        )
        .unwrap();
    }

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
            "schemaVersion":2,"runId":"run-1","review":{
                "queueSha256":sha256_hex(&std::fs::read(&queue_path).unwrap()),
                "cells":[{
                    "reviewCellKey":"cell-1","cellId":"cell-1","status":"review-required","theme":"light",
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
        seal_fixture(&report_path, &queue_path);
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
    }

    #[test]
    fn missing_or_nonpassed_formal_evidence_cannot_enter_manual_review() {
        for result in [None, Some("failed"), Some("blocked"), Some("incomplete")] {
            let (_directory, report_path, queue, decisions, _) = fixture();
            let mut report: Value =
                serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
            if let Some(result) = result {
                report["formal"] = json!({"result":result});
            } else {
                report.as_object_mut().unwrap().remove("formal");
            }
            std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
            assert!(
                finalize(
                    &report_path,
                    &queue,
                    Some(&decisions),
                    "2026-10-02T00:00:00Z"
                )
                .is_err(),
                "manual review accepted formal result {result:?}"
            );
        }
    }

    #[test]
    fn manual_results_follow_complete_cell_decisions_and_bind_reviewer_metadata() {
        for (decision, expected) in [
            ("pass", "passed"),
            ("gap", "incomplete"),
            ("blocked", "blocked"),
        ] {
            let (directory, report, queue, decisions, _) = fixture();
            std::fs::write(&decisions, serde_json::to_vec(&json!([{"reviewCellKey":"cell-1","decision":decision,"note":"Isolated contract fixture observation"}])).unwrap()).unwrap();
            let review =
                finalize(&report, &queue, Some(&decisions), "2026-10-02T00:00:00Z").unwrap();
            assert_eq!(review["manual"]["result"], expected);
            assert!(
                review["manual"]["reviewer"]
                    .as_str()
                    .unwrap()
                    .starts_with("uid:")
            );
            let output = directory.path().join("manual.json");
            write_new_review(&output, &review).unwrap();
            validate(&output, &report, &queue).unwrap();
            for field in ["reviewer", "reviewedAt", "theme", "cellId"] {
                let mut changed = review.clone();
                changed["decisions"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
                changed["manual"]["cells"] = changed["decisions"].clone();
                std::fs::write(&output, serde_json::to_vec(&changed).unwrap()).unwrap();
                assert!(
                    validate(&output, &report, &queue).is_err(),
                    "missing {field} accepted"
                );
            }
            let mut changed = review.clone();
            changed["manual"]["sourceSha256"] = json!("0".repeat(64));
            std::fs::write(&output, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(validate(&output, &report, &queue).is_err());
            for field in [
                "requestedOrigin",
                "reviewEnvironmentIdentity",
                "reviewEnvironmentSourceSha256",
            ] {
                let mut changed = review.clone();
                changed["decisions"][0][field] = json!("unrelated-origin-evidence");
                changed["manual"]["cells"] = changed["decisions"].clone();
                std::fs::write(&output, serde_json::to_vec(&changed).unwrap()).unwrap();
                assert!(
                    validate(&output, &report, &queue).is_err(),
                    "changed {field} accepted"
                );
            }
        }
    }

    #[test]
    fn incomplete_formal_cells_or_missing_artifacts_block_review() {
        for field in ["freshComplete", "sourceSha256", "runId"] {
            let (_directory, report_path, queue, decisions, _) = fixture();
            let mut report: Value =
                serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
            report["formal"].as_object_mut().unwrap().remove(field);
            std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
            assert!(
                finalize(
                    &report_path,
                    &queue,
                    Some(&decisions),
                    "2026-10-02T00:00:00Z"
                )
                .is_err()
            );
        }
        let (_directory, report, queue, decisions, viewport) = fixture();
        std::fs::remove_file(viewport).unwrap();
        assert!(finalize(&report, &queue, Some(&decisions), "2026-10-02T00:00:00Z").is_err());
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
        report["review"]["cells"][0]["status"] = json!("carried-pass");
        report["review"]["cells"][0]["decision"] = json!("pass");
        report["review"]["priorManifestSha256"] = json!("2".repeat(64));
        report["review"]["priorReviewedRunId"] = json!("prior-run");
        report["review"]["cells"][0]["carriedFrom"] = json!({"manualManifestSha256":"2".repeat(64),"formalRunId":"prior-run","sourceFingerprint":"a".repeat(64),"intentFingerprint":"b".repeat(64),"screenshots":{"viewportSha256":"3".repeat(64),"fullPageSha256":"4".repeat(64)},"reviewer":"isolated-test-reviewer","reviewedAt":"2026-09-04T00:00:00Z"});
        let queue = json!({"schemaVersion":1,"kind":QUEUE_KIND,"runId":"run-1","entries":[]});
        std::fs::write(&queue_path, serde_json::to_vec(&queue).unwrap()).unwrap();
        report["review"]["queueSha256"] = json!(sha256_hex(&std::fs::read(&queue_path).unwrap()));
        std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        seal_fixture(&report_path, &queue_path);
        let review = finalize(&report_path, &queue_path, None, "2026-09-04T00:01:00Z").unwrap();
        assert_eq!(review["decisions"][0]["decision"], "pass");
        assert_eq!(
            review["decisions"][0]["basis"],
            "carried-unchanged-ui-inputs-and-intent"
        );
        assert_eq!(summary(&review).unwrap()["blockingCount"], 0);
        drop(directory);
    }
}
