//! Read-only assurance over source-bound native test evidence.
//! This module evaluates evidence; it never launches tests or schedules work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use globset::Glob;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit_common::{sha256_file, sha256_hex};
use crate::audit_ledger::read_bytes_nofollow;
use crate::audit_queue::{FileEntry, validate_repo_relative_path_token};
use crate::audit_targets::{CoverageEvidence, CoverageLines, TestTarget};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
}

impl Artifact {
    fn resolve(&self, base: &Path) -> PathBuf {
        if self.path.is_absolute() {
            self.path.clone()
        } else {
            base.join(&self.path)
        }
    }

    fn read(&self, base: &Path) -> Result<Vec<u8>, String> {
        let path = self.resolve(base);
        let bytes = read_bytes_nofollow(&path, None)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "evidence artifact is missing or not a regular file".to_owned())?;
        if sha256_hex(&bytes) != self.sha256 {
            return Err("evidence artifact hash changed".to_owned());
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeEntry {
    pub file: String,
    pub role: SourceRole,
    /// Required for anything removed from the executable-code denominator.
    pub rationale: String,
    pub reference: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Product,
    Tests,
    Support,
    Excluded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Development,
    PreMerge,
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Unit,
    Component,
    Contract,
    Integration,
    E2e,
    Visual,
    Static,
    Setup,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    pub category: Category,
    pub tier: Tier,
    /// Browser/platform/feature identity; legitimate matrix variants differ here.
    #[serde(default)]
    pub variant: String,
    pub inputs: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub dependency_reasons: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub repeat_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReport {
    pub artifact: Artifact,
    pub format: ReportFormat,
    /// libtest listings/events do not include files. Bind each supplied listing
    /// to its source, or use the portable collected-tests format with per-test files.
    #[serde(default)]
    pub source_file: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportFormat {
    Junit,
    PlaywrightJson,
    RustList,
    RustJson,
    CollectedTests,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestResult {
    pub file: String,
    pub name: String,
    pub status: TestStatus,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    pub attempts: usize,
}

impl TestResult {
    pub fn reference(&self) -> String {
        format!("{}#{}", self.file, self.name)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    Collected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    pub check_id: String,
    pub start_ms: f64,
    pub duration_ms: f64,
    pub status: TestStatus,
    pub reports: Vec<NativeReport>,
    #[serde(default)]
    pub phase_ms: BTreeMap<String, f64>,
}

/// Produced alongside the native reports by the project's existing test workflow.
/// Never manufacture this binding later while auditing an old report.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunReceipt {
    pub schema_version: u32,
    pub run_id: String,
    pub source_hashes: BTreeMap<String, String>,
    pub config_hashes: BTreeMap<String, String>,
    pub environment: String,
    pub captured_at: String,
    pub tier: Tier,
    pub complete: bool,
    pub collection_complete: bool,
    pub branches_enabled: bool,
    pub status: TestStatus,
    pub duration_ms: f64,
    #[serde(default)]
    pub coverage_reports: Vec<Artifact>,
    #[serde(default)]
    pub supporting_artifacts: Vec<Artifact>,
    pub checks: Vec<CheckRun>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub reference: String,
    pub focused_ms: f64,
    pub pre_merge_ms: f64,
    pub release_ms: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiRequirement {
    pub id: String,
    pub file: String,
    pub journey: String,
    pub state: String,
    pub theme: String,
    pub viewport: String,
    pub interaction: String,
    pub expected: String,
    pub test: String,
    pub check_id: String,
    /// Source/config/requirement reference used by the reviewer to establish scope.
    pub reference: String,
    pub target_ids: Vec<String>,
    /// Optional existing formal report: geometry supports, but does not replace,
    /// a passing interaction test with an assessed outcome assertion.
    #[serde(default)]
    pub formal: Option<FormalEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FormalEvidence {
    pub report: Artifact,
    pub review_queue: Artifact,
    pub manual_review: Artifact,
    pub cell_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionCase {
    pub kind: String,
    pub observation: Artifact,
    #[serde(default)]
    pub additional_checks_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionObservation {
    pub schema_version: u32,
    pub base_revision: String,
    pub source_hashes: BTreeMap<String, String>,
    pub changed_files: Vec<String>,
    pub selected_checks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub criterion: String,
    pub status: Verdict,
    pub rationale: String,
    pub references: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRationale {
    pub rationale: String,
    pub reference: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssuranceInput {
    pub schema_version: u32,
    #[serde(default)]
    pub scope: Vec<ScopeEntry>,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub runs: Vec<Artifact>,
    /// source -> direct dependencies, including shared fixtures/configuration.
    #[serde(default)]
    pub source_dependencies: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub ui_requirements: Vec<UiRequirement>,
    #[serde(default)]
    pub ui_exclusions: BTreeMap<String, ScopeRationale>,
    #[serde(default)]
    pub selection_cases: Vec<SelectionCase>,
    #[serde(default)]
    pub selection_exemptions: BTreeMap<String, String>,
    #[serde(default)]
    pub budgets: Option<Budgets>,
    #[serde(default)]
    pub reviews: Vec<Review>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundInput {
    pub artifact: Artifact,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Met,
    Gaps,
    Unproven,
    NotApplicable,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub code: String,
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Assessment {
    pub status: Verdict,
    pub findings: Vec<Finding>,
    pub unknowns: Vec<Finding>,
    pub measurements: Vec<Value>,
}

impl Default for Assessment {
    fn default() -> Self {
        Self {
            status: Verdict::Met,
            findings: Vec::new(),
            unknowns: Vec::new(),
            measurements: Vec::new(),
        }
    }
}

impl Assessment {
    fn gap(&mut self, code: &str, subject: impl Into<String>, detail: impl Into<String>) {
        self.findings.push(Finding {
            code: code.into(),
            subject: subject.into(),
            detail: detail.into(),
        });
    }
    fn unknown(&mut self, code: &str, subject: impl Into<String>, detail: impl Into<String>) {
        self.unknowns.push(Finding {
            code: code.into(),
            subject: subject.into(),
            detail: detail.into(),
        });
    }
    fn finish(&mut self) {
        if !self.findings.is_empty() {
            self.status = Verdict::Gaps;
        } else if !self.unknowns.is_empty() {
            self.status = Verdict::Unproven;
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AssuranceReport {
    pub coverage: Assessment,
    pub ui: Assessment,
    pub efficiency: Assessment,
    pub evidence_issues: Vec<String>,
    pub tests: BTreeMap<String, TestResult>,
    pub test_coverage_files: BTreeMap<String, BTreeSet<String>>,
}

fn json_bytes<T: for<'de> Deserialize<'de>>(bytes: &[u8], label: &str) -> Result<T, String> {
    let object = crate::audit_findings::strict_json_object(bytes, label)?;
    serde_json::from_value(Value::Object(object)).map_err(|e| format!("invalid {label}: {e}"))
}

pub fn bind(path: &Path) -> Result<BoundInput, String> {
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|e| e.to_string())?
        .ok_or("assurance input is missing")?;
    let input: AssuranceInput = json_bytes(&bytes, "assurance input")?;
    if input.schema_version != 1 {
        return Err("unsupported assurance input schema".into());
    }
    Ok(BoundInput {
        artifact: Artifact {
            path: path.canonicalize().map_err(|e| e.to_string())?,
            sha256: sha256_hex(&bytes),
        },
    })
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn checked_test(mut test: TestResult) -> Result<TestResult, String> {
    validate_repo_relative_path_token(&test.file, "test result file")?;
    if test.name.trim().is_empty() || test.name.contains(['\n', '\r']) {
        return Err("test identity is empty or multiline".into());
    }
    if test.duration_ms.is_some_and(|value| !nonnegative(value)) {
        return Err("invalid native test duration".into());
    }
    if test.attempts == 0 && test.status != TestStatus::Collected {
        test.attempts = 1;
    }
    Ok(test)
}

pub fn native_tests(report: &NativeReport, base: &Path) -> Result<Vec<TestResult>, String> {
    let bytes = report.artifact.read(base)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "native test report is not UTF-8")?;
    let fallback = report.source_file.as_deref().unwrap_or("");
    let mut tests = Vec::new();
    match report.format {
        ReportFormat::CollectedTests => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Collection {
                schema_version: u32,
                tests: Vec<TestResult>,
            }
            let collection: Collection = json_bytes(&bytes, "collected tests")?;
            if collection.schema_version != 1 {
                return Err("unsupported collected-tests schema".into());
            }
            tests = collection.tests;
        }
        ReportFormat::RustList => {
            for line in text.lines() {
                if let Some(name) = line.strip_suffix(": test") {
                    tests.push(TestResult {
                        file: fallback.into(),
                        name: name.into(),
                        status: TestStatus::Collected,
                        duration_ms: None,
                        attempts: 0,
                    });
                }
            }
        }
        ReportFormat::RustJson => {
            let mut events: BTreeMap<String, TestResult> = BTreeMap::new();
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                let value: Value =
                    serde_json::from_str(line).map_err(|_| "invalid libtest JSON event")?;
                if value["type"] != "test" {
                    continue;
                }
                let Some(name) = value["name"].as_str() else {
                    return Err("libtest event has no name".into());
                };
                let status = match value["event"].as_str() {
                    Some("ok") => TestStatus::Passed,
                    Some("failed") => TestStatus::Failed,
                    Some("ignored") => TestStatus::Skipped,
                    _ => continue,
                };
                events.insert(
                    name.into(),
                    TestResult {
                        file: fallback.into(),
                        name: name.into(),
                        status,
                        duration_ms: value["exec_time"].as_f64().map(|seconds| seconds * 1000.0),
                        attempts: 1,
                    },
                );
            }
            tests = events.into_values().collect();
        }
        ReportFormat::Junit => {
            let mut reader = Reader::from_str(text);
            let mut current: Option<TestResult> = None;
            loop {
                match reader.read_event().map_err(|_| "invalid JUnit XML")? {
                    Event::Start(start) | Event::Empty(start) => {
                        let name = start.name();
                        if name.as_ref() == b"testcase" {
                            let mut attrs = BTreeMap::new();
                            for attr in start.attributes() {
                                let attr = attr.map_err(|_| "invalid JUnit attribute")?;
                                attrs.insert(
                                    String::from_utf8_lossy(attr.key.as_ref()).into_owned(),
                                    attr.decode_and_unescape_value(reader.decoder())
                                        .map_err(|_| "invalid JUnit value")?
                                        .into_owned(),
                                );
                            }
                            let test = TestResult {
                                file: attrs
                                    .get("file")
                                    .cloned()
                                    .unwrap_or_else(|| fallback.into()),
                                name: {
                                    let name = attrs.get("name").ok_or("JUnit case has no name")?;
                                    match attrs.get("classname").filter(|class| !class.is_empty()) {
                                        Some(class) => format!("{class}::{name}"),
                                        None => name.clone(),
                                    }
                                },
                                status: TestStatus::Passed,
                                duration_ms: attrs
                                    .get("time")
                                    .map(|value| {
                                        value.parse::<f64>().map(|seconds| seconds * 1000.0)
                                    })
                                    .transpose()
                                    .map_err(|_| "invalid JUnit time")?,
                                attempts: 1,
                            };
                            // Empty tags have no following testcase end event.
                            let offset = reader.buffer_position() as usize;
                            if text[..offset].trim_end().ends_with("/>") {
                                tests.push(test);
                            } else {
                                current = Some(test);
                            }
                        } else if let Some(test) = current.as_mut() {
                            if matches!(name.as_ref(), b"failure" | b"error") {
                                test.status = TestStatus::Failed;
                            }
                            if name.as_ref() == b"skipped" {
                                test.status = TestStatus::Skipped;
                            }
                        }
                    }
                    Event::End(end) if end.name().as_ref() == b"testcase" => {
                        if let Some(test) = current.take() {
                            tests.push(test);
                        }
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
        }
        ReportFormat::PlaywrightJson => {
            fn walk(
                value: &Value,
                parents: &[String],
                fallback: &str,
                out: &mut Vec<TestResult>,
            ) -> Result<(), String> {
                let mut titles = parents.to_vec();
                if let Some(title) = value["title"].as_str().filter(|title| !title.is_empty()) {
                    titles.push(title.into());
                }
                for spec in value["specs"].as_array().into_iter().flatten() {
                    for test in spec["tests"].as_array().into_iter().flatten() {
                        let results = test["results"].as_array();
                        let last = results.and_then(|results| results.last());
                        let status = match last.and_then(|last| last["status"].as_str()) {
                            Some("passed") => TestStatus::Passed,
                            Some("skipped") => TestStatus::Skipped,
                            Some("failed" | "timedOut" | "interrupted") => TestStatus::Failed,
                            _ => TestStatus::Collected,
                        };
                        let mut name = titles.clone();
                        name.push(
                            spec["title"]
                                .as_str()
                                .ok_or("Playwright spec has no title")?
                                .into(),
                        );
                        if let Some(project) = test["projectName"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                        {
                            name.push(format!("[{project}]"));
                        }
                        out.push(TestResult {
                            file: spec["file"].as_str().unwrap_or(fallback).replace('\\', "/"),
                            name: name.join(" > "),
                            status,
                            duration_ms: last.and_then(|last| last["duration"].as_f64()),
                            attempts: results.map_or(0, Vec::len),
                        });
                    }
                }
                for suite in value["suites"].as_array().into_iter().flatten() {
                    walk(suite, &titles, fallback, out)?;
                }
                Ok(())
            }
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| "invalid Playwright JSON")?;
            walk(&value, &[], fallback, &mut tests)?;
        }
    }
    if tests.is_empty() {
        return Err("native report contains no test identities".into());
    }
    tests.into_iter().map(checked_test).collect()
}

fn source_binding(
    repo: &Path,
    source: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> Result<(), String> {
    if source != expected {
        return Err("run source inventory/digests differ from the audited snapshot".into());
    }
    for (path, hash) in source {
        validate_repo_relative_path_token(path, "bound source")?;
        if sha256_file(&repo.join(path))? != *hash {
            return Err("bound source changed".into());
        }
    }
    Ok(())
}

struct LoadedRun {
    receipt: RunReceipt,
    tests: BTreeMap<String, Vec<TestResult>>,
}

fn load_run(
    artifact: &Artifact,
    base: &Path,
    repo: &Path,
    expected: &BTreeMap<String, String>,
) -> Result<LoadedRun, String> {
    let receipt: RunReceipt = json_bytes(&artifact.read(base)?, "test run receipt")?;
    if receipt.schema_version != 1
        || receipt.run_id.is_empty()
        || receipt.environment.trim().is_empty()
        || receipt.captured_at.trim().is_empty()
    {
        return Err("run receipt lacks version or provenance".into());
    }
    if !nonnegative(receipt.duration_ms) {
        return Err("invalid run duration".into());
    }
    source_binding(repo, &receipt.source_hashes, expected)?;
    if receipt.config_hashes.is_empty() {
        return Err("run receipt lacks configuration binding".into());
    }
    for (path, hash) in &receipt.config_hashes {
        validate_repo_relative_path_token(path, "run config")?;
        if sha256_file(&repo.join(path))? != *hash {
            return Err("run configuration changed".into());
        }
    }
    let run_base = artifact
        .resolve(base)
        .parent()
        .ok_or("run receipt has no parent")?
        .to_path_buf();
    let mut tests = BTreeMap::new();
    for check in &receipt.checks {
        if !nonnegative(check.start_ms)
            || !nonnegative(check.duration_ms)
            || check.start_ms + check.duration_ms > receipt.duration_ms + 1.0
            || check.phase_ms.values().any(|value| !nonnegative(*value))
        {
            return Err("invalid check timing".into());
        }
        let mut rows: Vec<TestResult> = Vec::new();
        for report in &check.reports {
            for test in native_tests(report, &run_base)? {
                if let Some(existing) = rows
                    .iter_mut()
                    .find(|row| row.reference() == test.reference())
                {
                    if existing.status == TestStatus::Collected {
                        *existing = test;
                        continue;
                    }
                    if test.status == TestStatus::Collected {
                        continue;
                    }
                }
                rows.push(test);
            }
        }
        for test in &rows {
            if !expected.contains_key(&test.file) {
                return Err("test report file is outside the source inventory".into());
            }
            if test
                .duration_ms
                .is_some_and(|duration| duration > check.duration_ms + 1.0)
            {
                return Err("native test duration exceeds the enclosing check wall time".into());
            }
        }
        if tests.insert(check.check_id.clone(), rows).is_some() {
            return Err("duplicate check in run receipt".into());
        }
    }
    for coverage in receipt
        .coverage_reports
        .iter()
        .chain(&receipt.supporting_artifacts)
    {
        coverage.read(&run_base)?;
    }
    Ok(LoadedRun { receipt, tests })
}

fn review(input: &AssuranceInput, criterion: &str, assessment: &mut Assessment) {
    let rows = input
        .reviews
        .iter()
        .filter(|row| row.criterion == criterion)
        .collect::<Vec<_>>();
    if rows.len() != 1 {
        assessment.unknown(
            "missing_review",
            criterion,
            "one evidence-backed assessment is required",
        );
        return;
    }
    let row = rows[0];
    if row.rationale.trim().is_empty()
        || row.references.is_empty()
        || row
            .references
            .iter()
            .any(|reference| reference.trim().is_empty())
    {
        assessment.unknown(
            "unsupported_review",
            criterion,
            "review needs rationale and evidence references",
        );
        return;
    }
    match row.status {
        Verdict::Gaps => assessment.gap("reviewed_gap", criterion, &row.rationale),
        Verdict::Unproven => assessment.unknown("unresolved_review", criterion, &row.rationale),
        _ => {}
    }
}

fn full_lines(lines: &CoverageLines) -> bool {
    !lines.measured_lines.is_empty() && lines.measured_lines == lines.covered_lines
}
fn full_branches(lines: &CoverageLines) -> bool {
    lines
        .branches
        .as_ref()
        .is_some_and(|branch| branch.found == branch.hit && branch.missing.is_empty())
}

fn qualified_run(run: &LoadedRun) -> bool {
    let receipt = &run.receipt;
    receipt.complete
        && receipt.collection_complete
        && receipt.status == TestStatus::Passed
        && receipt.tier == Tier::Release
        && run.tests.values().flatten().next().is_some()
        && receipt
            .checks
            .iter()
            .all(|check| check.status == TestStatus::Passed)
        && run
            .tests
            .values()
            .flatten()
            .all(|test| test.status == TestStatus::Passed)
}

fn assess_coverage(
    input: &AssuranceInput,
    files: &[FileEntry],
    coverage: &[CoverageEvidence],
    runs: &[LoadedRun],
    out: &mut Assessment,
) {
    let mut scope = BTreeMap::new();
    for entry in &input.scope {
        if scope.insert(entry.file.as_str(), entry).is_some() {
            out.gap(
                "duplicate_scope",
                &entry.file,
                "source scope must be unique",
            );
        }
        if !files.iter().any(|file| file.rel_path == entry.file) {
            out.gap(
                "unknown_scope_file",
                &entry.file,
                "scope entry is outside the audited inventory",
            );
        }
    }
    let qualified = runs
        .iter()
        .filter(|run| qualified_run(run))
        .collect::<Vec<_>>();
    for file in files {
        let Some(scope) = scope.get(file.rel_path.as_str()) else {
            out.unknown(
                "unclassified_source",
                &file.rel_path,
                "classify product code, test code or justified non-executable/support content",
            );
            continue;
        };
        if scope.rationale.trim().is_empty() || scope.reference.trim().is_empty() {
            out.unknown(
                "unsupported_scope",
                &file.rel_path,
                "scope requires rationale and a confirmed source reference",
            );
            continue;
        }
        if scope.role != SourceRole::Product {
            out.measurements.push(json!({"file":file.rel_path,"role":scope.role,"rationale":scope.rationale,"reference":scope.reference}));
            continue;
        }
        let mut measured = Vec::new();
        for artifact in coverage {
            let bound = qualified.iter().find(|run| {
                run.receipt.branches_enabled
                    && run
                        .receipt
                        .coverage_reports
                        .iter()
                        .any(|reference| reference.sha256 == artifact.sha256)
            });
            if let (Some(run), Some(lines)) = (bound, artifact.files.get(&file.rel_path)) {
                measured.push((run.receipt.run_id.as_str(), lines));
            }
        }
        if measured.is_empty() {
            out.unknown("unmeasured_source",&file.rel_path,"no complete passing release evidence binds coverage to this source and configuration");
            continue;
        }
        // Do not union ambiguous branch aggregates from different producers/runs.
        // Projects can supply their runner's correctly merged full report.
        let chosen = measured
            .iter()
            .find(|(_, lines)| full_lines(lines) && full_branches(lines))
            .copied()
            .unwrap_or(measured[0]);
        let (run, lines) = chosen;
        out.measurements.push(json!({"file":file.rel_path,"run_id":run,"lines_found":lines.measured_lines.len(),"lines_hit":lines.covered_lines.len(),"branches":lines.branches}));
        if lines.measured_lines.is_empty() {
            out.unknown(
                "empty_measurement",
                &file.rel_path,
                "empty executable denominator cannot prove coverage",
            );
        } else if !full_lines(lines) {
            out.gap(
                "uncovered_lines",
                &file.rel_path,
                "every in-scope executable line must be covered",
            );
        }
        if lines.branches.is_none() {
            out.unknown(
                "branches_unmeasured",
                &file.rel_path,
                "missing branch data is not zero branches",
            );
        } else if !full_branches(lines) {
            out.gap(
                "uncovered_branches",
                &file.rel_path,
                "every in-scope branch outcome must be covered",
            );
        }
    }
    review(input, "assertions", out);
}

fn required_checks(checks: &BTreeMap<String, &Check>, selected: &mut BTreeSet<String>) {
    loop {
        let before = selected.len();
        let required = selected
            .iter()
            .filter_map(|id| checks.get(id))
            .flat_map(|check| check.requires.iter().cloned())
            .collect::<Vec<_>>();
        selected.extend(required);
        if selected.len() == before {
            break;
        }
    }
}

fn affected_checks(input: &AssuranceInput, changed: &[String]) -> Result<BTreeSet<String>, String> {
    let checks = input
        .checks
        .iter()
        .map(|check| (check.id.clone(), check))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeSet::new();
    for change in changed {
        let mut affected = BTreeSet::from([change.clone()]);
        loop {
            let before = affected.len();
            for (file, dependencies) in &input.source_dependencies {
                if dependencies
                    .iter()
                    .any(|dependency| affected.contains(dependency))
                {
                    affected.insert(file.clone());
                }
            }
            if affected.len() == before {
                break;
            }
        }
        let mut mapped = false;
        for check in &input.checks {
            for pattern in &check.inputs {
                let matcher = Glob::new(pattern)
                    .map_err(|_| "invalid changed-input glob")?
                    .compile_matcher();
                if affected.iter().any(|file| matcher.is_match(file)) {
                    selected.insert(check.id.clone());
                    mapped = true;
                }
            }
        }
        if !mapped {
            return Ok(checks.keys().cloned().collect());
        }
    }
    required_checks(&checks, &mut selected);
    Ok(selected)
}

fn assess_selection(
    input: &AssuranceInput,
    base: &Path,
    expected: &BTreeMap<String, String>,
    out: &mut Assessment,
) {
    for kind in ["local", "shared", "ui", "fixture", "config", "unmapped"] {
        if !input.selection_cases.iter().any(|case| case.kind == kind)
            && input
                .selection_exemptions
                .get(kind)
                .is_none_or(|reason| reason.trim().is_empty())
        {
            out.unknown(
                "missing_selection_case",
                kind,
                "supply a measured selector example or an applicability rationale",
            );
        }
    }
    for case in &input.selection_cases {
        let result = (|| {
            let observation: SelectionObservation =
                json_bytes(&case.observation.read(base)?, "selection observation")?;
            if observation.schema_version != 1
                || observation.base_revision.trim().is_empty()
                || observation.source_hashes != *expected
                || observation.changed_files.is_empty()
            {
                return Err("selection evidence is incomplete or stale".to_owned());
            }
            for file in &observation.changed_files {
                validate_repo_relative_path_token(file, "changed file")?;
            }
            let selected = observation
                .selected_checks
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if selected.len() != observation.selected_checks.len() {
                return Err("selector returned duplicate checks".into());
            }
            let required = affected_checks(input, &observation.changed_files)?;
            if !required.is_subset(&selected) {
                out.gap(
                    "selection_omits_checks",
                    &case.kind,
                    "affected checks or required prerequisites were omitted",
                );
            }
            let unknown = selected
                .iter()
                .any(|id| !input.checks.iter().any(|check| &check.id == id));
            if unknown {
                out.gap(
                    "unknown_selected_check",
                    &case.kind,
                    "selected identity is absent from the execution plan",
                );
            }
            if !selected.is_subset(&required)
                && case
                    .additional_checks_reason
                    .as_ref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                out.gap(
                    "overbroad_selection",
                    &case.kind,
                    "extra checks need a concrete reason",
                );
            }
            out.measurements.push(json!({"kind":case.kind,"changed_files":observation.changed_files,"selected":selected,"required":required,"base_revision":observation.base_revision}));
            Ok(())
        })();
        if let Err(error) = result {
            out.unknown("selection_evidence", &case.kind, error);
        }
    }
}

fn assess_efficiency(
    input: &AssuranceInput,
    base: &Path,
    expected: &BTreeMap<String, String>,
    runs: &[LoadedRun],
    out: &mut Assessment,
) {
    let checks = input
        .checks
        .iter()
        .map(|check| (check.id.clone(), check))
        .collect::<BTreeMap<_, _>>();
    if checks.is_empty() {
        out.unknown(
            "missing_test_plan",
            "checks",
            "supply categorized checks and their dependencies",
        );
    }
    if checks.len() != input.checks.len() {
        out.gap(
            "duplicate_check_id",
            "checks",
            "check identities must be unique",
        );
    }
    for check in &input.checks {
        if check.id.trim().is_empty() || check.inputs.is_empty() {
            out.gap(
                "incomplete_check",
                &check.id,
                "check identity and changed-input mapping are required",
            );
        }
        for required in &check.requires {
            if !checks.contains_key(required) {
                out.gap(
                    "unknown_dependency",
                    &check.id,
                    "required check is absent from the plan",
                );
            }
            if check
                .dependency_reasons
                .get(required)
                .is_none_or(|reason| reason.trim().is_empty())
            {
                out.unknown("unjustified_dependency",&check.id,"explain the artifact or correctness dependency; host capacity is not a dependency");
            }
        }
        let mut closure = check.requires.iter().cloned().collect();
        required_checks(&checks, &mut closure);
        if closure.contains(&check.id) {
            out.gap(
                "dependency_cycle",
                &check.id,
                "checks cannot form a dependency cycle",
            );
        }
    }
    let budgets = input.budgets.as_ref().filter(|budgets| {
        !budgets.reference.trim().is_empty()
            && [budgets.focused_ms, budgets.pre_merge_ms, budgets.release_ms]
                .into_iter()
                .all(positive)
    });
    if budgets.is_none() {
        out.unknown(
            "missing_project_budgets",
            "timing",
            "no valid project-specific focused/pre-merge/release limits supplied",
        );
    }
    if runs.is_empty() {
        out.unknown(
            "missing_run_timings",
            "runs",
            "no source-bound native execution evidence supplied",
        );
    }
    let mut tiers = BTreeSet::new();
    for run in runs {
        let receipt = &run.receipt;
        tiers.insert(receipt.tier);
        if let Some(budget) = budgets {
            let limit = match receipt.tier {
                Tier::Development => budget.focused_ms,
                Tier::PreMerge => budget.pre_merge_ms,
                Tier::Release => budget.release_ms,
            };
            if receipt.duration_ms > limit {
                out.gap(
                    "feedback_budget_exceeded",
                    &receipt.run_id,
                    format!(
                        "measured {} ms exceeds project limit {limit} ms",
                        receipt.duration_ms
                    ),
                );
            }
        }
        let mut executions: BTreeMap<(String, String), Vec<&Check>> = BTreeMap::new();
        for observed in &receipt.checks {
            let Some(check) = checks.get(&observed.check_id) else {
                out.gap(
                    "uncategorized_check",
                    &observed.check_id,
                    "executed check is not in the categorized plan",
                );
                continue;
            };
            if check.tier > receipt.tier {
                out.gap(
                    "wrong_execution_tier",
                    &check.id,
                    "a higher-tier check ran in a lower-tier workflow",
                );
            }
            let tests = &run.tests[&observed.check_id];
            if tests.is_empty() && !matches!(check.category, Category::Setup | Category::Static) {
                out.unknown(
                    "empty_test_collection",
                    &check.id,
                    "no native test identities were collected",
                );
            }
            for test in tests {
                executions
                    .entry((test.reference(), check.variant.clone()))
                    .or_default()
                    .push(check);
                if test.attempts > 1 {
                    out.measurements.push(json!({"test":test.reference(),"run_id":receipt.run_id,"attempts":test.attempts,"review":"retries"}));
                }
                if matches!(test.status, TestStatus::Skipped | TestStatus::Collected) {
                    out.unknown(
                        "unexecuted_test",
                        test.reference(),
                        "collected or skipped tests do not prove execution",
                    );
                }
            }
            out.measurements.push(json!({"run_id":receipt.run_id,"check":check.id,"duration_ms":observed.duration_ms,"phase_ms":observed.phase_ms,"category":check.category,"tier":check.tier}));
        }
        for ((test, variant), owners) in executions {
            if owners.len() > 1
                && !owners.iter().any(|owner| {
                    owner
                        .repeat_reason
                        .as_ref()
                        .is_some_and(|reason| !reason.trim().is_empty())
                })
            {
                out.gap(
                    "duplicate_execution",
                    test,
                    format!(
                        "same test and variant {variant:?} selected {} times in one run",
                        owners.len()
                    ),
                );
            }
        }
        let sum = receipt
            .checks
            .iter()
            .map(|check| check.duration_ms)
            .sum::<f64>();
        out.measurements.push(json!({"run_id":receipt.run_id,"environment":receipt.environment,"tier":receipt.tier,"wall_ms":receipt.duration_ms,"check_work_ms":sum,"note":"check durations are concurrent work, not wall time"}));
        for (i, a) in receipt.checks.iter().enumerate() {
            for b in &receipt.checks[i + 1..] {
                let (Some(ac), Some(bc)) = (checks.get(&a.check_id), checks.get(&b.check_id))
                else {
                    continue;
                };
                let mut dependencies = ac.requires.iter().cloned().collect();
                required_checks(&checks, &mut dependencies);
                let mut other = bc.requires.iter().cloned().collect();
                required_checks(&checks, &mut other);
                let conflicting = ac
                    .resources
                    .iter()
                    .any(|resource| bc.resources.contains(resource));
                if dependencies.contains(&bc.id) || other.contains(&ac.id) || conflicting {
                    continue;
                }
                let overlap = (a.start_ms + a.duration_ms).min(b.start_ms + b.duration_ms)
                    - a.start_ms.max(b.start_ms);
                out.measurements.push(json!({"run_id":receipt.run_id,"independent_checks":[a.check_id,b.check_id],"overlap_ms":overlap.max(0.0),"review":if overlap<=0.0{"parallelism"}else{"none"}}));
            }
        }
    }
    for tier in [Tier::Development, Tier::PreMerge, Tier::Release] {
        if !tiers.contains(&tier) {
            out.unknown(
                "unmeasured_tier",
                format!("{tier:?}"),
                "timely execution at this checkpoint has not been measured",
            );
        }
    }
    assess_selection(input, base, expected, out);
    for criterion in [
        "duplication",
        "parallelism",
        "setup",
        "waiting",
        "retries",
        "layering",
        "selection",
    ] {
        review(input, criterion, out);
    }
}

fn assess_ui(
    input: &AssuranceInput,
    base: &Path,
    files: &[FileEntry],
    targets: &[TestTarget],
    runs: &[LoadedRun],
    out: &mut Assessment,
) {
    let interface = files
        .iter()
        .filter(|file| file.interface_relevant)
        .collect::<Vec<_>>();
    let not_applicable = input.reviews.iter().any(|review| {
        review.criterion == "ui_inventory"
            && review.status == Verdict::NotApplicable
            && !review.rationale.trim().is_empty()
            && !review.references.is_empty()
    });
    if (interface.is_empty() || not_applicable) && input.ui_requirements.is_empty() {
        out.status = Verdict::NotApplicable;
        return;
    }
    if input.ui_requirements.is_empty() {
        out.unknown(
            "missing_ui_inventory",
            "UI",
            "declare required journeys, controls, states, themes, viewports and outcomes",
        );
    }
    let mut ids = BTreeSet::new();
    let mut configuration_bindings = BTreeMap::new();
    for target in targets.iter().filter(|target| target.kind == "ui-control") {
        let excluded = input
            .ui_exclusions
            .get(&target.target_id)
            .is_some_and(|reason| {
                !reason.rationale.trim().is_empty() && !reason.reference.trim().is_empty()
            });
        if !excluded
            && !input
                .ui_requirements
                .iter()
                .any(|requirement| requirement.target_ids.contains(&target.target_id))
        {
            out.gap(
                "unmapped_ui_control",
                &target.target_id,
                "every discovered control needs a required interaction or explicit scope review",
            );
        }
    }
    for (id, reason) in &input.ui_exclusions {
        if !targets
            .iter()
            .any(|target| &target.target_id == id && target.kind == "ui-control")
            || reason.rationale.trim().is_empty()
            || reason.reference.trim().is_empty()
        {
            out.gap(
                "invalid_ui_exclusion",
                id,
                "exclusions require an exact discovered control, rationale and reference",
            );
        }
        out.measurements.push(json!({"excluded_control":id,"rationale":reason.rationale,"reference":reason.reference}));
    }
    for requirement in &input.ui_requirements {
        if !ids.insert(&requirement.id) {
            out.gap(
                "duplicate_ui_requirement",
                &requirement.id,
                "UI requirement identities must be unique",
            );
        }
        if [
            &requirement.id,
            &requirement.journey,
            &requirement.state,
            &requirement.theme,
            &requirement.viewport,
            &requirement.interaction,
            &requirement.expected,
            &requirement.reference,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            out.gap(
                "incomplete_ui_requirement",
                &requirement.id,
                "journey, configuration, action, expected outcome and source are required",
            );
        }
        if !files.iter().any(|file| file.rel_path == requirement.file) {
            out.gap(
                "unknown_ui_file",
                &requirement.id,
                "UI source is outside the audited inventory",
            );
        }
        for target in &requirement.target_ids {
            if !targets.iter().any(|candidate| {
                &candidate.target_id == target && candidate.rel_path == requirement.file
            }) {
                out.gap(
                    "unknown_ui_target",
                    &requirement.id,
                    "UI target is missing or belongs to another file",
                );
            }
        }
        let configuration = (&requirement.theme, &requirement.viewport);
        if configuration_bindings
            .insert((&requirement.check_id, &requirement.test), configuration)
            .is_some_and(|previous| previous != configuration)
        {
            out.gap(
                "ambiguous_ui_configuration",
                &requirement.id,
                "different themes/viewports require distinct collected test or check identities",
            );
        }
        let interaction_check = input.checks.iter().any(|check| {
            check.id == requirement.check_id
                && matches!(
                    check.category,
                    Category::Component | Category::Integration | Category::E2e
                )
        });
        let observed = interaction_check
            && runs.iter().any(|run| {
                run.receipt.status == TestStatus::Passed
                    && run.tests.get(&requirement.check_id).is_some_and(|tests| {
                        tests.iter().any(|test| {
                            test.reference() == requirement.test
                                && test.status == TestStatus::Passed
                        })
                    })
            });
        if !observed {
            out.gap(
                "missing_ui_execution",
                &requirement.id,
                "no source-bound passing interaction test matches this required cell",
            );
        }
        if let Some(formal) = &requirement.formal {
            let result: Result<(), String> = (|| {
                let bytes = formal.report.read(base)?;
                formal.review_queue.read(base)?;
                formal.manual_review.read(base)?;
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|_| "invalid formal UI report")?;
                let review = crate::formal_review::validate(
                    &formal.manual_review.resolve(base),
                    &formal.report.resolve(base),
                    &formal.review_queue.resolve(base),
                )?;
                let decision = review["decisions"].as_array().and_then(|rows| {
                    rows.iter()
                        .find(|row| row["reviewCellKey"] == formal.cell_key)
                });
                if !decision.is_some_and(|row| row["decision"] == "pass") {
                    return Err("required formal cell has no passing bound review".into());
                }
                let bound_to_run = runs.iter().any(|run| {
                    run.receipt.status == TestStatus::Passed
                        && run.tests.get(&requirement.check_id).is_some_and(|tests| {
                            tests.iter().any(|test| {
                                test.reference() == requirement.test
                                    && test.status == TestStatus::Passed
                            })
                        })
                        && run
                            .receipt
                            .supporting_artifacts
                            .iter()
                            .any(|artifact| artifact.sha256 == formal.report.sha256)
                });
                if !bound_to_run {
                    return Err(
                        "formal report lacks the interaction run's source/configuration binding"
                            .into(),
                    );
                }
                let page = value["pages"].as_array().and_then(|pages| {
                    pages
                        .iter()
                        .find(|page| page["review"]["reviewCellKey"] == formal.cell_key)
                });
                if !page.is_some_and(|page| {
                    page["outcome"] == "checked"
                        && page["sourceBinding"]["status"] == "matched"
                        && page["target"]["theme"] == requirement.theme
                        && page["findings"].as_array().is_some_and(|findings| {
                            !findings.iter().any(|finding| {
                                matches!(finding["severity"].as_str(), Some("critical" | "serious"))
                            })
                        })
                }) {
                    return Err(
                        "formal page is unbound, stale, incomplete or has blocking findings".into(),
                    );
                }
                let cell = value["review"]["cells"].as_array().and_then(|rows| {
                    rows.iter()
                        .find(|row| row["reviewCellKey"] == formal.cell_key)
                });
                if !cell.is_some_and(|cell| {
                    cell["stateName"] == requirement.state
                        && cell["viewport"]["name"] == requirement.viewport
                }) {
                    return Err("formal cell does not match the required state and viewport".into());
                }
                Ok(())
            })();
            if let Err(error) = result {
                out.gap("formal_ui_evidence", &requirement.id, error);
            }
        }
        out.measurements.push(json!({"requirement":requirement.id,"journey":requirement.journey,"state":requirement.state,"theme":requirement.theme,"viewport":requirement.viewport,"test":requirement.test,"observed":observed}));
    }
    // A reviewer reconciles dynamic routes and controls the source scanner cannot enumerate.
    review(input, "ui_inventory", out);
    review(input, "ui_assertions", out);
}

pub fn evaluate(
    repo: &Path,
    files: &[FileEntry],
    targets: &[TestTarget],
    coverage: &[CoverageEvidence],
    bound: Option<&BoundInput>,
) -> AssuranceReport {
    let mut out = AssuranceReport::default();
    let Some(bound) = bound else {
        for assessment in [&mut out.coverage, &mut out.ui, &mut out.efficiency] {
            assessment.unknown(
                "missing_assurance_input",
                "audit",
                "supply source classifications, native run evidence and reviewed requirements",
            );
            assessment.finish();
        }
        if !files.iter().any(|file| file.interface_relevant) {
            out.ui = Assessment {
                status: Verdict::NotApplicable,
                ..Default::default()
            };
        }
        return out;
    };
    let loaded = (|| {
        let input: AssuranceInput = json_bytes(&bound.artifact.read(repo)?, "assurance input")?;
        if input.schema_version != 1 {
            return Err("unsupported assurance input schema".to_owned());
        }
        Ok(input)
    })();
    let input = match loaded {
        Ok(input) => input,
        Err(error) => {
            out.evidence_issues.push(error);
            for assessment in [&mut out.coverage, &mut out.ui, &mut out.efficiency] {
                assessment.unknown(
                    "invalid_assurance_input",
                    "audit",
                    "assurance evidence cannot be read or verified",
                );
                assessment.finish();
            }
            return out;
        }
    };
    let base = bound.artifact.path.parent().unwrap_or(repo);
    let expected = files
        .iter()
        .map(|file| (file.rel_path.clone(), file.sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut runs = Vec::new();
    let mut run_ids = BTreeSet::new();
    for artifact in &input.runs {
        match load_run(artifact, base, repo, &expected) {
            Ok(run) => {
                if !run_ids.insert(run.receipt.run_id.clone()) {
                    out.evidence_issues.push("duplicate run identity".into());
                    continue;
                }
                for test in run.tests.values().flatten() {
                    out.tests.insert(test.reference(), test.clone());
                }
                runs.push(run);
            }
            Err(error) => out.evidence_issues.push(error),
        }
    }
    for run in runs
        .iter()
        .filter(|run| qualified_run(run) && run.receipt.branches_enabled)
    {
        let covered_files = coverage
            .iter()
            .filter(|record| {
                run.receipt
                    .coverage_reports
                    .iter()
                    .any(|artifact| artifact.sha256 == record.sha256)
            })
            .flat_map(|record| record.files.iter())
            .filter(|(_, lines)| full_lines(lines) && full_branches(lines))
            .map(|(file, _)| file.clone())
            .collect::<BTreeSet<_>>();
        for test in run.tests.values().flatten() {
            out.test_coverage_files
                .entry(test.reference())
                .or_default()
                .extend(covered_files.iter().cloned());
        }
    }
    assess_coverage(&input, files, coverage, &runs, &mut out.coverage);
    assess_ui(&input, base, files, targets, &runs, &mut out.ui);
    assess_efficiency(&input, base, &expected, &runs, &mut out.efficiency);
    for assessment in [&mut out.coverage, &mut out.ui, &mut out.efficiency] {
        if !out.evidence_issues.is_empty() {
            assessment.unknown(
                "invalid_run_evidence",
                "audit",
                "one or more evidence artifacts are invalid or stale",
            );
        }
        assessment.finish();
    }
    out
}

pub fn example(files: &[FileEntry]) -> Value {
    json!({"schema_version":1,"scope":files.iter().map(|file|json!({"file":file.rel_path,"role":if file.kind=="source"{"product"}else{"support"},"rationale":"","reference":""})).collect::<Vec<_>>(),
        "checks":[],"runs":[],"source_dependencies":{},"ui_requirements":[],"ui_exclusions":{},"selection_cases":[],"selection_exemptions":{},"budgets":null,
        "reviews":(["assertions","ui_inventory","ui_assertions","duplication","parallelism","setup","waiting","retries","layering","selection"].map(|criterion|json!({"criterion":criterion,"status":"unproven","rationale":"","references":[]})))})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit_targets::{self, AuditUnit, CoverageBranches};

    struct Fixture {
        dir: tempfile::TempDir,
        files: Vec<FileEntry>,
        input: AssuranceInput,
        coverage: Vec<CoverageEvidence>,
        targets: Vec<TestTarget>,
    }

    fn artifact(root: &Path, name: &str, bytes: &[u8]) -> Artifact {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        Artifact {
            sha256: sha256_file(&path).unwrap(),
            path,
        }
    }

    fn save<T: Serialize>(root: &Path, name: &str, value: &T) -> Artifact {
        artifact(root, name, &serde_json::to_vec(value).unwrap())
    }

    fn file(root: &Path, name: &str, text: &str, interface: bool) -> FileEntry {
        let value = artifact(root, name, text.as_bytes());
        FileEntry {
            rel_path: name.into(),
            sha256: value.sha256,
            size_bytes: text.len(),
            kind: if name.ends_with(".json") {
                "config"
            } else {
                "source"
            }
            .into(),
            interface_relevant: interface,
        }
    }

    fn make_fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![
            file(
                root,
                "src/value.mjs",
                "export function value(flag) { return flag ? 1 : 2; }\n",
                false,
            ),
            file(
                root,
                "tests/value.test.mjs",
                "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport { value } from '../src/value.mjs';\ntest('true value', () => assert.equal(value(true), 1));\ntest('false value', () => assert.equal(value(false), 2));\n",
                false,
            ),
            file(root, "package.json", "{}\n", false),
        ];
        let cover=artifact(root,"coverage.info",b"TN:fixture\nSF:src/value.mjs\nDA:1,1\nBRDA:1,0,0,1\nBRDA:1,0,1,1\nBRF:2\nBRH:2\nend_of_record\n");
        let coverage =
            audit_targets::ingest_coverage_reports(root, std::slice::from_ref(&cover.path))
                .unwrap();
        let source_hashes = files
            .iter()
            .map(|f| (f.rel_path.clone(), f.sha256.clone()))
            .collect::<BTreeMap<_, _>>();
        let reports = save(
            root,
            "native.json",
            &json!({"schema_version":1,"tests":[
                {"file":"tests/value.test.mjs","name":"true value","status":"passed","duration_ms":2.0,"attempts":1},
                {"file":"tests/value.test.mjs","name":"false value","status":"passed","duration_ms":2.0,"attempts":1}
            ]}),
        );
        let mut runs = Vec::new();
        for (name, tier) in [
            ("dev", Tier::Development),
            ("merge", Tier::PreMerge),
            ("release", Tier::Release),
        ] {
            runs.push(save(
                root,
                &format!("{name}.json"),
                &RunReceipt {
                    schema_version: 1,
                    run_id: name.into(),
                    source_hashes: source_hashes.clone(),
                    config_hashes: BTreeMap::from([(
                        "package.json".into(),
                        files[2].sha256.clone(),
                    )]),
                    environment: "isolated synthetic timing fixture".into(),
                    captured_at: "2026-09-10T00:00:00Z".into(),
                    tier,
                    complete: true,
                    collection_complete: true,
                    branches_enabled: true,
                    status: TestStatus::Passed,
                    duration_ms: 10.0,
                    coverage_reports: vec![cover.clone()],
                    supporting_artifacts: vec![],
                    checks: vec![CheckRun {
                        check_id: "unit".into(),
                        start_ms: 0.0,
                        duration_ms: 8.0,
                        status: TestStatus::Passed,
                        reports: vec![NativeReport {
                            artifact: reports.clone(),
                            format: ReportFormat::CollectedTests,
                            source_file: None,
                        }],
                        phase_ms: BTreeMap::from([("setup".into(), 1.0)]),
                    }],
                },
            ));
        }
        let mut selection_cases = Vec::new();
        for (kind, path) in [
            ("local", "src/value.mjs"),
            ("shared", "src/value.mjs"),
            ("fixture", "tests/value.test.mjs"),
            ("config", "package.json"),
            ("unmapped", "new-file.rs"),
        ] {
            let observation = save(
                root,
                &format!("select-{kind}.json"),
                &SelectionObservation {
                    schema_version: 1,
                    base_revision: "fixture-base".into(),
                    source_hashes: source_hashes.clone(),
                    changed_files: vec![path.into()],
                    selected_checks: vec!["unit".into()],
                },
            );
            selection_cases.push(SelectionCase {
                kind: kind.into(),
                observation,
                additional_checks_reason: None,
            });
        }
        let input = AssuranceInput {
            schema_version: 1,
            scope: files
                .iter()
                .map(|f| ScopeEntry {
                    file: f.rel_path.clone(),
                    role: if f.rel_path.starts_with("src/") {
                        SourceRole::Product
                    } else if f.rel_path.starts_with("tests/") {
                        SourceRole::Tests
                    } else {
                        SourceRole::Support
                    },
                    rationale: "Declared fixture source role".into(),
                    reference: "fixture acceptance".into(),
                })
                .collect(),
            checks: vec![Check {
                id: "unit".into(),
                category: Category::Unit,
                tier: Tier::Development,
                variant: String::new(),
                inputs: vec!["src/**".into(), "tests/**".into(), "package.json".into()],
                requires: vec![],
                dependency_reasons: BTreeMap::new(),
                resources: vec![],
                repeat_reason: None,
            }],
            runs,
            source_dependencies: BTreeMap::new(),
            ui_requirements: vec![],
            ui_exclusions: BTreeMap::new(),
            selection_cases,
            selection_exemptions: BTreeMap::from([(
                "ui".into(),
                "This fixture has no rendered UI".into(),
            )]),
            budgets: Some(Budgets {
                reference: "fixture owner budget".into(),
                focused_ms: 20.0,
                pre_merge_ms: 30.0,
                release_ms: 40.0,
            }),
            reviews: [
                "assertions",
                "ui_inventory",
                "ui_assertions",
                "duplication",
                "parallelism",
                "setup",
                "waiting",
                "retries",
                "layering",
                "selection",
            ]
            .into_iter()
            .map(|criterion| Review {
                criterion: criterion.into(),
                status: Verdict::Met,
                rationale: "Reviewed the fixture's required behavior and native evidence".into(),
                references: vec!["tests/value.test.mjs".into()],
            })
            .collect(),
        };
        Fixture {
            dir,
            files,
            input,
            coverage,
            targets: vec![],
        }
    }

    impl Fixture {
        fn evaluate(&self) -> AssuranceReport {
            let path = save(self.dir.path(), "assurance.json", &self.input).path;
            evaluate(
                self.dir.path(),
                &self.files,
                &self.targets,
                &self.coverage,
                Some(&bind(&path).unwrap()),
            )
        }
        fn edit_run(&mut self, index: usize, edit: impl FnOnce(&mut RunReceipt)) {
            let reference = &self.input.runs[index];
            let mut run: RunReceipt =
                json_bytes(&reference.read(self.dir.path()).unwrap(), "fixture receipt").unwrap();
            edit(&mut run);
            let name = reference
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            self.input.runs[index] = save(self.dir.path(), &name, &run);
        }
    }

    #[test]
    fn complete_source_bound_evidence_meets_independent_assessments() {
        let fixture = make_fixture();
        let report = fixture.evaluate();
        assert!(
            report.evidence_issues.is_empty(),
            "{:#?}",
            report.evidence_issues
        );
        assert_eq!(
            report.coverage.status,
            Verdict::Met,
            "{:#?}",
            report.coverage
        );
        assert_eq!(report.ui.status, Verdict::NotApplicable);
        assert_eq!(
            report.efficiency.status,
            Verdict::Met,
            "{:#?}",
            report.efficiency
        );
    }

    #[test]
    fn absent_evidence_or_budget_does_not_become_a_pass() {
        let mut fixture = make_fixture();
        fixture.input.budgets = None;
        assert_eq!(fixture.evaluate().efficiency.status, Verdict::Unproven);
        let report = evaluate(fixture.dir.path(), &fixture.files, &[], &[], None);
        assert_eq!(report.coverage.status, Verdict::Unproven);
        assert_eq!(report.efficiency.status, Verdict::Unproven);
    }

    #[test]
    fn uncovered_branches_omitted_sources_and_partial_runs_cannot_prove_coverage() {
        let mut fixture = make_fixture();
        fixture.coverage[0]
            .files
            .get_mut("src/value.mjs")
            .unwrap()
            .branches = Some(CoverageBranches {
            found: 2,
            hit: 1,
            missing: vec!["1:0:1".into()],
        });
        assert_eq!(fixture.evaluate().coverage.status, Verdict::Gaps);
        fixture.coverage[0].files.remove("src/value.mjs");
        assert_eq!(fixture.evaluate().coverage.status, Verdict::Unproven);
        fixture.edit_run(2, |run| run.complete = false);
        assert_eq!(fixture.evaluate().coverage.status, Verdict::Unproven);
    }

    #[test]
    fn stale_source_config_and_tampered_reports_are_rejected() {
        let mut fixture = make_fixture();
        fixture.edit_run(2, |run| {
            run.source_hashes
                .insert("src/value.mjs".into(), "0".repeat(64));
        });
        assert!(!fixture.evaluate().evidence_issues.is_empty());
        let mut fixture = make_fixture();
        fixture.edit_run(2, |run| {
            run.config_hashes
                .insert("package.json".into(), "0".repeat(64));
        });
        assert!(!fixture.evaluate().evidence_issues.is_empty());
        let fixture = make_fixture();
        std::fs::write(fixture.dir.path().join("native.json"), "{}").unwrap();
        assert!(!fixture.evaluate().evidence_issues.is_empty());
    }

    #[test]
    fn duplicate_execution_is_distinct_from_intentional_variants_and_repeats() {
        let mut fixture = make_fixture();
        let mut extra = fixture.input.checks[0].clone();
        extra.id = "again".into();
        fixture.input.checks.push(extra);
        fixture.edit_run(2, |run| {
            let mut check = run.checks[0].clone();
            check.check_id = "again".into();
            run.checks.push(check);
        });
        assert!(
            fixture
                .evaluate()
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "duplicate_execution")
        );
        fixture.input.checks[1].variant = "different supported platform".into();
        assert!(
            !fixture
                .evaluate()
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "duplicate_execution")
        );
        fixture.input.checks[1].variant.clear();
        fixture.input.checks[1].repeat_reason = Some("Owner-required repeatability check".into());
        assert!(
            !fixture
                .evaluate()
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "duplicate_execution")
        );
    }

    #[test]
    fn budgets_compare_wall_time_without_adding_concurrent_durations() {
        let mut fixture = make_fixture();
        let mut extra = fixture.input.checks[0].clone();
        extra.id = "other".into();
        extra.variant = "other".into();
        fixture.input.checks.push(extra);
        fixture.edit_run(2, |run| {
            let mut check = run.checks[0].clone();
            check.check_id = "other".into();
            run.checks.push(check);
        });
        let report = fixture.evaluate();
        assert!(
            report
                .efficiency
                .measurements
                .iter()
                .any(|row| row["overlap_ms"] == 8.0)
        );
        assert!(
            !report
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "feedback_budget_exceeded")
        );
        fixture.edit_run(2, |run| run.duration_ms = 41.0);
        assert!(
            fixture
                .evaluate()
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "feedback_budget_exceeded")
        );
        fixture.input.checks[0].resources = vec!["exclusive-db".into()];
        fixture.input.checks[1].resources = vec!["exclusive-db".into()];
        assert!(
            !fixture
                .evaluate()
                .efficiency
                .measurements
                .iter()
                .any(|row| row.get("independent_checks").is_some())
        );
    }

    #[test]
    fn selection_follows_dependencies_and_falls_back_for_unmapped_changes() {
        let mut fixture = make_fixture();
        fixture.input.checks[0].inputs = vec!["src/value.mjs".into()];
        let mut build = fixture.input.checks[0].clone();
        build.id = "build".into();
        build.inputs = vec!["package.json".into()];
        build.category = Category::Setup;
        fixture.input.checks[0].requires = vec!["build".into()];
        fixture.input.checks[0]
            .dependency_reasons
            .insert("build".into(), "requires compiled fixture".into());
        fixture.input.checks.push(build);
        fixture
            .input
            .source_dependencies
            .insert("src/value.mjs".into(), vec!["src/shared.mjs".into()]);
        assert_eq!(
            affected_checks(&fixture.input, &["src/shared.mjs".into()]).unwrap(),
            BTreeSet::from(["build".into(), "unit".into()])
        );
        assert_eq!(
            affected_checks(&fixture.input, &["unknown.mjs".into()])
                .unwrap()
                .len(),
            2
        );
        assert!(
            fixture
                .evaluate()
                .efficiency
                .findings
                .iter()
                .any(|f| f.code == "selection_omits_checks")
        );
    }

    #[test]
    fn missing_ui_control_state_or_configuration_evidence_is_visible() {
        let mut fixture = make_fixture();
        let entry = file(
            fixture.dir.path(),
            "src/App.tsx",
            "export function App() {return <><button>Save</button><button>Cancel</button></>;}\n",
            true,
        );
        fixture.files.push(entry);
        fixture.targets = audit_targets::discover_targets(
            fixture.dir.path(),
            &[AuditUnit {
                unit_id: "ui".into(),
                rel_path: "src/App.tsx".into(),
                start_line: None,
                end_line: None,
                start_byte: None,
                interface_relevant: true,
            }],
        );
        let hashes = fixture
            .files
            .iter()
            .map(|f| (f.rel_path.clone(), f.sha256.clone()))
            .collect::<BTreeMap<_, _>>();
        for index in 0..3 {
            fixture.edit_run(index, |run| run.source_hashes = hashes.clone());
        }
        fixture.input.checks[0].category = Category::E2e;
        fixture.input.ui_requirements = fixture
            .targets
            .iter()
            .filter(|target| target.kind == "ui-control")
            .enumerate()
            .map(|(i, target)| UiRequirement {
                id: format!("cell-{i}"),
                file: target.rel_path.clone(),
                journey: "profile".into(),
                state: "base".into(),
                theme: "light".into(),
                viewport: "desktop".into(),
                interaction: target.symbol.clone(),
                expected: "assert actual downstream state".into(),
                test: format!(
                    "tests/value.test.mjs#{}",
                    if i == 0 { "true value" } else { "false value" }
                ),
                check_id: "unit".into(),
                reference: "fixture UI contract".into(),
                target_ids: vec![target.target_id.clone()],
                formal: None,
            })
            .collect();
        assert_eq!(fixture.evaluate().ui.status, Verdict::Met);
        let removed = fixture.input.ui_requirements.pop().unwrap();
        assert!(
            fixture
                .evaluate()
                .ui
                .findings
                .iter()
                .any(|f| f.code == "unmapped_ui_control")
        );
        fixture.input.ui_requirements.push(removed);
        fixture.input.ui_requirements[1].test = "tests/value.test.mjs#unexecuted state".into();
        assert!(
            fixture
                .evaluate()
                .ui
                .findings
                .iter()
                .any(|f| f.code == "missing_ui_execution")
        );
        fixture.input.ui_requirements[1].test = fixture.input.ui_requirements[0].test.clone();
        fixture.input.ui_requirements[1].theme = "dark".into();
        assert!(
            fixture
                .evaluate()
                .ui
                .findings
                .iter()
                .any(|f| f.code == "ambiguous_ui_configuration")
        );
    }

    #[test]
    fn native_formats_preserve_parameterized_identities_outcomes_and_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                ReportFormat::Junit,
                "junit.xml",
                "<testsuite><testcase name=\"case [1]\" time=\"0.01\"/><testcase name=\"case [2]\"><skipped/></testcase></testsuite>",
            ),
            (
                ReportFormat::RustList,
                "list.txt",
                "tests::case_a: test\ntests::case_b: test\n",
            ),
            (
                ReportFormat::RustJson,
                "rust.jsonl",
                "{\"type\":\"test\",\"name\":\"tests::case_a\",\"event\":\"ok\",\"exec_time\":0.01}\n{\"type\":\"test\",\"name\":\"tests::case_b\",\"event\":\"failed\"}\n",
            ),
            (
                ReportFormat::PlaywrightJson,
                "playwright.json",
                r#"{"suites":[{"title":"profile","specs":[{"file":"test.spec.ts","title":"save [1]","tests":[{"projectName":"chromium","results":[{"status":"failed","duration":10},{"status":"passed","duration":11}]}]}]}]}"#,
            ),
        ];
        for (format, name, body) in cases {
            let results = native_tests(
                &NativeReport {
                    artifact: artifact(dir.path(), name, body.as_bytes()),
                    format,
                    source_file: Some("test.spec.ts".into()),
                },
                dir.path(),
            )
            .unwrap();
            assert!(!results.is_empty());
            match format {
                ReportFormat::Junit => {
                    assert_eq!(results[0].name, "case [1]");
                    assert_eq!(results[1].status, TestStatus::Skipped);
                }
                ReportFormat::RustList => {
                    assert!(results.iter().all(|t| t.status == TestStatus::Collected))
                }
                ReportFormat::RustJson => assert_eq!(results[1].status, TestStatus::Failed),
                ReportFormat::PlaywrightJson => {
                    assert_eq!(results[0].name, "profile > save [1] > [chromium]");
                    assert_eq!(results[0].attempts, 2);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn real_node_junit_producer_flows_through_the_native_reader() {
        let fixture = make_fixture();
        let output = std::process::Command::new("node")
            .args(["--test-reporter=junit", "tests/value.test.mjs"])
            .current_dir(fixture.dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reference = artifact(fixture.dir.path(), "real-junit.xml", &output.stdout);
        let tests = native_tests(
            &NativeReport {
                artifact: reference,
                format: ReportFormat::Junit,
                source_file: Some("tests/value.test.mjs".into()),
            },
            fixture.dir.path(),
        )
        .unwrap();
        assert_eq!(tests.len(), 2);
        assert!(tests.iter().all(|test| test.status == TestStatus::Passed));
        assert_eq!(
            tests[0].reference(),
            "tests/value.test.mjs#test::true value"
        );
    }

    #[test]
    fn junit_class_names_preserve_distinct_tests_and_impossible_timings_fail() {
        let dir = tempfile::tempdir().unwrap();
        let artifact=artifact(dir.path(),"classes.xml",br#"<testsuite><testcase name="same" classname="First"/><testcase name="same" classname="Second"/></testsuite>"#);
        let rows = native_tests(
            &NativeReport {
                artifact,
                format: ReportFormat::Junit,
                source_file: Some("tests/example.test.ts".into()),
            },
            dir.path(),
        )
        .unwrap();
        assert_ne!(rows[0].reference(), rows[1].reference());
        let mut fixture = make_fixture();
        fixture.edit_run(2, |run| run.checks[0].duration_ms = 0.0);
        assert!(
            fixture
                .evaluate()
                .evidence_issues
                .iter()
                .any(|issue| issue.contains("native test duration"))
        );
    }
}
