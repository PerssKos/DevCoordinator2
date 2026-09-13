//! Full-repository test-coverage audit queue and verifier.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::audit_common::{
    canonical_value_sha256, duplicate_values, iter_report_files, parse_markdown_table_dicts,
    section_bodies, section_order,
};
use crate::audit_ledger::{
    create_directory_all_nofollow, read_bytes_nofollow, validate_directory_nofollow,
    write_bytes_nofollow,
};
use crate::audit_queue::{self, ArtifactOwnership, AuditUnit, CollectOptions, FileEntry};
use crate::audit_targets::{self, TestTarget};
use crate::test_assurance::{self, AssuranceReport, BoundInput, Verdict};

pub const ARTIFACT_OWNER: &str = "full-repo-test-coverage-audit";
pub const ARTIFACT_MARKER: &str = ".full-repo-test-coverage-audit-artifacts.json";

fn ownership() -> ArtifactOwnership {
    let mut value = ArtifactOwnership {
        owner: ARTIFACT_OWNER.to_owned(),
        marker_name: ARTIFACT_MARKER.to_owned(),
        ..ArtifactOwnership::default()
    };
    value.known_generated_artifacts.extend(
        [
            "ui_test_coverage_audit.md",
            "visual_e2e_coverage_audit.md",
            "review_ledger.json",
            "assurance-input.example.json",
            "assurance-report.json",
            "test_inventory.json",
        ]
        .map(str::to_owned),
    );
    value
}

fn inherited_worker_contract() -> &'static str {
    "## Dispatch Contract\n\nPass this entire prompt and applicable project decisions in a fresh isolated context. Workers inherit the parent settings; do not pass model or reasoning overrides.\n"
}

fn target_units(units: &[AuditUnit]) -> Vec<audit_targets::AuditUnit> {
    units
        .iter()
        .map(|unit| audit_targets::AuditUnit {
            unit_id: unit.unit_id.clone(),
            rel_path: unit.rel_path.clone(),
            start_line: unit.start_line,
            end_line: unit.end_line,
            start_byte: unit.start_byte,
            interface_relevant: unit.interface_relevant,
        })
        .collect()
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    write_bytes_nofollow(path, text.as_bytes(), 0o600).map_err(|error| error.to_string())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn unit_lines(entries: &[AuditUnit]) -> String {
    entries
        .iter()
        .map(|entry| {
            if let (Some(start), Some(end)) = (entry.start_line, entry.end_line) {
                format!(
                    "- Unit `{}`: `{}` lines {start}-{end} ({}, interface={}, sha256=`{}`)",
                    entry.unit_id,
                    entry.rel_path,
                    entry.kind,
                    entry.interface_relevant,
                    entry.sha256
                )
            } else if let (Some(start), Some(end)) = (entry.start_byte, entry.end_byte) {
                format!(
                    "- Unit `{}`: `{}` bytes {start}-{end} ({}, interface={}, sha256=`{}`)",
                    entry.unit_id,
                    entry.rel_path,
                    entry.kind,
                    entry.interface_relevant,
                    entry.sha256
                )
            } else {
                format!(
                    "- Unit `{}`: `{}` ({}, {} bytes, interface={}, sha256=`{}`)",
                    entry.unit_id,
                    entry.rel_path,
                    entry.kind,
                    entry.size_bytes,
                    entry.interface_relevant,
                    entry.sha256
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn batch_prompt(
    repo: &Path,
    run_id: &str,
    index: usize,
    total: usize,
    entries: &[AuditUnit],
    targets: &[TestTarget],
    report: &Path,
) -> Result<String, String> {
    let target_rows = targets
        .iter()
        .map(|target| {
            format!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                target.target_id,
                target.unit_id,
                target.rel_path,
                target.symbol,
                target.kind,
                target.line,
                target.structural_basis
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "# Full Repo Test Coverage Audit Batch {index:03}/{total:03}\n\nRun ID: `{run_id}`\nRepo root: `{}`\nBatch ID: `batch_{index:03}`\n\n{}\n{}\n\nYou are auditing test coverage for this assigned batch. Do not edit the audited repository; write only the exact audit artifact authorized above. Inspect every owned unit and report whether reasonable behavior targets, intended features, UI elements, states, handlers, and journeys have meaningful tests.\n\n## Files You Own\n\n{}\n\nFor ranged units use the exact unit id. Mark every File Coverage row CHECKED.\n\n## Structurally Discovered Targets You Must Map Exactly\n\n| Target ID | Unit | File | Symbol/Behavior | Kind | Line | Discovery Basis |\n| --- | --- | --- | --- | --- | ---: | --- |\n{target_rows}\n\nEvery deterministic target needs exactly one inventory row. Add manual targets only with stable `manual-` ids. Use the shared `test_inventory.json` for discovery and inspect referenced assertion bodies. Structural matching is not empirical coverage. Assess happy, invalid, empty/boundary, failure, async/concurrency, permission, persistence, navigation, rollback, integration, UI-state, and feature-completion cases.\n\n## Required Report File\n\n## Run ID\n{run_id}\n\n## Batch ID\nbatch_{index:03}\n\n## Batch Summary\nBriefly summarize the files.\n\n## File Coverage\n| Unit | Status | SHA-256 | Purpose |\n| --- | --- | --- | --- |\n\n## Test Target Inventory\n| Target ID | Unit | File | Target | Kind | Disposition | Evidence Level | Existing Test Evidence | Scenario Assessment | Recommendation |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n\nUse TESTED/UNTESTED/NOT_REASONABLE and EMPIRICAL/STRUCTURAL/MANUAL/NONE honestly. Real structural/empirical evidence is `test/path#test name`; manual evidence begins `manual:`; untested is `NONE` / `None found`; exclusions begin `not reasonable:`.\n\n## Coverage Findings\nUse `No findings.` or one block per gap with Priority, Files, Target ID, Target, Existing test evidence, Missing scenarios/boundaries, and Suggested test direction. Every UNTESTED target has one Target-ID-bound finding.\n\n## No Gap Notes\nList adequate targets and why.\n\n## Open Questions\nList unresolved ambiguity or `None.`\n",
        repo.display(),
        audit_queue::artifact_delivery_contract(report)?,
        inherited_worker_contract(),
        unit_lines(entries),
    ))
}

fn auxiliary_prompt(
    repo: &Path,
    run_id: &str,
    entries: &[FileEntry],
    report: &Path,
    visual: bool,
) -> Result<String, String> {
    let files = entries
        .iter()
        .map(|entry| {
            format!(
                "- `{}` ({}, sha256=`{}`)",
                entry.rel_path, entry.kind, entry.sha256
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (title, worker, sources, checks) = if visual {
        (
            "Visual And E2E Test Coverage Audit",
            "visual_e2e_coverage",
            "Visual/E2E Tooling",
            "Visual/E2E Coverage Checks",
        )
    } else {
        (
            "UI And User Journey Test Coverage Audit",
            "ui_test_coverage",
            "Journey/Test Sources",
            "UI Coverage Checks",
        )
    };
    Ok(format!(
        "# {title}\n\nRun ID: `{run_id}`\nRepo root: `{}`\nWorker: `{worker}`\n\n{}\n{}\n\nDo not edit the audited repository. Audit intended routes, controls, forms, states, UI elements, feature paths, and journeys for meaningful component, integration, e2e, visual, or fixture-mode coverage. CLI/library packages with no rendered UI may mark visual checks not applicable with evidence.\n\n## Interface-Relevant Files\n\n{files}\n\n## Run ID\n{run_id}\n\n## Worker\n{worker}\n\n## {sources}\nList evidence sources and tooling.\n\n## {checks}\nList journeys, controls, states, features, and existing evidence.\n\n## Findings\nUse `No findings.` or blocks with Priority, Files, Target, Existing test evidence, Missing scenarios/boundaries, and Suggested test direction.\n\n## Open Questions\nList blockers or `None.`\n",
        repo.display(),
        audit_queue::artifact_delivery_contract(report)?,
        inherited_worker_contract(),
    ))
}

fn stale_archive(out: &Path, reports: &Path, stamp: &str) -> Result<Option<PathBuf>, String> {
    let metadata = match reports.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect reports directory: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("reports path must be a non-symlink directory".to_owned());
    }
    if std::fs::read_dir(reports)
        .map_err(|error| error.to_string())?
        .next()
        .is_none()
    {
        return Ok(None);
    }
    let mut suffix = 1usize;
    let mut archive = out.join(format!("reports.stale.{stamp}"));
    while archive.exists() {
        suffix += 1;
        archive = out.join(format!("reports.stale.{stamp}.{suffix}"));
    }
    std::fs::rename(reports, &archive)
        .map_err(|error| format!("cannot archive reports: {error}"))?;
    Ok(Some(archive))
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub repo: PathBuf,
    pub out: PathBuf,
    pub run_id: String,
    pub generated_at: String,
    pub archive_stamp: String,
    pub verifier_program: PathBuf,
    pub batch_size: usize,
    pub max_batch_bytes: usize,
    pub collection: CollectOptions,
    pub coverage_reports: Vec<PathBuf>,
    pub assurance_input: Option<PathBuf>,
}

pub fn build(options: &BuildOptions) -> Result<Value, String> {
    let repo = validate_directory_nofollow(&options.repo).map_err(|error| error.to_string())?;
    let out = options.out.clone();
    let owner = ownership();
    let existing = audit_queue::ensure_output_dir_safe(&out, &repo, &owner)?;
    create_directory_all_nofollow(&out, 0o700).map_err(|error| error.to_string())?;
    if existing.is_none() {
        audit_queue::write_ownership_marker(&out, &repo, &[], &options.generated_at, &owner)?;
    }
    let marker = audit_queue::read_ownership_marker(&out, &owner);
    audit_queue::clean_generated_artifacts(&out, marker.as_ref(), &owner)?;
    let reports = out.join("reports");
    let archive = stale_archive(&out, &reports, &options.archive_stamp)?;
    create_directory_all_nofollow(&reports, 0o700).map_err(|error| error.to_string())?;
    let logs = create_directory_all_nofollow(&out.join("logs"), 0o700)
        .map_err(|error| error.to_string())?;
    let collection = audit_queue::collect_files(&repo, &options.collection);
    let units = audit_queue::audit_units_for(&repo, &collection.entries, options.max_batch_bytes);
    let batches = audit_queue::batch_files(&units, options.batch_size, options.max_batch_bytes)?;
    audit_queue::validate_generated_artifact_tokens(&collection.entries, &units)?;
    let targets = audit_targets::discover_targets(&repo, &target_units(&units));
    let empirical = audit_targets::ingest_coverage_reports(&repo, &options.coverage_reports)?;
    let assurance_input = options
        .assurance_input
        .as_ref()
        .map(|path| {
            test_assurance::bind(&if path.is_absolute() {
                path.clone()
            } else {
                repo.join(path)
            })
        })
        .transpose()?;
    audit_queue::write_json(
        &out.join("assurance-input.example.json"),
        &test_assurance::example(&collection.entries),
    )?;
    let initial_assurance = test_assurance::evaluate(
        &repo,
        &collection.entries,
        &targets,
        &empirical,
        assurance_input.as_ref(),
    );
    audit_queue::write_json(
        &out.join("test_inventory.json"),
        &json!({
            "structural_declarations":crate::test_catalog::source_inventory(&repo,&collection.entries),
            "native_tests":initial_assurance.tests,"evidence_issues":initial_assurance.evidence_issues,
            "scope":"source declarations and source-bound native collection; assertions require review"
        }),
    )?;
    let mut targets_by_unit: BTreeMap<&str, Vec<&TestTarget>> = BTreeMap::new();
    for target in &targets {
        targets_by_unit
            .entry(&target.unit_id)
            .or_default()
            .push(target);
    }
    let mut batch_records = Vec::new();
    let mut all_paths = Vec::new();
    let mut all_units = Vec::new();
    for (offset, batch) in batches.iter().enumerate() {
        let index = offset + 1;
        let name = format!("batch_{index:03}.md");
        let batch_targets = batch
            .iter()
            .flat_map(|unit| {
                targets_by_unit
                    .get(unit.unit_id.as_str())
                    .into_iter()
                    .flatten()
            })
            .map(|target| (*target).clone())
            .collect::<Vec<_>>();
        write_text(
            &out.join(&name),
            &batch_prompt(
                &repo,
                &options.run_id,
                index,
                batches.len(),
                batch,
                &batch_targets,
                &reports.join(&name),
            )?,
        )?;
        let paths = batch
            .iter()
            .map(|unit| unit.rel_path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let unit_ids = batch
            .iter()
            .map(|unit| unit.unit_id.clone())
            .collect::<Vec<_>>();
        all_paths.extend(paths.iter().cloned());
        all_units.extend(unit_ids.iter().cloned());
        batch_records.push(json!({
            "id":format!("batch_{index:03}"),"prompt":name,
            "report":format!("reports/batch_{index:03}.md"),"file_count":paths.len(),
            "coverage_unit_count":batch.len(),
            "interface_file_count":batch.iter().filter(|unit| unit.interface_relevant).count(),
            "byte_count":batch.iter().map(|unit| unit.size_bytes).sum::<usize>(),
            "files":paths,"coverage_units":unit_ids,"purpose":audit_queue::purpose_for(batch),
        }));
    }
    let source_paths = collection
        .entries
        .iter()
        .map(|entry| entry.rel_path.clone())
        .collect::<BTreeSet<_>>();
    let unit_ids = units
        .iter()
        .map(|unit| unit.unit_id.clone())
        .collect::<BTreeSet<_>>();
    let batched_paths = all_paths.iter().cloned().collect::<BTreeSet<_>>();
    let batched_units = all_units.iter().cloned().collect::<BTreeSet<_>>();
    let duplicate_units = duplicate_values(&all_units);
    let scope_warnings = collection
        .excluded
        .iter()
        .filter(|entry| entry.get("scope_warning") == Some(&json!(true)))
        .cloned()
        .collect::<Vec<_>>();
    let pruned_hints = collection
        .excluded
        .iter()
        .filter(|entry| {
            entry.get("entry_type") == Some(&json!("directory"))
                && entry.get("contains_source_like_samples") == Some(&json!(true))
        })
        .cloned()
        .collect::<Vec<_>>();
    let interface = collection
        .entries
        .iter()
        .filter(|entry| entry.interface_relevant)
        .cloned()
        .collect::<Vec<_>>();
    let ui_required = !interface.is_empty();
    if ui_required {
        write_text(
            &out.join("ui_test_coverage_audit.md"),
            &auxiliary_prompt(
                &repo,
                &options.run_id,
                &interface,
                &reports.join("ui_test_coverage_audit.md"),
                false,
            )?,
        )?;
    }
    let verifier_args = vec![
        options.verifier_program.to_string_lossy().into_owned(),
        "audit".to_owned(),
        "test-coverage".to_owned(),
        "verify".to_owned(),
        "--manifest".to_owned(),
        out.join("manifest.json").to_string_lossy().into_owned(),
        "--reports".to_owned(),
        reports.to_string_lossy().into_owned(),
    ];
    let mut generated = vec![
        "audit_index.md",
        "review_ledger.json",
        "assurance-input.example.json",
        "assurance-report.json",
        "test_inventory.json",
        "excluded_files.json",
        "manifest.json",
        "queue_complete.json",
        "final-report.md",
        "logs",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if ui_required {
        generated.extend(["ui_test_coverage_audit.md".to_owned()]);
    }
    generated.extend(
        archive
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned()),
    );
    generated.extend(
        batch_records
            .iter()
            .filter_map(|batch| batch["prompt"].as_str().map(str::to_owned)),
    );
    let missing_paths = source_paths
        .difference(&batched_paths)
        .cloned()
        .collect::<Vec<_>>();
    let extra_paths = batched_paths
        .difference(&source_paths)
        .cloned()
        .collect::<Vec<_>>();
    let missing_units = unit_ids
        .difference(&batched_units)
        .cloned()
        .collect::<Vec<_>>();
    let extra_units = batched_units
        .difference(&unit_ids)
        .cloned()
        .collect::<Vec<_>>();
    let all_units_once =
        missing_units.is_empty() && extra_units.is_empty() && duplicate_units.is_empty();
    let manifest = json!({
        "repo_root":repo.to_string_lossy(),"run_id":options.run_id,"audit_kind":"test-coverage","assurance_version":2,
        "generated_at":options.generated_at,"reports_dir":reports.to_string_lossy(),
        "logs_dir":logs.to_string_lossy(),"final_report":out.join("final-report.md").to_string_lossy(),
        "archived_reports_dir":archive.as_ref().map(|path|path.to_string_lossy().into_owned()),
        "artifact_marker":out.join(ARTIFACT_MARKER).to_string_lossy(),
        "review_ledger":out.join("review_ledger.json").to_string_lossy(),
        "generated_artifacts":generated,"verifier_command":verifier_args.iter().map(|arg|shell_quote(arg)).collect::<Vec<_>>().join(" "),
        "verifier_args":verifier_args,"source_file_count":collection.entries.len(),
        "interface_file_count":interface.len(),"scope_warning_count":scope_warnings.len(),
        "pruned_directory_review_hint_count":pruned_hints.len(),
        "excluded_file_count":collection.excluded.len(),
        "excluded_files_sha256":canonical_value_sha256(&Value::Array(collection.excluded.clone()))?,
        "batch_count":batches.len(),"source_files":collection.entries,
        "coverage_unit_count":units.len(),"coverage_units":units,"batches":batch_records,
        "test_coverage_audit":{
            "ui_required":ui_required,"interface_files":interface.iter().map(|entry|entry.rel_path.clone()).collect::<Vec<_>>(),
            "ui_prompt":if ui_required {json!("ui_test_coverage_audit.md")} else {Value::Null},
            "ui_report":if ui_required {json!("reports/ui_test_coverage_audit.md")} else {Value::Null},
            "visual_prompt":Value::Null,
            "visual_report":Value::Null,
            "target_count":targets.len(),"target_inventory":targets,
            "empirical_coverage":empirical,
            "assurance_input":assurance_input,
            "coverage_claim_scope":if options.coverage_reports.is_empty() {
                "structural/manual audit only; no runtime coverage evidence supplied"
            } else {"runtime measurements supplied; source-bound line/branch, UI and efficiency verdicts are evaluated separately"},
        },
        "coverage_invariants":{
            "unique_batched_file_count":batched_paths.len(),"unique_batched_unit_count":batched_units.len(),
            "missing_from_batches":missing_paths,
            "duplicates_in_batches":audit_queue::duplicate_whole_file_paths_for_batches(&batches),
            "extra_in_batches":extra_paths,"missing_units_from_batches":missing_units,
            "duplicate_units_in_batches":duplicate_units,"extra_units_in_batches":extra_units,
            "all_coverage_units_queued_exactly_once":all_units_once,
            "all_source_files_queued_exactly_once":all_units_once && source_paths == batched_paths,
        },
        "scope_warnings":scope_warnings,"pruned_directory_review_hints":pruned_hints,
    });
    audit_queue::write_json(&out.join("manifest.json"), &manifest)?;
    audit_queue::write_json(
        &out.join("excluded_files.json"),
        &Value::Array(collection.excluded.clone()),
    )?;
    write_support_artifacts(&out, &repo, &manifest, &owner, &options.generated_at)?;
    Ok(manifest)
}

fn write_support_artifacts(
    out: &Path,
    repo: &Path,
    manifest: &Value,
    owner: &ArtifactOwnership,
    generated_at: &str,
) -> Result<(), String> {
    let coverage = &manifest["test_coverage_audit"];
    let ui_required = coverage["ui_required"] == true;
    let batch_workers = manifest["batches"].as_array().into_iter().flatten().map(|batch| {
        let id = batch["id"].as_str().unwrap_or("");
        json!({"batch_id":id,"status":"pending","prompt":batch["prompt"],
            "report":format!("reports/{id}.md"),"agent_id":Value::Null,"runtime_provenance":Value::Null})
    }).collect::<Vec<_>>();
    let ledger = json!({
        "run_id":manifest["run_id"],"repo_root":manifest["repo_root"],"audit_kind":"test-coverage",
        "provenance_scope":"review assignments and completion; settings are inherited",
        "lead_review":{"status":"pending","agent_id":Value::Null,"runtime_provenance":Value::Null},
        "fallback":{"status":"not-started","reason":""},
        "ui_test_coverage_worker":{
            "status":if ui_required {"pending"} else {"not-applicable"},
            "prompt":coverage["ui_prompt"],"report":coverage["ui_report"],
            "agent_id":Value::Null,"runtime_provenance":Value::Null
        },
        "batch_workers":batch_workers,
        "pruned_directory_review":{
            "status":if manifest["pruned_directory_review_hint_count"].as_u64().unwrap_or(0)>0 {"pending"} else {"not-applicable"},
            "hint_count":manifest["pruned_directory_review_hint_count"],"decisions":[]
        }
    });
    audit_queue::write_json(&out.join("review_ledger.json"), &ledger)?;
    let marker = json!({
        "run_id":manifest["run_id"],"phase":"queue_generated","audit_verified":false,
        "audit_kind":"test-coverage","manifest":"manifest.json","audit_index":"audit_index.md",
        "review_ledger":"review_ledger.json","excluded_files":"excluded_files.json",
        "reports_dir":"reports","logs_dir":"logs","final_report":"final-report.md",
        "ownership_marker":ARTIFACT_MARKER,"batch_count":manifest["batch_count"],
        "source_file_count":manifest["source_file_count"],
        "marker_semantics":"Queue artifacts were generated; worker reports and review ledger still require verifier completion.",
    });
    audit_queue::write_json(&out.join("queue_complete.json"), &marker)?;
    let rows = manifest["batches"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|batch| {
            format!(
                "| {} | `{}` | {} | {} | {} | {} |",
                batch["id"].as_str().unwrap_or(""),
                batch["prompt"].as_str().unwrap_or(""),
                batch["file_count"],
                batch["coverage_unit_count"],
                batch["interface_file_count"],
                batch["purpose"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let extra = if ui_required {
        format!(
            "- Coordinated UI coverage prompt: {} -> {}",
            coverage["ui_prompt"].as_str().unwrap_or(""),
            coverage["ui_report"].as_str().unwrap_or("")
        )
    } else {
        "- No interface-relevant files were queued.".to_owned()
    };
    let index = format!(
        "# Full Repo Test Coverage Audit Index\n\nRepo root: `{}`\nOutput directory: `{}`\nRun ID: `{}`\nAudit kind: `test-coverage`\n\nSource files queued: **{}**\nCoverage units queued: **{}**\nBatches: **{}**\nScope warnings: **{}**\n\n## Dispatch\n\n1. Fill `review_ledger.json` as workers are assigned.\n2. Dispatch one fresh isolated worker per batch prompt inheriting parent settings and the full prompt plus project-ledger requirements.\n3. Workers save complete reports and return only bounded `REPORT_SAVED` receipts.\n4. Dispatch the coordinated UI reviewer when listed.\n5. Run verifier: `{}`\n6. Keep verbose output in `logs/`, synthesis in `final-report.md`, and chat compact.\n\n{extra}\n\n## Batches\n\n| Batch | Prompt | Files | Units | Interface Files | Purpose |\n| --- | --- | ---: | ---: | ---: | --- |\n{rows}\n",
        repo.display(),
        out.display(),
        manifest["run_id"].as_str().unwrap_or(""),
        manifest["source_file_count"],
        manifest["coverage_unit_count"],
        manifest["batch_count"],
        manifest["scope_warning_count"],
        manifest["verifier_command"].as_str().unwrap_or("")
    );
    write_text(&out.join("audit_index.md"), &index)?;
    let generated = manifest["generated_artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    audit_queue::write_ownership_marker(out, repo, &generated, generated_at, owner)
}

#[derive(Clone, Debug)]
struct VerifyContext {
    raw: Map<String, Value>,
    path: PathBuf,
    root: PathBuf,
    repo: PathBuf,
    source_hashes: BTreeMap<String, String>,
    unit_to_file: BTreeMap<String, String>,
    unit_hashes: BTreeMap<String, String>,
    batches: Vec<Value>,
    expected_by_batch: BTreeMap<String, BTreeSet<String>>,
    files_by_batch: BTreeMap<String, BTreeSet<String>>,
    targets: BTreeMap<String, Value>,
    targets_by_batch: BTreeMap<String, BTreeSet<String>>,
    empirical_lines: BTreeMap<String, BTreeSet<usize>>,
    empirical_issues: Vec<Value>,
    assurance: AssuranceReport,
    declared_tests: BTreeSet<String>,
}

fn load_verify_context(path: &Path) -> Result<VerifyContext, String> {
    let root = path
        .parent()
        .ok_or_else(|| "manifest has no parent".to_owned())?;
    let root = validate_directory_nofollow(root).map_err(|error| error.to_string())?;
    let bytes = read_bytes_nofollow(path, Some(&root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("manifest is missing: {}", path.display()))?;
    let raw = crate::audit_findings::strict_json_object(&bytes, "manifest")?;
    if raw.get("audit_kind") != Some(&json!("test-coverage")) {
        return Err("manifest audit_kind must be 'test-coverage'.".to_owned());
    }
    if raw
        .get("assurance_version")
        .is_some_and(|version| version != &json!(2))
    {
        return Err("unsupported assurance manifest version".to_owned());
    }
    let repo_text = raw
        .get("repo_root")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest repo_root must be a string".to_owned())?;
    let repo =
        validate_directory_nofollow(Path::new(repo_text)).map_err(|error| error.to_string())?;
    let source_values = raw
        .get("source_files")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest source_files must be a list.".to_owned())?;
    let mut source_hashes = BTreeMap::new();
    for (index, item) in source_values.iter().enumerate() {
        let item = item
            .as_object()
            .ok_or_else(|| format!("source_files[{index}] must be an object"))?;
        let rel = item
            .get("rel_path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("source_files[{index}] must contain rel_path."))?;
        audit_queue::validate_repo_relative_path_token(rel, "source file")?;
        let sha = item
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| format!("source_files[{index}].sha256 must be a SHA-256 hex digest."))?;
        if source_hashes
            .insert(rel.to_owned(), sha.to_owned())
            .is_some()
        {
            return Err(format!(
                "source_files rel_path values must be unique: {rel}"
            ));
        }
    }
    let unit_values = raw
        .get("coverage_units")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest coverage_units must be a list.".to_owned())?;
    let mut unit_to_file = BTreeMap::new();
    let mut unit_hashes = BTreeMap::new();
    for (index, unit) in unit_values.iter().enumerate() {
        let unit = unit
            .as_object()
            .ok_or_else(|| format!("coverage_units[{index}] must be an object"))?;
        let id = unit
            .get("unit_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("coverage_units[{index}] must contain unit_id"))?;
        let rel = unit
            .get("rel_path")
            .and_then(Value::as_str)
            .filter(|rel| source_hashes.contains_key(*rel))
            .ok_or_else(|| {
                format!("coverage_units[{index}].rel_path is absent from source_files")
            })?;
        if unit_to_file.insert(id.to_owned(), rel.to_owned()).is_some() {
            return Err(format!(
                "coverage_units unit_id values must be unique: {id}"
            ));
        }
        let sha = unit
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or(&source_hashes[rel]);
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "coverage_units[{index}].sha256 must be a SHA-256 digest"
            ));
        }
        unit_hashes.insert(id.to_owned(), sha.to_owned());
    }
    let batches = raw
        .get("batches")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "manifest batches must be a list.".to_owned())?;
    let mut expected_by_batch = BTreeMap::new();
    let mut files_by_batch = BTreeMap::new();
    let mut assigned = Vec::new();
    let mut batch_by_unit = BTreeMap::new();
    for (index, batch) in batches.iter().enumerate() {
        let object = batch
            .as_object()
            .ok_or_else(|| format!("batches[{index}] must be an object"))?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("batches[{index}] must contain id."))?;
        let units = object
            .get("coverage_units")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("batches[{index}].coverage_units must be a list"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "batch unit must be text".to_owned())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if units.iter().any(|unit| !unit_to_file.contains_key(unit)) {
            return Err(format!("batch {id} references unknown coverage units"));
        }
        let files = object
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("batches[{index}].files must be a list"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "batch file must be text".to_owned())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        for unit in &units {
            assigned.push(unit.clone());
            batch_by_unit.insert(unit.clone(), id.to_owned());
        }
        if expected_by_batch.insert(id.to_owned(), units).is_some() {
            return Err(format!("duplicate batch id: {id}"));
        }
        files_by_batch.insert(id.to_owned(), files);
    }
    let assigned_set = assigned.iter().cloned().collect::<BTreeSet<_>>();
    let all_units = unit_to_file.keys().cloned().collect::<BTreeSet<_>>();
    if assigned_set != all_units || !duplicate_values(&assigned).is_empty() {
        return Err("coverage unit assignment mismatch".to_owned());
    }
    let coverage = raw
        .get("test_coverage_audit")
        .and_then(Value::as_object)
        .ok_or_else(|| "manifest test_coverage_audit must be an object.".to_owned())?;
    let target_values = coverage
        .get("target_inventory")
        .and_then(Value::as_array)
        .ok_or_else(|| "test_coverage_audit.target_inventory must be a list.".to_owned())?;
    let mut targets = BTreeMap::new();
    let mut targets_by_batch = expected_by_batch
        .keys()
        .map(|id| (id.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for (index, target) in target_values.iter().enumerate() {
        let object = target
            .as_object()
            .ok_or_else(|| format!("target_inventory[{index}] must be an object."))?;
        let id = object
            .get("target_id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("target-"))
            .ok_or_else(|| format!("target_inventory[{index}].target_id must be deterministic"))?;
        let unit = object
            .get("unit_id")
            .and_then(Value::as_str)
            .filter(|unit| unit_to_file.contains_key(*unit))
            .ok_or_else(|| format!("target_inventory[{index}] must bind an exact unit"))?;
        if object.get("rel_path").and_then(Value::as_str) != Some(&unit_to_file[unit])
            || object
                .get("symbol")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || object.get("line").and_then(Value::as_u64).is_none()
        {
            return Err(format!("target_inventory[{index}] is incomplete"));
        }
        if targets.insert(id.to_owned(), target.clone()).is_some() {
            return Err(format!("target_inventory target ids must be unique: {id}"));
        }
        targets_by_batch
            .get_mut(&batch_by_unit[unit])
            .expect("batch exists")
            .insert(id.to_owned());
    }
    if coverage.get("target_count").and_then(Value::as_u64) != Some(targets.len() as u64) {
        return Err("test_coverage_audit.target_count does not match target_inventory.".to_owned());
    }
    let mut empirical_lines: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut empirical_issues = Vec::new();
    for (index, record) in coverage
        .get("empirical_coverage")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(record) = record.as_object() else {
            empirical_issues
                .push(json!({"record":index,"reason":"coverage evidence must be an object"}));
            continue;
        };
        let evidence_path = record
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        if evidence_path
            .as_ref()
            .is_none_or(|path| !path.is_absolute())
        {
            empirical_issues.push(json!({"record":index,"field":"path","reason":"coverage evidence path must be absolute"}));
        } else if let Some(evidence_path) = evidence_path {
            match read_bytes_nofollow(&evidence_path, None) {
                Ok(Some(bytes)) => {
                    let digest = crate::audit_common::sha256_file(&evidence_path).unwrap_or_default();
                    if record.get("sha256").and_then(Value::as_str) != Some(&digest) || bytes.is_empty() {
                        empirical_issues.push(json!({"record":index,"field":"sha256","reason":"coverage evidence hash changed"}));
                    }
                }
                _ => empirical_issues.push(json!({"record":index,"field":"path","reason":"coverage evidence path must be an existing regular file"})),
            }
        }
        if !matches!(
            record.get("format").and_then(Value::as_str),
            Some("lcov" | "cobertura-xml" | "coverage.py-json" | "istanbul-json")
        ) {
            empirical_issues.push(json!({"record":index,"field":"format","reason":"unsupported empirical coverage format"}));
        }
        let Some(files) = record.get("files").and_then(Value::as_object) else {
            empirical_issues
                .push(json!({"record":index,"field":"files","reason":"must be an object"}));
            continue;
        };
        for (rel, lines) in files {
            let Some(lines) = lines
                .as_object()
                .filter(|_| source_hashes.contains_key(rel))
            else {
                empirical_issues.push(json!({"record":index,"file":rel,"reason":"coverage file is not a manifest source or line data is invalid"}));
                continue;
            };
            let measured = positive_lines(lines.get("measured_lines"));
            let covered = positive_lines(lines.get("covered_lines"));
            match (measured, covered) {
                (Some(measured), Some(covered)) if covered.is_subset(&measured) => {
                    empirical_lines.entry(rel.clone()).or_default().extend(covered);
                }
                _ => empirical_issues.push(json!({"record":index,"file":rel,"reason":"covered lines must be positive and a subset of measured lines"})),
            }
        }
    }
    let records: Vec<audit_targets::CoverageEvidence> = serde_json::from_value(
        coverage
            .get("empirical_coverage")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|error| format!("invalid coverage evidence: {error}"))?;
    if raw.get("assurance_version") == Some(&json!(2)) {
        for record in &records {
            match audit_targets::ingest_coverage_reports(&repo, &[PathBuf::from(&record.path)]) {
                Ok(actual) if actual.first() == Some(record) => {}
                _ => empirical_issues.push(
                    json!({"reason":"normalized coverage differs from its original artifact"}),
                ),
            }
        }
    }
    let bound: Option<BoundInput> = serde_json::from_value(
        coverage
            .get("assurance_input")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| format!("invalid assurance binding: {error}"))?;
    let files: Vec<FileEntry> = serde_json::from_value(raw["source_files"].clone())
        .map_err(|error| format!("invalid source inventory: {error}"))?;
    let assurance_targets: Vec<TestTarget> =
        serde_json::from_value(coverage["target_inventory"].clone())
            .map_err(|error| format!("invalid test target inventory: {error}"))?;
    let assurance =
        test_assurance::evaluate(&repo, &files, &assurance_targets, &records, bound.as_ref());
    let declared_tests = crate::test_catalog::source_inventory(&repo, &files);
    Ok(VerifyContext {
        raw,
        path: root.join(path.file_name().unwrap_or_default()),
        root,
        repo,
        source_hashes,
        unit_to_file,
        unit_hashes,
        batches,
        expected_by_batch,
        files_by_batch,
        targets,
        targets_by_batch,
        empirical_lines,
        empirical_issues,
        assurance,
        declared_tests,
    })
}

fn positive_lines(value: Option<&Value>) -> Option<BTreeSet<usize>> {
    value?
        .as_array()?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|line| *line > 0)
                .and_then(|line| usize::try_from(line).ok())
        })
        .collect()
}

fn first_value(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .trim_matches('`')
        .to_owned()
}

fn finding_blocks(text: &str) -> Vec<BTreeMap<String, String>> {
    if text.trim() == "No findings." {
        return Vec::new();
    }
    let field = Regex::new(r"^-\s+([^:]+):\s*(.*)$").expect("constant finding regex");
    let mut blocks = Vec::new();
    let mut current = BTreeMap::new();
    for line in text.lines() {
        let Some(captures) = field.captures(line.trim()) else {
            continue;
        };
        let key = captures[1].trim().to_owned();
        if key == "Priority" && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.insert(key, captures[2].trim().to_owned());
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn finding_issues(
    text: &str,
    allowed: &BTreeSet<String>,
    path: &Path,
    section: &str,
) -> Vec<Value> {
    if text.trim().is_empty() {
        return vec![json!({"path":path,"section":section,"reason":"findings section is empty"})];
    }
    if text.trim() == "No findings." {
        return Vec::new();
    }
    let blocks = finding_blocks(text);
    if blocks.is_empty() {
        return vec![
            json!({"path":path,"section":section,"reason":"findings must use required field blocks or exact sentinel"}),
        ];
    }
    let required = [
        "Priority",
        "Files",
        "Target",
        "Existing test evidence",
        "Missing scenarios/boundaries",
        "Suggested test direction",
    ];
    let mut issues = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let missing = required
            .iter()
            .filter(|field| block.get(**field).is_none_or(String::is_empty))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            issues.push(
                json!({"path":path,"section":section,"finding":index+1,"missing_fields":missing}),
            );
        }
        if block
            .get("Priority")
            .is_some_and(|value| !matches!(value.as_str(), "P0" | "P1" | "P2" | "P3"))
        {
            issues
                .push(json!({"path":path,"section":section,"finding":index+1,"field":"Priority"}));
        }
        let files = block
            .get("Files")
            .map(|value| value.replace('`', ""))
            .unwrap_or_default()
            .split([',', ';'])
            .map(str::trim)
            .filter(|value| {
                !value.is_empty()
                    && !matches!(value.to_lowercase().as_str(), "none" | "not applicable")
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let unknown = files.difference(allowed).cloned().collect::<Vec<_>>();
        if files.is_empty() || !unknown.is_empty() {
            issues.push(json!({"path":path,"section":section,"finding":index+1,"field":"Files","out_of_scope":unknown}));
        }
    }
    issues
}

fn test_reference_issue(repo: &Path, value: &str, legacy: bool) -> Option<Value> {
    let normalized = value.trim().trim_matches('`');
    let Some((raw_path, symbol)) = normalized.split_once('#') else {
        return Some(json!({
            "reason":"structural/empirical test evidence must be real test/path#test-symbol",
            "actual":value,
        }));
    };
    if symbol.trim().is_empty()
        || audit_queue::validate_repo_relative_path_token(raw_path, "test evidence").is_err()
    {
        return Some(json!({"reason":"test evidence path#symbol is invalid","actual":value}));
    }
    let path = repo.join(raw_path);
    let text = read_bytes_nofollow(&path, Some(repo))
        .ok()
        .flatten()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    match text {
        None => Some(
            json!({"reason":"test evidence path does not resolve inside the audited repo","actual":value}),
        ),
        Some(text)
            if !(if legacy {
                audit_queue::is_test_source_path(raw_path) && text.contains(symbol.trim())
            } else {
                crate::test_catalog::declares_test(raw_path, &text, symbol.trim())
            }) =>
        {
            Some(
                json!({"reason":"test name is not an unambiguous supported test declaration; supply native collection evidence for dynamic or unsupported tests","actual":value}),
            )
        }
        Some(_) => None,
    }
}

fn verify_batch_report(
    path: &Path,
    context: &VerifyContext,
    batch_id: &str,
) -> Result<Vec<Value>, String> {
    let bytes = read_bytes_nofollow(path, Some(&context.root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("report missing: {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| "report is not UTF-8".to_owned())?;
    let bodies = section_bodies(&text);
    let expected_sections = [
        "run id",
        "batch id",
        "batch summary",
        "file coverage",
        "test target inventory",
        "coverage findings",
        "no gap notes",
        "open questions",
    ];
    let mut issues = Vec::new();
    if section_order(&text) != expected_sections {
        issues.push(json!({"path":path,"reason":"batch report sections must match required order","expected":expected_sections,"actual":section_order(&text)}));
    }
    if first_value(bodies.get("run id").map(String::as_str).unwrap_or(""))
        != context.raw["run_id"].as_str().unwrap_or("")
    {
        issues.push(json!({"path":path,"field":"Run ID"}));
    }
    if first_value(bodies.get("batch id").map(String::as_str).unwrap_or("")) != batch_id {
        issues.push(json!({"path":path,"field":"Batch ID","expected":batch_id}));
    }
    let expected_units = &context.expected_by_batch[batch_id];
    let expected_files = &context.files_by_batch[batch_id];
    let coverage_rows = parse_markdown_table_dicts(
        bodies
            .get("file coverage")
            .map(String::as_str)
            .unwrap_or(""),
    );
    if coverage_rows.is_empty() {
        issues
            .push(json!({"path":path,"section":"File Coverage","reason":"missing coverage table"}));
    }
    let covered = coverage_rows
        .iter()
        .filter_map(|row| row.get("unit"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = expected_units
        .difference(&covered)
        .cloned()
        .collect::<Vec<_>>();
    let extra = covered
        .difference(expected_units)
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() || !extra.is_empty() {
        issues.push(json!({"path":path,"section":"File Coverage","missing_units":missing,"extra_units":extra}));
    }
    for row in &coverage_rows {
        let unit = row.get("unit").map(String::as_str).unwrap_or("");
        if row.get("status").map(String::as_str) != Some("CHECKED") {
            issues
                .push(json!({"path":path,"section":"File Coverage","unit":unit,"field":"Status"}));
        }
        if context.unit_hashes.get(unit).map(String::as_str)
            != row.get("sha-256").map(String::as_str)
        {
            issues
                .push(json!({"path":path,"section":"File Coverage","unit":unit,"field":"SHA-256"}));
        }
        if row
            .get("purpose")
            .is_none_or(|value| value.trim().is_empty())
        {
            issues
                .push(json!({"path":path,"section":"File Coverage","unit":unit,"field":"Purpose"}));
        }
    }
    let inventory = parse_markdown_table_dicts(
        bodies
            .get("test target inventory")
            .map(String::as_str)
            .unwrap_or(""),
    );
    let columns = [
        "target id",
        "unit",
        "file",
        "target",
        "kind",
        "disposition",
        "evidence level",
        "existing test evidence",
        "scenario assessment",
        "recommendation",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if inventory.is_empty() {
        issues.push(json!({"path":path,"section":"Test Target Inventory","reason":"missing target inventory table"}));
    } else if inventory[0]
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != columns
    {
        issues.push(json!({"path":path,"section":"Test Target Inventory","reason":"target inventory headers must exactly expose disposition and evidence level"}));
    }
    let expected_targets = &context.targets_by_batch[batch_id];
    let observed = inventory
        .iter()
        .filter_map(|row| row.get("target id"))
        .cloned()
        .collect::<Vec<_>>();
    let observed_set = observed.iter().cloned().collect::<BTreeSet<_>>();
    let missing_targets = expected_targets
        .difference(&observed_set)
        .cloned()
        .collect::<Vec<_>>();
    let manual_re = Regex::new(r"^manual-[A-Za-z0-9_-]{4,80}$").expect("manual target regex");
    let extra_targets = observed_set
        .difference(expected_targets)
        .filter(|id| !manual_re.is_match(id))
        .cloned()
        .collect::<Vec<_>>();
    let duplicate_targets = duplicate_values(&observed);
    if !missing_targets.is_empty() || !extra_targets.is_empty() || !duplicate_targets.is_empty() {
        issues.push(json!({
            "path":path,"section":"Test Target Inventory",
            "reason":"every deterministic target needs exactly one tested/untested/not-reasonable mapping",
            "missing_targets":missing_targets,"extra_targets":extra_targets,"duplicate_targets":duplicate_targets,
        }));
    }
    let mut untested = BTreeSet::new();
    for row in &inventory {
        let target_id = row.get("target id").map(String::as_str).unwrap_or("");
        let unit = row.get("unit").map(String::as_str).unwrap_or("");
        let file = row.get("file").map(String::as_str).unwrap_or("");
        if !expected_units.contains(unit) {
            issues.push(json!({"path":path,"section":"Test Target Inventory","unit":unit,"reason":"unit is outside this batch"}));
        }
        if !expected_files.contains(file)
            || context
                .unit_to_file
                .get(unit)
                .is_some_and(|expected| expected != file)
        {
            issues.push(json!({"path":path,"section":"Test Target Inventory","file":file,"reason":"file is outside this batch or mismatched to unit"}));
        }
        if let Some(target) = context.targets.get(target_id) {
            for (field, expected) in [
                ("unit", &target["unit_id"]),
                ("file", &target["rel_path"]),
                ("target", &target["symbol"]),
                ("kind", &target["kind"]),
            ] {
                if row.get(field).map(|value| json!(value)) != Some(expected.clone()) {
                    issues.push(json!({"path":path,"target_id":target_id,"field":field,"expected":expected}));
                }
            }
        }
        for field in [
            "target",
            "kind",
            "disposition",
            "evidence level",
            "existing test evidence",
            "scenario assessment",
            "recommendation",
        ] {
            if row.get(field).is_none_or(|value| value.trim().is_empty()) {
                issues.push(
                    json!({"path":path,"target_id":target_id,"field":field,"reason":"empty"}),
                );
            }
        }
        let disposition = row.get("disposition").map(String::as_str).unwrap_or("");
        let level = row.get("evidence level").map(String::as_str).unwrap_or("");
        let evidence = row
            .get("existing test evidence")
            .map(String::as_str)
            .unwrap_or("");
        if !matches!(disposition, "TESTED" | "UNTESTED" | "NOT_REASONABLE") {
            issues.push(json!({"path":path,"target_id":target_id,"field":"Disposition"}));
        }
        if !matches!(level, "EMPIRICAL" | "STRUCTURAL" | "MANUAL" | "NONE") {
            issues.push(json!({"path":path,"target_id":target_id,"field":"Evidence Level"}));
        }
        match disposition {
            "TESTED" if matches!(level, "STRUCTURAL" | "EMPIRICAL") => {
                if !context.declared_tests.contains(evidence) && !context.assurance.tests.contains_key(evidence) && let Some(issue) = test_reference_issue(&context.repo, evidence, context.raw.get("assurance_version").is_none()) {
                    issues.push(json!({"path":path,"target_id":target_id,"detail":issue}));
                }
                if level == "EMPIRICAL" && context.raw.get("assurance_version") == Some(&json!(2)) {
                    let measured = context.assurance.test_coverage_files.get(evidence).is_some_and(|files| files.contains(file));
                    let passed = context.assurance.tests.get(evidence).is_some_and(|test|test.status==test_assurance::TestStatus::Passed);
                    if !measured || !passed {issues.push(json!({"path":path,"target_id":target_id,"reason":"EMPIRICAL requires passing native test evidence and source-bound complete line and branch measurements; a declaration-line hit is insufficient"}));}
                } else if level == "EMPIRICAL"
                    && context.targets.get(target_id).is_some_and(|target| {
                        let line = target["line"].as_u64().unwrap_or(0) as usize;
                        let rel = target["rel_path"].as_str().unwrap_or("");
                        !context.empirical_lines.get(rel).is_some_and(|lines| lines.contains(&line))
                    })
                {
                    issues.push(json!({"path":path,"target_id":target_id,"reason":"EMPIRICAL claim is not backed by supplied coverage at target line"}));
                }
            }
            "TESTED" if level == "MANUAL" => {
                if !evidence.to_lowercase().starts_with("manual:")
                    || evidence.split_once(':').map(|(_, value)| value.trim().len()).unwrap_or(0) < 12
                {
                    issues.push(json!({"path":path,"target_id":target_id,"reason":"manual TESTED claims require concrete manual evidence"}));
                }
            }
            "TESTED" => issues.push(json!({"path":path,"target_id":target_id,"reason":"TESTED targets cannot use NONE evidence"})),
            "UNTESTED" => {
                untested.insert(target_id.to_owned());
                if level != "NONE" || !evidence.eq_ignore_ascii_case("None found") {
                    issues.push(json!({"path":path,"target_id":target_id,"reason":"UNTESTED targets must use NONE / None found honestly"}));
                }
            }
            "NOT_REASONABLE" => {
                if level != "MANUAL"
                    || !evidence.to_lowercase().starts_with("not reasonable:")
                    || evidence.split_once(':').map(|(_, value)| value.trim().len()).unwrap_or(0) < 12
                {
                    issues.push(json!({"path":path,"target_id":target_id,"reason":"NOT_REASONABLE requires MANUAL rationale"}));
                }
            }
            _ => {}
        }
    }
    let findings = bodies
        .get("coverage findings")
        .map(String::as_str)
        .unwrap_or("");
    issues.extend(finding_issues(
        findings,
        expected_files,
        path,
        "Coverage Findings",
    ));
    let blocks = finding_blocks(findings);
    let finding_targets = blocks
        .iter()
        .filter_map(|block| block.get("Target ID"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing_findings = untested
        .difference(&finding_targets)
        .cloned()
        .collect::<Vec<_>>();
    if !missing_findings.is_empty() {
        issues.push(json!({"path":path,"reason":"every UNTESTED target requires a finding bound by Target ID","missing_targets":missing_findings}));
    }
    if bodies
        .get("batch summary")
        .is_none_or(|value| value.trim().is_empty())
        || bodies
            .get("no gap notes")
            .is_none_or(|value| value.trim().is_empty())
    {
        issues
            .push(json!({"path":path,"reason":"Batch Summary and No Gap Notes must not be empty"}));
    }
    Ok(issues)
}

fn verify_aux(
    path: &Path,
    context: &VerifyContext,
    sections: &[&str],
    worker: &str,
) -> Result<Vec<Value>, String> {
    let bytes = read_bytes_nofollow(path, Some(&context.root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "worker report missing".to_owned())?;
    let text = String::from_utf8(bytes).map_err(|_| "worker report not UTF-8".to_owned())?;
    let bodies = section_bodies(&text);
    let mut issues = Vec::new();
    if section_order(&text) != sections {
        issues
            .push(json!({"path":path,"reason":"worker report sections must match required order"}));
    }
    if first_value(bodies.get("run id").map(String::as_str).unwrap_or(""))
        != context.raw["run_id"].as_str().unwrap_or("")
        || first_value(bodies.get("worker").map(String::as_str).unwrap_or("")) != worker
    {
        issues.push(json!({"path":path,"reason":"worker or run id mismatch"}));
    }
    issues.extend(finding_issues(
        bodies.get("findings").map(String::as_str).unwrap_or(""),
        &context.source_hashes.keys().cloned().collect(),
        path,
        "Findings",
    ));
    for section in sections {
        if bodies
            .get(*section)
            .is_none_or(|value| value.trim().is_empty())
        {
            issues.push(json!({"path":path,"section":section,"reason":"empty"}));
        }
    }
    Ok(issues)
}

fn marker_issues(context: &VerifyContext) -> Vec<Value> {
    let path = context.root.join("queue_complete.json");
    let marker = read_bytes_nofollow(&path, Some(&context.root))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(Value::Object(marker)) = marker else {
        return vec![json!({"path":path,"reason":"queue_complete.json is missing or invalid"})];
    };
    let expected = [
        ("run_id", context.raw["run_id"].clone()),
        ("phase", json!("queue_generated")),
        ("audit_verified", json!(false)),
        ("audit_kind", json!("test-coverage")),
        ("manifest", json!("manifest.json")),
        ("audit_index", json!("audit_index.md")),
        if context.raw.get("assurance_version") == Some(&json!(2)) {
            ("review_ledger", json!("review_ledger.json"))
        } else {
            ("effort_ledger", json!("effort_ledger.json"))
        },
        ("excluded_files", json!("excluded_files.json")),
        ("reports_dir", json!("reports")),
        ("ownership_marker", json!(ARTIFACT_MARKER)),
        ("batch_count", context.raw["batch_count"].clone()),
        (
            "source_file_count",
            context.raw["source_file_count"].clone(),
        ),
    ];
    expected
        .into_iter()
        .filter_map(|(field, expected)| {
            (marker.get(field) != Some(&expected)).then(|| {
                json!({"path":path,"field":field,"expected":expected,"actual":marker.get(field)})
            })
        })
        .collect()
}

fn excluded_issues(context: &VerifyContext) -> Vec<Value> {
    let path = context.root.join("excluded_files.json");
    let values = read_bytes_nofollow(&path, Some(&context.root))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(Value::Array(values)) = values else {
        return vec![json!({"path":path,"reason":"excluded_files.json is missing or invalid"})];
    };
    let mut issues = Vec::new();
    if context.raw["excluded_file_count"].as_u64() != Some(values.len() as u64) {
        issues.push(json!({"path":path,"field":"excluded_file_count"}));
    }
    if canonical_value_sha256(&Value::Array(values.clone()))
        .ok()
        .as_deref()
        != context.raw["excluded_files_sha256"].as_str()
    {
        issues.push(json!({"path":path,"field":"excluded_files_sha256"}));
    }
    let warnings = values
        .iter()
        .filter(|value| value.get("scope_warning") == Some(&json!(true)))
        .cloned()
        .collect::<Vec<_>>();
    if !warnings.is_empty() {
        issues.push(
            json!({"path":path,"reason":"unresolved scope warnings","scope_warnings":warnings}),
        );
    }
    issues
}

fn review_issues(context: &VerifyContext) -> Vec<Value> {
    let modern = context.raw.get("assurance_version") == Some(&json!(2));
    let path = context.root.join(if modern {
        "review_ledger.json"
    } else {
        "effort_ledger.json"
    });
    let ledger = read_bytes_nofollow(&path, Some(&context.root))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(Value::Object(ledger)) = ledger else {
        return vec![json!({"path":path,"reason":"review ledger is missing or invalid"})];
    };
    let mut issues = Vec::new();
    if ledger.get("run_id") != context.raw.get("run_id") {
        issues.push(json!({"path":path,"field":"run_id"}));
    }
    let complete = |row: Option<&Value>| {
        row.is_some_and(|row| {
            matches!(
                row["status"].as_str(),
                Some("completed" | "confirmed" | "manual-fallback-completed")
            ) && row["agent_id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty())
                && row["runtime_provenance"]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty())
        })
    };
    if !complete(ledger.get(if modern { "lead_review" } else { "lead_effort" })) {
        issues.push(json!({"path":path,"reason":"lead review needs completed, confirmed or manual-fallback-completed status with nonempty agent_id and runtime_provenance"}));
    }
    let workers = ledger
        .get("batch_workers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for batch in &context.batches {
        let id = batch["id"].as_str().unwrap_or("");
        let rows = workers
            .iter()
            .filter(|row| row["batch_id"] == id)
            .collect::<Vec<_>>();
        if rows.len() != 1 || !complete(rows.first().copied()) {
            issues.push(json!({"path":path,"batch_id":id,"reason":"batch review needs one completed, confirmed or manual-fallback-completed row with nonempty agent_id and runtime_provenance"}));
        }
    }
    if context.raw["test_coverage_audit"]["ui_required"] == true {
        for name in ["ui_test_coverage_worker", "visual_e2e_coverage_worker"] {
            if modern && name == "visual_e2e_coverage_worker" {
                continue;
            }
            if !complete(ledger.get(name)) {
                issues.push(json!({"path":path,"field":name,"reason":"UI review is incomplete"}));
            }
        }
    }
    issues
}

fn current_hash_issues(context: &VerifyContext) -> Vec<Value> {
    context
        .source_hashes
        .iter()
        .filter_map(|(rel, expected)| {
            let bytes = read_bytes_nofollow(&context.repo.join(rel), Some(&context.repo))
                .ok()
                .flatten();
            match bytes {
                None => Some(json!({"path":rel,"reason":"source file is missing"})),
                Some(_) => {
                    let actual = crate::audit_common::sha256_file(&context.repo.join(rel)).unwrap_or_default();
                    (actual != *expected).then(|| json!({"path":rel,"expected":expected,"actual":actual,"reason":"current source hash differs from manifest"}))
                }
            }
        })
        .collect()
}

pub fn verify(
    manifest: &Path,
    report_inputs: &[PathBuf],
    skip_current_hash_check: bool,
) -> Result<Value, String> {
    let context = load_verify_context(manifest)?;
    let reports = iter_report_files(report_inputs)?;
    let expected_root = context.root.join("reports");
    let mut reports_by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for report in reports {
        reports_by_name
            .entry(
                report
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            )
            .or_default()
            .push(report);
    }
    let mut missing_reports = Vec::new();
    let mut report_issues = Vec::new();
    for batch in &context.batches {
        let id = batch["id"].as_str().unwrap_or("");
        let name = format!("{id}.md");
        let candidates = reports_by_name.get(&name).cloned().unwrap_or_default();
        if candidates.len() != 1 {
            missing_reports.push(json!({"report":name,"count":candidates.len()}));
        } else if candidates[0] != expected_root.join(&name) {
            report_issues.push(
                json!({"path":candidates[0],"reason":"report must use exact manifest-owned path"}),
            );
        } else {
            report_issues.extend(verify_batch_report(&candidates[0], &context, id)?);
        }
    }
    let coverage = &context.raw["test_coverage_audit"];
    if coverage["ui_required"] == true {
        for (name, sections, worker) in [
            (
                "ui_test_coverage_audit.md",
                [
                    "run id",
                    "worker",
                    "journey/test sources",
                    "ui coverage checks",
                    "findings",
                    "open questions",
                ],
                "ui_test_coverage",
            ),
            (
                "visual_e2e_coverage_audit.md",
                [
                    "run id",
                    "worker",
                    "visual/e2e tooling",
                    "visual/e2e coverage checks",
                    "findings",
                    "open questions",
                ],
                "visual_e2e_coverage",
            ),
        ] {
            if context.raw.get("assurance_version") == Some(&json!(2))
                && name == "visual_e2e_coverage_audit.md"
            {
                continue;
            }
            let candidates = reports_by_name.get(name).cloned().unwrap_or_default();
            if candidates.len() != 1 {
                missing_reports.push(json!({"report":name,"count":candidates.len()}));
            } else if candidates[0] != expected_root.join(name) {
                report_issues.push(json!({"path":candidates[0],"reason":"report must use exact manifest-owned path"}));
            } else {
                report_issues.extend(verify_aux(&candidates[0], &context, &sections, worker)?);
            }
        }
    }
    let issues = json!({
        "completion_marker_mismatches":marker_issues(&context),
        "excluded_file_issues":excluded_issues(&context),
        "review_ledger_issues":review_issues(&context),
        "missing_reports":missing_reports,"report_issues":report_issues,
        "current_hash_mismatches":if skip_current_hash_check {Vec::new()} else {current_hash_issues(&context)},
        "empirical_coverage_issues":context.empirical_issues,
        "assurance_evidence_issues":context.assurance.evidence_issues,
    });
    let ok = issues
        .as_object()
        .expect("issues object")
        .values()
        .all(|value| value.as_array().is_some_and(Vec::is_empty));
    let assurance_path = context.root.join("assurance-report.json");
    if context.raw.get("assurance_version") == Some(&json!(2)) {
        audit_queue::write_json(
            &assurance_path,
            &serde_json::to_value(&context.assurance).map_err(|error| error.to_string())?,
        )?;
    }
    let status = |assessment: &test_assurance::Assessment| {
        if skip_current_hash_check && assessment.status == Verdict::Met {
            Verdict::Unproven
        } else {
            assessment.status
        }
    };
    let coverage_status = status(&context.assurance.coverage);
    let ui_status = status(&context.assurance.ui);
    let efficiency_status = status(&context.assurance.efficiency);
    let assurance_met = context.raw.get("assurance_version") == Some(&json!(2))
        && ok
        && coverage_status == Verdict::Met
        && matches!(ui_status, Verdict::Met | Verdict::NotApplicable)
        && efficiency_status == Verdict::Met;
    Ok(json!({
        "ok":ok,"manifest":context.path.to_string_lossy(),
        "run_id":context.raw["run_id"],"issues":issues,
        "assurance_met":assurance_met,
        "assurance":{"coverage":coverage_status,"ui":ui_status,"efficiency":efficiency_status,
            "source_check_skipped":skip_current_hash_check,"report":if context.raw.get("assurance_version")==Some(&json!(2)){Some(assurance_path)}else{None}},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn regression_test_references_resolve_declarations_not_comments_or_filenames() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("tests/math.test.ts"),
            "// test('adds', () => {});\n",
        );
        assert!(test_reference_issue(dir.path(), "tests/math.test.ts#adds", false).is_some());
        write(
            &dir.path().join("src/lib.rs"),
            "#[cfg(test)] mod tests {\n#[test]\nfn adds() { assert_eq!(1 + 1, 2); }\n}\n",
        );
        assert!(test_reference_issue(dir.path(), "src/lib.rs#adds", false).is_none());
        write(
            &dir.path().join("tests/example_test.go"),
            "func TestWorks(t *testing.T) { /* legacy source reference */ }\n",
        );
        assert!(
            test_reference_issue(dir.path(), "tests/example_test.go#TestWorks", true).is_none()
        );
        assert!(
            test_reference_issue(dir.path(), "tests/example_test.go#TestWorks", false).is_some()
        );
    }

    #[test]
    fn regression_new_queues_do_not_select_or_record_agent_effort() {
        let fixture = fixture(true, Vec::new());
        let ledger = std::fs::read_to_string(fixture.out.join("review_ledger.json")).unwrap();
        assert!(!ledger.contains("effort"));
        assert!(!fixture.out.join("visual_e2e_coverage_audit.md").exists());
        let prompt = std::fs::read_to_string(fixture.out.join("batch_001.md")).unwrap();
        assert!(!prompt.contains("low-effort"));
        assert!(!prompt.contains("runtime/user-selected effort"));
    }

    #[test]
    fn legacy_review_ledgers_ignore_obsolete_effort_without_gaining_assurance() {
        let fixture = fixture(true, Vec::new());
        let mut manifest = write_reports(&fixture);
        let path = fixture.out.join("review_ledger.json");
        let mut ledger: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger["lead_effort"] = ledger["lead_review"].take();
        ledger["lead_effort"]["actual_reasoning_effort"] = json!("inherited");
        ledger["visual_e2e_coverage_worker"] = ledger["ui_test_coverage_worker"].clone();
        for worker in ledger["batch_workers"].as_array_mut().unwrap() {
            worker
                .as_object_mut()
                .unwrap()
                .remove("actual_reasoning_effort");
        }
        audit_queue::write_json(&fixture.out.join("effort_ledger.json"), &ledger).unwrap();
        manifest
            .as_object_mut()
            .unwrap()
            .remove("assurance_version");
        audit_queue::write_json(&fixture.out.join("manifest.json"), &manifest).unwrap();
        let marker_path = fixture.out.join("queue_complete.json");
        let mut marker: Value =
            serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
        marker.as_object_mut().unwrap().remove("review_ledger");
        marker["effort_ledger"] = json!("effort_ledger.json");
        audit_queue::write_json(&marker_path, &marker).unwrap();
        let result = verify(
            &fixture.out.join("manifest.json"),
            &[fixture.out.join("reports")],
            false,
        )
        .unwrap();
        assert_eq!(result["ok"], true, "{result:#}");
        assert_eq!(result["assurance_met"], false);
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        repo: PathBuf,
        out: PathBuf,
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(repo: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    fn fixture(ui: bool, coverage: Vec<PathBuf>) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        let out = directory.path().join("out");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        if ui {
            write(
                &repo.join("src/math.ts"),
                "export function clamp(value: number, min: number, max: number) {\n  if (min > max) throw new Error('invalid');\n  return Math.max(min, Math.min(max, value));\n}\n\nexport async function loadUser(id: string) { return id; }\n",
            );
            write(
                &repo.join("src/App.tsx"),
                "export function App() { return <button>Save profile</button>; }\n",
            );
            write(
                &repo.join("tests/math.test.ts"),
                "test('clamp returns in-range values', () => { expect(true).toBe(true); });\n",
            );
            write(
                &repo.join("package.json"),
                "{\"scripts\":{\"test\":\"vitest\"}}\n",
            );
        } else {
            write(
                &repo.join("src/tool.py"),
                "def normalize_name(value: str) -> str:\n    return value.strip().lower()\n",
            );
        }
        git(&repo, &["add", "-A"]);
        build(&BuildOptions {
            repo: repo.clone(),
            out: out.clone(),
            run_id: "selftest-run".to_owned(),
            generated_at: "2026-09-04T00:00:00Z".to_owned(),
            archive_stamp: "20260904T000000Z".to_owned(),
            verifier_program: PathBuf::from("/usr/local/bin/devcoordinator2-tooling"),
            batch_size: 8,
            max_batch_bytes: 60_000,
            collection: CollectOptions {
                include_config: true,
                ..Default::default()
            },
            coverage_reports: coverage,
            assurance_input: None,
        })
        .unwrap();
        Fixture {
            _directory: directory,
            repo,
            out,
        }
    }

    fn complete_ledger(out: &Path) {
        let path = out.join("review_ledger.json");
        let mut ledger: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger["subagent_capability_check"] = json!({
            "status":"completed","spawn_tool":"self-test","can_set_reasoning_effort":true,"notes":"fixture"
        });
        ledger["lead_review"] = json!({
            "required_reasoning_effort":"high-or-higher","actual_reasoning_effort":"high",
            "status":"completed","agent_id":"self-test-lead",
            "runtime_provenance":"self-test fixture","evidence":"self-test fixture"
        });
        for worker in ledger["batch_workers"].as_array_mut().unwrap() {
            worker["status"] = json!("completed");
            worker["agent_id"] = json!("self-test");
            worker["actual_reasoning_effort"] = json!("low");
            worker["runtime_provenance"] = json!("self-test fixture");
        }
        for key in ["ui_test_coverage_worker", "visual_e2e_coverage_worker"] {
            if ledger[key]["status"] == "pending" {
                ledger[key]["status"] = json!("completed");
                ledger[key]["agent_id"] = json!("self-test");
                ledger[key]["actual_reasoning_effort"] = json!("low");
                ledger[key]["runtime_provenance"] = json!("self-test fixture");
            }
        }
        if ledger["pruned_directory_review"]["status"] == "pending" {
            ledger["pruned_directory_review"]["status"] = json!("completed");
        }
        audit_queue::write_json(&path, &ledger).unwrap();
    }

    fn batch_report(manifest: &Value, batch: &Value) -> String {
        let units = manifest["coverage_units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| {
                (
                    unit["unit_id"].as_str().unwrap(),
                    unit["sha256"].as_str().unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let owned = batch["coverage_units"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        let coverage_rows = owned
            .iter()
            .map(|unit| {
                format!(
                    "| {unit} | CHECKED | {} | Source/test coverage unit |",
                    units[unit]
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut inventory = Vec::new();
        let mut findings = Vec::new();
        for target in manifest["test_coverage_audit"]["target_inventory"]
            .as_array()
            .unwrap()
        {
            if !owned.contains(target["unit_id"].as_str().unwrap()) {
                continue;
            }
            let id = target["target_id"].as_str().unwrap();
            let symbol = target["symbol"].as_str().unwrap();
            let kind = target["kind"].as_str().unwrap();
            let rel = target["rel_path"].as_str().unwrap();
            let unit = target["unit_id"].as_str().unwrap();
            let (disposition, level, evidence, assessment, recommendation) = if symbol == "clamp" {
                (
                    "TESTED",
                    "STRUCTURAL",
                    "tests/math.test.ts#clamp returns in-range values",
                    "Happy path exists; invalid ranges and boundaries are missing",
                    "Add boundary and thrown-error unit tests",
                )
            } else if kind == "unit-review" {
                (
                    "NOT_REASONABLE",
                    "MANUAL",
                    "Not reasonable: supporting fixture or config has no independently executable behavior target",
                    "Reviewed structurally as support-only in this fixture",
                    "No direct target test; owning behavior covers this support file",
                )
            } else {
                findings.push(format!(
                    "- Priority: P1\n- Files: {rel}\n- Target ID: {id}\n- Target: {symbol}\n- Existing test evidence: None found\n- Missing scenarios/boundaries: happy path, invalid input, failure behavior, and state transitions\n- Suggested test direction: add focused tests with observable result assertions"
                ));
                (
                    "UNTESTED",
                    "NONE",
                    "None found",
                    "No bound test path or runtime evidence; important scenarios are absent",
                    "Add focused unit or component coverage with observable assertions",
                )
            };
            inventory.push(format!(
                "| {id} | {unit} | {rel} | {symbol} | {kind} | {disposition} | {level} | {evidence} | {assessment} | {recommendation} |"
            ));
        }
        format!(
            "## Run ID\n{}\n\n## Batch ID\n{}\n\n## Batch Summary\nFixture batch used by the Rust self-test.\n\n## File Coverage\n| Unit | Status | SHA-256 | Purpose |\n| --- | --- | --- | --- |\n{coverage_rows}\n\n## Test Target Inventory\n| Target ID | Unit | File | Target | Kind | Disposition | Evidence Level | Existing Test Evidence | Scenario Assessment | Recommendation |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n{}\n\n## Coverage Findings\n{}\n\n## No Gap Notes\nSupporting files were mapped to findings or justified as non-targets.\n\n## Open Questions\nNone.\n",
            manifest["run_id"].as_str().unwrap(),
            batch["id"].as_str().unwrap(),
            inventory.join("\n"),
            if findings.is_empty() {
                "No findings.".to_owned()
            } else {
                findings.join("\n\n")
            },
        )
    }

    fn write_reports(fixture: &Fixture) -> Value {
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(fixture.out.join("manifest.json")).unwrap())
                .unwrap();
        complete_ledger(&fixture.out);
        for batch in manifest["batches"].as_array().unwrap() {
            let id = batch["id"].as_str().unwrap();
            write(
                &fixture.out.join(format!("reports/{id}.md")),
                &batch_report(&manifest, batch),
            );
        }
        if manifest["test_coverage_audit"]["ui_required"] == true {
            write(
                &fixture.out.join("reports/ui_test_coverage_audit.md"),
                &format!(
                    "## Run ID\n{}\n\n## Worker\nui_test_coverage\n\n## Journey/Test Sources\nsrc/App.tsx and tests/math.test.ts.\n\n## UI Coverage Checks\nSave profile lacks component evidence.\n\n## Findings\n- Priority: P1\n- Files: src/App.tsx\n- Target: Save profile button\n- Existing test evidence: None found\n- Missing scenarios/boundaries: click behavior, status update, and failure state\n- Suggested test direction: add a component or e2e test for the save path\n\n## Open Questions\nNone.\n",
                    manifest["run_id"].as_str().unwrap()
                ),
            );
            write(
                &fixture.out.join("reports/visual_e2e_coverage_audit.md"),
                &format!(
                    "## Run ID\n{}\n\n## Worker\nvisual_e2e_coverage\n\n## Visual/E2E Tooling\nNo safe browser harness exists in this fixture.\n\n## Visual/E2E Coverage Checks\nThe profile screen has no render evidence.\n\n## Findings\n- Priority: P2\n- Files: src/App.tsx\n- Target: profile screen visual state\n- Existing test evidence: None found\n- Missing scenarios/boundaries: desktop and mobile render checks\n- Suggested test direction: add lightweight visual and e2e smoke checks\n\n## Open Questions\nNone.\n",
                    manifest["run_id"].as_str().unwrap()
                ),
            );
        }
        manifest
    }

    #[test]
    fn complete_ui_and_cli_audits_verify_and_preserve_manifest_contracts() {
        for ui in [true, false] {
            let fixture = fixture(ui, Vec::new());
            let manifest = write_reports(&fixture);
            assert_eq!(manifest["audit_kind"], "test-coverage");
            assert_eq!(manifest["test_coverage_audit"]["ui_required"], ui);
            assert!(
                manifest["verifier_args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|value| !value.as_str().unwrap_or("").contains("python"))
            );
            assert!(fixture.out.join("logs").is_dir());
            let result = verify(
                &fixture.out.join("manifest.json"),
                &[fixture.out.join("reports")],
                false,
            )
            .unwrap();
            assert_eq!(result["ok"], true, "{result:#}");
        }
    }

    #[test]
    fn missing_target_bad_reference_weak_exclusion_and_review_fail() {
        let fixture = fixture(true, Vec::new());
        let manifest = write_reports(&fixture);
        let batch = fixture.out.join("reports/batch_001.md");
        let original = std::fs::read_to_string(&batch).unwrap();
        let load_target = manifest["test_coverage_audit"]["target_inventory"]
            .as_array()
            .unwrap()
            .iter()
            .find(|target| target["symbol"] == "loadUser")
            .unwrap()["target_id"]
            .as_str()
            .unwrap();
        write(
            &batch,
            &original
                .lines()
                .filter(|line| !line.contains(load_target))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(
            verify(
                &fixture.out.join("manifest.json"),
                &[fixture.out.join("reports")],
                false
            )
            .unwrap()["ok"],
            false
        );
        write(
            &batch,
            &original.replace(
                "tests/math.test.ts#clamp returns in-range values",
                "tests/math.test.ts#invented symbol",
            ),
        );
        let invalid = verify(
            &fixture.out.join("manifest.json"),
            &[fixture.out.join("reports")],
            false,
        )
        .unwrap();
        assert!(
            invalid
                .to_string()
                .contains("unambiguous supported test declaration")
        );
        write(&batch, &original.replace("Not reasonable: supporting fixture or config has no independently executable behavior target", "Not reasonable: trivial"));
        assert!(
            verify(
                &fixture.out.join("manifest.json"),
                &[fixture.out.join("reports")],
                false
            )
            .unwrap()
            .to_string()
            .contains("NOT_REASONABLE")
        );
        write(&batch, &original);
        let ledger_path = fixture.out.join("review_ledger.json");
        let mut ledger: Value =
            serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
        ledger["batch_workers"][0]["agent_id"] = Value::Null;
        audit_queue::write_json(&ledger_path, &ledger).unwrap();
        assert!(
            !verify(
                &fixture.out.join("manifest.json"),
                &[fixture.out.join("reports")],
                false
            )
            .unwrap()["issues"]["review_ledger_issues"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empirical_claims_require_untampered_line_evidence_and_sources_stay_fresh() {
        let directory = tempfile::tempdir().unwrap();
        let coverage = directory.path().join("coverage.info");
        // Build once to learn the eventual repository path, then use a second fixture for empirical proof.
        let fixture = fixture(true, Vec::new());
        write(
            &coverage,
            &format!(
                "TN:self-test\nSF:{}\nDA:1,1\nDA:2,0\nend_of_record\n",
                fixture.repo.join("src/math.ts").display()
            ),
        );
        // Rebuild the same owned output with coverage; the builder safely cleans its generated files.
        build(&BuildOptions {
            repo: fixture.repo.clone(),
            out: fixture.out.clone(),
            run_id: "selftest-run".to_owned(),
            generated_at: "2026-09-04T00:01:00Z".to_owned(),
            archive_stamp: "20260904T000100Z".to_owned(),
            verifier_program: PathBuf::from("/usr/local/bin/devcoordinator2-tooling"),
            batch_size: 8,
            max_batch_bytes: 60_000,
            collection: CollectOptions {
                include_config: true,
                ..Default::default()
            },
            coverage_reports: vec![coverage.clone()],
            assurance_input: None,
        })
        .unwrap();
        write_reports(&fixture);
        let clean = verify(
            &fixture.out.join("manifest.json"),
            &[fixture.out.join("reports")],
            false,
        )
        .unwrap();
        assert_eq!(clean["ok"], true, "{clean:#}");
        write(&coverage, "changed\n");
        let stale = verify(
            &fixture.out.join("manifest.json"),
            &[fixture.out.join("reports")],
            false,
        )
        .unwrap();
        assert!(
            !stale["issues"]["empirical_coverage_issues"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        write(
            &fixture.repo.join("src/math.ts"),
            "export function changed() { return 1; }\n",
        );
        let drift = verify(
            &fixture.out.join("manifest.json"),
            &[fixture.out.join("reports")],
            false,
        )
        .unwrap();
        assert!(
            !drift["issues"]["current_hash_mismatches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_reports_and_scope_warnings_block_completion() {
        let fixture = fixture(true, Vec::new());
        write_reports(&fixture);
        std::fs::remove_file(fixture.out.join("reports/batch_001.md")).unwrap();
        assert!(
            !verify(
                &fixture.out.join("manifest.json"),
                &[fixture.out.join("reports")],
                false
            )
            .unwrap()["issues"]["missing_reports"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        let out = directory.path().join("out");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        write(&repo.join("src/tool.py"), "def tool():\n    return 1\n");
        git(&repo, &["add", "-A"]);
        build(&BuildOptions {
            repo: repo.clone(),
            out: out.clone(),
            run_id: "scope-run".to_owned(),
            generated_at: "2026-09-04T00:00:00Z".to_owned(),
            archive_stamp: "20260904T000000Z".to_owned(),
            verifier_program: PathBuf::from("/usr/local/bin/devcoordinator2-tooling"),
            batch_size: 8,
            max_batch_bytes: 60_000,
            collection: CollectOptions {
                include_config: true,
                exclude_globs: vec!["src/tool.py".to_owned()],
                ..Default::default()
            },
            coverage_reports: Vec::new(),
            assurance_input: None,
        })
        .unwrap();
        complete_ledger(&out);
        let result = verify(&out.join("manifest.json"), &[out.join("reports")], false).unwrap();
        assert!(
            !result["issues"]["excluded_file_issues"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
